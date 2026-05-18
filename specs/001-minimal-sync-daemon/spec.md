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

### Session 2026-05-11 — Quality checklist resolution

Resolves the 40 items in `checklists/quality.md` (CHK001..CHK040). Highlights:

- Quantified previously ambiguous terms (file matching, conflict suffix format, "nominal network conditions", "steady state", back-off parameters, file mode).
- Reconciled the SIGKILL gap (SC-005 now covers SIGTERM, SIGKILL, and clean reboot).
- Clarified the delete-vs-edit conflict outcome (the edited version is preserved under the canonical name, the delete is suppressed, a conflict record is opened).
- Added requirements for unlink, remote-folder-deleted, unreadable-local-file, disk-full-mid-download, missing-local-path, schema-migration, observability.
- Promoted OAuth scopes, rclone availability, `changes.list` token semantics, and clock assumptions out of `research.md` into the spec.

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
   on Drive, **Then** the daemon validates that the remote exists and that the local path is
   either an existing directory or creatable (it is auto-created if missing), and persists the
   mapping.
3. **Given** a local folder with content and an empty remote folder, **When** the initial sync
   runs, **Then** every local file and subfolder is uploaded to Drive with identical names and
   relative paths.
4. **Given** an empty local folder and a remote folder with content, **When** the initial sync
   runs, **Then** every remote file and subfolder is downloaded locally with identical names
   and relative paths.
5. **Given** both sides have overlapping content, **When** the initial sync runs, **Then**
   matching files are left untouched. **Two files match** if and only if they share the same
   relative path AND the same content fingerprint `(size, MD5)`. Files present on only one
   side are propagated; no data is overwritten or deleted.
6. **Given** the initial sync is interrupted partway through (process killed, host rebooted),
   **When** the daemon is restarted, **Then** the initial sync resumes from where it stopped:
   already-transferred items are not re-transferred, partial files (incomplete downloads) are
   discarded, and the final state is equivalent to a non-interrupted run.

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
   under nominal network conditions.
2. **Given** a running daemon, **When** a change happens on Drive (file created, modified,
   deleted, renamed, or moved from another client), **Then** the corresponding change appears
   locally within 90 seconds under nominal network conditions.
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
   the current state (idle / syncing / paused / blocked / error), counts of pending operations,
   the timestamp of the last successful sync, and a one-line summary of the last error if any.
2. **Given** a file that has been modified on both sides since the last sync (a true conflict),
   **When** the daemon detects it, **Then** both versions are preserved (one keeps the
   original name, the other is suffixed with a conflict marker that includes a timestamp),
   and the conflict is listed in the status output until acknowledged.
3. **Given** the daemon has crashed (SIGTERM, SIGKILL) or the host was rebooted mid-sync,
   **When** the daemon restarts, **Then** it resumes from the last persisted state without
   re-uploading or re-downloading already-synced content, and without losing in-flight changes
   that had been acknowledged on at least one side.
4. **Given** the OAuth refresh token has expired or been revoked, **When** the daemon tries to
   call the remote API, **Then** the daemon stops sync activity, surfaces a clear "re-link
   account" message in the status, and stays alive (does not exit) so the user can re-link
   without restarting from scratch.

---

### Edge Cases

- **True conflict**: a file is edited on both sides between two successful sync points. The
  daemon MUST preserve both versions; it MUST NOT silently pick one. See FR-006 for the
  precise rename rule.
- **Delete vs. edit**: a file is deleted on one side and edited on the other since the last
  sync. The edited version is **always** preserved under the canonical relative path, the
  delete is suppressed, and a conflict record is opened that flags the asymmetry — no
  `.conflict-*` companion file is created in this case (there is nothing to preserve on the
  deletion side).
- **Native Google Docs / Sheets / Slides** (no file equivalent on disk): out of scope for this
  feature. The daemon MUST skip them (no `sync_item` row is created), log a one-line notice
  the first time each is encountered, and silently ignore subsequent observations of the same
  item.
- **Files larger than free local disk space or remote quota**: surface a clear error in
  status, skip the file, continue with the rest.
- **Disk-full DURING a download**: if free space is exhausted mid-transfer, the download MUST
  be aborted, any partial bytes MUST be discarded, the error MUST be surfaced in status as
  transient, and the operation MUST be retried after the back-off window.
