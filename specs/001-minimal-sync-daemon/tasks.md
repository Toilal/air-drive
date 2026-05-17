---
description: "Task list for feature 001-minimal-sync-daemon"
---

# Tasks: Minimal Sync Daemon

**Input**: Design documents from `specs/001-minimal-sync-daemon/`
**Prerequisites**: plan.md ✅, spec.md ✅, research.md ✅, data-model.md ✅, contracts/ ✅

**Tests**: Test tasks are included. Rationale: the constitution's Quality Gates mandate
`cargo test` green and cover specific integration scenarios (sync engine cycle, conflict,
recovery, multi-instance) before merge. The success criteria SC-005 and SC-006 also require
verifiable behaviour, which means tests.

**Organization**: Tasks are grouped by user story so each story is independently
implementable and testable. Setup + Foundational phases come first as blocking prerequisites.

## Format: `[TaskID] [P?] [Story] Description`

- `[P]` — can run in parallel with other `[P]` tasks (different files, no shared state)
- `[Story]` — `[US1]`, `[US2]`, or `[US3]` for tasks belonging to a user story; no label for
  Setup / Foundational / Polish
- File paths are exact and absolute under the crate root

## Path Conventions

Single Rust crate at the repo root. `src/` for application code, `tests/integration/` for
hermetic integration tests, `tests/e2e/` for CI-gated real-Drive smoke (optional). See
`plan.md` for the full module map.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: project skeleton, toolchain config, CI scaffolding.

- [X] T001 Initialize the Cargo crate at the repo root (`cargo init --name air-drive --bin`), with edition 2024 and `rust-version` set to current stable in `Cargo.toml`
- [X] T002 [P] Declare all production dependencies in `Cargo.toml`: `tokio` (multi-thread + macros + signal + sync + fs + io-util + process + net), `notify` ^6, `reqwest` ^0.12 (with `rustls-tls` + `json`), `serde` + `serde_json` (with `derive`), `serde_with`, `yup-oauth2` ^11, `rusqlite` ^0.31 (with `bundled`), `clap` ^4 (with `derive`), `thiserror`, `tracing`, `tracing-subscriber` (with `fmt` + `env-filter`), `fd-lock`, `toml`, `directories` for XDG paths
- [X] T003 [P] Declare dev-dependencies in `Cargo.toml`: `wiremock`, `tempfile`, `assert_cmd`, `predicates`, `serde_json` (already present)
- [X] T004 [P] Add crate-level attrs to `src/main.rs`: `#![forbid(unsafe_code)]`, `#![deny(missing_docs)]` (warn for tests), and clippy lints `clippy::unwrap_used = "warn"`, `clippy::expect_used = "warn"`, `clippy::panic = "warn"`, only in non-test code
- [X] T005 [P] Add `rustfmt.toml` at the repo root with `edition = "2024"` and `max_width = 100`. The options `imports_granularity = "Crate"` and `group_imports = "StdExternalCrate"` are **nightly-only** (`unstable_features`) and would produce warnings on stable rustfmt — they are intentionally documented as commented-out lines in `rustfmt.toml` to be reintroduced if/when stabilised. The project stays on stable Rust per constitution principle I.
- [X] T006 [P] Add `clippy.toml` at the repo root setting `allow-unwrap-in-tests = true`, `allow-expect-in-tests = true`, `allow-panic-in-tests = true`
- [X] T007 [P] Create GitHub Actions workflow `.github/workflows/ci.yml` running on push + pull_request to `main` and feature branches: jobs `fmt` (`cargo fmt --all -- --check`), `clippy` (`cargo clippy --all-targets --all-features -- -D warnings`), `test` (`cargo test --workspace`); cache `~/.cargo` and `target/` with `Swatinem/rust-cache`
- [X] T008 [P] Create the module skeleton: empty `src/cli/mod.rs`, `src/daemon/mod.rs`, `src/engine/mod.rs`, `src/drive/mod.rs`, `src/watch/mod.rs`, `src/reconcile/mod.rs`, `src/state/mod.rs`, `src/config/mod.rs`, `src/error.rs`. Wire them as `pub mod` declarations in `src/main.rs`

**Checkpoint**: `cargo build` succeeds, `cargo fmt --check` clean, CI green on an empty branch.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: cross-story infrastructure. **No user story task may start until this phase is
complete.**

