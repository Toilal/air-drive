# Contributing to air-drive

Thanks for your interest in improving air-drive! This document explains how to
set up your environment, the conventions the project follows, and the quality
gates every change must pass before it lands on `main`.

By contributing you agree that your contributions are licensed under the
project's [Apache License 2.0](./LICENSE).

## Language

All content written to the repository — code, comments, identifiers, docs,
commit messages, PR titles and descriptions, issue text — **MUST be in
English**. Conversational discussion (issues, reviews) can happen in any
language, but anything committed to the tree is English.

## Project principles

Before making non-trivial changes, read [`CLAUDE.md`](./CLAUDE.md). It is the
project constitution and the source of truth for architecture decisions. The
points most likely to affect a contribution:

- **Rust-first, memory-safe.** Stable Rust, `#![forbid(unsafe_code)]` at the
  crate level. Justify any exception at review.
- **No `panic!` / `unwrap()` / `expect()` in daemon code.** Every expected
  error path flows through `Result<T, E>` with a `thiserror` error type. These
  are lints (`unwrap_used`, `expect_used`, `panic` at warn level) and are
  allowed in tests only.
- **Event-driven sync is the primary mode.** inotify locally, `changes.list` +
  `pageToken` remotely. Periodic polling is a safety net, never the main path —
  a feature that relies solely on polling must document why.
- **Pluggable engine.** Application code talks to the `SyncEngine` trait, never
  to `rclone`'s CLI directly. `RcloneEngine` is the current implementation; a
  native engine is the long-term goal.
- **Apache-2.0, no paywall.** New dependencies must be Apache-2.0–compatible
  (MIT, BSD-2/3-Clause, ISC, MPL-2.0, Apache-2.0). Linked GPL/AGPL is
  forbidden; a GPL/AGPL tool invoked across a process boundary is case-by-case
  and must be listed in `THIRD_PARTY_LICENSES`.

## Prerequisites

- **Rust** stable, edition 2024 (`rust-version = "1.85"` minimum). Install via
  [rustup](https://rustup.rs/).
- **rclone** v1.65+ on your `$PATH` for end-to-end runs (the mocked test suite
  does not need it).
- A C toolchain for `rusqlite`'s bundled SQLite build.

```sh
git clone https://github.com/Toilal/air-drive
cd air-drive
cargo build
```

## Development workflow

### Branches

Use a short, descriptive branch name. CI runs on every pull request targeting
`main`.

### Commits

- **Atomic**: one commit = one coherent change.
- **Conventional Commits** with a scope mirroring the `src/` module layout,
  in the imperative present tense. Examples from history:
  - `feat(cli): prompt-or-fail policy for missing local + remote roots`
  - `fix(oauth): switch Drive scope from drive.file to drive`
  - `chore(github): add issue templates for bug reports and feature requests`

### Running the checks locally

These are the exact gates CI enforces (`.github/workflows/ci.yml`). Run them
before opening a PR:

```sh
cargo fmt --all -- --check                                   # formatting
cargo clippy --all-targets --all-features -- -D warnings     # lints (warnings = errors)
cargo test --workspace                                       # unit + mocked integration
```

`rustfmt` is configured for edition 2024 and `max_width = 100`
(see [`rustfmt.toml`](./rustfmt.toml)). Crate-level import grouping is a
nightly-only rustfmt feature and is intentionally left off for stable.

## Tests

The suite has three tiers:

| Tier              | Location             | Drive API     | When it runs                          |
| ----------------- | -------------------- | ------------- | ------------------------------------- |
| Unit              | `src/**`             | n/a           | every push / PR                       |
| Integration       | `tests/integration` | mocked        | every push / PR                       |
| End-to-end (e2e)  | `tests/e2e`          | **real** Drive + rclone | `push` to `main` + manual dispatch |

- **No live Drive calls in CI for PRs** — integration tests run against a
  mocked Drive API. Keep it that way.
- The e2e suite needs real OAuth credentials supplied as GitHub Secrets and is
  isolated in `.github/workflows/e2e.yml`. See
  [`tests/e2e/README.md`](./tests/e2e/README.md) for the GCP OAuth project
  setup and how to run it locally.
- Per the quality gates, engine-level integration coverage must include at
  least: a nominal bisync cycle, a simple conflict, remote connection loss, and
  a daemon restart that resumes from persisted SQLite state.

## Persistence and state

Sync state — Drive `pageToken`, `bisync` state, unresolved conflicts, tracked
folders, accounts — lives in SQLite (`rusqlite`), never in memory only. The
schema is versioned with explicit migrations (`src/state/`). The data model is
multi-account from day one; do not introduce single-account assumptions.

## OAuth scope

The daemon requests the broad `https://www.googleapis.com/auth/drive` scope on
purpose — `drive.file` only exposes files the daemon created, which cannot sync
a pre-populated folder. Do not narrow it. Rationale is in [`CLAUDE.md`](./CLAUDE.md)
§V and the README's OAuth section.

## Pull requests

Before requesting review, confirm:

- [ ] All three CI gates pass locally (`fmt`, `clippy -D warnings`, `test`).
- [ ] No new `unwrap()` / `expect()` / `panic!()` in non-test daemon code.
- [ ] New dependencies are Apache-2.0–compatible.
- [ ] Public types and functions have `///` doc comments (`missing_docs` is
      warn-level).
- [ ] Any new feature flag has a retirement timeline.
- [ ] Commits are atomic with Conventional-Commit messages in English.

Open the PR against `main`. A maintainer will review; expect questions tied to
the principles above.

## Reporting bugs and requesting features

Use the issue templates in `.github/ISSUE_TEMPLATE/` (bug report / feature
request). For security-sensitive reports, please avoid filing a public issue
and contact a maintainer directly.
