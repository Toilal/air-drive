# CLI Contract Delta: `air-drive setup --uninstall-service`

**Feature**: 002-uninstall-service-flag
**Date**: 2026-05-23
**Targets**: `specs/001-minimal-sync-daemon/contracts/cli.md` — section `### air-drive setup`

This document is a **delta**: it describes only the changes to apply to the existing CLI
contract, not the full subcommand reference. The full reference continues to live in
`specs/001-minimal-sync-daemon/contracts/cli.md` and is updated by this feature's
implementation.

## Updated USAGE block

The existing block:

```text
USAGE: air-drive setup [--install-service]
```

is replaced by:

```text
USAGE: air-drive setup [--install-service | --uninstall-service]
```

`--install-service` and `--uninstall-service` are mutually exclusive. Passing both in
the same invocation produces a clap-formatted error and exits non-zero (clap's standard
"the argument 'X' cannot be used with 'Y'" message).

## Updated flag table

The existing table gains one row:

| Flag | Effect |
|---|---|
| `--install-service` | After successful setup, install the systemd user unit `~/.config/systemd/user/air-drive.service` and enable it for the current user. *(unchanged)* |
| `--uninstall-service` | Reverse `--install-service`: stop and disable the user-scope `air-drive.service` unit, remove the unit file from `~/.config/systemd/user/`, and refresh the systemd user-scope cache. Idempotent — exits 0 even when there is nothing to remove. Does NOT touch the daemon's config, state, OAuth tokens, linked account, folder mapping, or watched local folder. |

## Exit codes

The new flag does not introduce new exit codes; it reuses the existing scheme:

| Exit code | Meaning when `--uninstall-service` is invoked |
|---|---|
| `0` | Success — the unit is no longer active, the file is gone, the cache is refreshed. Also returned when there was nothing to remove (idempotent no-op) and when `systemctl` was unavailable but the file was successfully removed (or already absent). |
| `1` (`GenericError`) | Unexpected I/O error while removing the unit file (permission denied, etc.). |

Argument-parser errors (clap) return clap's own exit code (typically `2`), independently
of the air-drive `ExitCode` enum.

## Observable side effects (success path)

When `--uninstall-service` succeeds against a host where the unit was installed and
active:

1. The `air-drive.service` unit is no longer reported by `systemctl --user
   list-unit-files`.
2. The file at `~/.config/systemd/user/air-drive.service` no longer exists.
3. Any air-drive process that systemd was supervising has exited (via the SIGTERM that
   `disable --now` sends).
4. A single human-readable confirmation line is printed to stderr describing what was
   removed (unit file path, brief mention of the stop/disable step).

## Observable side effects (graceful-degradation path)

When `systemctl` is unavailable:

1. A `WARN`-level log line is emitted via `tracing` ("systemctl not found; skipping
   systemd interactions").
2. If the unit file is present, it is still removed.
3. The command exits 0.

## Out of scope for this contract

- System-wide units under `/etc/systemd/system/`. The contract only covers the
  user-scope unit at `$XDG_CONFIG_HOME/systemd/user/`.
- Tear-down of the daemon's persistent state, tokens, account link, or local folder.
  Those remain the responsibility of `air-drive unlink` (existing contract).
- A future `air-drive uninstall` top-level subcommand for one-shot full removal.