- [X] T009 Implement crate-wide `Error` enum in `src/error.rs` using `thiserror`. Variants at minimum: `Io`, `Sqlite`, `Drive`, `Oauth`, `Config`, `Rclone`, `Lock`, `Mapping`. Add a `Result<T> = std::result::Result<T, Error>` alias in `src/lib.rs` (or `src/main.rs` if no lib split)
- [X] T010 [P] Implement XDG path resolver in `src/config/paths.rs` using the `directories` crate. Exposes `config_dir() -> PathBuf` (config + state), `cache_dir() -> PathBuf` (rclone binary cache), `runtime_dir() -> PathBuf` (control socket); each function MUST also accept an override from a `--config-dir` global CLI flag
- [X] T011 [P] Implement TOML config load/save in `src/config/mod.rs`: `Config` struct mirroring `contracts/config.md` ([oauth], [mapping], [daemon], [rclone] sections), `Config::load(path)` returning `Ok(Default)` when the file is absent, `Config::save(path)` writing 0644
- [X] T012 [P] Implement `tracing` init in `src/observability.rs` (new file): `init_tracing(verbose: u8, log_file: Option<&Path>)` setting up `tracing-subscriber` with an `EnvFilter` (default level = warn, `-v`/`-vv`/`-vvv` lifts to info/debug/trace) and an optional file layer. Operation log lines MUST include the fields `event`, `op_id`, `item_id` (when applicable), and `relative_path` (when applicable) — provide a shared `op_span!(...)` macro for callers (FR-025)
- [ ] T013 Implement SQLite connection + migrations runner in `src/state/mod.rs`: `Db::open(path)` opens with `PRAGMA journal_mode = WAL`, `PRAGMA synchronous = NORMAL`, `PRAGMA foreign_keys = ON`, then runs `migrate_to_latest()` which checks the `schema_version` table and applies migrations from `src/state/schema.rs` forward. If `schema_version.version` is **greater** than the binary's known migrations (FR-024), `Db::open` MUST return `Error::Config` with a clear "upgrade required" message and the daemon MUST refuse to start. Downgrades are not supported
- [ ] T014 Implement schema v1 in `src/state/schema.rs` with `MIGRATIONS: &[&str]` containing the CREATE TABLE statements for `schema_version`, `account`, `folder_mapping`, `sync_item` (+ unique index on `(mapping_id, relative_path)`), `pending_operation` (+ index on `next_attempt_at`), `conflict_record`, `drive_change_cursor`. SQL exactly as described in `data-model.md`
- [ ] T015 [P] Implement Account repository in `src/state/accounts.rs`: `upsert(email, linked_at) -> AccountId`, `get_single() -> Option<Account>`, `touch_linked_at()`. Pure SQL via `rusqlite`
- [ ] T016 [P] Implement FolderMapping repository in `src/state/mapping.rs`: `upsert(account_id, local_path, remote_folder_id, remote_folder_name) -> MappingId`, `get_single() -> Option<FolderMapping>`
- [ ] T017 [P] Implement SyncItem repository in `src/state/items.rs`: `insert(SyncItem) -> ItemId`, `get_by_relative_path(mapping_id, path)`, `update_fingerprint(id, size, md5)`, `set_state(id, state)`, `delete(id)`, `iter_for_mapping(mapping_id)` returning an iterator
- [ ] T018 [P] Implement PendingOperation repository in `src/state/ops.rs`: `enqueue(item_id, op, payload) -> OpId`, `next_due(now) -> Option<PendingOperation>` (uses the `next_attempt_at` index), `mark_attempt(id, error, next_at)`, `delete(id)`, `count_by_op()` (for status output)
- [ ] T019 [P] Implement ConflictRecord repository in `src/state/conflicts.rs`: `insert(item_id, original, conflict_path, detected_at) -> ConflictId`, `list_unresolved() -> Vec<ConflictRecord>`, `delete(id)`
- [ ] T020 [P] Implement DriveChangeCursor repository in `src/state/cursor.rs`: `get(mapping_id) -> Option<String>`, `set(mapping_id, token)`
- [ ] T021 [P] Define `SyncEngine` trait in `src/engine/mod.rs` with async methods: `upload(local: &Path, remote_parent_id: &str, name: &str) -> Result<RemoteFile>`, `download(remote_id: &str, local: &Path) -> Result<()>`, `move_remote(remote_id, new_parent_id, new_name) -> Result<()>`, `delete_remote(remote_id) -> Result<()>`. Plus `Operation` enum and `RemoteFile` struct (id, md5, size, mime). The trait does NOT mention rclone
- [ ] T022 [P] Define `clap` CLI in `src/cli/mod.rs`: top-level struct with global flags (`--config-dir`, `--log-file`, `-v`/`-vv`/`-vvv`, `--no-download-rclone`) and a `Command` enum with `Link`, `Map`, `Start`, `Pause`, `Resume`, `Status`, `Setup` variants per `contracts/cli.md`. Exit-code mapping centralised in a single `cli_exit(result) -> ExitCode` helper
- [ ] T023 [P] Implement single-instance file lock in `src/daemon/lock.rs` using `fd-lock`: `Lock::acquire(config_dir) -> Result<Lock>` returning a guard that releases on drop; on `WouldBlock`, read the PID from the lock file and return `Error::Lock { pid }`. Also detects stale locks (PID gone from `/proc`) at acquire time

