# air-drive

Open source Google Drive sync client for Linux (then macOS and Windows), with
bidirectional **event-driven synchronization on both sides** — inotify locally,
`changes.list` + `pageToken` remotely. The sync engine is `rclone`, embedded
as a subprocess via `tokio::process::Command`, behind a `SyncEngine` trait
that keeps the door open for a native Rust engine later.

## Language

All content produced for this project — code, comments, identifiers, docs,
commit messages, PR titles and descriptions, issue text — MUST be written in
**English**.

This overrides any global instruction to respond in another language.
Conversational replies to the user can still be in their language of choice,
but anything written to disk or to a remote MUST be in English.

## TL;DR for agents

- **Language**: Rust stable, `#![forbid(unsafe_code)]` by default.
- **Async**: tokio.
- **No `panic!` / `unwrap` / `expect`** in daemon code — use `Result<T, E>`
  with `thiserror`.
- **Event-driven sync** is the primary mode; periodic polling is only a
  safety net.
- **rclone** is invoked as a subprocess, behind a `SyncEngine` trait.
- **UI**: Tauri v2.
- **License**: Apache-2.0. Dependencies MUST be Apache-2.0–compatible (MIT,
  BSD, ISC, MPL-2.0, Apache-2.0). Linked GPL/AGPL is forbidden.
