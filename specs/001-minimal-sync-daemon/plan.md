# Implementation Plan: Minimal Sync Daemon

**Branch**: `001-minimal-sync-daemon` | **Date**: 2026-05-11 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `specs/001-minimal-sync-daemon/spec.md`

## Summary

Ship a Linux CLI daemon that keeps one local folder in sync with one remote Google Drive
folder, bidirectionally, event-driven on both sides. The MVP closes the gap left by every
existing OSS tool: inotify locally + Drive `changes.list` + `pageToken` remotely, with a
small reconciliation layer on top.

Technical approach: a single Rust crate exposing composable CLI subcommands (`link`, `map`,
`start`, `pause`, `resume`, `status`, plus a `setup` orchestrator). The daemon runs an async
event loop on `tokio` with three sources of work: local `notify` events, remote `changes.list`
polls every ≤ 60 s, and a long-interval safety-net timer. Each work item turns into one of a
small set of atomic operations (upload, download, delete, rename, move, mark-conflict),
applied by a `SyncEngine` trait whose initial implementation drives the embedded `rclone`
binary via `tokio::process::Command` for per-file transfers and uses the Drive REST API
directly for metadata-only operations (list, rename, delete, change cursor). State persists
in a single SQLite file under XDG paths; OAuth tokens persist in a separate 0600 file.

## Technical Context

**Language/Version**: Rust stable (rustup, edition 2024), `#![forbid(unsafe_code)]` at crate level
**Primary Dependencies**: `tokio` (async runtime), `notify` (local watcher), `reqwest` + `serde` / `serde_json` (HTTP/JSON), `yup-oauth2` (OAuth desktop flow with PKCE), `rusqlite` (state DB), `clap` (CLI), `thiserror` (error types), `tracing` + `tracing-subscriber` (structured logs), `fd-lock` or `fslock` (process lock); subprocess invocation of bundled `rclone` ≥ 1.65 for per-file transfers
**Storage**: SQLite (single file, embedded, `rusqlite`) under `$XDG_CONFIG_HOME/air-drive/state.db`; OAuth tokens in a separate 0600 file `$XDG_CONFIG_HOME/air-drive/tokens.json`; runtime lock file under the same directory; logs to stderr (and optionally `$XDG_STATE_HOME/air-drive/air-drive.log` when `--log-file` is set)
**Testing**: `cargo test` (unit and integration). Integration tests use a mocked Drive API (`wiremock`) and a temporary local folder. The `rclone` subprocess is exercised in a smaller set of end-to-end tests against a sandbox folder and a mock HTTP server pretending to be Drive (`rclone` supports any S3/HTTP backend; for Drive the e2e tier ultimately uses a real Google test account, optional and gated on a CI secret)
**Target Platform**: Linux x86_64 (MVP). Linux aarch64 is a stretch goal. macOS / Windows are explicitly out of scope for this feature; principle V of the constitution is partially deferred to a later feature
**Project Type**: Single-crate Rust CLI / daemon application (no UI in this MVP)
**Performance Goals**: local change → upload p95 ≤ 10 s (FR-003 + SC-003), remote change → local p95 ≤ 90 s (FR-004 + SC-004), initial sync 1 GB / 1 000 files ≤ 5 min on 50 Mbps (SC-002), steady-state RSS ≤ 200 MB over 7 days (SC-007), Drive API usage ≤ 10 % of 1 000 req / 100 s quota in steady state (SC-008)
**Constraints**: no `panic!` / `unwrap()` / `expect()` in daemon code; no `unsafe`; sync state always recoverable from SQLite + filesystem after SIGTERM or clean reboot (FR-010, SC-005); zero silent overwrites in true conflicts (FR-006, SC-006); single-instance per config directory enforced by file lock (FR-017)
**Scale/Scope**: 1 linked account, 1 folder mapping, up to ~50 000 watched files and ~5 GB total content in this feature; design must not silently break beyond this but is not required to be optimal for it

## Constitution Check

*GATE: must pass before Phase 0. Re-checked after Phase 1.*

The constitution at `.specify/memory/constitution.md` v1.0.0 defines five principles. Below is
the planned compliance for this feature.

| Principle | Compliance plan | Status |
|---|---|---|
| **I. Rust-First, Memory-Safe by Default** | Stable Rust, `#![forbid(unsafe_code)]` at crate root, `thiserror`-based error types, lints `clippy::unwrap_used` and `clippy::expect_used` at warn level on the `src/` tree (`cfg(test)` excluded). Daemon code returns `Result<T, E>` end-to-end | ✅ |
| **II. Event-Driven Synchronization** | Primary path: `notify` for local events (≤ 2 s detection — FR-003) and `changes.list` + `pageToken` polling at ≤ 60 s for remote (FR-004). A safety-net timer at 5 min reconciles missed events. No `rclone bisync` in the steady-state loop — only per-file operations | ✅ |
| **III. Open Source under Apache-2.0, No Paywall** | All planned deps are MIT or Apache-2.0 (verified in research.md). `rclone` (MIT) is invoked as a subprocess (process boundary). `THIRD_PARTY_LICENSES` will be added when the v1.0 bundle ships rclone — out of scope for this feature (the MVP downloads rclone post-install with checksum verification, per the constitution) | ✅ |
| **IV. Pluggable Sync Engine via Trait Abstraction** | A `SyncEngine` trait is defined for atomic per-file operations (upload, download, delete, move). `RcloneEngine` is the only implementation in this feature. Metadata-only operations on Drive (list, rename, delete, change cursor) go through a separate `DriveApi` client — the trait does NOT leak rclone CLI specifics. A future `NativeEngine` (pure Rust transfers) can be substituted without touching reconcile/state code | ✅ |
| **V. Cross-Platform & Self-Contained Distribution** | This feature ships Linux x86_64 only. Bundling per platform is out of scope; the binary downloads rclone post-install with SHA-256 verification (the "MVP" packaging path from the constitution). macOS / Windows support is a later feature | ⚠ Partial — narrower scope by design |