**Checkpoint**: foundation ready. `cargo test` runs (no tests yet, but compiles). All
foundational modules importable and have at least one trivial unit test (round-trip on the
repository inserts).

---

## Phase 3: User Story 1 — First-time setup and initial sync (Priority: P1) 🎯 MVP

**Goal**: A user can `air-drive link` (OAuth), `air-drive map <local> <remote>`, and run
`air-drive start --initial-sync` to reconcile the two folders into an equivalent state.

**Independent Test**: with an empty local folder and a Drive folder containing a small mixed
dataset (≤ 50 files, ≤ 100 MB), the sequence `link → map → start --initial-sync` finishes
with the local folder containing the same files with identical content. Same in reverse with
empty Drive and non-empty local.

### Tests for User Story 1 (write FIRST, ensure they FAIL before implementation)

- [ ] T024 [P] [US1] Test scaffolding: build a Drive HTTP mock (wiremock) plus a tempdir
  fixture in `tests/integration/common/mod.rs`, exporting `DriveMock::new()` and
  `fs_fixture()`. The Drive mock answers `about.user`, `files.list`, `files.get`,
  `files.create` (multipart), `files.update`, `files.delete`, `changes.getStartPageToken`,
  `changes.list`
- [ ] T025 [P] [US1] Integration test US1.1 — `link` persists account: invoke `air-drive
  link` against the mock with a canned OAuth code; assert the account row exists in
  `state.db` with the expected email — in `tests/integration/initial_sync.rs`
- [ ] T026 [P] [US1] Integration test US1.2 — `map` validates and persists mapping with a
  resolvable remote folder ID; rejects a non-existing local path with exit `4`; rejects an
  unresolvable remote with exit `5` — in `tests/integration/initial_sync.rs`
- [ ] T027 [P] [US1] Integration test US1.3 — empty local, Drive holds 10 files in 3
  subfolders: after `start --initial-sync`, the local tree mirrors Drive content (paths +
  bytes) — in `tests/integration/initial_sync.rs`
- [ ] T028 [P] [US1] Integration test US1.4 — non-empty local, empty Drive: after `start
  --initial-sync`, every local file appears on the Drive mock with identical sizes and
  parent folders — in `tests/integration/initial_sync.rs`
- [ ] T029 [P] [US1] Integration test US1.5 — overlapping content (3 files match by md5,
  2 only local, 2 only remote): matching files are not re-uploaded, missing-on-each-side
  files are propagated — in `tests/integration/initial_sync.rs`

### Implementation for User Story 1

- [ ] T030 [US1] Implement Drive HTTP client base in `src/drive/http.rs`: `DriveHttp` struct
  wrapping a `reqwest::Client`, with a configurable `base_url` (test-only override),
  exponential backoff + jitter for HTTP 429 / 503, request budget tracking. Headers include
  `Authorization: Bearer <token>` injected via a closure `Fn() -> impl Future<Output=String>`
- [ ] T031 [US1] Implement OAuth flow with PKCE in `src/drive/auth.rs` using
  `yup-oauth2::InstalledFlowAuthenticator` configured with `InstalledFlowReturnMethod::HTTPRedirect`,
  scopes `drive.file` + `drive.metadata.readonly`. Reads embedded `client_id` constant
  (overridable via `Config.oauth.client_id`). Caller passes `tokens.json` path; the auth helper
  enforces `0600` perms at startup (refuse otherwise)