- **Docs**: the `docs/` tree (see [Documentation](#documentation)) MUST be kept
  in sync with every change that affects it — same commit as the code.

## Documentation

End-user and contributor documentation lives under [`docs/`](./docs/), indexed
by [`docs/README.md`](./docs/README.md):

- **User guides** ([`docs/user/`](./docs/user/)):
  [`installation.md`](./docs/user/installation.md),
  [`cli.md`](./docs/user/cli.md),
  [`configuration.md`](./docs/user/configuration.md),
  [`oauth-setup.md`](./docs/user/oauth-setup.md).
- **Internals** ([`docs/dev/`](./docs/dev/)):
  [`architecture.md`](./docs/dev/architecture.md),
  [`sync-model.md`](./docs/dev/sync-model.md),
  [`state-schema.md`](./docs/dev/state-schema.md),
  [`development.md`](./docs/dev/development.md).
- **Project**: [`docs/roadmap/`](./docs/roadmap/) — one ordered file per planned
  feature; see [Roadmap](#roadmap) below.

These docs are **part of the change, not an afterthought**. Any change that
alters documented behaviour MUST update the affected doc **in the same commit**.
Treat a stale doc as a bug. In particular:

- New / changed / removed **CLI command, flag, or exit code** → `docs/user/cli.md`.
- New / changed / removed **`config.toml` key**, on-disk path, or ignore-pattern
  default → `docs/user/configuration.md`.
- **OAuth scope** or client-bootstrap flow change → `docs/user/oauth-setup.md`.
- **Install / service** flow change → `docs/user/installation.md`.
- New module, loop, or **`SyncEngine`** change → `docs/dev/architecture.md`.
- **Event handling, reconciliation, conflict, or retry** change →
  `docs/dev/sync-model.md`.
- **SQLite schema migration** (new `Vn_SCHEMA`, bumped `LATEST_VERSION`) →
  `docs/dev/state-schema.md`.
- **Build, test tier, or quality-gate** change → `docs/dev/development.md`
  (and [`CONTRIBUTING.md`](./CONTRIBUTING.md) if the workflow changes).

When adding a brand-new documentable surface, also add it to the index in
`docs/README.md`. All docs are English (see [Language](#language)).

## Roadmap

[`docs/roadmap/`](./docs/roadmap/) is a **living document** — keep it alive as
the project evolves, not as a write-once wish list. Each planned feature is its
own `NNN-slug.md` file, ordered by a three-digit prefix (gaps of 10 so items can
be reordered without renumbering), and indexed in
[`docs/roadmap/README.md`](./docs/roadmap/README.md).

Use the roadmap as **lightweight specs before development**. Before starting a
feature, read its roadmap file (and refine it if scope is unclear) — the
*Goal / Today / Approach / Acceptance* sections are the brief you build against.
When a new direction is decided, add a roadmap file for it (with its tracking
issue) **before** writing code.

When a feature is **finished**, **delete its roadmap entry**: remove the
`NNN-slug.md` file and its row in the index, in the same merge that ships the
feature. The roadmap describes what is *not yet done* — a shipped feature lives
in the code and the `docs/user/` + `docs/dev/` documentation, never in the
roadmap. Treat a roadmap entry for already-shipped work as a bug.

Keep the roadmap and the [GitHub issue tracker](https://github.com/Toilal/air-drive/issues)
consistent: a roadmap file should link its issue where one exists.

## Core principles

### I. Rust-first, memory-safe by default

Stable Rust, `#![forbid(unsafe_code)]` at the crate level unless explicitly
justified per crate. Async runtime is `tokio`. Daemon code MUST avoid
unjustified `unwrap()`, `expect()` and `panic!()`: every expected error path
flows through `Result<T, E>` with an explicit error type (typically
`thiserror`).

*Why*: the daemon runs continuously and touches user files. A panic means a
lost sync session, possibly corrupted state.

### II. Event-driven synchronization

The primary operating mode MUST be event-driven on both sides:

- **Local**: `notify` events (inotify on Linux, FSEvents on macOS,
  ReadDirectoryChangesW on Windows), debounced before triggering a sync cycle.
- **Remote**: `changes.list` + `pageToken` via the Drive API, short polling
  (≤ 60 s) as long as no push mechanism without a public HTTPS endpoint is
  available.

A periodic safety-net timer (≥ 5 min) is allowed as a guard against missed
events, but MUST NOT be the primary mode. Any feature that relies solely on
periodic polling MUST explicitly document why the event-driven mode does not
apply.

*Why*: this is the product differentiator. No free OSS tool today does
event-driven sync on both sides — that is the project's reason to exist.

### III. Apache-2.0 with no paywall

The project is licensed under Apache License 2.0 and stays that way. No
feature is gated behind a paywall, a required account, or a commercial
feature flag. Every dependency MUST be Apache-2.0–compatible: MIT,
BSD-2/3-Clause, ISC, MPL-2.0, Apache-2.0. GPL/AGPL dependencies (linked in
code) are **forbidden**; GPL/AGPL tools invoked as a subprocess (process
boundary) remain allowed on a case-by-case basis and MUST be listed in
`THIRD_PARTY_LICENSES`.

Distribution of the `rclone` binary (MIT): copyright notice and MIT license
text MUST be included in any bundle that redistributes rclone (AppImage,
`.app`, Windows installer).

### IV. Pluggable sync engine via trait abstraction

The sync engine MUST be encapsulated behind the `SyncEngine` Rust trait.
Application code MUST NOT depend directly on `rclone`'s CLI specifics. The
initial implementation is `RcloneEngine`, which drives the `rclone` binary
via `tokio::process::Command`. A native Rust implementation (`NativeEngine`)
remains the long-term goal and MUST be substitutable without modifying the
rest of the daemon.

*Why*: `rclone` brings ~8 years of solved edge cases (native Google Docs,
shortcuts, shared folders, throttling, error recovery, renames).
Reimplementing that from day one slows the MVP. The abstraction guarantees
we are not locked into rclone forever.

### V. Full `drive` OAuth scope

The daemon requests the broad `https://www.googleapis.com/auth/drive` scope —
not the narrower `drive.file`. A sync client whose visibility is limited to
files it created itself cannot sync an already-populated Drive folder, which
defeats the product. Per-folder write grants do not exist in Google's OAuth
surface, so a combination like `drive.readonly` + `drive.file` would still hide
pre-existing content from the local watcher's perspective.

*Trade-off*: `drive` is classified as a sensitive scope. Clients in `Testing`
mode on the OAuth consent screen cap refresh-token lifetime at 7 days, and
moving to `Production` requires Google's OAuth verification review (security
assessment, homepage + privacy-policy URLs, demo video). This is accepted as
the cost of doing the job correctly.

*Why*: any sync client that cannot see the user's existing files is a
non-starter. Issue #4 captures the original report.

### VI. Cross-platform, self-contained distribution

A single binary per platform, with no non-trivial system dependencies beyond
a system webview (Tauri). Target platforms by priority: Linux x86_64, Linux
aarch64, macOS (aarch64 and x86_64), Windows x86_64.

The `rclone` binary MUST be embedded:

- **MVP**: post-install download from `downloads.rclone.org` with SHA-256
  verification, cached at `~/.cache/air-drive/bin/rclone`.
- **v1.0**: full bundle (Linux AppImage, macOS `.app`, Windows installer).

UI served via Tauri (Rust backend + system webview). A tray-only UI built on
`tray-icon` + `tao` is an acceptable fallback if Tauri causes issues on a
given platform.

## Technology stack

Canonical stack. Any deviation MUST be justified at review.

- **Toolchain**: stable Rust (rustup), latest supported edition.
- **Async runtime**: `tokio` (multi-thread).
- **Local watcher**: `notify`.
- **HTTP + JSON**: `reqwest` + `serde` / `serde_json`.
- **Google OAuth**: `yup-oauth2` (desktop flow, refresh tokens persisted).
- **Drive API**: hand-written REST calls via `reqwest` by default;
  `google-drive3` is allowed but reserved for endpoints that are too verbose
  to call by hand.
- **Persistence**: `rusqlite` (single-file embedded SQLite). Versioned
  schema, explicit migrations.
- **UI**: `tauri` v2, lightweight frontend (Svelte, Vue, or React — TBD).
- **External sync engine**: `rclone` v1.65+ (`bisync` stable).

Runtime constraints:

- **Drive API quota**: 1000 req / 100 s / user. Polling code MUST budget its
  rate to stay under 10 % of that limit in steady state.
- **Multi-account**: the data model MUST support N Drive accounts from day
  one — no single-account schema with "we'll extend it later".
- **Sync state**: Drive `pageToken`, `bisync` state, unresolved conflicts,
  and tracked folders MUST all be persisted to SQLite, never in memory only.

## Quality gates

Every change MUST pass these gates before landing on `main`:

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets --all-features -- -D warnings` clean.
3. `cargo test` green on Linux x86_64 at a minimum. Cross-platform tests run
   in CI on at least Linux and macOS.
4. **No unjustified `panic!()` / `unwrap()` / `expect()`** in daemon code
   (`src/`, tests excluded) — `clippy::unwrap_used` and `clippy::expect_used`
   are enabled at least at warn level in `Cargo.toml`.
5. Integration tests on the sync engine: at minimum cover a nominal bisync
   cycle, a simple conflict, remote connection loss, and daemon restart with
   persisted state.
6. Mocked Drive API in integration tests (no live calls in CI).
7. Atomic commits: one commit = one coherent change, message in imperative
   present tense.

Non-blocking but expected: public `///` doc comments on types and functions
exposed by each crate; no feature flag left without a retirement timeline.
