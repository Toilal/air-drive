# CLI Surface Contract

This document is the authoritative source for the public command surface (FR-018). Implementation MUST match exactly — argument names, exit codes, and output shape are part of the contract and break compat when changed.

## Binary

`air-drive` — invoked as `air-drive <subcommand> [args...]`.

## Global flags

| Flag | Description |
|---|---|
| `--config-dir <path>` | Override the XDG config directory. Default: `$XDG_CONFIG_HOME/air-drive` (typically `~/.config/air-drive`). |
| `--log-file <path>` | Duplicate logs to this file in addition to stderr. Default: disabled. |
| `--verbose`, `-v` | Increase log verbosity. Repeatable: `-v` = info, `-vv` = debug, `-vvv` = trace. Default: warn. |
| `--help`, `-h` | Print help. |
| `--version`, `-V` | Print version and exit `0`. |

## Subcommands

### `air-drive link`

Run the OAuth flow to link a Google Drive account. Idempotent: re-running re-runs consent.

```text
USAGE: air-drive link [--account-label <label>]
```

| Flag | Description |
|---|---|
| `--account-label <label>` | Human-friendly label stored alongside the account. Default: the user's primary email captured from `about.user`. |

Exit codes: `0` on success, `2` on OAuth error, `3` on network failure.

### `air-drive map`

Record the local↔remote folder mapping. Replaces any existing mapping (the MVP supports exactly one).

```text
USAGE: air-drive map <local-path> <remote-folder>
```

| Argument | Description |
|---|---|
| `<local-path>` | Absolute or `~`-expanded path. Must exist and be a directory. |
| `<remote-folder>` | One of: Drive file ID, Drive folder URL, or path notation `path:My Drive/Sync`. The CLI resolves it to an ID by calling `files.get` / `files.list`. |

Exit codes: `0` on success, `4` if the local path doesn't exist or isn't a directory, `5` if the remote folder cannot be resolved.

### `air-drive start`

Run the daemon in the foreground. Blocks until SIGTERM / SIGINT. Acquires the single-instance lock (FR-017).

```text
USAGE: air-drive start [--initial-sync] [--remote-poll-interval <seconds>]
```

| Flag | Description |
|---|---|
| `--initial-sync` | If the daemon has never run on this mapping, perform the initial reconciliation. If omitted on a never-mapped DB, the daemon refuses to start. |
| `--remote-poll-interval <seconds>` | Override `daemon.remote_poll_interval_seconds` from config (10..60). |

Exit codes: `0` on clean shutdown, `1` on uncaught error, `6` if the single-instance lock is held by another live PID (FR-017).

### `air-drive pause`

Signal a running daemon to stop sync activity while keeping watchers attached.

```text
USAGE: air-drive pause
```

Exit codes: `0` on success, `7` if no running daemon is found.

### `air-drive resume`

Signal a paused daemon to resume sync activity.

```text
USAGE: air-drive resume
```

Exit codes: `0` on success, `7` if no running daemon is found.

### `air-drive status [--json]`

Print the current daemon state. Default output is human-readable; `--json` emits machine-readable JSON per `status.schema.json`.

```text
USAGE: air-drive status [--json]
```

Exit codes: `0` always (status output is the carrier of any error info).

### `air-drive unlink`

Remove the linked account, delete the OAuth tokens file, and clear the folder mapping from the state DB. The local watched folder contents are **not** touched.

```text
USAGE: air-drive unlink [--yes]
```

| Flag | Description |
|---|---|
| `--yes`, `-y` | Skip the interactive confirmation prompt (useful for scripts). |

Exit codes: `0` on success, `8` if a daemon is currently running against this config (refuse to unlink while running; the user must `air-drive pause` and stop the daemon first).

### `air-drive setup`

Interactive wrapper: runs `link`, prompts for the local and remote folders, runs `map`, then `start --initial-sync`. Intended for first-time use.

```text
USAGE: air-drive setup [--install-service]
```

| Flag | Description |
|---|---|
| `--install-service` | After successful setup, install the systemd user unit `~/.config/systemd/user/air-drive.service` and enable it for the current user. |

Exit codes: forwards the first non-zero exit code from the underlying commands.

## Stability promise

This MVP ships the commands above as a stable v0 surface. Adding new subcommands and new optional flags is backward-compatible. Removing or renaming a subcommand, a flag, or changing an exit code is a breaking change and requires a MAJOR version bump.