- [ ] T032 [US1] Implement Drive metadata client in `src/drive/metadata.rs`: `about_user() ->
  AboutUser`, `get_file(id)`, `list_children(parent_id)`, `resolve_path(path_spec) -> FileId`
  (supports `path:My Drive/Sync` notation, `https://drive.google.com/...` URL, raw file ID)
- [ ] T033 [P] [US1] Implement rclone binary resolver in `src/engine/rclone_path.rs` per
  `research.md §5`: config-path → `$PATH` (with version probe via `rclone version`) → cache
  → download from `downloads.rclone.org` with SHA-256 verification; respect
  `--no-download-rclone` flag. Returns `RcloneBinary { path, version, source }`
- [ ] T034 [US1] Implement `RcloneEngine` in `src/engine/rclone.rs` driving the resolved
  binary via `tokio::process::Command`. Subcommands used: `copyto`, `moveto`,
  `--metadata-mapper`-free invocations only. Implements every method of the `SyncEngine`
  trait. Captures stderr on failure for `Error::Rclone { stderr }`
- [ ] T034b [US1] Implement download staging in `src/engine/rclone.rs` (FR-010): downloads
  MUST land first in `<local_root>/.air-drive-partial/<op-id>` and only be `rename(2)`'d
  into the final path after the download completes and the md5 matches the remote. Failed
  downloads MUST delete the staging file. Add an orphan-cleanup pass on daemon startup
  that removes any leftover entries under `.air-drive-partial/`
- [ ] T035 [P] [US1] Implement MD5+size fingerprint helper in `src/reconcile/fingerprint.rs`:
  `compute_local(path) -> (size, md5)` (streamed read, hashes via `md-5` crate), `from_remote(file: &RemoteFile) -> Option<(size, md5)>` (None when Drive omits md5 — e.g. native Docs)
- [ ] T036 [US1] Implement initial reconciliation in `src/reconcile/mod.rs`: walks the local
  tree and the remote tree in parallel, builds three sets (only-local, only-remote, both),
  emits the corresponding `Operation`s, executes them via the `SyncEngine`, populates the
  `sync_item` table with fingerprints. Persists a `drive_change_cursor` baseline (calling
  `changes.getStartPageToken`) AFTER reconciliation completes so we don't replay events
  already covered
- [ ] T037 [US1] Implement `link` subcommand in `src/cli/link.rs`: load config, drive the
  OAuth flow, persist tokens, write the account row. Exit `0` on success, `2` on OAuth
  error, `3` on network failure. If the `[oauth].client_id` override is configured but
  rejected by Google during the OAuth dance (invalid or unauthorised), exit `2` with a
  message naming the offending config key (FR-001)
- [ ] T038 [US1] Implement `map` subcommand in `src/cli/map.rs`: canonicalise the local
  path, create it if missing, resolve the remote path/URL/ID to a Drive file ID via
  `drive::metadata::resolve_path`, persist the mapping
- [ ] T039 [US1] Implement `start --initial-sync` path in `src/cli/start.rs`: acquire lock,
  load config + account + mapping, run `reconcile::initial`, exit when the queue is empty.
  (The continuous loop is added in US2.) Refuse to start without `--initial-sync` if the
  drive_change_cursor is empty
- [ ] T040 [US1] Implement `setup` subcommand in `src/cli/setup.rs`: orchestrates `link →
  map → start --initial-sync` with interactive prompts (use `dialoguer`); forwards the
  first non-zero exit; supports `--install-service` to drop `~/.config/systemd/user/air-drive.service`
- [ ] T040b [P] [US1] Implement `unlink` subcommand in `src/cli/unlink.rs` (FR-018, FR-019):
  refuses (exit 8) if the lock is held by a live daemon; otherwise deletes `tokens.json`,
  clears the `account` and `folder_mapping` rows from `state.db`, leaves the local watched
  folder contents untouched. Honors `--yes` to skip the confirmation prompt

**Checkpoint**: US1 fully functional. Initial reconciliation works end-to-end against a
mocked Drive and a real local filesystem; the integration tests T024-T029 pass.

---

## Phase 4: User Story 2 — Continuous bidirectional sync (Priority: P1)

**Goal**: A running daemon propagates local edits to Drive within 10 s and remote edits to
local within 90 s, in both directions, including create/modify/delete/rename/move.

