# air-drive

Open source Google Drive sync client for Linux (then macOS and Windows), with
bidirectional **event-driven** synchronization on both sides.

## Language

All content produced for this project — code, comments, identifiers, specs, plans, tasks,
docs, commit messages, MR titles and descriptions, issue text — MUST be written in **English**.

This overrides any global instruction to respond in another language. Conversational replies
to the user can still be in their language of choice, but anything written to disk or to a
remote (commits, PRs, issues, code, docs) MUST be in English.

## Source of truth

The project constitution at [`.specify/memory/constitution.md`](./.specify/memory/constitution.md)
is the source of truth for principles, technology stack, quality gates, and governance.

When a README, a comment, or a convention conflicts with the constitution, **the constitution
wins**. Any change to those principles MUST go through the `/speckit-constitution` flow with a
proper Sync Impact Report and version bump.

## TL;DR for agents

- **Language**: Rust stable, `#![forbid(unsafe_code)]` by default.
- **Async**: tokio.
- **No `panic!`/`unwrap`/`expect`** in daemon code — use `Result<T, E>` with `thiserror`.
- **Event-driven sync** is the primary mode; periodic polling is only a safety net.
- **rclone** is invoked as a subprocess via `tokio::process::Command`, behind a
  `SyncEngine` trait — a native Rust engine remains the long-term goal.
- **UI**: Tauri v2.
- **License**: Apache-2.0. Dependencies MUST be compatible (MIT, BSD, ISC, MPL-2.0,
  Apache-2.0). Linked GPL/AGPL is forbidden.

## Workflow

Use the Specify Kit slash commands (`/speckit-*`) for spec-driven development. Quality
gates (cargo fmt, clippy `-D warnings`, tests) MUST pass before merging any MR.
