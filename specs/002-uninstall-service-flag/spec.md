# Feature Specification: Uninstall systemd user unit (`setup --uninstall-service`)

**Feature Branch**: `002-uninstall-service-flag`
**Created**: 2026-05-23
**Status**: Draft
**Input**: User description: "Add a symmetric way to uninstall the systemd user unit installed by `air-drive setup --install-service`."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - One-command uninstall of the daemon service (Priority: P1)

A user who previously installed the daemon as a systemd user unit (via `air-drive setup
--install-service`) decides to stop having it auto-start on login — they want to evaluate
another sync tool, free the slot for manual `air-drive start` runs, or simply tear down a
test setup. They expect a single command to undo what `--install-service` did, with the
same level of polish: stop the running unit, disable it, remove the unit file, and clean
up the systemd cache.

**Why this priority**: this is the headline of the feature. The whole point is restoring
symmetry with the existing install flag so the install/uninstall lifecycle is discoverable
from `--help` alone. Without this story the feature does not exist.

**Independent Test**: starting from a host where `air-drive setup --install-service` has
been run successfully and the service is `active (running)`, running `air-drive setup
--uninstall-service` returns exit code 0, the service is no longer listed by `systemctl
--user list-unit-files`, the unit file is gone from the user's systemd config directory,
and no further air-drive process is running on behalf of systemd.

**Acceptance Scenarios**:

1. **Given** the unit is installed and active, **When** the user runs `air-drive setup
   --uninstall-service`, **Then** the running daemon stops, the unit is disabled (no
   auto-start on next login), the unit file is removed from the user-scope systemd config
   directory, the systemd cache is refreshed, and the command exits 0 with a one-line
   confirmation message.
2. **Given** the unit is installed but not active (the user previously stopped it
   manually), **When** they run `air-drive setup --uninstall-service`, **Then** the unit
   file is still removed, the cache is refreshed, and the command exits 0.
3. **Given** the unit file has been hand-edited (the user added env overrides or changed
   the `ExecStart` path), **When** they run `air-drive setup --uninstall-service`,
   **Then** the edited file is still removed without prompting — the user opted into
   removal by invoking the flag.

---

### User Story 2 - Idempotent re-run on a clean system (Priority: P2)

An operator running an automation script (provisioning, ansible playbook, CI cleanup
step) wants to be able to call the uninstall command unconditionally without first
checking whether the unit is installed. Re-running on a host that has nothing to clean up
must succeed silently.

**Why this priority**: scripts and orchestration tools call commands without prior state
inspection. A non-zero exit on "nothing to do" forces every caller to wrap the command in
a conditional, which is friction. Idempotency is a small effort with outsized payoff.

**Independent Test**: on a host where no `air-drive.service` unit exists in the user-scope
systemd directory, running `air-drive setup --uninstall-service` returns exit code 0
within a few seconds with a message indicating nothing was removed; running it again
immediately produces the same outcome.

**Acceptance Scenarios**:

1. **Given** no unit file is present, **When** the user runs the uninstall flag, **Then**
   the command exits 0 with a clear message that there was nothing to remove.
2. **Given** a successful uninstall just completed, **When** the user runs the same
   command a second time, **Then** the second invocation is also a successful no-op.

---

### User Story 3 - Graceful fallback on a non-systemd host (Priority: P3)

A user on a host without systemd (a container, a non-Linux dev environment, a minimal
distribution that uses a different init) may still have a leftover unit file from a
previous host or a misplaced copy operation. The uninstall flag should still be able to
remove the file even though `systemctl` cannot be invoked.

**Why this priority**: less common than the first two scenarios, but cheap to support and
prevents the command from being uselessly fragile on systems where it could still do
meaningful work (delete a stray file).

**Independent Test**: on a host where `systemctl` is not available on `PATH` and a unit
file is present at the user-scope systemd location, running `air-drive setup
--uninstall-service` logs a warning that `systemctl` was not found, still removes the
file, and exits 0.

**Acceptance Scenarios**:

1. **Given** `systemctl` is unavailable and a unit file is present, **When** the user
   runs the uninstall flag, **Then** the file is removed, a warning is emitted, and the
   command exits 0.
2. **Given** `systemctl` is unavailable and no unit file is present, **When** the user
   runs the uninstall flag, **Then** the command emits a warning, exits 0, and does
   nothing destructive.

---

### Edge Cases

- **Daemon currently running** (lock file held): `systemctl --user disable --now`
  terminates the process via SIGTERM, which the daemon's existing graceful-shutdown path
  handles. No explicit pre-flight lock check is added by this feature.
