# Feature Specification: Minimal Sync Daemon

**Feature Branch**: `001-minimal-sync-daemon`
**Created**: 2026-05-11
**Status**: Draft
**Input**: User description: "Minimal sync daemon: single Google Drive account, desktop OAuth, one locally selected folder, bidirectional event-driven sync (inotify + changes.list + pageToken), CLI binary only — no UI."

## Clarifications

### Session 2026-05-11

- Q: How is the Google OAuth client distributed? → A: Hybrid — ship a project-owned OAuth `client_id` embedded in the binary (PKCE, no secret) as the default for one-click install; allow advanced users to override the `client_id` via a config file to use their own Google Cloud project.
- Q: In a true conflict (both sides edited since last sync), which version keeps the original file name? → A: The remote (Drive) version keeps the original name; the local version is renamed with the conflict suffix.
- Q: How does the daemon handle a second instance launched against the same configuration directory? → A: Lock-and-refuse — a file lock in the configuration directory; the second instance exits non-zero with a clear message pointing at the running PID.
- Q: What does the first-launch UX look like — single guided command or composable building blocks? → A: Both. Composable commands (`link`, `map`, `start`, `pause`, `resume`, `status`) are the canonical, individually testable surface; `setup` is a thin wrapper that runs them in order with prompts.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - First-time setup and initial sync (Priority: P1)

A Linux user installs the air-drive command-line tool, runs it for the first time, links their
personal Google Drive account through a desktop OAuth flow, picks one remote folder on Drive
and one local folder on disk, and lets the daemon perform the initial bidirectional reconciliation.
After the initial sync, the contents of the local folder and the remote folder are equivalent
(same files, same hierarchy, same content), with no data lost on either side.

**Why this priority**: without setup and an initial sync, nothing else works. This is the MVP
slice that proves the whole technical stack (OAuth, remote access, local I/O, reconciliation)
hangs together.

**Independent Test**: starting from an empty local folder and a Drive folder containing a
known small dataset (≤ 50 files, mixed types, ≤ 100 MB), running the setup and initial sync
results in the local folder containing the same files with identical content, and vice versa
when the local folder is non-empty and the Drive folder is empty.

**Acceptance Scenarios**:

1. **Given** a fresh install with no linked account, **When** the user runs the setup command,
   **Then** they are guided through OAuth (a browser window opens, they grant access, the daemon
   captures the consent), and on success their Drive account is linked and persisted.
2. **Given** a linked account, **When** the user picks a local folder path and a remote folder
   on Drive, **Then** the daemon validates that both exist and persists the mapping.
3. **Given** a local folder with content and an empty remote folder, **When** the initial sync
   runs, **Then** every local file and subfolder is uploaded to Drive with identical names and
   relative paths.
4. **Given** an empty local folder and a remote folder with content, **When** the initial sync
   runs, **Then** every remote file and subfolder is downloaded locally with identical names
   and relative paths.
5. **Given** both sides have overlapping content (some files match by name and content, some
   only exist on one side), **When** the initial sync runs, **Then** matching files are left
   untouched, missing files are propagated to the other side, and no data is overwritten or
   deleted.

---

### User Story 2 - Continuous bidirectional sync (Priority: P1)

Once the initial sync is done and the daemon keeps running, the user expects local edits to
appear on Drive within seconds, and remote edits (made on the web UI, a phone, or another
device) to appear locally within a minute. This is the everyday experience and the reason the
project exists at all.

**Why this priority**: this is the product differentiator — event-driven on both sides. Without
it, the daemon is just another periodic syncer. It is co-P1 with Story 1 because shipping
initial sync without continuous sync would not deliver any value: the user could just `rclone
copy` once.

**Independent Test**: with the daemon running on an already-synced folder pair, modifying a
file locally triggers an upload within 10 seconds; modifying a file on Drive's web UI triggers
a local update within 90 seconds. Deleting and renaming follow the same propagation latency.

**Acceptance Scenarios**:

