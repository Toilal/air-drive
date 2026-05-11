# On-disk Config Contract

Files written to `$XDG_CONFIG_HOME/air-drive/` (default `~/.config/air-drive/`).

## `config.toml` (user-editable)

```toml
# OAuth client override (optional — Q1 clarification: hybrid)
# Remove the entire [oauth] block to use the project's embedded default client_id.
[oauth]
client_id = "1234567890-abc.apps.googleusercontent.com"

# Folder mapping display info. The authoritative remote_folder_id lives in state.db.
[mapping]
local_path = "/home/alice/Drive"
remote_folder_name = "alice@gmail.com / My Drive / Sync"

# Daemon tuning
[daemon]
remote_poll_interval_seconds = 30   # 10..60, default 30
safety_net_interval_seconds = 300   # default 300
log_file = ""                       # empty disables file logging

# rclone binary resolution (optional)
[rclone]
# path = "/usr/local/bin/rclone"    # explicit override; bypasses PATH / cache / download
# min_version = "1.65"              # informational only — code constant is the real gate
```

The `[rclone]` section is optional. When absent, the daemon resolves the rclone binary in
this order: `$PATH`, then `$XDG_CACHE_HOME/air-drive/bin/rclone`, then (unless
`--no-download-rclone` is passed at startup) download from `downloads.rclone.org` with
SHA-256 verification. See `research.md §5` for the full resolution order.

The file is created with mode `0644`. The daemon refuses to start if it contains an unknown
top-level key (forward-compatible: warnings turn into errors only on MAJOR bumps).

## `tokens.json` (owned by `yup-oauth2`, mode `0600`)

Schema is defined by the `yup-oauth2` crate's disk storage. Permissions MUST be `0600`; the
daemon refuses to start otherwise (FR-016).

## `state.db` (SQLite, mode `0600`)

Schema versioned via the `schema_version` table. v1 is defined in `data-model.md`. Opened in
WAL mode (`PRAGMA journal_mode = WAL`). Backups are user-initiated (`sqlite3 state.db
".backup ..."`); the daemon does not snapshot.

## `lock` (single-instance lock, mode `0600`)

Plain text file holding the daemon PID. `flock(LOCK_EX | LOCK_NB)` is the actual lock; the
PID is informational for FR-017 error messages.

## `air-drive.log` (optional, when `daemon.log_file` set or `--log-file` flag)

Plain text, `tracing-subscriber` default format. No rotation in this feature — see
`research.md §9`.