- **`systemctl` returns a non-zero exit code that is not "unit not loaded"**: the command
  surfaces the error in the log but still attempts to remove the unit file and refresh
  the cache. The final exit code reflects whether the user-visible artifact (the file) was
  removed; a residual systemd-state error is logged but does not fail the command.
- **Mutually exclusive flags**: passing `--install-service` and `--uninstall-service` in
  the same invocation is rejected at argument parsing with a clear error and a non-zero
  exit; the command does not silently pick one.
- **Permission denied removing the unit file**: extremely unlikely under XDG semantics
  (the file lives in the user's own config directory) but if it does happen the command
  reports the I/O error and exits non-zero — the user needs to act.
- **Unit file lives outside the expected XDG location** (system-wide install at
  `/etc/systemd/system/`, or a custom drop-in directory): out of scope. The command only
  manages the file it would install — the user-scope unit. System-wide installs are not
  produced by `--install-service` in the first place.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `setup` subcommand MUST accept a `--uninstall-service` flag that
  reverses the side effects of `--install-service`.
- **FR-002**: When invoked, the command MUST stop and disable the user-scope
  `air-drive.service` unit if it is currently loaded by systemd, regardless of whether it
  is active.
- **FR-003**: The command MUST remove the unit file from the user-scope systemd config
  directory if it is present, even when the file contents differ from the bundled
  template.
- **FR-004**: The command MUST refresh the systemd user-scope cache after removing the
  file so subsequent `systemctl --user` invocations see a consistent view.
- **FR-005**: The command MUST NOT modify or delete any of: the project configuration
  file, the persistent state database, the OAuth tokens file, the linked Drive account,
  the folder mapping records, the watched local folder, or the air-drive binary. Account
  and credential cleanup remain the responsibility of `air-drive unlink`.
- **FR-006**: The command MUST be idempotent: invocations on a host with no unit file
  and no loaded unit MUST exit successfully without producing an error.
- **FR-007**: When `systemctl` is not available on the host, the command MUST log a
  visible warning, skip the systemd interactions, still attempt to remove the unit file
  if present, and exit successfully.
- **FR-008**: The `--install-service` and `--uninstall-service` flags MUST be mutually
  exclusive. Passing both in the same invocation MUST fail at argument parsing with a
  clear error.
- **FR-009**: On success, the command MUST emit a single concise confirmation line
  identifying what was removed (unit file path, unit state) so the user has visible
  evidence of what changed.
- **FR-010**: The command MUST honour the same XDG path resolution as `--install-service`
  so the install and uninstall halves always operate on the same file.

### Key Entities

- **Systemd user unit file**: the on-disk artifact at the user-scope systemd config
  directory that systemd reads to know how to run the daemon. Owned by the user. Its
  presence is what makes the daemon auto-start on login.
- **Systemd unit state**: the in-memory record systemd keeps about whether a unit is
  loaded, enabled, and active. Survives independently of the file once loaded and must
  be cleared explicitly via `disable` and a cache refresh.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user who installed the service can uninstall it with a single command,
  without consulting documentation or running ancillary shell commands.
- **SC-002**: The uninstall command completes in under 5 seconds on a typical desktop
  Linux host, including the systemd stop, disable, file removal, and cache refresh.
- **SC-003**: Re-running the uninstall command on an already-clean host returns success
  in under 1 second and produces no destructive side effects.
- **SC-004**: After a successful uninstall, the user's local watched folder, Drive
  account link, and persistent state are observably unchanged — listing the local folder
  shows the same files, and `air-drive status` (run after a manual `air-drive start`)
  reports the same account and mapping as before.
- **SC-005**: The install/uninstall pair, run back-to-back ten times in a loop, leaves
  the host in the same state as before the first install — no residual unit file, no
  residual enabled state, no orphan processes.

## Assumptions

- Linux with systemd as the user-session init is the only platform the `--install-service`
  /  `--uninstall-service` pair targets. Other platforms either skip the flag or fall
  back to the graceful-degradation path described in FR-007.
- The user invokes `--uninstall-service` from a session that can reach the same XDG
  configuration directory used at install time. Mismatched `XDG_CONFIG_HOME` or running
  as a different user is the user's responsibility and out of scope.
- `air-drive unlink` remains the canonical command for clearing the Drive account,
  tokens, and folder mapping. Users wanting a full wipe combine the two commands.
- The systemd interactions rely on the user-scope `systemctl --user` flow already used
  by `--install-service`. System-wide installs (root-owned units under
  `/etc/systemd/system/`) are not produced or managed by this feature.
- A future top-level `air-drive uninstall` subcommand (binary + config + state + service
  in one shot) is out of scope here; it would build on top of this flag.