1. **Given** a running daemon on a synced folder pair, **When** the user creates, modifies, or
   deletes a file locally, **Then** the corresponding change appears on Drive within 10 seconds
   in steady network conditions.
2. **Given** a running daemon, **When** a change happens on Drive (file created, modified,
   deleted, renamed, or moved from another client), **Then** the corresponding change appears
   locally within 90 seconds.
3. **Given** a running daemon, **When** a file is renamed locally, **Then** the same rename
   propagates to Drive without re-uploading content.
4. **Given** a running daemon, **When** a subfolder is moved locally, **Then** the move
   propagates to Drive without re-uploading or re-downloading files.
5. **Given** a running daemon, **When** the network connection drops, **Then** the daemon
   queues pending changes locally; **and When** the network comes back, **Then** the queue is
   drained and the two sides converge again with no manual action.

---

### User Story 3 - Status, conflicts, and recovery (Priority: P2)

The user needs to know what the daemon is doing — is it idle, syncing, errored, blocked on a
conflict? When something unexpected happens (file modified on both sides between syncs,
permission error, quota error, expired OAuth refresh), the user needs a clear, actionable
report and a way to restart the daemon cleanly after a crash or reboot.

**Why this priority**: required for trust. A sync tool that silently fails or silently corrupts
data is worse than no tool. P2 because the MVP can ship without a polished status UX as long
as failures are visible somewhere (log file, exit code, status command).

**Independent Test**: while a sync is in progress, running the status command reports counts
of pending uploads, pending downloads, last successful sync timestamp, and last error if any.
Forcing a conflict (edit the same file on both sides while offline) does not destroy either
version and shows up clearly in the status output.

**Acceptance Scenarios**:

1. **Given** a running daemon, **When** the user runs the status command, **Then** they see
   the current state (idle / syncing / blocked / error), counts of pending operations, the
   timestamp of the last successful sync, and a one-line summary of the last error if any.
2. **Given** a file that has been modified on both sides since the last sync (a true conflict),
   **When** the daemon detects it, **Then** both versions are preserved (one keeps the
   original name, the other is suffixed with a conflict marker that includes a timestamp),
   and the conflict is listed in the status output until acknowledged.
3. **Given** the daemon has crashed or the host was rebooted mid-sync, **When** the daemon
   restarts, **Then** it resumes from the last persisted state without re-uploading or
   re-downloading already-synced content, and without losing in-flight changes that had been
   acknowledged on at least one side.
4. **Given** the OAuth refresh token has expired or been revoked, **When** the daemon tries to
   call the remote API, **Then** the daemon stops sync activity, surfaces a clear "re-link
   account" message in the status, and stays alive (does not exit) so the user can re-link
   without restarting from scratch.

---

### Edge Cases

- **True conflict**: a file is edited on both sides between two successful sync points. The
  daemon MUST preserve both versions; it MUST NOT silently pick one.
- **Delete vs. edit**: a file is deleted on one side and edited on the other since the last
  sync. Treat as a conflict — preserve the edited version, suppress the delete, surface to the
  user.
- **Native Google Docs / Sheets / Slides** (no file equivalent on disk): out of scope for this
  feature; the daemon MUST skip them and log a one-line notice per item the first time it sees
  them (not on every cycle).
- **Files larger than free disk space or remote quota**: surface a clear error in status, skip
  the file, continue with the rest.
- **Names invalid on the local filesystem** (e.g., Drive allows two files with names that only
  differ by case in the same folder; Linux is case-sensitive but Windows/macOS are not):
  surface a clear error per offending file, skip it, continue.
- **Symbolic links and special files** in the local folder: skip them with a log notice, do
  not attempt to upload.
- **Hidden files** (dotfiles on Linux): synced like any other file by default.
- **Extended network loss** (hours/days): on reconnect, the daemon catches up without
  re-uploading already-synced content.
