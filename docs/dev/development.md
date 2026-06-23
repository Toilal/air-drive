# Development

How to build, test, and validate changes to air-drive. The contribution
workflow (branches, commit style, PR checklist) lives in
[`../../CONTRIBUTING.md`](../../CONTRIBUTING.md) — this page is the day-to-day
developer reference and does not duplicate it.

## Prerequisites

- **Rust** stable, edition 2024 (`rust-version = 1.85` minimum) via
  [rustup](https://rustup.rs/).
- **rclone** v1.65+ on `$PATH` for end-to-end runs (the mocked suite doesn't
  need it).
- A C toolchain for `rusqlite`'s bundled SQLite build.

## Build

```sh
cargo build              # debug
cargo build --release    # → target/release/air-drive
```

The build embeds a git-aware version string (see `build.rs`) with
SemVer-correct ordering when built off a tag.

## Quality gates

These are the exact checks CI enforces (`.github/workflows/ci.yml`). Run them
before pushing:

```sh
cargo fmt --all -- --check                                   # formatting
cargo clippy --all-targets --all-features -- -D warnings     # lints (warnings = errors)
cargo test --workspace                                       # unit + mocked integration
```

`rustfmt` is pinned to edition 2024, `max_width = 100` (`rustfmt.toml`).
Crate-level import grouping is nightly-only and intentionally left off.

### No panics in daemon code

`#![forbid(unsafe_code)]` is set crate-wide, and `unwrap_used`, `expect_used`,
`panic` are clippy lints at **warn** level (`Cargo.toml [lints.clippy]`).
`missing_docs` is warn too. These are allowed in tests only (`clippy.toml`).
Every expected error path flows through `Result<T, E>` with the `thiserror`
types in `src/error.rs`.

## Test tiers

| Tier             | Location             | Drive API               | Runs                                  |
| ---------------- | -------------------- | ----------------------- | ------------------------------------- |
| Unit             | `src/**`             | n/a                     | every push / PR                       |
| Integration      | `tests/integration`  | mocked                  | every push / PR                       |
| End-to-end (e2e) | `tests/e2e`          | **real** Drive + rclone | `push`/tag on `main`, PRs, manual dispatch |

```sh
cargo test --workspace                       # unit + mocked integration
cargo test --test rclone_drive -- --ignored  # e2e (needs real credentials)
```

- **CI never makes live Drive calls for PR validation** — the integration suite
  runs against a mocked Drive API. Keep it that way.
- The e2e suite needs real OAuth credentials supplied as GitHub Secrets
  (`AIR_DRIVE_E2E_*`) and is isolated in `.github/workflows/e2e.yml`. Fork PRs
  don't receive secrets, so the e2e job is skipped for them. See
  [`../../tests/e2e/README.md`](../../tests/e2e/README.md) for the GCP OAuth
  project setup and how to run it locally.
- Engine integration coverage must include at minimum: a nominal bisync cycle, a
  simple conflict, remote connection loss, and a daemon restart that resumes from
  persisted SQLite state (constitution quality gate #5).
- `tests/integration/sync_matrix.rs` exercises each sync scenario across the four
  cells `{local-origin, remote-origin} × {live, startup}` — *live* = the change
  happens while the daemon runs; *startup* = it happens while stopped and must be
  recovered on the next start (remote via the cursor, local via the startup
  scan). Covers the file lifecycle — `create` / `modify` / `delete` — each across
  all four cells (12 tests); further scenarios (rename, nested dirs) plug into the
  same harness.

## Running the daemon locally

Use an isolated config dir so you don't touch your real setup:

```sh
cargo run -- --config-dir /tmp/air-drive-dev link
cargo run -- --config-dir /tmp/air-drive-dev map ~/Drive 'path:My Drive/Sync'
cargo run -- --config-dir /tmp/air-drive-dev -vv start --initial-sync
```

`--config-dir` co-locates cache and runtime under the same dir, which keeps
parallel/dev daemons from sharing the control socket. `-v`/`-vv`/`-vvv` raise log
verbosity.

## Where to look

See [architecture](architecture.md) for the module map, [sync model](sync-model.md)
for how events become operations, and [state schema](state-schema.md) for the
SQLite layout.