**Independent Test**: with the daemon running on an already-synced pair, edits made on
either side propagate within the latency targets; deletions and renames propagate without
re-transferring content; network drop + reconnect drains the queued ops.

### Tests for User Story 2

- [ ] T041 [P] [US2] Integration test US2.1 — local create propagates within 10 s — in
  `tests/integration/continuous_sync.rs`
- [ ] T042 [P] [US2] Integration test US2.1 — local modify propagates (verify Drive sees the
  new md5) — in `tests/integration/continuous_sync.rs`
- [ ] T043 [P] [US2] Integration test US2.1 — local delete propagates — in
  `tests/integration/continuous_sync.rs`
- [ ] T044 [P] [US2] Integration test US2.3 — local rename propagates via `moveto`, no
  re-upload (assert the mock saw zero `files.create` calls) — in
  `tests/integration/continuous_sync.rs`
- [ ] T045 [P] [US2] Integration test US2.4 — subfolder move propagates — in
  `tests/integration/continuous_sync.rs`
- [ ] T046 [P] [US2] Integration test US2.2 — remote create propagates locally — in
  `tests/integration/continuous_sync.rs`
- [ ] T047 [P] [US2] Integration test US2.2 — remote modify propagates locally — in
  `tests/integration/continuous_sync.rs`
- [ ] T048 [P] [US2] Integration test US2.2 — remote delete propagates locally — in
  `tests/integration/continuous_sync.rs`
- [ ] T049 [P] [US2] Integration test US2.5 — simulate network drop (mock returns 503 for 30 s),
  events queue, queue drains on recovery — in `tests/integration/continuous_sync.rs`

### Implementation for User Story 2

- [ ] T050 [US2] Implement local watcher in `src/watch/mod.rs`: `Watcher::start(local_root)
  -> mpsc::Receiver<WatchEvent>`. Wraps `notify` v6 with `RecommendedWatcher`, recursive
  mode. Maps raw events to a `WatchEvent` enum (`Created`, `Modified`, `Deleted`, `Renamed
  { from, to }`)
- [ ] T051 [P] [US2] Implement debounce in `src/watch/debounce.rs`: 200 ms window per logical
  path, coalesces burst events (editor saves emit Create/Remove/Create) into a single final
  state per path
- [ ] T051b [P] [US2] Skip symlinks and special files (FR-013) in `src/watch/mod.rs`: on
  each event, `lstat` the path and drop it (with a `tracing::info` notice) if the file type
  is neither regular file nor directory. Covered by a unit test in the same file
- [ ] T052 [P] [US2] Implement Drive change poller in `src/drive/changes.rs`: long-lived
  task that loops on `changes.list?pageToken=...&supportsAllDrives=false`, persists
  `newStartPageToken` after each page, filters results to descendants of the mapped remote
  folder, emits `RemoteChange` events on a channel
- [ ] T053 [US2] Implement reconciler in `src/reconcile/mod.rs` (extending T036): a function
  `reconcile_local(WatchEvent) -> Vec<Operation>` and `reconcile_remote(RemoteChange) ->
  Vec<Operation>` that consult the sync_item table to decide whether an event represents a
  real change or a known echo (a remote change we caused via our own upload). Native Google
  Docs / Sheets / Slides (`mimeType` starts with `application/vnd.google-apps.` other than
  `folder`) MUST NOT produce a `sync_item` row; log a one-line notice the first time each is
  seen and silently ignore subsequent observations (FR-011)
- [ ] T053b [P] [US2] Handle EACCES on local read (FR-021) in `src/reconcile/mod.rs`: catch
  `io::ErrorKind::PermissionDenied` per file, surface as a transient `last_error.kind="io"`
  in status, do not block the rest of the queue, re-enqueue for the next 5 min safety-net
  cycle
- [ ] T054 [US2] Implement conflict detection in `src/reconcile/conflict.rs` (FR-006, Q2
  clarification): on a local + remote change that both diverge from the last fingerprint,
  rename the local file to `<stem>.conflict-YYYYMMDDTHHMMSSZ.<ext>`, insert a
  `conflict_record`, and let the next cycle upload both the canonical (remote-derived) and
  the conflict file
- [ ] T054b [US2] Clear conflict records on user resolution (FR-006) in
  `src/reconcile/conflict.rs`: when a watcher event reports the deletion or rename of either
  side of a conflict (canonical or `.conflict-*` companion), remove the corresponding
  `conflict_record` row