- **Clock skew** between local machine and the remote: must not produce phantom conflicts;
  reconciliation MUST rely on content-derived signals (checksums or equivalent), not solely on
  modification timestamps.
- **OAuth token revoked from the user's Google account settings**: daemon detects, stops sync,
  surfaces re-link instruction, stays alive.
- **Rate limiting / quota exceeded** on the remote API: daemon backs off and retries with
  exponential delay; surfaces in status as transient.
- **Sync paused** by the user: daemon stops sync activity but keeps watching for events, and
  resumes cleanly when un-paused.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST allow the user to link exactly one Google Drive account using a
  desktop OAuth flow that opens the user's default browser, captures the consent, and persists
  credentials securely on disk such that subsequent runs do not require re-consent. The flow
  MUST use a project-owned OAuth `client_id` embedded in the binary with the PKCE extension
  (no `client_secret` shipped). The user MAY override the embedded `client_id` via a config
  file to use their own Google Cloud project — the daemon MUST read this override at startup
  and prefer it over the embedded default when present.
- **FR-002**: The system MUST allow the user to designate exactly one local folder and one
  remote (Drive) folder to be kept in sync, and persist that mapping across restarts.
- **FR-003**: Local changes (create, modify, delete, rename, move) inside the watched local
  folder MUST be detected within 2 seconds of occurrence.
- **FR-004**: Remote changes on the linked Drive account inside the watched remote folder
  MUST be detected within 60 seconds of occurrence under nominal network conditions.
- **FR-005**: Every detected change MUST be propagated bidirectionally so that, in the absence
  of conflicts, the contents of the local folder and the remote folder converge to be
  equivalent (same files, same hierarchy, same content).
- **FR-006**: When the same file has been modified on both sides between two successful sync
  points, the system MUST preserve both versions. The **remote (Drive) version retains the
  canonical name**; the **local version is renamed** on disk to
  `<stem>.conflict-<UTC-timestamp>.<ext>` (and uploaded to Drive under that conflict name on
  the next cycle so both sides see both versions). The conflict MUST be surfaced via the
  status command until the user resolves it by deleting or renaming the offending file.
- **FR-007**: The system MUST persist enough state across restarts to resume sync from the last
  known good point without re-uploading or re-downloading already-synced content.
- **FR-008**: The system MUST expose a `status` command that returns the current daemon state
  (idle / syncing / blocked / error), counts of pending uploads, pending downloads, and
  unresolved conflicts, the timestamp of the last successful sync, and the last error message
  if any. Output MUST be available in both human-readable and machine-readable form.
- **FR-009**: The system MUST refresh expired OAuth access tokens transparently using the
  stored refresh token, and MUST surface a clear "re-link account" state if the refresh token
  is revoked or expired.
- **FR-010**: After a crash, kill, or reboot, the daemon MUST be restartable and MUST recover
  to a consistent state where neither side has been corrupted (no half-written files, no
  duplicate uploads).
- **FR-011**: The system MUST skip native Google Docs / Sheets / Slides items (which have no
  native filesystem equivalent) and log a one-line notice the first time each is encountered.
  These items are out of scope for this feature.
- **FR-012**: The system MUST respect the remote API's per-user rate limit and back off
  gracefully when throttled (exponential retry with jitter, surfaced as transient in status).
- **FR-013**: Symbolic links and special files (sockets, FIFOs, block/char devices) inside the
  watched local folder MUST be skipped with a log notice rather than uploaded.
- **FR-014**: The daemon MUST run in the foreground when launched manually and MUST be
  installable as a user-level service that starts on login (the user-scoped service unit
  shipped with the binary is in scope; system-wide installation is not).
- **FR-015**: The system MUST provide commands to pause and resume sync without losing watched
  state.
- **FR-016**: All credentials (OAuth tokens) MUST be stored with restrictive permissions on
  the local filesystem (readable only by the owning user).
