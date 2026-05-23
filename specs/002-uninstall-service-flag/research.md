# Research: Uninstall systemd user unit (`setup --uninstall-service`)

**Feature**: 002-uninstall-service-flag
**Date**: 2026-05-23

This feature does not contain any `NEEDS CLARIFICATION` markers — the spec was complete on
first pass. This document captures the decisions made implicitly during planning so they
survive code review and future revisits.

## Decisions

### D1 — Flag on `setup`, not a new subcommand

**Decision**: extend the existing `Setup` subcommand with a second flag
(`--uninstall-service`), rather than adding a new top-level subcommand
(`air-drive uninstall-service` or `air-drive uninstall`).

**Rationale**:

- Direct symmetry with `--install-service` keeps the install/uninstall pair discoverable
  from the same `air-drive setup --help` page.
- The spec explicitly puts a separate `air-drive uninstall` (wipe everything) **out of
  scope**. A future feature can add that as a top-level subcommand without conflicting
  with this flag.
- No new dispatcher wiring; the change is one extra argument and one extra branch inside
  `setup::run`.

**Alternatives considered**:

- *New subcommand `air-drive uninstall-service`*: rejected — multiplies the surface area,
  splits the discoverability of install vs uninstall across two `--help` pages.
- *Repurpose `--install-service` with a `--no` prefix flag*: rejected — clap supports
  `--no-install-service` semantics but the inversion is confusing for a verb-style flag
  that has its own side effects.

### D2 — Mutual exclusion enforced by clap

**Decision**: declare the two flags as mutually exclusive at the argument-parser level
using clap's `conflicts_with = "install_service"`. Passing both produces a clap-formatted
error and a non-zero exit, before any side-effect code runs.

**Rationale**:

- FR-008 mandates rejection of `--install-service --uninstall-service` in the same call.
- Clap's built-in conflict handling produces a consistent error message format that
  matches the rest of the CLI; no custom validation logic.
- Fails fast — no risk of partial state where install runs, then uninstall, then leaves
  the unit half-applied.

**Alternatives considered**:

- *Runtime check inside `setup::run`*: rejected — duplicates logic clap already provides
  for free, and produces a less polished error message.

### D3 — Tolerate missing `systemctl` (graceful degradation)

**Decision**: when `Command::new("systemctl").output()` returns
`Err(io::ErrorKind::NotFound)`, log a `tracing::warn!` line and continue to the file
removal step. The command exits 0 even though the systemd interactions were skipped.

**Rationale**:

- FR-007 mandates this behaviour.
- A unit file can exist on a non-systemd host (someone copied it manually, ran inside a
  container with a leftover from the host, etc.) — removing the file is still a useful
  outcome and the only thing the command can do.
- Aligned with the existing install path, which already surfaces "could not invoke
  `systemctl` (not on a systemd host?)" — install treats it as an error, but install
  cannot proceed without systemd; uninstall *can* (file deletion is still meaningful).

**Alternatives considered**:

- *Mirror the install path's hard-fail on missing `systemctl`*: rejected — would block
  legitimate cleanup on a host that has a stale file but no init system.

### D4 — Do not verify file contents before removal

**Decision**: if the unit file at the user-scope path exists, remove it unconditionally.
Do not hash-check it against the bundled `SYSTEMD_UNIT_TEMPLATE` first.

**Rationale**:

- FR-003 explicitly allows removal of edited files. The user opted in by passing
  `--uninstall-service`.
- A hash check would add complexity (where does the user's edits get backed up? Do we
  fail the command? Prompt?) without delivering proportional safety.
- Symmetric with the install path, which overwrites any existing file without comparison.

**Alternatives considered**:

- *Refuse to remove a file that differs from the template; require `--force`*: rejected —
  premature gating, the user already passed an explicit uninstall flag.
- *Move the edited file to a `.bak` next to itself*: rejected — adds clutter the user
  didn't ask for; if they had edits worth preserving, version control or a backup tool
  is the right place.

### D5 — Synchronous subprocess invocation (not async)

**Decision**: keep `std::process::Command` (blocking) for the new `systemctl` calls,
matching the existing `install_systemd_unit`. Do **not** switch to
`tokio::process::Command`.

**Rationale**:

- The CLI dispatcher is `async` purely because the daemon's `start` subcommand needs it
  (Tokio runtime is already running). The `setup` subcommand has no concurrent work; the
  three `systemctl` calls run strictly in sequence and there is nothing else to overlap.
- `std::process::Command::output()` is the cleanest API for "spawn, wait, capture
  stdout/stderr, return exit status". The async equivalent adds a future and an
  `await` for zero benefit here.
- Consistency with `install_systemd_unit` makes the diff smaller and the code more
  reviewable.

**Alternatives considered**:

- *Use `tokio::process::Command` for consistency with the rest of the codebase*:
  rejected — the rest of the codebase uses tokio because it has concurrent I/O; this code
  path has none.

### D6 — Three sequential `systemctl` calls, not one

**Decision**: invoke `systemctl --user disable --now air-drive.service` first, then
remove the file, then `systemctl --user daemon-reload`.

**Rationale**:

- `disable --now` collapses stop + disable into one call (the install path already uses
  the symmetric `enable --now`).
- `daemon-reload` after removing the file is what makes `systemctl --user
  list-unit-files` stop reporting the unit immediately. Without it, systemd holds a
  stale reference to the now-deleted file until next boot.
- The order matters: stopping before removing the file means systemd has a coherent view
  of what it is doing; removing the file before daemon-reload means daemon-reload sees
  the absence and prunes its cache.

**Alternatives considered**:

- *Skip `daemon-reload`*: rejected — leaves systemd reporting a stale unit until the
  next session restart.
- *`reset-failed` after disable*: rejected — only useful when a unit is in the failed
  state, which is not relevant for normal uninstall.

### D7 — Testing strategy: stub `systemctl` on `$PATH`

**Decision**: the integration test creates a temporary directory containing a shell-script
shim named `systemctl` that records its arguments to a file and exits 0, then prepends
that directory to `$PATH` before invoking the binary. The test also points
`$XDG_CONFIG_HOME` at a temporary directory to control where the unit file lands.

**Rationale**:

- The CI runners are Linux but may not have a user-scope systemd session available;
  invoking the real `systemctl --user` in CI is unreliable.
- A shim shows exactly which arguments the production code sent (covers FR-002 and
  FR-004 — `disable --now` and `daemon-reload` were actually called) without depending
  on live systemd behaviour.
- The `directories::BaseDirs` API honours `$XDG_CONFIG_HOME` (already verified by the
  existing install test pattern).

**Alternatives considered**:

- *Mock `Command::new` via a trait*: rejected — overkill for a single call site; would
  force a refactor of the install path for symmetry, doubling the diff.
- *Run the real `systemctl --user` inside a per-CI-job systemd user session*: rejected —
  fragile, slow, distribution-dependent.

## Open Questions

None. The spec resolved everything in scope; out-of-scope items (full `air-drive
uninstall`, packaging hooks) are explicitly deferred to future features.
