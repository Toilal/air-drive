# `tests/e2e/` — real Google Drive + real rclone

This suite exists because the mocked integration tests (`tests/integration/`) bypass
two non-trivial pieces of code:

- The `RcloneEngine` subprocess invocation against an actual `rclone` binary —
  including the `RCLONE_CONFIG_AIRDRIVE_*` env-var handoff and the `lsjson` parse.
- The OAuth refresh dance against Google's real token endpoint.

These tests exercise the production code path with no overrides. They're
`#[ignore]`d so they don't fire on every `cargo test`, and they self-skip when the
required env vars are missing.

## Scenarios

| Test | What it validates |
|------|--------------------|
| `e1_link_reaches_real_drive` | `air-drive link` reaches `about.user` and persists a real email. |
| `e2_initial_sync_uploads_via_rclone` | Local file → Drive via `rclone copyto`, md5 verified through `files.list`. |
| `e3_initial_sync_downloads_via_rclone` | File seeded on Drive → fresh local dir via `rclone copyto`, byte content verified. |

## Required env vars

| Name | Content | GitHub Secret |
|------|---------|---------------|
| `AIR_DRIVE_E2E_TOKENS` | Contents of `tokens.json` (refresh token JSON). | `AIR_DRIVE_E2E_TOKENS` |
| `AIR_DRIVE_E2E_CLIENT_ID` | GCP OAuth Desktop client id. | `AIR_DRIVE_E2E_CLIENT_ID` |
| `AIR_DRIVE_E2E_PARENT_FOLDER_ID` | Drive folder ID under which tests create their per-run sub-folder. | `AIR_DRIVE_E2E_PARENT_FOLDER_ID` |

If any of the three is unset or empty, every test prints a `[e2e]` skip notice and
returns success. So running `cargo test -- --ignored` on a dev machine without
credentials is safe.

## One-time setup

### Automated path (recommended)

After completing the manual GCP steps below (you'll have a `client_id` in hand),
a single command handles everything else — the OAuth dance, parent-folder
creation, and pushing the three secrets to GitHub:

```sh
cargo run --example setup_e2e -- \
    --client-id <YOUR_OAUTH_DESKTOP_CLIENT_ID> \
    --config-dir /tmp/air-drive-e2e-setup
```

What the script does:

1. Checks `gh auth status` (you need to be `gh auth login`-ed).
2. Drives the OAuth dance — your default browser opens, you sign in as the test
   account, approve. `tokens.json` is written to the `--config-dir` at `0600`.
3. Looks for an existing `air-drive-e2e-parent` folder under My Drive; reuses
   it if found, otherwise creates one. Idempotent across re-runs.
4. Pushes `AIR_DRIVE_E2E_TOKENS`, `AIR_DRIVE_E2E_CLIENT_ID`, and
   `AIR_DRIVE_E2E_PARENT_FOLDER_ID` to the current repository's Actions
   secrets via `gh secret set`. Pass `--repo owner/name` to target a different
   repo.

Pass `--dry-run` to see the resolved values without pushing. Pass
`--force-new-token` if the cached refresh token expired and you need to
re-approve.

What it can't do (do it once in the Cloud Console):

- Create the GCP project.
- Enable the Drive API.
- Create the OAuth **Desktop** client (`gcloud` has no command for that
  specific client type — only IAP/web clients are scriptable).

The remainder of this section is the manual click-by-click for the four
prerequisite GCP steps.

### 1. Dedicated Google account

Don't use your personal account. Create something like
`air-drive-ci+yourhandle@gmail.com`. The test parent folder lives in this account's
My Drive.

### 2. Google Cloud project + OAuth credentials

1. <https://console.cloud.google.com/> → **New project**, name it `air-drive-ci`.
2. **APIs & Services → Library → Google Drive API → Enable**.
3. **APIs & Services → OAuth consent screen**:
   - User type: **External**, status: **Testing** (Google's review path isn't
     needed for ourselves).
   - Add the test account email to **Test users**.
   - Scopes: add `.../auth/drive.file` and `.../auth/drive.metadata.readonly`.
4. **APIs & Services → Credentials → Create credentials → OAuth client ID**:
   - Application type: **Desktop app**.
   - The resulting client id is the `AIR_DRIVE_E2E_CLIENT_ID` value.
   - **No client secret** — PKCE handles authentication; the secret is unused.

### 3. Parent folder

Sign in to Drive as the test account. Create a folder named
`air-drive-e2e-parent` (or whatever). Open it, copy the folder ID from the URL
(`https://drive.google.com/drive/folders/<ID>`). That's
`AIR_DRIVE_E2E_PARENT_FOLDER_ID`.

### 4. Acquire the token + create the parent folder + push the secrets

Use the automated path from the top of this section:

```sh
cargo run --example setup_e2e -- \
    --client-id <YOUR_AIR_DRIVE_E2E_CLIENT_ID> \
    --config-dir /tmp/air-drive-e2e-setup
```

If you'd rather do it by hand (e.g. for a forked CI account you don't have
`gh` access to), the legacy manual flow still works:

1. Run `air-drive link --config-dir /tmp/e2e-setup` to drive the OAuth dance.
2. Capture `cat /tmp/e2e-setup/tokens.json` as the `AIR_DRIVE_E2E_TOKENS` value.
3. Create the parent folder manually in the Drive web UI, copy its ID.
4. In repo settings, `Settings → Secrets and variables → Actions → New
   repository secret`, add the three secrets:
   - `AIR_DRIVE_E2E_TOKENS`
   - `AIR_DRIVE_E2E_CLIENT_ID`
   - `AIR_DRIVE_E2E_PARENT_FOLDER_ID`

The `.github/workflows/e2e.yml` workflow picks them up from
`secrets.AIR_DRIVE_E2E_*` regardless of how they got there.

## Running locally

```sh
export AIR_DRIVE_E2E_TOKENS="$(cat /tmp/e2e-setup/tokens.json)"
export AIR_DRIVE_E2E_CLIENT_ID="..."
export AIR_DRIVE_E2E_PARENT_FOLDER_ID="..."

cargo test --test rclone_drive -- --ignored --nocapture
```

`rclone` MUST be on `$PATH`. Install with `sudo apt install rclone` on Debian /
Ubuntu, or grab the binary from <https://rclone.org/install>.

## CI

`.github/workflows/e2e.yml` runs on `push to main` and via
`workflow_dispatch`. It installs `rclone` via apt and pipes the secrets in. PRs
don't trigger it — quota stays predictable, and forked PRs can't access the
secrets anyway.

A concurrency group (`e2e-drive`) serialises runs against the same Drive account
so two simultaneous merges don't fight for quota.

## Isolation + cleanup

Each test creates a fresh sub-folder named `air-drive-e2e-<timestamp>-<hex>`
under the configured parent. The harness:

- Sweeps any `air-drive-e2e-*` folder older than 24 h **before** creating its
  own — this rescues space leaked by crashed / cancelled runs.
- Calls `cleanup()` at the end of the test to trash the per-run folder.
- Errors during cleanup are logged but don't fail the test (the sweep on the
  next run picks them up).

## Trade-offs / known limits

- **Quota**: 1 000 requests / 100 s / user. The current scenarios stay well
  below that, but adding many more files per test will eventually need rate
  limiting in the harness.
- **Refresh token expiry**: Google revokes tokens after ~6 months of inactivity,
  and apps in `Testing` mode on the OAuth consent screen cap refresh-token
  lifetime at 7 days. Either re-acquire periodically OR move the consent screen
  to `Production` (requires Google to verify scopes — for a `drive.file`-only
  app this is approval-light).
- **Concurrency**: workflow-level concurrency group avoids parallel runs against
  the same account. Two engineers running locally at the same time can still
  collide; UUID-named folders mitigate the worst of it.
- **No `service account` path**: the spec targets personal Drive sync via
  OAuth — service accounts don't apply (they only see Shared Drives, which
  require a paid Workspace plan).
