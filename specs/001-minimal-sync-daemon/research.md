# Research: Minimal Sync Daemon — Phase 0

This document resolves the technical unknowns surfaced in `plan.md` and locks the choices
that downstream phases (data-model, contracts, tasks) build on. Each entry follows the
**Decision / Rationale / Alternatives considered** structure.

## 1. Remote change detection: `changes.list` + `pageToken`

**Decision**: poll `https://www.googleapis.com/drive/v3/changes` with a stored `pageToken`
every 30 s by default (configurable down to 10 s and up to 60 s). On startup, if no
`pageToken` is stored, call `changes.getStartPageToken` to bootstrap it. Filter results in
memory to keep only changes that fall under the configured remote folder subtree.

**Rationale**: `changes.list` returns only deltas since the last token, and the response
includes a `nextPageToken` and (when no more changes) a `newStartPageToken` we persist for
the next cycle. A single `changes.list` call is one quota unit, so at 30 s polling that's
~120 calls / hour — far under the 10 % budget of the 1 000 req / 100 s per-user quota
(SC-008). Drive does not provide free-of-charge push notifications without a public HTTPS
endpoint (`changes.watch` requires a verified domain and a publicly reachable URL — out of
scope for a CLI tool).

**Alternatives considered**:

- `changes.watch` with `webhook` mode — requires a publicly reachable HTTPS endpoint, often a
  reverse proxy or tunneling service (ngrok-like). Bad fit for a local CLI daemon and
  introduces a hard dependency on external infrastructure.
- Per-file `files.watch` — one watch per file, expensive in API calls; same public-HTTPS
  requirement.
- Polling `files.list` with `q=modifiedTime > ...` — requires full listing every cycle,
  scales linearly with folder size and burns far more quota.

## 2. Local change detection: `notify` crate and rename semantics

**Decision**: use the `notify` crate (v6+) in its default backend (inotify on Linux), wrap it
in a 200 ms debounce window per logical path to coalesce burst events (editor saves often
emit a flurry of `Create / Modify / Remove / Create` events as files are atomically replaced).

For rename detection on Linux: inotify pairs `MovedFrom` and `MovedTo` events by a shared
`cookie` value. `notify` exposes this via `EventKind::Modify(ModifyKind::Name(_))` with a
`RenameMode::Both` payload when both halves of a rename fall within a watched subtree, and as
two unpaired events (`From` / `To`) when only one half is inside the tree. We treat unpaired
halves as delete + create.

**Rationale**: matches the FR-003 budget (changes detected within 2 s). Debounce keeps us
from generating multiple `Upload` ops for a single editor save. The Linux-specific rename
handling is rich enough to support FR-005 (rename propagation without re-upload).

**Alternatives considered**:

- Raw `inotify-rs` — better control but more code to write and platform-locked anyway.
- `watchexec-events` — higher-level wrapper, but adds opinions we don't need.
- Polling the filesystem — burns CPU at any reasonable cadence and misses fast successive
  edits. Reserved as the safety-net path only.

## 3. OAuth flow: PKCE with `yup-oauth2`

**Decision**: use `yup-oauth2` with its `InstalledFlowAuthenticator` configured for the
**installed application** flow + PKCE. The embedded `client_id` is read from a build-time
constant; the user can override via `oauth.client_id` in the config file.

The flow opens a local HTTP listener on `127.0.0.1:<random-port>` for the redirect, opens
the user's browser at the Google consent URL with `code_challenge` derived from a
high-entropy `code_verifier`, exchanges the returned code for an access + refresh token
pair (no `client_secret` sent), and persists both in a 0600 file.

**Rationale**: this is the standard OAuth 2.0 flow for native apps per RFC 8252, exactly what
Google recommends for installed apps with sensitive scopes. PKCE removes the need to ship a
`client_secret` (constitution III). `yup-oauth2` already implements automatic refresh-token
rotation via its `Authenticator::token` API — the daemon calls it on every authenticated
request and gets a fresh access token transparently (FR-009).

**Alternatives considered**:

- Out-of-band copy-paste flow — deprecated by Google in 2022.
- Device-code flow — works without a browser on the same machine but adds friction; reserve
  for a later "headless mode" feature.
- Hand-rolled OAuth — pointless given a maintained crate exists.

## 4. Scopes

**Decision**: request **only** `https://www.googleapis.com/auth/drive.file` plus
`https://www.googleapis.com/auth/drive.metadata.readonly` for initial discovery of the
remote folder during `air-drive map`.

`drive.file` grants read/write **only** on files the app has created or that the user
explicitly opens via the Google Picker; combined with the `map` step picking a folder, the
daemon ends up with full access to that folder and everything created under it from within
the app, but not to the rest of the user's Drive.