- **Names invalid on the local filesystem**: Drive can host names that the local filesystem
  refuses (case-only differences within a folder on case-insensitive systems; characters
  forbidden on the local OS: `/`, `\`, `:`, NUL, control chars; names with leading or trailing
  whitespace; names ending in `.`). Per offending file: surface a clear error in status, skip,
  continue with the rest.
- **Symbolic links and special files** in the local folder: skip them with a log notice, do
  not attempt to upload.
- **Hidden files** (dotfiles on Linux): synced like any other file by default.
- **Extended network loss** (hours/days): on reconnect, the daemon catches up without
  re-uploading already-synced content.
- **Clock skew** between local machine and the remote: must not produce phantom conflicts.
  Reconciliation MUST rely on content fingerprints, never on modification timestamps. The
  wall clock is used solely to build the conflict-suffix string (FR-006); a badly skewed
  clock produces an unusual suffix but no data loss.
- **OAuth refresh failure — transient outage** (Google identity provider returns 5xx): treat
  as a transient error, retain "syncing" state, retry with the FR-012 back-off ladder.
- **OAuth refresh failure — revocation** (`invalid_grant` / HTTP 400 on refresh): transition
  to state `blocked { kind: auth }`, surface a clear "re-link account" message, stay alive.
- **Burst local edits in a single debounce window**: when an editor saves a file two or more
  times in under 200 ms, the final on-disk state is the one that propagates; intermediate
  states are coalesced and never become independent sync events.
- **Ephemeral local files** (created and deleted before any sync cycle picks them up): the
  daemon MUST coalesce these into a no-op; nothing reaches Drive.
- **Watched remote folder deleted on Drive**: the daemon detects the loss via
  `changes.list`, transitions to state `blocked`, surfaces a clear "remote folder gone"
  error, and does NOT delete the local content. The user resolves by re-running `map` against
  a new remote folder or restoring the deleted one on Drive.
- **Local watched folder removed between runs** (deleted or moved while the daemon was
  down): the daemon refuses to start, transitions to state `blocked`, does NOT recreate the
  folder automatically. The user resolves via `map` to a new local path or restores the old
  one.
- **Local file becomes unreadable mid-flight** (permissions changed, EACCES at read time):
  surface a per-file error in status, skip the file, retry it on the next safety-net cycle
  (every 5 min). Do not block the rest of the queue.
- **Rate limiting / quota exceeded** on the remote API: daemon backs off and retries with
  FR-012 parameters; surfaces in status as transient.
- **Sync paused** by the user: daemon stops sync activity but keeps watching for events.
  On resume, the daemon reconciles the **current** state vs. the persisted state — it does
  not replay individual events that occurred while paused, and a single convergence pass
  brings the two sides back in sync.
- **OAuth client `client_id` override invalid or unauthorised**: the daemon refuses to start
  with a clear error pointing at the offending config entry.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST allow the user to link exactly one Google Drive account using a
  desktop OAuth flow that opens the user's default browser, captures the consent, and persists
  credentials securely on disk such that subsequent runs do not require re-consent. The flow
  MUST use a project-owned OAuth `client_id` embedded in the binary with the PKCE extension
  (no `client_secret` shipped) and MUST request only the scopes
  `https://www.googleapis.com/auth/drive.file` and
  `https://www.googleapis.com/auth/drive.metadata.readonly`. The user MAY override the
  embedded `client_id` via the `[oauth].client_id` config entry; the daemon MUST refuse to
  start if the override resolves to an invalid or unauthorised client.
- **FR-002**: The system MUST allow the user to designate exactly one local folder and one
  remote (Drive) folder to be kept in sync, and persist that mapping across restarts. If the
  local folder does not exist when `map` runs, the daemon MUST create it (single-level
  creation only). If the remote folder cannot be resolved to a Drive file ID, `map` MUST
  fail with a clear error.
- **FR-003**: Local changes (create, modify, delete, rename, move) inside the watched local
  folder MUST be detected within 2 seconds of occurrence.
- **FR-004**: Remote changes on the linked Drive account inside the watched remote folder
  MUST be detected within 60 seconds of occurrence under nominal network conditions.
- **FR-005**: Every detected change MUST be propagated bidirectionally so that, in the absence
  of conflicts, the contents of the local folder and the remote folder converge to be
  equivalent (same relative paths, same hierarchy, same content). Two items at the same
  relative path are **considered matching** if and only if their `(size, MD5)` fingerprints
  agree.
- **FR-006**: When the same file has been modified on both sides between two successful sync
  points, the system MUST preserve both versions. The **remote (Drive) version retains the
  canonical name**; the **local version is renamed** on disk to
  `<stem>.conflict-<UTC-timestamp>.<ext>`, where `<UTC-timestamp>` is in basic ISO 8601
  second-precision format `YYYYMMDDTHHMMSSZ`. The local-side renamed file is then uploaded to
  Drive under that conflict name on the next cycle so both sides see both versions. The
  conflict MUST be surfaced via the status command. The conflict is **considered resolved**
  when the user deletes or renames either the canonical file or the `.conflict-…` file via
  any means observable by the watcher or the remote change feed; the daemon MUST then remove
  the corresponding conflict record.
- **FR-007**: The system MUST persist enough state across restarts to resume sync from the last
  known good point without re-uploading or re-downloading already-synced content.
- **FR-008**: The system MUST expose a `status` command that returns the current daemon state
  (idle / syncing / paused / blocked / error), counts of pending uploads, pending downloads,
  and unresolved conflicts, the timestamp of the last successful sync, and the last error
  message if any. Output MUST be available in human-readable form by default and in JSON
  form conforming to `contracts/status.schema.json` when invoked with `--json`.
- **FR-009**: The system MUST refresh expired OAuth access tokens transparently using the
  stored refresh token. On transient refresh failures (network / 5xx from the identity
  provider) the daemon MUST retry with the FR-012 back-off and stay in its current state. On
  hard refresh failures (`invalid_grant` / HTTP 400) the daemon MUST transition to state
  `blocked { kind: auth }`, surface a clear "re-link account" message in status, and stay
  alive.
- **FR-010**: After a crash (SIGTERM, SIGKILL), an OS kill, or a reboot, the daemon MUST be
  restartable and MUST recover to a consistent state. **Consistent state** means: no file
  under the local watched folder is partially written (downloads are staged under
  `.air-drive-partial/<op-id>` and only atomically renamed into place once fully fetched and
  checksum-verified); no remote file has been left in an indeterminate state (resumable
  uploads are either fully committed or fully discarded); no `sync_item` row claims a
  fingerprint different from the actual file content.
- **FR-011**: The system MUST skip native Google Docs / Sheets / Slides items (which have no
  native filesystem equivalent): no `sync_item` row is created, no local file is materialised,
  and a one-line notice is logged the first time each item is encountered.
- **FR-012**: The system MUST respect the remote API's per-user rate limit and back off
  gracefully when throttled. The back-off schedule is exponential with jitter: initial delay
  **1 second**, doubling per attempt up to a **maximum of 60 seconds**, jitter **±20 %**, max
  **10 attempts** before the operation is moved to a quarantine state requiring manual
  resolution. Retries DO count against the API quota (they are real calls).
- **FR-013**: Symbolic links and special files (sockets, FIFOs, block/char devices) inside the
  watched local folder MUST be skipped with a log notice rather than uploaded.
- **FR-014**: The daemon MUST run in the foreground when launched manually and MUST ship a
  **systemd user unit** that can be installed via `air-drive setup --install-service` (or
  manually) to start on login. System-wide systemd units are out of scope.
- **FR-015**: The system MUST provide commands to pause and resume sync without losing
  watched state. On resume, the daemon performs a single convergence pass against the current
  filesystem and remote state rather than replaying individual events that occurred during
  the pause.
- **FR-016**: All credentials (OAuth tokens) MUST be stored in a file with explicit mode
  `0600` (owner read+write, no group, no world). The daemon MUST verify the mode at startup
  and refuse to start with a clear error if the mode is looser.
- **FR-017**: The daemon MUST acquire an exclusive file lock in its configuration directory at
  startup. If the lock is already held by another live process, the new instance MUST exit
  with a non-zero status and a clear error message that identifies the running daemon's PID.
  The lock MUST be released cleanly on graceful shutdown and MUST be auto-detected as stale
  (e.g., when the holding PID no longer exists) on subsequent starts.
- **FR-018**: The system MUST expose the following composable CLI commands as the canonical
  surface, each independently runnable and testable: `link` (perform OAuth and persist the
  account), `map` (record the local-folder ↔ remote-folder mapping), `start` (run the daemon
  in the foreground), `pause`, `resume`, `status`, and `unlink` (remove the linked account
  and clear all locally persisted credentials and state). The system MUST also expose a
  `setup` command that orchestrates `link` then `map` then `start` with interactive prompts to
  ease first-time use.
- **FR-019**: `unlink` MUST remove the OAuth tokens file, delete the account row from the
  state DB, and clear the folder mapping. It MUST NOT touch the local watched folder
  contents.
- **FR-020**: If the daemon detects that the watched remote folder has been deleted on
  Drive, it MUST stop sync activity, transition to state `blocked { kind: remote }`, surface
  a clear error in status, and MUST NOT delete any local content.
- **FR-021**: If a local file inside the watched folder becomes unreadable (e.g., EACCES) at
  read time, the daemon MUST surface a per-file error in status, skip the file for the
  current cycle, and retry it on the next safety-net cycle.
- **FR-022**: If the local filesystem runs out of free space during a download, the daemon
  MUST abort the transfer, discard any partial bytes (the staging file under
  `.air-drive-partial/` is removed), surface the error as transient in status, and retry
  according to FR-012.
- **FR-023**: If the configured local path is missing at daemon startup, the daemon MUST
  refuse to start syncing, transition to state `blocked { kind: mapping }`, and surface a
  clear error. The local path MUST NOT be auto-recreated outside the explicit `map` command.
- **FR-024**: The state database schema MUST evolve forward-only. The daemon MUST refuse to
  start when the on-disk schema is newer than what the running binary supports, with a clear
  "upgrade required" error. Downgrades are not supported.
- **FR-025**: The daemon MUST emit structured logs using `tracing` (or equivalent) at default
  level `warn`. Each operation log line MUST include the fields `event`, `op_id`, `item_id`
  (when applicable), and `relative_path` (when applicable). Log destination is stderr by
  default; a `--log-file <path>` flag duplicates to a file.

### Key Entities *(include if feature involves data)*

- **Drive Account**: the linked Google account. Holds a refresh token, an access token, an
  expiry. There is exactly one Drive Account in this MVP.
- **Folder Mapping**: a pair of (local path, remote folder identifier). There is exactly one
  Folder Mapping in this MVP.
- **Sync State**: the persisted bookkeeping that lets the daemon resume — last seen remote
  change cursor, set of known synced items with their content fingerprints, set of pending
  operations, set of unresolved conflicts, schema version.
- **Sync Item**: a logical file or folder participating in the sync. Tracked by its local
  path, remote identifier, last known content fingerprint, and last sync timestamp.
- **Conflict Record**: a marker for a file modified on both sides; references the two
  preserved versions and the detection time. Cleared by FR-006 when the user resolves.

## Success Criteria *(mandatory)*

### Measurement notes

The success criteria below assume **nominal network conditions** (packet loss < 1 %, RTT
< 200 ms, downstream and upstream bandwidth ≥ 10 Mbps). Percentile-based criteria assume a
sample of at least **100 events** before the percentile is computed. Latency measurement
windows start at the moment the change is observable (filesystem `mtime` set locally;
`modifiedTime` returned by Drive remotely) and end at the moment the corresponding change is
durably visible on the other side.

### Measurable Outcomes

- **SC-001**: A new user can complete OAuth setup and folder selection in under 3 minutes from
  first launch, measured end-to-end (start the CLI → `status` reports state `syncing`).
- **SC-002**: An initial sync of up to 1 GB and up to 1 000 files completes in under 5
  minutes on a 50 Mbps connection. The measurement window starts when the single-instance
  lock is acquired and ends when `status` first reports state `idle`.
- **SC-003**: A local file modification appears on the remote side within 10 seconds in 95 %
  of cases (p95 over ≥ 100 events).
- **SC-004**: A remote file modification appears on the local side within 90 seconds in 95 %
  of cases (p95 over ≥ 100 events).
- **SC-005**: After a daemon shutdown (SIGTERM, SIGKILL) or a host reboot during sync, the
  daemon resumes and reaches state `idle` with **zero lost edits** and **zero corrupted
  files** in 100 % of cases. A **lost edit** is a change durably persisted on one side that
  never reaches the other side after convergence. A **corrupted file** is a file whose
  byte-content under its canonical name differs from the source it was synced from.
- **SC-006**: In the event of a true conflict, both versions are preserved in 100 % of cases;
  zero silent overwrites are acceptable.
- **SC-007**: The daemon runs continuously for 7 days under normal use without leaking memory
  beyond a steady-state ceiling of 200 MB resident, assuming up to 50 000 watched files and
  an average of one change per minute.
- **SC-008**: Remote API quota usage stays below 10 % of the per-user limit in **steady
  state**, defined as the mapping at rest with no more than one user-initiated change per
  minute.

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
  are used to avoid phantom conflicts caused by clock skew. The wall clock is assumed
  broadly correct (within minutes of UTC) so that conflict-suffix names are intelligible,
  but a badly skewed clock yields only unusual names — never data loss.
- **The user has a working browser** on the same machine for OAuth consent. Headless / SSH-only
  setup is out of scope for this MVP.
- **The `rclone` binary is reachable** at start: either pre-installed on `$PATH`, or
  previously cached at `$XDG_CACHE_HOME/air-drive/bin/rclone`, or downloadable from
  `downloads.rclone.org`. See `research.md §5` for the full resolution order. If neither path
  yields a usable rclone, the daemon refuses to start with a clear error.

## External Dependencies

- **Google Drive API `changes.list` + `pageToken`**: the daemon depends on the documented
  behaviour that `pageToken` values are monotonic and that no change is omitted across
  consecutive pages when the daemon persists `newStartPageToken` after each page. If Google
  changes this contract, the daemon's remote change detection is no longer correct.
- **Google Drive API rate limit**: per-user limits are assumed to follow Google's published
  quota (1 000 req / 100 s / user at the time of writing). SC-008 is calibrated against
  this limit.
- **`rclone` ≥ 1.65**: per-file `copyto` / `moveto` semantics in modern rclone. See
  `research.md §5`.
