<!--
SYNC IMPACT REPORT
==================
Version change: (initial) → 1.0.0
Modified principles: N/A (initial ratification — all five principles newly defined)
Added sections:
  - Core Principles (×5)
  - Technology Stack & Constraints
  - Quality Gates & Development Workflow
  - Governance
Removed sections: N/A
Templates requiring updates:
  - ✅ .specify/templates/plan-template.md — "Constitution Check" slot present, no changes required
  - ✅ .specify/templates/spec-template.md — no principle-specific content to align
  - ✅ .specify/templates/tasks-template.md — task categories remain generic, no changes required
  - ✅ .specify/templates/commands/*.md — directory not present, nothing to update
Runtime guidance docs:
  - ✅ CLAUDE.md — present, references this constitution as source of truth and mandates English
  - ✅ README.md — present, English skeleton with link to this constitution
  - ✅ LICENSE — Apache-2.0 text added
  - ✅ NOTICE — attribution file added
  - ⚠ THIRD_PARTY_LICENSES — to be created when the rclone binary is bundled (v1.0)
Follow-up TODOs: N/A
-->

# air-drive Constitution

air-drive is an open source Google Drive sync client for Linux (later macOS and Windows),
with **bidirectional event-driven synchronization on both sides** — inotify locally,
`changes.list` + `pageToken` remotely. The sync engine is `rclone`, embedded as a subprocess
at start, behind an abstraction that keeps the door open for a native Rust engine later.

## Core Principles

### I. Rust-First, Memory-Safe by Default

All application code MUST be written in stable Rust, compiled with `#![forbid(unsafe_code)]`
at the crate level unless explicitly justified per crate. The async runtime is `tokio`. Daemon
code MUST avoid unjustified `unwrap()`, `expect()` and `panic!()`: every expected error path
flows through `Result<T, E>` with an explicit error type (typically `thiserror`).

**Rationale**: the daemon runs in the background continuously and touches user files. A panic
means a lost sync session, possibly corrupted state. Rust safety plus a no-panic discipline in
the daemon are the first line of defense.

### II. Event-Driven Synchronization

The primary operating mode MUST be event-driven on both sides:

- **Local**: `notify` events (inotify on Linux, FSEvents on macOS, ReadDirectoryChangesW on
  Windows), debounced before triggering a sync cycle.
- **Remote**: `changes.list` + `pageToken` via the Drive API, short polling (≤ 60 s) as long
  as no push mechanism without a public HTTPS endpoint is available.

A periodic safety-net timer (≥ 5 min) is allowed as a guard against missed events, but MUST
NOT be the primary mode. Any feature that relies solely on periodic polling MUST explicitly
document why the event-driven mode does not apply.

**Rationale**: this is the product differentiator. No free OSS tool today does event-driven
sync on both sides — that is the project's reason to exist.

### III. Open Source under Apache-2.0, No Paywall

The project is licensed under **Apache License 2.0** and stays that way. No feature MUST be
gated behind a paywall, a required account, or a commercial feature flag. Every dependency
MUST be Apache-2.0–compatible: MIT, BSD-2/3-Clause, ISC, MPL-2.0, and Apache-2.0 are notably
compatible. GPL/AGPL dependencies (linked in code) are **forbidden**; GPL/AGPL tools invoked
as a subprocess (process boundary) remain allowed on a case-by-case basis and MUST be listed
in `THIRD_PARTY_LICENSES`.

Distribution of the `rclone` binary (MIT): the copyright notice and the MIT license text MUST
be included in any bundle that redistributes rclone (AppImage, `.app`, Windows installer),
typically via `THIRD_PARTY_LICENSES`.

**Rationale**: this directly addresses the original frustration — no official Drive client on
Linux, and the only comfortable alternative (Insync) is paid. Apache-2.0 provides an explicit
patent grant (useful as soon as we touch OAuth and third-party APIs) while remaining permissive
enough to maximize adoption and contributions.

### IV. Pluggable Sync Engine via Trait Abstraction

The sync engine MUST be encapsulated behind a Rust trait (typically `SyncEngine`). Application
code MUST NOT depend directly on `rclone`'s CLI specifics. The initial implementation is
`RcloneEngine`, which drives the `rclone` binary via `tokio::process::Command`. A native Rust
implementation (`NativeEngine`) remains the long-term goal and MUST be substitutable without
modifying the rest of the daemon.

**Rationale**: `rclone` brings ~8 years of solved edge cases (native Google Docs, shortcuts,
shared folders, throttling, error recovery, renames). Reimplementing that from day one slows
the MVP. The abstraction guarantees we are not locked into rclone forever.

### V. Cross-Platform & Self-Contained Distribution

The deliverable MUST be a single binary per platform, with no non-trivial system dependencies
beyond a system webview (used by Tauri). Target platforms, by priority: Linux x86_64, Linux
aarch64, macOS (aarch64 and x86_64), Windows x86_64.

The `rclone` binary MUST be embedded:

- **MVP**: post-install download from `downloads.rclone.org` with verification of the
  rclone-published SHA256 checksum, cached locally at `~/.cache/air-drive/bin/rclone`.
- **v1.0**: full bundle (Linux AppImage, macOS `.app`, Windows installer).

The UI MUST be served via Tauri (Rust backend + system webview). A tray-only UI built on
`tray-icon` + `tao` is an acceptable fallback if Tauri causes issues on a given platform.

**Rationale**: to match the Insync user experience, the user must not have to install a Python
toolchain, npm, or anything similar.

## Technology Stack & Constraints

Canonical stack. Any deviation MUST be justified in the MR concerned and approved at review.

- **Toolchain**: stable Rust (rustup), latest supported edition.
- **Async runtime**: `tokio` (multi-thread).
- **Local watcher**: `notify`.
- **HTTP + JSON**: `reqwest` + `serde` / `serde_json`.
- **Google OAuth**: `yup-oauth2` (desktop flow, refresh tokens persisted).
- **Drive API**: hand-written REST calls via `reqwest` by default; `google-drive3` is allowed
  but reserved for endpoints that are too verbose to call by hand.
- **Persistence**: `rusqlite` (single-file embedded SQLite). Versioned schema, explicit
  migrations.
- **UI**: `tauri` v2, lightweight frontend (Svelte, Vue, or React — to be decided).
- **External sync engine**: `rclone` v1.65+ (`bisync` stable).

Runtime constraints:

- **Drive API quota**: 1000 req / 100 s / user. Any polling code MUST budget its rate to stay
  under 10 % of that limit in steady state.
- **Multi-account**: the data model MUST support N Drive accounts from day one — no
  single-account schema with "we'll extend it later".
- **Sync state**: Drive `pageToken`, `bisync` state, unresolved conflicts, and tracked folders
  MUST all be persisted to SQLite, never in memory only.

## Quality Gates & Development Workflow

Every MR MUST pass these gates before merge:

1. **Format**: `cargo fmt --all -- --check` clean.
2. **Lint**: `cargo clippy --all-targets --all-features -- -D warnings` clean.
3. **Tests**: `cargo test` green on Linux x86_64 at a minimum. Cross-platform tests run in CI
   on at least Linux and macOS.
4. **No unjustified `panic!()` / `unwrap()` / `expect()`** in daemon code (`src/`, tests
   excluded) — `clippy::unwrap_used` and `clippy::expect_used` enabled at least at warn level.
5. **Integration tests** on the sync engine: at minimum cover a nominal bisync cycle, a simple
   conflict, remote connection loss, and daemon restart with persisted state.
6. **Mocked Drive API** in integration tests (no live calls in CI).
7. **Atomic commits**: one commit = one coherent change, message in imperative present tense.
8. **Issue reference** in the MR if an issue exists.

Non-blocking gates, still expected:

- Public documentation (`///`) on types and functions exposed by each crate.
- No feature flag left behind without a retirement timeline.

## Governance

This constitution supersedes other project practices. In case of conflict with a README, a
comment, or an informal convention, the constitution wins.

**Amendment procedure**: every change goes through an MR that modifies this file and provides
an updated Sync Impact Report (see the HTML comment header). Amendments require the main
maintainer's approval. Purely editorial changes can be merged in self-review.

**Versioning**: MAJOR for incompatible removal/redefinition of a principle or governance rule,
MINOR for a new principle/section or material expansion, PATCH for clarification, rewording,
or typo fix.

**Compliance review**: every feature MR MUST verify compliance with the principles (the
"Constitution Check" slot in `plan-template.md` is used for this). A justified violation MUST
be documented in the "Complexity Tracking" table of the corresponding plan.

**Runtime guidance**: `CLAUDE.md` at the project root provides operational context for agents
and contributors. It references this constitution as the source of truth.

**Version**: 1.0.0 | **Ratified**: 2026-05-11 | **Last Amended**: 2026-05-11