**Rationale**: minimum permission to ship the feature. Drive's OAuth review process is
significantly faster for non-restricted scopes; `drive.file` is the "best-practice" scope
explicitly recommended by Google in 2024+ for this exact use case (per-folder sync).

**Alternatives considered**:

- `https://www.googleapis.com/auth/drive` (full Drive) — easier code, but a heavy scope that
  triggers Google's annual security assessment requirement. Costly and unjustified for a
  per-folder tool.
- `drive.appdata` — restricted to a hidden per-app data folder; can't expose user-visible
  files.

## 5. `rclone` integration: per-file ops, not `bisync`

**Decision**: do NOT use `rclone bisync` in the steady-state event loop. Use it (or
equivalently `rclone copy` + `rclone copy` in the reverse direction with `--update`) **only**
at first-time mapping when both sides may have pre-existing content. After that, every
reconciled change is one of: `rclone copyto <src> <dst>` (upload or download a single file),
`rclone moveto <src> <dst>` (rename / move), or direct Drive REST `files.delete` / `unlink`
(local).

The `SyncEngine` trait exposes these atomic operations. `RcloneEngine` implements them by
spawning the resolved rclone binary with the right subcommand. The trait does not leak rclone
CLI flag syntax.

**Binary resolution order** (`engine::rclone_path`):

1. **`[rclone] path` in `config.toml`** — if set, use it. Fail at startup if the file is
   missing, not executable, or reports `rclone version < 1.65`.
2. **`rclone` on `$PATH`** — probe `rclone version`. If ≥ 1.65, use it; log one info line.
3. **`$XDG_CACHE_HOME/air-drive/bin/rclone`** — if the cached binary exists and is ≥ 1.65,
   use it.
4. **Download** — fetch the matching platform binary from `downloads.rclone.org`, verify the
   SHA-256 against the rclone-published checksum file, place it at the path from step 3,
   set mode `0755`. This is the constitution-mandated MVP path.
5. **`--no-download-rclone`** flag — disable step 4 entirely; if we reach this point, the
   daemon fails at startup with a clear error pointing the user at the missing binary.

The resolved `(path, version, source)` triple is exposed via `air-drive status --json` under
`rclone.{path,version,source}` (source ∈ `"config" | "path" | "cache" | "downloaded"`) so
the user can audit which rclone is actually in use.

**Rationale**: `bisync` is full-tree comparison — wrong granularity for an event-driven
loop. Per-file ops let us turn one inotify event into one bounded subprocess call. Keeping
the trait at "atomic op" granularity is what gives us the freedom of a future
`NativeEngine` (principle IV). The resolution order favours a user-managed rclone if present
(saves bandwidth and disk, respects power users) while falling back to the
constitution-mandated download path when nothing usable is found.

**Alternatives considered**:

- Drive REST directly for transfers — works for small files, but for resumable uploads
  > 5 MB and for chunked downloads we'd reimplement what rclone already does well. Acceptable
  long-term plan, not for this MVP.
- `rclone rcd` (HTTP remote control) — a long-running rclone daemon we'd talk to over HTTP.
  Lower per-op overhead than spawning rclone N times, but adds a second daemon to supervise.
  Worth revisiting if profiling shows subprocess spawn cost dominates; deferred.

## 6. Content fingerprinting for change detection

**Decision**: for any file > 0 bytes, store `(size, md5)` as the fingerprint. The local MD5
is computed lazily (on watcher event, cached by inode) and Drive returns MD5 in the
`md5Checksum` field of `files.get`/`files.list` for binary files.

For files where Drive does not return an MD5 (i.e. native Google Docs — already excluded by
FR-011), skip and log.

**Rationale**: matches Drive's own change-detection signal, so we never get false conflicts
from clock skew (edge case "Clock skew" in spec.md). Size short-circuits the hash for the
overwhelmingly common case (no change). Lazy + cached avoids hashing the entire local tree
on every event.

**Alternatives considered**:

- mtime alone — explicitly rejected by the spec (false conflicts from clock skew).
- BLAKE3 — faster, but we'd still need to cross-check against Drive-provided MD5, defeating
  the purpose.
- SHA-256 — Drive doesn't expose it for arbitrary files; we'd have to compute both. No.

## 7. Single-instance lock

**Decision**: use the `fd-lock` crate to acquire a non-blocking exclusive lock (`flock`
LOCK_EX | LOCK_NB) on a `lock` file in the configuration directory. The PID is written into
the file post-lock for the error message in FR-017.