- [ ] T055 [P] [US2] Implement op dispatcher in `src/daemon/runtime.rs`: pulls due ops from
  the queue (`PendingOperation::next_due`), executes via the `SyncEngine`, handles
  exponential back-off with jitter on failure (initial 1 s, doubling, max 60 s, ±20 %
  jitter, 10 attempts before the op is moved to a quarantine state requiring manual
  resolution — FR-012), updates the queue row accordingly. Single-worker serialisation per
  `sync_item_id`
- [ ] T055b [US2] Handle ENOSPC mid-download (FR-022) in `src/daemon/runtime.rs`: on
  `io::ErrorKind::StorageFull` during a download, abort, delete the staging file under
  `.air-drive-partial/<op-id>` (T034b), surface in status as transient
  `last_error.kind="io"`, apply FR-012 back-off
- [ ] T056 [US2] Implement daemon event loop in `src/daemon/mod.rs`: spawns the watcher, the
  change poller, the safety-net timer (5 min reconciliation), and the op dispatcher; routes
  events through `reconcile_*` into the `PendingOperation` queue. Backed by `tokio::select!`
  + `tokio_util::sync::CancellationToken`
- [ ] T057 [P] [US2] Implement graceful shutdown in `src/daemon/shutdown.rs`: SIGTERM /
  SIGINT trigger the `CancellationToken`, the loop drains the in-flight op then exits clean
- [ ] T058 [US2] Update `start` in `src/cli/start.rs` to run the daemon loop (remove the
  early-exit-after-initial-sync from T039 once `drive_change_cursor` is populated)

**Checkpoint**: US1 + US2 functional. The daemon stays alive and reflects edits in both
directions; tests T041-T049 pass.

---

## Phase 5: User Story 3 — Status, conflicts, and recovery (Priority: P2)

**Goal**: The user can run `air-drive status` (human + JSON) and trust that crashes,
reboots, conflicts, and revoked tokens are surfaced and recoverable.

**Independent Test**: while syncing, `status --json` validates against
`contracts/status.schema.json`; a forced conflict produces both files and shows up in
status; killing the daemon with `-9` and restarting resumes without data loss.

### Tests for User Story 3

- [ ] T059 [P] [US3] Integration test US3.1 — `status --json` validates against
  `contracts/status.schema.json` (use the `jsonschema` crate to validate) — in
  `tests/integration/status.rs`
- [ ] T060 [P] [US3] Integration test US3.1 — status reports correct counts mid-sync and
  the last error message after a forced failure — in `tests/integration/status.rs`
- [ ] T061 [P] [US3] Integration test US3.2 — both sides edit the same file while daemon is
  offline; on restart, both versions are preserved, conflict listed in status — in
  `tests/integration/conflict.rs`
- [ ] T062 [P] [US3] Integration test US3.3 — `kill -9` during a 50 MB upload; restart;
  daemon resumes; no half-written file under the local path; eventual convergence — in
  `tests/integration/recovery.rs`
- [ ] T063 [P] [US3] Integration test US3.4 — refresh token revoked by the mock (HTTP 400
  on token refresh); daemon transitions to state `blocked` with kind `auth`, stays alive,
  status surfaces "re-link account" — in `tests/integration/relink.rs`
- [ ] T064 [P] [US3] Integration test FR-017 — second `air-drive start` against the same
  config dir exits with code 6 and a message naming the running PID — in
  `tests/integration/multi_instance.rs`

### Implementation for User Story 3

- [ ] T065 [US3] Implement pause flag + control socket in `src/daemon/pause.rs`: a Unix
  socket at `$XDG_RUNTIME_DIR/air-drive/control.sock` accepting `pause` / `resume` /
  `status-snapshot` commands. The pause flag is a `tokio::sync::watch::Sender<bool>` read
  by the dispatcher and the reconciler
- [ ] T066 [P] [US3] Implement `pause` subcommand in `src/cli/pause.rs`: connects to the
  control socket, sends `pause`, exits 0 on ack, 7 if no daemon
- [ ] T067 [P] [US3] Implement `resume` subcommand in `src/cli/resume.rs`: same as T066
  with `resume`. On the daemon side, `resume` MUST trigger a **single convergence pass**
  against the current filesystem and remote state; it MUST NOT replay individual events
  that arrived during the pause (FR-015)
