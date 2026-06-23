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
| `log_file`                     | string | `""`    | Optional log file path; empty disables file logging (stderr only). The `--log-file` flag overrides this. |
| `log_level`                    | string | `""`    | Persistent log level. Empty = unset (the `-v` flags / `RUST_LOG` / the built-in `warn` default apply). A bare level (`"info"`, `"debug"`, …) applies to the `air_drive` target; a value containing `=` is a full `RUST_LOG`-style directive (e.g. `"air_drive=debug,rclone=warn"`). **Precedence**: `RUST_LOG` > `-v` flags > `log_level` > `warn`. |
| `log_format`                   | string | `"text"` | Log record format: `text` (human-readable) or `json` (structured, one object per record — for journald/Loki). Applies to both stderr and the log file. |
| `log_color`                    | string | `"auto"` | ANSI colour policy for the stderr layer: `auto` (colour only on a terminal), `always`, or `never`. The file layer is always colour-free. |

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
| `symlinks`        | `"skip"` \| `"follow"` | `"skip"` | How symlinks under the watched root are handled. See [Symlinks](#symlinks). |

#### Symlinks

By default (`symlinks = "skip"`) symlinks under the watched root are ignored
entirely — not uploaded, and not reported.

Set `symlinks = "follow"` to resolve each symlink to its target and sync it as a
regular file or directory (the target's bytes are uploaded). Two safety rails
apply automatically:

- **Escape guard** — a link whose target resolves **outside** the watched root
  is skipped, so a stray symlink can't pull unrelated files into the sync.
- **Cycle guard** — directory-symlink loops (a link pointing back to an
  ancestor, mutually-referential links) are detected and broken, so a walk can't
  recurse forever.

Note that following a **directory** symlink syncs its contents, but live edits
*inside* a symlinked directory are picked up on the next start / safety-net pass
rather than instantly — the inotify watcher does not descend through symlinks.

Recording the link itself (a `preserve` mode) is not yet supported; Drive has no
native symlink type ([issue #2](https://github.com/Toilal/air-drive/issues/2)).

#### Default ignore patterns

The default `ignore_patterns` cover well-known editor/OS scratch files:

- **vim** — `.*.swp`, `.*.swo`, `.*.swx`, `.*.swn`, `4913`
- **emacs** — `#*#`, `*~`, `.#*`
- **gedit / nautilus** — `.goutputstream-*`
- **LibreOffice** — `.~lock.*#`
- **MS Office** — `~$*`
- **JetBrains** — `*.___jb_tmp___`, `*.___jb_old___`
- **OS metadata** — `.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`

## Native Google Docs

Google's own formats — Docs, Sheets, Slides, Drawings, Forms — are not stored as
regular files on Drive and have no downloadable bytes, so they can't be synced
like ordinary documents. air-drive represents each one locally as a small
**shortcut file**: a JSON pointer named after the doc with a type-specific
extension (a Doc `Notes` → `Notes.gdoc`, a Sheet `Budget` → `Budget.gsheet`,
Slides → `.gslides`, anything else → `.glink`). Opening it gives you the doc's
web URL so you can jump straight to it in the browser.

These shortcuts are **one-way**: air-drive creates, renames, and removes them to
follow the doc on Drive, but never uploads a shortcut back. Editing or deleting a
`.gdoc` file locally does **not** change the underlying Google Doc. Every shortcut
is listed under the `skipped` section of [`air-drive status`](cli.md#status) so
these files are visible rather than silently missing.

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
log_level = "air_drive=debug,rclone=warn"
log_format = "text"
log_color = "auto"

[rclone]
path = "/usr/local/bin/rclone"

[watch]
auto_create_root = false
ignore_patterns = [".*.swp", "*~", ".DS_Store"]
symlinks = "skip"
```
