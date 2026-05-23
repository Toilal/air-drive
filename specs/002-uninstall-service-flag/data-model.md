# Data Model: Uninstall systemd user unit (`setup --uninstall-service`)

**Feature**: 002-uninstall-service-flag
**Date**: 2026-05-23

## Persisted state

**None.** This feature does not introduce, modify, or delete any rows in `state.db`,
any keys in `config.toml`, or any other persistent artifact owned by air-drive.

The two entities listed in the spec — the systemd user unit file and the systemd unit
state — are owned by the user's filesystem and by the systemd user manager respectively.
They are **observed and mutated** by this feature, but they are not part of air-drive's
data model.

## External artifacts touched

| Artifact | Owner | Location | Operations performed |
|---|---|---|---|
| Unit file `air-drive.service` | The user (XDG config dir) | `$XDG_CONFIG_HOME/systemd/user/air-drive.service` (typically `~/.config/systemd/user/air-drive.service`) | Existence check, removal (no read, no write, no rename). |
| Systemd user-scope unit state | `systemd --user` | In-memory, managed by the user systemd manager | `systemctl --user disable --now air-drive.service` (stop + disable); `systemctl --user daemon-reload` (refresh cache). |

## Artifacts explicitly NOT touched

The following are listed for completeness because FR-005 makes their preservation a
hard requirement. Any future maintainer adding code to this feature MUST keep this
list intact.

- `$XDG_CONFIG_HOME/air-drive/config.toml`
- `$XDG_CONFIG_HOME/air-drive/state.db` (and the SQLite `-shm` / `-wal` siblings)
- `$XDG_CONFIG_HOME/air-drive/tokens.json`
- `$XDG_CONFIG_HOME/air-drive/lock`
- The `account` and `folder_mapping` rows in `state.db`
- The watched local folder (whatever `local_path` was configured) and its contents
- The air-drive binary at `~/.local/bin/air-drive` (or wherever the user installed it)

`air-drive unlink` remains the canonical command for clearing the account, tokens, and
mapping. Users wanting a full wipe combine the two commands.

## Schema migrations

None. No schema change is required by this feature.