**Verdict**: PASS. The only deviation from the constitution is the narrower platform scope
(Linux only for this MVP), which the constitution itself anticipates as the first phase of
its distribution roadmap. No violations to track in *Complexity Tracking*.

## Project Structure

### Documentation (this feature)

```text
specs/001-minimal-sync-daemon/
├── plan.md              # This file (/speckit-plan output)
├── spec.md              # Feature spec (already in repo)
├── research.md          # Phase 0 output: tech unknowns resolved
├── data-model.md        # Phase 1 output: entities and persistence schema
├── quickstart.md        # Phase 1 output: how to bring up the dev loop
├── contracts/           # Phase 1 output: external/internal interface contracts
│   ├── cli.md           # CLI command surface (clap)
│   ├── status.schema.json  # JSON schema for `status --json` output
│   └── config.md        # On-disk config file format
├── checklists/
│   └── requirements.md  # Spec quality checklist (already in repo)
└── tasks.md             # Phase 2 output (NOT created here — /speckit-tasks)
```

### Source Code (repository root)

```text
src/
├── main.rs                # Binary entry: tracing init + clap dispatch
├── cli/
│   ├── mod.rs             # Subcommand enum + dispatcher
│   ├── link.rs            # `air-drive link` — OAuth + persist account
│   ├── map.rs             # `air-drive map <local> <remote>` — persist mapping
│   ├── start.rs           # `air-drive start` — run daemon loop
│   ├── pause.rs           # `air-drive pause` — flip pause flag via control socket
│   ├── resume.rs          # `air-drive resume` — flip pause flag via control socket
│   ├── status.rs          # `air-drive status [--json]` — read state, return summary
│   ├── unlink.rs          # `air-drive unlink` — remove account, tokens, mapping
│   └── setup.rs           # `air-drive setup` — interactive wrapper (link+map+start, optional service install)
├── daemon/
│   ├── mod.rs             # Orchestrates watcher + drive_poller + reconciler
│   ├── lock.rs            # Single-instance file lock (FR-017)
│   ├── pause.rs           # In-process pause flag + control socket
│   └── shutdown.rs        # SIGTERM / SIGINT handling, clean drain
├── engine/
│   ├── mod.rs             # `SyncEngine` trait + operation types
│   ├── rclone.rs          # `RcloneEngine` impl driving the rclone subprocess
│   └── rclone_path.rs     # rclone binary resolution + post-install download
├── drive/
│   ├── mod.rs             # Drive API client facade
│   ├── auth.rs            # OAuth + PKCE flow via yup-oauth2; token storage
│   ├── changes.rs         # `changes.list` + pageToken poller
│   ├── metadata.rs        # files.get / files.list / files.update (rename)
│   └── http.rs            # reqwest client, retry/backoff, quota budget
├── watch/
│   ├── mod.rs             # `notify` setup, event filtering, debounce
│   └── debounce.rs        # Coalesce burst events
├── reconcile/
│   ├── mod.rs             # Map local + remote events to atomic ops
│   ├── conflict.rs        # Conflict detection + .conflict-<ts> renaming (FR-006)
│   └── fingerprint.rs     # MD5 + size content matching (avoid phantom conflicts)
├── state/
│   ├── mod.rs             # Repository facade
│   ├── schema.rs          # CREATE TABLE statements + migration runner
│   ├── items.rs           # SyncItem CRUD
│   ├── conflicts.rs       # ConflictRecord CRUD
│   ├── ops.rs             # PendingOperation queue
│   └── cursor.rs          # last seen pageToken
├── config/
│   ├── mod.rs             # Config file load/save (TOML)
│   └── paths.rs           # XDG resolution for config / data / state dirs
└── error.rs               # Crate-wide `Error` enum (thiserror)

tests/
├── unit/                  # Cargo treats each file under tests/ as its own crate;
│                          # we put module-level unit tests inline (`mod tests` in src/)
├── integration/
│   ├── initial_sync.rs    # US1 scenarios
│   ├── continuous_sync.rs # US2 scenarios
│   ├── status.rs          # US3 scenario 1
│   ├── conflict.rs        # US3 scenario 2 (FR-006, SC-006)
│   ├── recovery.rs        # US3 scenario 3 (FR-010, SC-005)
│   ├── relink.rs          # US3 scenario 4 (FR-009)
│   ├── multi_instance.rs  # FR-017
│   └── common/
│       ├── mod.rs
│       ├── drive_mock.rs  # wiremock-based Drive API stub
│       └── fs_fixture.rs  # Tempdir builder helpers
└── e2e/                   # Optional, CI-gated by AIR_DRIVE_E2E_TOKEN
    └── real_drive.rs      # Real Google test-account smoke
```

**Structure Decision**: single Rust crate with module folders. A workspace split (e.g.
extracting `engine`, `drive`, `state` as their own crates) is YAGNI for this feature; the
trait boundaries are enforced at the module level via `pub(crate)` and the trait shape itself.
A workspace can be introduced later without changing the public CLI surface or state DB.

## Complexity Tracking

> Fill ONLY if Constitution Check has violations that must be justified.

No violations to track. The single deviation (Linux-only this feature) is explicitly within
the constitution's phased delivery model and does not require justification under *Complexity
Tracking*.
