# CLI reference

`air-drive` is a single binary with a handful of subcommands. This page is the
canonical reference; run `air-drive <command> --help` for the same information
inline.

```
air-drive [GLOBAL OPTIONS] <COMMAND>
```

## Global options

Available on every subcommand (`global = true`):

| Option                  | Description                                                                                  |
| ----------------------- | -------------------------------------------------------------------------------------------- |
| `--config-dir <PATH>`   | Override the XDG config directory (default `$XDG_CONFIG_HOME/air-drive`). Only the **config** dir moves; cache and runtime stay on XDG defaults. |
| `--log-file <PATH>`     | Duplicate logs to this file in addition to stderr.                                            |
| `-v`, `--verbose`       | Increase log verbosity. Repeatable: `-v` info, `-vv` debug, `-vvv` trace.                     |
| `--no-download-rclone`  | Disable the post-install rclone auto-download. The daemon fails at startup if `rclone` is not found via config / `$PATH` / cache. |
| `--version`             | Print the version.                                                                            |
| `-h`, `--help`          | Print help.                                                                                   |

## Commands

### `link`

Link a Google Drive account via the desktop OAuth flow (opens your browser).

```sh
air-drive link [--account-label <LABEL>]
```

- `--account-label <LABEL>` — human-friendly label stored alongside the
  account. Defaults to the captured email.

### `map`

Record the local↔remote folder mapping.

```sh
air-drive map <LOCAL_PATH> <REMOTE_FOLDER>
```

- `<LOCAL_PATH>` — local folder path. Created on the fly if missing when
  `[watch].auto_create_root = true`; otherwise you are prompted (or it errors on
  non-interactive stdin).
- `<REMOTE_FOLDER>` — one of:
  - a Drive **file ID**,
  - a Drive **folder URL**,
  - `path:My Drive/Sync` notation. Missing path segments are created when
    `[mapping].auto_create_remote_root = true`, otherwise you are prompted.

### `start`

Run the daemon in the foreground.

```sh
air-drive start [--initial-sync] [--remote-poll-interval <SECONDS>]
```

- `--initial-sync` — perform the initial reconciliation pass if the Drive change
  cursor is empty.
- `--remote-poll-interval <SECONDS>` — override
  `[daemon].remote_poll_interval_seconds` (clamped to `10..=60`).

### `pause` / `resume`

Signal a running daemon to pause or resume sync via the control socket.

```sh
air-drive pause
air-drive resume
```

### `status`

Print the current daemon state (account, mapping, pending counters, blocked
state).

```sh
air-drive status [--json]
```

- `--json` — emit machine-readable JSON instead of the human summary.

### `unlink`

Remove the linked account, tokens, and mapping. Refuses while the daemon is
running.

```sh
air-drive unlink [-y|--yes]
```

- `-y`, `--yes` — skip the interactive confirmation prompt.

### `setup`

Interactive first-time setup (`link` → `map` → `start --initial-sync`), and
systemd service management.

```sh
air-drive setup [--install-service | --uninstall-service]
```

- `--install-service` — install the systemd user unit at
  `~/.config/systemd/user/air-drive.service` and enable it.
- `--uninstall-service` — reverse of the above: stop and disable the unit,
  remove the file, refresh the user-scope cache. Idempotent (exits 0 with
  nothing to remove). Leaves config, state, tokens, account, mapping, and binary
  untouched. Mutually exclusive with `--install-service`.

### `init`

Bootstrap a personal Google Cloud OAuth client (Desktop) and write `[oauth]`
into `config.toml`. Needed when the embedded `client_id` is unusable. See
[OAuth setup](oauth-setup.md).

```sh
air-drive init [--force] [--link]
```

- `--force` — overwrite an existing `[oauth].client_id`.
- `--link` — run `air-drive link` immediately after writing the config.

## Exit codes

| Code | Name                    | Meaning                                                        |
| ---- | ----------------------- | -------------------------------------------------------------- |
| 0    | `Ok`                    | Success.                                                       |
| 1    | `GenericError`          | Uncaught error.                                                |
| 2    | `OauthError`            | OAuth error during `link`.                                     |
| 3    | `NetworkError`          | Network failure during `link`.                                 |
| 4    | `MapLocalInvalid`       | Local path supplied to `map` doesn't exist or isn't a directory. |
| 5    | `MapRemoteUnresolvable` | Remote folder supplied to `map` cannot be resolved.           |
| 6    | `LockHeld`              | Single-instance lock held by another live daemon.             |
| 7    | `NoDaemonRunning`       | `pause` / `resume` invoked but no running daemon found.        |
| 8    | `UnlinkWhileRunning`    | `unlink` refused because the daemon is running.                |
