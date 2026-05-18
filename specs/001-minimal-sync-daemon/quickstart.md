# Quickstart — Minimal Sync Daemon

Bring up the dev loop on a fresh Linux host. Times below assume a stable home connection.

## Prerequisites

- Linux x86_64 (Ubuntu 22.04+ or equivalent).
- Rust stable via `rustup` (latest).
- `rclone` ≥ 1.65 — **optional**. If found on `$PATH`, it's used as-is; otherwise
  `air-drive` downloads its own copy on first run to `~/.cache/air-drive/bin/rclone` with
  SHA-256 verification. You can also pin a specific binary via `[rclone].path` in
  `config.toml`. See `research.md §5` for the full resolution order.
- A personal Google account with at least one folder you can experiment with on Drive
  (create `Drive/air-drive-test/` first).
- A Google Cloud project with the **Drive API** enabled and an **OAuth client of type
  "Desktop app"**. Note its `client_id` (no `client_secret` needed — we use PKCE).

## 1. Clone and build

```bash
git clone git@github.com:Toilal/air-drive.git
cd air-drive
git switch 001-minimal-sync-daemon
cargo build --release
```

The binary lands at `target/release/air-drive`.

## 2. Configure the OAuth client (development)

During development you'll use your own Google Cloud OAuth client (the embedded
project-owned `client_id` is only baked into release builds). Drop your `client_id` into a
local config:

```bash
mkdir -p ~/.config/air-drive
cat > ~/.config/air-drive/config.toml <<'EOF'
[oauth]
client_id = "PASTE-YOUR-CLIENT-ID-HERE.apps.googleusercontent.com"
EOF
```

## 3. Link the account

```bash
./target/release/air-drive link
```

A browser tab opens against `accounts.google.com`. Grant access to the requested scopes
(`drive.file` + `drive.metadata.readonly`). The terminal logs `link OK; email=…`. Tokens land
in `~/.config/air-drive/tokens.json` (mode `0600`).

## 4. Map a folder pair

```bash
./target/release/air-drive map \
    ~/Drive/sandbox \
    "path:My Drive/air-drive-test"
```

The local path is created if it does not exist. The remote path is resolved to a Drive file
ID and persisted in `state.db`.

## 5. First run with initial sync

```bash
./target/release/air-drive start --initial-sync -vv
```

You should see:

```text
INFO  air_drive::daemon: lock acquired
INFO  air_drive::engine::rclone: rclone path resolved → /home/.../bin/rclone (v1.65.x)
INFO  air_drive::reconcile: initial reconciliation: local=0 remote=42 → 42 downloads queued
INFO  air_drive::engine::rclone: download "doc.pdf" → 1.2 MB in 0.4s
...
INFO  air_drive::daemon: idle (last sync 12s ago, 0 pending)
```

Leave the daemon running.

## 6. Try it out

In a second terminal:

```bash
echo "hello drive" > ~/Drive/sandbox/hello.txt
# within ~10s, hello.txt appears on Drive's web UI
```

On Drive's web UI, create a new file `from-web.txt` in `air-drive-test`. Within ~90s, it
appears in `~/Drive/sandbox/`.

## 7. Inspect status

```bash
./target/release/air-drive status
```

Or machine-readable:

```bash
./target/release/air-drive status --json | jq .
```

## 8. Force a conflict (to exercise FR-006 + SC-006)

1. Stop the daemon (`Ctrl-C`).
2. Edit `hello.txt` locally.
3. Edit `hello.txt` on Drive's web UI.
4. Restart the daemon.

`hello.txt` on disk now contains the Drive version (canonical name). Your local version is
preserved as `hello.conflict-<UTC-ts>.txt`. `air-drive status` lists the conflict.

## 9. Force a recovery (FR-010 + SC-005)

1. While syncing a 100 MB file, `kill -9` the daemon.
2. Restart it. The daemon resumes from the persisted state. No half-written file remains
   under the local path; the upload (or download) restarts where it can.

## 10. Run the test suite

```bash
# Unit + integration (hermetic, no Drive credentials needed)
cargo test

# E2E against a real Drive account (CI-gated)
AIR_DRIVE_E2E_TOKEN=… cargo test --features e2e --test e2e
```

## Troubleshooting

- **`error: another daemon is running (pid 12345)`** — FR-017. Kill the other instance or
  wait for it to drain.
- **`error: tokens.json has insecure permissions (got 0644, want 0600)`** — FR-016. Run
  `chmod 600 ~/.config/air-drive/tokens.json`.
- **`error: re-link required (refresh token revoked)`** — FR-009 / US3.4. Re-run
  `air-drive link`.
- **`warning: skipping native Google Doc "Plans.gdoc"`** — FR-011. Expected; Google Docs are
  out of scope for this feature.
