# Configuration

air-drive reads a single TOML file, `config.toml`, from its config directory.
Every section and key is optional — a missing file is equivalent to all
defaults. Unknown keys are **rejected** (`deny_unknown_fields`), so a typo fails
loudly instead of being silently ignored.

## On-disk locations

air-drive follows the XDG base directory spec. Three directories are tracked:

| Directory   | Default                          | Holds                                                        |
| ----------- | -------------------------------- | ----------------------------------------------------------- |
| **config**  | `$XDG_CONFIG_HOME/air-drive` (`~/.config/air-drive`) | `config.toml`, `tokens.json`, `state.db`, `lock` |
| **cache**   | `$XDG_CACHE_HOME/air-drive` (`~/.cache/air-drive`)   | the downloaded `rclone` binary (`bin/rclone`)    |
| **runtime** | `$XDG_RUNTIME_DIR/air-drive` (falls back to `<config>/runtime`) | the control socket (`control.sock`)   |

Directories are created with mode `0700` (owner-only — the config dir holds
`tokens.json`); `config.toml` is written `0644`.

The `--config-dir` flag overrides only the **config** directory; cache and
runtime stay on their XDG defaults, so relocating config doesn't split your
rclone cache.

## Sections

### `[oauth]` — OAuth client override

Override the project-owned OAuth client with your own Google Cloud Desktop
client. Leave both unset to use the embedded default. See
[OAuth setup](oauth-setup.md).

| Key             | Type   | Default | Description                                                            |
| --------------- | ------ | ------- | --------------------------------------------------------------------- |
| `client_id`     | string | *(embedded)* | Your Google Cloud OAuth `client_id`.                            |
| `client_secret` | string | *(embedded)* | Companion secret for the Desktop client (distributed with the app, not actually secret). Set together with `client_id`. |

### `[mapping]` — folder mapping display info

Display metadata for the mapped pair. The authoritative `remote_folder_id` and
`remote_folder_spec` live in `state.db`, not here.

| Key                       | Type    | Default | Description                                                                 |
| ------------------------- | ------- | ------- | --------------------------------------------------------------------------- |
| `local_path`              | string  | *(none)* | Absolute path of the watched local folder (display only).                  |
| `remote_folder_name`      | string  | *(none)* | Human-readable remote folder path (display only).                           |
| `auto_create_remote_root` | bool    | `false` | When `true`, `map` creates missing `path:`-notation segments on Drive without prompting. Only applies to `path:` notation — bare IDs and URLs reference a specific resource that cannot be synthesised. |

### `[daemon]` — runtime tuning

| Key                            | Type   | Default | Description                                                                      |
| ------------------------------ | ------ | ------- | ------------------------------------------------------------------------------- |
| `remote_poll_interval_seconds` | int    | `30`    | How often the daemon polls Drive `changes.list`. Clamped to `[10, 60]` at startup. |
| `safety_net_interval_seconds`  | int    | `300`   | Interval of the safety-net reconciliation cycle. Must stay ≥ 5 min (constitution principle II). |
| `log_file`                     | string | `""`    | Optional log file path; empty disables file logging (stderr only).               |

### `[rclone]` — engine binary override

| Key           | Type   | Default | Description                                                                            |
| ------------- | ------ | ------- | ------------------------------------------------------------------------------------- |
| `path`        | string | *(none)* | Absolute path to a user-provided `rclone`. When set, the daemon uses it instead of probing `$PATH` / cache / downloading. |
| `min_version` | string | *(none)* | Minimum acceptable rclone version (informational; the actual check uses a compiled-in constant). |

### `[watch]` — local watcher tuning

| Key               | Type        | Default        | Description                                                                  |
| ----------------- | ----------- | -------------- | --------------------------------------------------------------------------- |
| `ignore_patterns` | list<string> | *(seeded, see below)* | Glob patterns matched against the **file name** (not the full path). Matching files are never synced (no upload, rename, or delete propagation). Overriding the list replaces it wholesale. |
| `auto_create_root` | bool       | `false`        | When `true`, `map` / `start` create `mapping.local_path` (and parents) without prompting if missing. When `false`, the CLI prompts; on non-interactive stdin (systemd, piped script) or decline, it refuses to start with an actionable error. |

#### Default ignore patterns

The default `ignore_patterns` cover well-known editor/OS scratch files:

- **vim** — `.*.swp`, `.*.swo`, `.*.swx`, `.*.swn`, `4913`
- **emacs** — `#*#`, `*~`, `.#*`
- **gedit / nautilus** — `.goutputstream-*`
- **LibreOffice** — `.~lock.*#`
- **MS Office** — `~$*`
- **JetBrains** — `*.___jb_tmp___`, `*.___jb_old___`
- **OS metadata** — `.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`

## Auto-migration

On startup air-drive auto-migrates `config.toml` to the current shape,
**preserving your comments**. New keys are added with their defaults; nothing you
set is dropped. This means you can safely upgrade the binary without hand-editing
the config.

## Example

```toml
[oauth]
client_id = "1234567890-abc.apps.googleusercontent.com"
client_secret = "GOCSPX-xxxxxxxxxxxxxxxx"

[mapping]
local_path = "/home/alice/Drive"
remote_folder_name = "My Drive/Sync"
auto_create_remote_root = false

[daemon]
remote_poll_interval_seconds = 30
safety_net_interval_seconds = 300
log_file = ""

[rclone]
path = "/usr/local/bin/rclone"

[watch]
auto_create_root = false
ignore_patterns = [".*.swp", "*~", ".DS_Store"]
```