- [ ] T068 [US3] Implement status snapshot assembly in `src/cli/status.rs`: reads from
  state.db (pending counts via `PendingOperation::count_by_op`, last sync from a new
  `sync_log` table OR a single `state_meta.last_sync_at` row — pick the latter), reads the
  control socket for live state when a daemon is running; falls back to "no daemon" when
  the socket is absent. Outputs human-readable text by default
- [ ] T069 [P] [US3] Implement `status --json` output in `src/cli/status.rs`: serialises
  the snapshot into a `serde` struct that matches `contracts/status.schema.json` v1 exactly;
  the daemon's resolved `RcloneBinary` is exposed under `rclone.{path,version,source}`
- [ ] T070 [US3] Implement re-link-required state in `src/daemon/mod.rs` (FR-009):
  distinguish **transient refresh failures** (network error or 5xx from the identity
  provider — retry with the FR-012 back-off, stay in the current state) from **hard refresh
  failures** (`invalid_grant`, HTTP 400 — set state to `blocked { kind: auth }`, surface in
  status, do NOT exit; resume sync on successful re-link detected by the auth module on next
  call)
- [ ] T070b [US3] Detect "watched remote folder deleted on Drive" (FR-020) in
  `src/drive/changes.rs`: when `changes.list` reports the mapped remote folder ID with
  `removed: true` or `trashed: true`, transition the daemon to state `blocked` with
  `last_error.kind="remote"`, surface a clear error in status, and stop dispatching new
  operations. Do NOT delete local content
- [ ] T071 [US3] Wire single-instance lock acquisition into `start` (FR-017): acquire
  `Lock` from T023 at the very top of `cli::start`; on `Error::Lock { pid }`, print a
  user-facing message and return exit code 6
- [ ] T071b [US3] Verify local watched path on startup (FR-023) in `src/cli/start.rs`:
  after the lock is acquired, `std::fs::metadata` the configured `local_path`. If it does
  not exist or is not a directory, transition to state `blocked` with
  `last_error.kind="mapping"`, print a clear error pointing the user at `air-drive map`,
  and do NOT auto-recreate the directory
- [ ] T072 [US3] Implement crash recovery on startup in `src/daemon/mod.rs` (FR-010,
  SC-005): on `start`, before launching watchers, walk the `pending_operation` queue and
  re-enqueue any rows whose `next_attempt_at` is in the past; treat in-flight ops (none on
  startup) as not-yet-attempted. Verify the local-tree fingerprints against `sync_item` to
  detect torn writes (file present but md5 mismatched) and re-queue the download

**Checkpoint**: all three user stories functional. The daemon recovers from kills, surfaces
conflicts cleanly, and refuses to double-start. Tests T024-T064 pass.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Hardening, docs, performance sanity. Not gating MVP but expected before the
feature is "done".

- [ ] T073 [P] Run `cargo clippy --all-targets --all-features -- -D warnings` and fix every
  warning across the crate
- [ ] T074 [P] Add `///` doc comments on every public type and function in `src/`. CI's
  `missing_docs` warn level catches gaps
- [ ] T075 [P] Walk through every step of `specs/001-minimal-sync-daemon/quickstart.md` on a
  fresh VM; fix any drift between the doc and the implementation
- [ ] T076 [P] Performance smoke for SC-002 / SC-003 / SC-004: a small bench harness in
  `tests/perf/` (CI-gated) that runs the initial sync on a 1 GB / 1 000-file fixture and
  measures p95 latencies. Fails CI if outside budget
- [ ] T077 [P] Memory smoke for SC-007: a 24-hour soak in CI's nightly job, asserting RSS
  stays under 200 MB
- [ ] T077b [P] Quota verification smoke for SC-008 in `tests/perf/quota.rs` (CI-gated):
  run the daemon idle on a mapping with one synthetic change per minute for 10 min;
  assert the count of HTTP requests to the Drive API mock stays below 10 % of the
  1 000 req / 100 s per-user budget over the window
- [ ] T078 Update root `README.md` with a status section linking to `specs/001-minimal-sync-daemon/spec.md`,
  `plan.md`, and `quickstart.md`
- [ ] T078b [P] Ship a `air-drive.service` **systemd user unit template** (FR-014) under
  `assets/systemd/air-drive.service`. The `setup --install-service` command (T040) copies
  it to `~/.config/systemd/user/air-drive.service` and runs
  `systemctl --user enable --now air-drive.service`