Stale-lock detection: if `flock` succeeds but a previous PID is recorded that no longer
exists in `/proc`, log a one-line warning and continue. (The successful `flock` itself
already proves no other live holder; the PID-existence check just catches "we are the
recovery party after a crash" for clearer logs.)

**Rationale**: `flock` is the right primitive on Linux, kernel-enforced and automatically
released on process death. `fd-lock` gives a clean Rust wrapper. Required by FR-017.

**Alternatives considered**:

- PID file only — racy and well known to be insufficient.
- Abstract Unix socket — works but is Linux-specific; `flock` is also Linux-specific in
  practice (we are Linux-only for this feature anyway).
- `fslock` crate — equivalent functionality; `fd-lock` is slightly more modern and
  maintained by the Tokio org. Either is fine.

## 8. Persistence: SQLite layout and migrations

**Decision**: single SQLite database `state.db` opened in WAL mode (`PRAGMA journal_mode =
WAL`) with `synchronous = NORMAL`. Schema versioning via a `schema_version` table; migrations
are written as forward-only SQL applied at startup before any other query. Initial schema is
v1.

WAL gives us crash recovery without sacrificing concurrent reader/writer access (the
status command reads while the daemon writes). `synchronous = NORMAL` is the right balance
for sync-loop write rate (we are not financial software).

Detailed table layout → `data-model.md`.

**Alternatives considered**:

- `sled` — pure-Rust embedded KV. Faster for raw ops, but no SQL means harder ad-hoc
  inspection (`sqlite3 state.db` is a real debugging asset for an OSS desktop tool).
- Flat JSON files — fine for the OAuth tokens (kept that way), wrong for the change ledger.
- PostgreSQL / external DB — absurd for a desktop daemon.

## 9. Logging

**Decision**: `tracing` for instrumentation throughout the crate, with `tracing-subscriber`
formatting to stderr by default. The `--log-file <path>` flag (default off) duplicates to a
file under `$XDG_STATE_HOME/air-drive/`. No log rotation in this feature (deferred); document
that users running as a service should rely on `logrotate` or systemd's `LogMaxFileSize`.

**Rationale**: `tracing` is the de facto Rust standard. Stderr-only by default keeps the
daemon well-behaved under systemd (which captures stderr into the journal automatically).

**Alternatives considered**:

- `log` + `env_logger` — works but lacks structured spans, which we want around the
  per-file operations for debugging.
- Custom logger — gratuitous.

## 10. Test harness for the Drive API

**Decision**: use `wiremock` as a local HTTP mock server in integration tests. Replace the
Drive base URL through a `DriveApi::with_base_url(...)` constructor (test-only). The
`changes.list` endpoint is the highest-value mock target; `files.get`, `files.list`,
`files.create` (resumable session), `files.update`, `files.delete` round out the coverage.

For the `rclone` subprocess: in unit tests, the `SyncEngine` trait is mocked. In integration
tests that exercise `RcloneEngine`, we run `rclone` against an `rclone` "local" remote (it
supports any filesystem path as a backend) pointing to a tempdir — this verifies our
argument construction is correct without involving Drive at all. The Drive-specific
behaviour is then covered by the `DriveApi` tests + a smaller e2e tier (real test account,
optional).

**Rationale**: this keeps the integration suite hermetic, fast, and CI-friendly. The real
e2e tier is gated on a `AIR_DRIVE_E2E_TOKEN` secret so contributors without one can still
run the full suite locally.

**Alternatives considered**:

- Test against real Drive only — fragile, slow, hits real quota, blocks anonymous PR runs.
- Custom mock crate — wiremock is solid and idiomatic.

## 11. License compatibility of dependencies

All planned direct dependencies have been verified compatible with Apache-2.0 (constitution
III):

| Crate | License |
|---|---|
| `tokio` | MIT |
| `notify` | CC0-1.0 / Artistic-2.0 (dual) → compatible |
| `reqwest` | MIT / Apache-2.0 |
| `serde`, `serde_json` | MIT / Apache-2.0 |
| `yup-oauth2` | MIT |
| `rusqlite` | MIT |
| `clap` | MIT / Apache-2.0 |
| `thiserror` | MIT / Apache-2.0 |
| `tracing`, `tracing-subscriber` | MIT |
| `fd-lock` | MIT / Apache-2.0 |
| `wiremock` (dev-dep) | Apache-2.0 |
| `tempfile` (dev-dep) | MIT / Apache-2.0 |

`rclone` (MIT) is a subprocess, not a linked dependency — the MIT notice will be included in
`THIRD_PARTY_LICENSES` when the v1.0 bundle ships the binary. Out of scope for this feature.

No GPL/AGPL crate in the planned tree.

## 12. Open items deferred to later features

- macOS / Windows support (constitution V partial).
- v1.0 bundle (AppImage / `.app` / Windows installer).
- Multi-account, multi-mapping.
- UI (Tauri).
- Log rotation.
- Token storage in the OS keyring (Secret Service on Linux). MVP uses a 0600 file; keyring
  is a follow-up.
- HTTP control channel for `pause` / `resume` from another machine. MVP uses a local Unix
  socket at `$XDG_RUNTIME_DIR/air-drive/control.sock`.
