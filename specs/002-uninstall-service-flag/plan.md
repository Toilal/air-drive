# Implementation Plan: Uninstall systemd user unit (`setup --uninstall-service`)

**Branch**: `002-uninstall-service-flag` | **Date**: 2026-05-23 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/002-uninstall-service-flag/spec.md`

## Summary

Add a `--uninstall-service` flag to `air-drive setup` that mirrors the existing
`--install-service` flag. The new code path stops and disables the user-scope
`air-drive.service` unit, removes the unit file from the user-scope systemd config
directory, and refreshes the systemd cache — three side effects, all idempotent, all
no-op when there is nothing to clean up.

The implementation lives next to the existing `install_systemd_unit` function in
`src/cli/setup.rs`, reuses the same `directories::BaseDirs` path resolution to
guarantee install/uninstall always touch the same file, and reuses the same
`std::process::Command` blocking subprocess pattern (the CLI is a short-lived one-shot,
so there is no value in switching to `tokio::process::Command` for this code path).

The flag is gated as mutually exclusive with `--install-service` at the clap level via
`conflicts_with`. Tests live as a focused integration test under `tests/integration/`
that uses a fake `$XDG_CONFIG_HOME` and a stub `systemctl` shim on `$PATH` so the
test does not depend on a live systemd.

## Technical Context

**Language/Version**: Rust stable (edition 2024), `#![forbid(unsafe_code)]` at the crate level (constitution principle I).
**Primary Dependencies**: `clap` v4 (already a direct dep — argument parsing and `conflicts_with` group), `directories` (already — XDG path resolution), `tracing` (already — log the uninstall steps).
**Storage**: N/A — the command is stateless from air-drive's perspective. It only touches a file on disk and invokes `systemctl`.
**Testing**: `cargo test` for unit coverage of the path-resolution helper, and a single integration test under `tests/integration/` that drives the binary via `assert_cmd` (already used by the existing integration suite) with a temporary `$XDG_CONFIG_HOME` and a stub `systemctl` shim on `$PATH`.
**Target Platform**: Linux x86_64 / aarch64 with systemd user-session as the primary target; the graceful-degradation path (FR-007) covers any other host where `systemctl` is unavailable.
**Project Type**: CLI daemon (single binary).
**Performance Goals**: SC-002 — uninstall completes in ≤ 5 s on a typical desktop; SC-003 — no-op uninstall completes in ≤ 1 s. Both are guard-rails against accidental long timeouts in the systemctl path, not aggressive targets.
**Constraints**: No `panic!` / `unwrap()` / `expect()` in CLI code paths (constitution principle I extends to all `src/`); all error paths flow through `Result<T, E>` with `crate::error::Error`.
**Scale/Scope**: Tiny. One CLI flag, ~80 lines of new Rust in `src/cli/setup.rs` plus ~120 lines of integration test, plus a one-paragraph update to the existing CLI contract (`specs/001-minimal-sync-daemon/contracts/cli.md`).

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Applicability | Verdict |
|---|---|---|
| **I. Rust-First, Memory-Safe by Default** | Applies — new code is in the daemon crate. | ✅ Pass. New code follows the existing `install_systemd_unit` pattern: `Result<ExitCode>` return type, no `unwrap()` / `expect()` / `panic!()`, errors mapped to `crate::error::Error` variants. |
| **II. Event-Driven Synchronization** | Does not apply — this is a one-shot CLI command, not part of the sync loop. | ✅ N/A. |
| **III. Open Source under Apache-2.0** | Applies — no new dependencies introduced, no paywall, no GPL pulled in. | ✅ Pass. |
| **IV. Pluggable Sync Engine** | Does not apply — no engine interaction. | ✅ N/A. |
| **V. Cross-Platform & Self-Contained Distribution** | Applies — feature is Linux/systemd-specific. | ✅ Pass. The flag silently degrades on non-systemd hosts (FR-007); no new system dependency. |

Quality gates:

- `cargo fmt` clean — enforced by CI, no special handling.
- `cargo clippy -D warnings` — enforced by CI; the new function follows the same shape as `install_systemd_unit`, no new lint surface.
- Integration test added (constitution §"Quality Gates" item 5 — at minimum an integration test exists for this new code path).

**Result**: all gates pass with zero violations. No entries in the Complexity Tracking table.

## Project Structure

### Documentation (this feature)

```text
specs/002-uninstall-service-flag/
├── plan.md              # This file
├── research.md          # Phase 0 — decisions + rejected alternatives
├── data-model.md        # Phase 1 — N/A note (no persistent state)
├── quickstart.md        # Phase 1 — end-user how-to + dev test recipe
├── contracts/
│   └── cli-uninstall-service.md  # Delta to the existing CLI contract
├── checklists/
│   └── requirements.md  # Spec quality checklist (already created)
└── tasks.md             # Phase 2 output (/speckit-tasks command — NOT in this command)
```

### Source Code (repository root)

```text
src/
├── cli/
│   ├── mod.rs         # Edit: extend `Setup { install_service }` with `uninstall_service`,
│   │                  #       wire `conflicts_with = "install_service"`, dispatch both flags
│   │                  #       to `setup::run`.
│   └── setup.rs       # Edit: add `uninstall_systemd_unit()` next to `install_systemd_unit()`,
│                      #       extend `pub async fn run(...)` signature with the new flag.
└── error.rs           # No new variant needed — reuse `Error::Config` and the existing
                       # `From<io::Error>` impl.

tests/
└── integration/
    └── setup_uninstall_service.rs  # New file: full-flow integration test using
                                    # `assert_cmd` + temp `$XDG_CONFIG_HOME` + stub
                                    # `systemctl` shim on `$PATH`.

specs/001-minimal-sync-daemon/contracts/
└── cli.md             # Edit: add `--uninstall-service` row to the `air-drive setup`
                       # USAGE block.
```

**Structure Decision**: this is a focused single-binary CLI feature — Option 1 (single project) from the template. No new module hierarchy is introduced; the change is two new functions inside the existing `src/cli/setup.rs` and a clap argument extension in `src/cli/mod.rs`. The integration test slots into the existing `tests/integration/` tree which already has precedent for binary-level test cases.

## Complexity Tracking

> No constitution-check violations. Section intentionally empty.

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| — | — | — |