- [ ] T079 [P] Add `--version` output to include the resolved rclone version when present
  (after the lock is acquired and the binary resolved)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies; can start immediately.
- **Foundational (Phase 2)**: depends on Setup; **blocks every user story**.
- **US1 (Phase 3)**: depends on Foundational; is the MVP. Can ship without US2 / US3.
- **US2 (Phase 4)**: depends on Foundational AND on the `reconcile` module from US1 (T036).
  Tests T041-T049 can be drafted in parallel with US1 work but won't pass until T053+ land.
- **US3 (Phase 5)**: depends on Foundational. Can run in parallel with US2 once Foundational
  is done — status surface and lock work do not touch the reconciler. T072 (crash recovery)
  reaches into US2 code; sequence it after T056.
- **Polish (Phase 6)**: depends on all three user stories.

### Within-story dependencies

- **US1**: T030 (HTTP) → T031 (OAuth) → T037 (link). T032 (metadata) is parallel with T031.
  T033 (rclone path) → T034 (engine impl) → T036 (reconciler). T036 → T039 (start
  --initial-sync) → T040 (setup wrapper).
- **US2**: T050 + T051 (watcher) parallel with T052 (drive poller). T053 (reconciler
  extension) needs T050 + T052. T055 (dispatcher) needs T021 (trait) + T034 (engine, from
  US1). T056 (loop) is the last to land.
- **US3**: T065 (control socket) before T066/T067 (pause/resume CLI). T068 (status assembly)
  + T069 (status JSON) can land in parallel with T070-T072.

### Parallel opportunities

- Within Phase 1: T002-T008 are all `[P]`.
- Within Phase 2: T010-T012 parallel; then T013 (DB connection) is sequential; then
  T014 (schema); then T015-T021 are all `[P]` after T013/T014.
- US1 tests T024-T029 are all `[P]`.
- US1 impl: T030, T032, T033, T035 are all `[P]` after Foundational; then T031 needs T030;
  then T034 needs T033, T036 needs T034 + T035, etc.
- US2 tests T041-T049 are all `[P]`.
- US3 tests T059-T064 are all `[P]`.
- Polish: T073-T077, T079 are all `[P]`.

---

## Implementation Strategy

### MVP first (US1 only)

1. Complete Phase 1 (T001-T008).
2. Complete Phase 2 (T009-T023). This is the longest single-developer phase.
3. Complete Phase 3 (T024-T040). **STOP**: validate US1 by running `air-drive setup`
   against a real Drive folder and watching the initial sync converge.
4. Ship a v0.1 tag at this point — initial reconciliation is genuinely useful by itself.

### Incremental delivery

- MVP (US1) → tag `v0.1.0`. Demo: `air-drive setup` against a Drive folder.
- + US2 → tag `v0.2.0`. Demo: edit a file, watch it reflect within 10 s; edit on the web,
  watch it reflect within 90 s.
- + US3 → tag `v0.3.0`. Demo: `kill -9` the daemon mid-sync, restart, no data lost; force a
  conflict, see both versions; `air-drive status --json | jq`.

### Single-developer order

Sequential through phases, but use parallel `[P]` tasks within phases to keep the
build/clippy/test cycle hot (write the SQL repos in T015-T020 in parallel batches: each
file is independent and ~50 lines).

### Parallel team strategy (if applicable)

- Once Foundational is done, US1 and US3 can be split between two developers (US3's
  status + lock + recovery don't depend on the reconciler). US2 needs US1's
  `reconcile::initial` and is best owned by whoever shipped US1.

---

## Notes

- `[P]` tasks: different files, no shared state.
- `[US1]` / `[US2]` / `[US3]` map a task to its story for traceability.
- Each user story is delivered as an independent value-bearing increment. Stopping after
  any of them ships something useful.
- Tests are integration tests: hermetic, no real Drive, no real network. The e2e tier
  against a real Drive account is gated by `AIR_DRIVE_E2E_TOKEN` and lives outside
  `tests/integration/`.
- Commit after each task or coherent group. Verify tests fail before implementing the
  corresponding code path (the test tasks come first inside each story phase).
- Avoid touching files outside the listed module for a given task. If a task forces an
  edit elsewhere, that's a sign the foundational layer is missing something — surface it.
