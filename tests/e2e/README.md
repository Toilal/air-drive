# `tests/e2e/` — real Google Drive + real rclone

This suite exists because the mocked integration tests (`tests/integration/`)
bypass two non-trivial pieces of code:

- The `RcloneEngine` subprocess invocation against an actual `rclone` binary —
  the `RCLONE_CONFIG_AIRDRIVE_*` env-var handoff, the `copyto` / `moveto`
  / `delete` invocations, and the metadata-lookup-then-`copyto` dance the
  download path uses (rclone Drive addresses by path, we have an id).
- The OAuth refresh dance against Google's real token endpoint, including the
  Desktop-OAuth quirk that Google requires a `client_secret` even though PKCE
  handles the actual auth proof.

These tests exercise the production code path with no test-only overrides.
They're `#[ignore]`d so they don't fire on every `cargo test`, and they
self-skip when the required env vars are missing.

## Scenarios

| Test | What it validates |
|------|--------------------|
| `e1_link_reaches_real_drive` | `air-drive link` reaches `about.user` and persists a real email. |
| `e2_initial_sync_uploads_via_rclone` | Local file → Drive via `rclone copyto`, md5 verified through `files.list`. |
| `e3_initial_sync_downloads_via_rclone` | File seeded on Drive → fresh local dir via `rclone copyto`, byte content verified. |
| `e4_initial_sync_creates_empty_local_dir_on_drive` | An empty local dir is created as a folder on Drive (#1). |
| `e5_initial_sync_creates_empty_drive_dir_locally` | An empty Drive folder is materialised locally (#1). |
| `e6_initial_sync_uploads_nested_file_into_created_dir` | A nested file lands in a Drive folder created on the fly (dir-create + upload interplay, #1). |
| `e7_local_dir_rename_propagates_via_rclone` | A folder renamed locally moves the same Drive folder (continuous daemon, #7). |
| `e8_remote_dir_rename_propagates_locally` | A folder renamed on Drive moves the local dir (continuous daemon, #7). |
| `e9_remote_trash_then_restore_does_not_duplicate` | A file trashed on Drive is removed locally; restoring re-links it with no duplicate, same id (#8). |
| `e10_native_google_doc_becomes_local_shortcut` | A native Google Doc on Drive becomes a local `.gdoc` shortcut and is never uploaded back (#3). |

## Required env vars

| Name | Content | GitHub Secret |
|------|---------|---------------|
| `AIR_DRIVE_E2E_TOKENS` | Contents of `tokens.json` (refresh token JSON written by yup-oauth2). | `AIR_DRIVE_E2E_TOKENS` |
| `AIR_DRIVE_E2E_CLIENT_ID` | GCP OAuth Desktop client id (`xxx.apps.googleusercontent.com`). | `AIR_DRIVE_E2E_CLIENT_ID` |
| `AIR_DRIVE_E2E_CLIENT_SECRET` | Companion `client_secret` for the Desktop client (`GOCSPX-...`). Google's token endpoint requires it even in PKCE; the value is distributed with the app and not actually confidential. | `AIR_DRIVE_E2E_CLIENT_SECRET` |
| `AIR_DRIVE_E2E_PARENT_FOLDER_ID` | Drive folder ID under which each test creates a UUID-named scratch sub-folder. | `AIR_DRIVE_E2E_PARENT_FOLDER_ID` |

If any of the four is unset or empty, every test prints a `[e2e]` skip notice
and returns success. So running `cargo test -- --ignored` on a dev machine
without credentials is safe.

Local runs auto-load `<repo>/.env` via `dotenvy::dotenv()` — no manual
`set -a; source .env` step. CI passes the four secrets through the workflow
YAML; `dotenvy` doesn't override existing env, so CI wins over any stray
`.env` left in the checkout.

## One-time setup

### Automated path (recommended)

After completing the manual GCP steps below (you'll have a `client_id` and a
`client_secret` in hand), a single command handles everything else — the
OAuth dance, parent-folder creation, the `.env` write, and pushing the four
secrets to GitHub:

```sh
cargo run --example setup_e2e -- \
    --client-id <YOUR_OAUTH_DESKTOP_CLIENT_ID> \
    --client-secret <YOUR_OAUTH_DESKTOP_CLIENT_SECRET> \
    --config-dir /tmp/air-drive-e2e-setup
```

What the script does, in order:

1. **`gh auth status`** — fail-fast if not `gh auth login`-ed.
2. **OAuth dance via `webbrowser::open`** — a default-browser tab opens on
   the Google consent screen. Sign in with the test account, click **Allow**.
   `tokens.json` (the refresh token JSON) lands in `--config-dir` at `0600`.
3. **Parent folder** — looks for an existing `air-drive-e2e-parent` under My
   Drive; reuses it if found, otherwise creates one. Idempotent.
4. **`.env`** — writes `<cwd>/.env` (override with `--env-file PATH`) at
   `0600` containing all four resolved values in `KEY='VALUE'` form. Always
   written, even with `--dry-run`.
5. **GitHub Secrets** — pushes the four values via `gh secret set --body`.
   Pass `--repo owner/name` to target a different repo. Skipped under
   `--dry-run`.

Useful flags:

- `--dry-run` — keeps everything local (browser, folder, `.env`), skips
  only the GitHub push. Handy when you're iterating on the script.
- `--force-new-token` — wipes the cached `tokens.json` so the OAuth dance
  fires fresh. Use when Google has revoked the refresh token (typical after
  the 7-day window in `Testing` mode of the consent screen).
- `--parent-folder-name <NAME>` — override the default folder name when you
  want to share the same test account across multiple repos.

What the script can't do (do these once in the Cloud Console):

- Create the GCP project.
- Enable the Drive API.
- Create the OAuth **Desktop** client. `gcloud` only scripts IAP and Web
  client types; there's no equivalent for the Desktop variant.

### 1. Dedicated Google account

Don't use your personal account. Create something like
`air-drive-ci+<handle>@gmail.com`. Sign in **with that account** before
proceeding so the GCP console + browser OAuth dance both pick it up.

### 2. Google Cloud project + OAuth credentials

1. <https://console.cloud.google.com/projectcreate> → **New project**, name
   it `air-drive-ci`.
2. <https://console.cloud.google.com/apis/library/drive.googleapis.com> →
   **Enable** the Drive API.
3. <https://console.cloud.google.com/apis/credentials/consent> — configure
   the OAuth consent screen:
   - User type: **External**, status: **Testing** (no Google review needed).
   - **Test users**: add the test account email. Without this step the
     OAuth dance fails with `403 access_denied`.
   - Scopes: `.../auth/drive` (full read/write) is requested at runtime —
     listing it here is optional. The consent prompt will read
     *"See, edit, create, and delete all of your Google Drive files"*.
4. <https://console.cloud.google.com/apis/credentials> → **+ Create
   credentials → OAuth client ID**:
   - Application type: **Desktop app**.
   - Copy the `Client ID` (`xxx.apps.googleusercontent.com`).
   - Copy the `Client secret` (`GOCSPX-...`). Google requires this at the
     token endpoint even in the Desktop / PKCE flow; the value is
     distributed with the app, not actually confidential (cf. rclone,
     gcloud, Insync — all ship a hardcoded one).

### 3. Run `setup_e2e`

```sh
cargo run --example setup_e2e -- \
    --client-id <copied-from-step-2> \
    --client-secret <copied-from-step-2> \
    --config-dir /tmp/air-drive-e2e-setup
```

Once it finishes, the four GitHub Secrets are set, the `.env` is on disk,
and the local e2e suite is ready.

### Manual fallback (no `gh` access)

If you can't use the `gh` CLI (forked CI account, restricted PAT…), do
steps 1–3 above to acquire `tokens.json`, then dry-run the script:

```sh
cargo run --example setup_e2e -- \
    --client-id ... --client-secret ... \
    --config-dir /tmp/e2e-setup --dry-run
```

The `.env` is written, the folder is created, and the dry-run summary
prints the four values. Open `Settings → Secrets and variables → Actions`
in the repo settings and paste each of:

- `AIR_DRIVE_E2E_TOKENS` — content of `/tmp/e2e-setup/tokens.json`
- `AIR_DRIVE_E2E_CLIENT_ID`
- `AIR_DRIVE_E2E_CLIENT_SECRET`
- `AIR_DRIVE_E2E_PARENT_FOLDER_ID`

## Running locally

After `setup_e2e` ran successfully:

```sh
cargo test --test rclone_drive -- --ignored
```

The harness reads `<repo>/.env` automatically via `dotenvy`. `rclone` MUST
be on `$PATH` (`sudo apt install rclone` on Debian / Ubuntu, or grab a
binary from <https://rclone.org/install>).

To re-trigger a fresh OAuth dance (expired refresh token), re-run the
setup script with `--force-new-token` — the `.env` and the secrets are
re-written from the new state.

## CI

`.github/workflows/e2e.yml` runs on `push to main` and via
`workflow_dispatch`. It installs `rclone` via apt and pipes the four
secrets in. PRs don't trigger it — quota stays predictable, and forked
PRs can't access the secrets anyway.

A concurrency group (`e2e-drive`) serialises runs against the same Drive
account so two simultaneous merges don't fight for quota.

Trigger a CI run manually:

```sh
gh workflow run e2e
gh run watch
```

## Isolation + cleanup

Each test creates a fresh sub-folder named
`air-drive-e2e-<unix-ts>-<hex>` under the configured parent. The harness:

- Sweeps any `air-drive-e2e-*` folder older than 24 h **before** creating
  its own — rescues space leaked by crashed / cancelled runs.
- Calls `cleanup()` at the end of every test to trash the per-run folder.
- Errors during cleanup are logged but don't fail the test (the sweep on
  the next run picks them up).

## Trade-offs / known limits

- **Quota**: 1 000 requests / 100 s / user. Current scenarios stay well
  below that; adding many more files per test will eventually need rate
  limiting in the harness.
- **Refresh token expiry**: Google revokes tokens after ~6 months of
  inactivity, and apps in `Testing` mode on the consent screen cap
  refresh-token lifetime at 7 days. Either re-run `setup_e2e
  --force-new-token` periodically, OR move the consent screen to
  `Production` (Google reviews scopes — the full `drive` scope triggers the
  sensitive-scope verification path, not the approval-light track).
- **Concurrency**: the workflow-level concurrency group avoids parallel
  CI runs against the same account. Two engineers running locally at the
  same time can still collide; UUID-named per-run folders mitigate the
  worst of it.
- **No `service account` path**: the spec targets personal Drive sync via
  OAuth — service accounts don't apply (they only see Shared Drives,
  which require a paid Workspace plan).