- **FR-017**: The daemon MUST acquire an exclusive file lock in its configuration directory at
  startup. If the lock is already held by another live process, the new instance MUST exit
  with a non-zero status and a clear error message that identifies the running daemon's PID.
  The lock MUST be released cleanly on graceful shutdown and MUST be auto-detected as stale
  (e.g., when the holding PID no longer exists) on subsequent starts.
- **FR-018**: The system MUST expose the following composable CLI commands as the canonical
  surface, each independently runnable and testable: `link` (perform OAuth and persist the
  account), `map` (record the local-folder ↔ remote-folder mapping), `start` (run the daemon
  in the foreground), `pause`, `resume`, and `status`. The system MUST also expose a `setup`
  command that orchestrates `link` then `map` then `start` with interactive prompts to ease
  first-time use.

### Key Entities *(include if feature involves data)*

- **Drive Account**: the linked Google account. Holds a refresh token, an access token, an
  expiry. There is exactly one Drive Account in this MVP.
- **Folder Mapping**: a pair of (local path, remote folder identifier). There is exactly one
  Folder Mapping in this MVP.
- **Sync State**: the persisted bookkeeping that lets the daemon resume — last seen remote
  change cursor, set of known synced items with their content fingerprints, set of pending
  operations, set of unresolved conflicts.
- **Sync Item**: a logical file or folder participating in the sync. Tracked by its local
  path, remote identifier, last known content fingerprint, and last sync timestamp.
- **Conflict Record**: a marker for a file modified on both sides; references the two
  preserved versions and the detection time.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A new user can complete OAuth setup and folder selection in under 3 minutes from
  first launch, measured end-to-end (start the CLI → see "syncing" in status).
- **SC-002**: An initial sync of up to 1 GB and up to 1 000 files completes in under 5 minutes
  on a 50 Mbps connection.
- **SC-003**: A local file modification appears on the remote side within 10 seconds in 95 %
  of cases under nominal network conditions.
- **SC-004**: A remote file modification appears on the local side within 90 seconds in 95 %
  of cases under nominal network conditions.
- **SC-005**: After a crash or reboot during sync, the daemon resumes and reaches an
  "idle / converged" state with **zero** lost edits and **zero** corrupted files in 100 % of
  cases where the prior shutdown signal was SIGTERM or the host was cleanly rebooted.
- **SC-006**: In the event of a true conflict, both versions are preserved in 100 % of cases;
  zero silent overwrites are acceptable.
- **SC-007**: The daemon runs continuously for 7 days under normal use without leaking memory
  beyond a steady-state ceiling of 200 MB resident.
- **SC-008**: Remote API quota usage stays below 10 % of the per-user limit in steady state
  (idle + occasional changes).

## Assumptions

- **Conflict resolution policy**: when a file is modified on both sides between sync points,
  the daemon preserves both versions and surfaces the conflict — it does not auto-merge, does
  not silently pick a winner, and does not prompt the user interactively. The user resolves
  manually by reading the status output and deleting or renaming the offending file. This is
  the safest default for a CLI-only MVP.
- **Scope per mapping**: a folder mapping covers the entire subtree recursively; there is no
  partial-tree selection or per-extension filtering in this MVP.
- **Single account, single mapping**: multi-account and multi-folder support are out of scope
  for this feature. Later features can extend the data model without migration since the
  constitution already requires multi-account support from day one.
- **No UI**: this feature ships a CLI binary only. A graphical UI is a later feature.
- **Linux first**: this MVP targets Linux. macOS and Windows support are out of scope and
  will follow.
- **Personal Google account** with normal user permissions on the chosen remote folder (read +
  write). Shared drives, organization-owned content, and folders not owned by the user are
  out of scope.
- **Hidden files** (dotfiles) are synced like any other file. The user can exclude them by not
  placing them in the watched folder.
- **Timestamps are not the sole source of truth** for change detection; content fingerprints
  are used to avoid phantom conflicts caused by clock skew.
- **The user has a working browser** on the same machine for OAuth consent. Headless / SSH-only
  setup is out of scope for this MVP.
