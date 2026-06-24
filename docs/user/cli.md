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

Run the daemon in the foreground (the default), or in the background with
`-d`/`--detached`.

```sh
air-drive start [--remote-poll-interval <SECONDS>] [-d|--detached]
```

On the **first** start of a mapping (the Drive change cursor is empty), the
daemon performs the initial reconciliation pass automatically. On an interactive
terminal it asks for confirmation first — so a wrong `local_path` or an
unexpected full download can be vetoed — and proceeds without prompting when
stdin is not a TTY (systemd, scripts, CI). Declining at the prompt exits without
starting.

- `--remote-poll-interval <SECONDS>` — override
  `[daemon].remote_poll_interval_seconds` (clamped to `10..=60`).
- `-d`, `--detached` — fork the daemon into the background and return
  immediately. Stdout/stderr are redirected to `<config-dir>/daemon.log` and the
  process is placed in its own process group so it survives the launching shell.
  Stop it with [`air-drive stop`](#stop). For a supervised, always-on service
  prefer the systemd unit (`air-drive setup --install-service`).

### `pause` / `resume`

Signal a running daemon to pause or resume sync via the control socket.

```sh
air-drive pause
air-drive resume
```

### `stop`

Stop a running daemon by sending it `SIGTERM` — the same graceful shutdown as
Ctrl-C or `systemctl stop`. The target PID is read from the single-instance lock
file. Exits `7` when no daemon is running on this config dir.

```sh
air-drive stop
```

### `status`

Print the current daemon state (account, mapping, pending counters, blocked
state, unresolved conflicts, and any skipped items).

```sh
air-drive status [--json]
```

- `--json` — emit machine-readable JSON instead of the human summary.

The `skipped` section lists **native Google Docs** (Docs, Sheets, Slides, …),
which cannot be synced as plain files and are instead represented locally as
shortcut files — see [configuration.md](configuration.md#native-google-docs).

### `unlink`

Remove the linked account, tokens, and mapping. Refuses while the daemon is
running.

```sh
air-drive unlink [-y|--yes]
```

- `-y`, `--yes` — skip the interactive confirmation prompt.

### `setup`

Interactive first-time setup (`link` → `map` → `start`), and systemd service
management.

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
into `config.toml`. Needed when the embedded `client_id` is unusable. The wizard
asks whether your account is in a Google Workspace org and branches between an
**Internal** audience and an **External**-plus-**publish-to-Production** flow, so
the resulting client isn't capped to 7-day token lifetimes. See
[OAuth setup](oauth-setup.md).

```sh
air-drive init [--force] [--link]
```

- `--force` — overwrite an existing `[oauth].client_id`.
- `--link` — run `air-drive link` immediately after writing the config.

### `shell`

Manage desktop shell integration: a per-file sync-status emblem in the file
manager. Today this targets **GNOME Files (Nautilus)** on Linux (the Ubuntu/GNOME
default); other desktops report "not yet supported" rather than half-installing.

```sh
air-drive shell install [--skip-deps]
air-drive shell uninstall
air-drive shell status
```

- `install` — detects the platform and file manager, ensures the
  `python3-nautilus` bridge is present (installing it via the host package
  manager when run on a terminal, or printing the exact command otherwise), and
  deploys the extension to
  `~/.local/share/nautilus-python/extensions/air-drive-overlay.py`. Fully restart
  the file manager to load it — `killall nautilus` (a plain `nautilus -q` can
  leave a cached background instance), or log out and back in.
  - `--skip-deps` — deploy the extension only; don't try to install
    `python3-nautilus` (use when you manage packages yourself).
- `uninstall` — remove the deployed extension (idempotent). Leaves the shared
  `python3-nautilus` system package installed.
- `status` — report what's detected (platform, file manager, dependency,
  extension) without changing anything.

The extension reads each file's status from the running daemon over its control
socket and paints an emblem: synced, syncing/pending, or conflict. With no daemon
running (or a file outside the mapping) it shows no emblem.

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
| 7    | `NoDaemonRunning`       | `pause` / `resume` / `stop` invoked but no running daemon found. |
| 8    | `UnlinkWhileRunning`    | `unlink` refused because the daemon is running.                |
