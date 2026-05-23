---

description: "Task list for implementing `air-drive setup --uninstall-service`"
---

# Tasks: Uninstall systemd user unit (`setup --uninstall-service`)

**Input**: Design documents from `/specs/002-uninstall-service-flag/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/cli-uninstall-service.md, quickstart.md

**Tests**: included — the plan calls for one integration test per user story, driven via
`assert_cmd` with a stub `systemctl` shim on `$PATH` and a temporary `$XDG_CONFIG_HOME`.
The integration suite under `tests/integration/` is the right home (constitution Quality
Gate #5).

**Organization**: tasks are grouped by user story so each P-level slice is independently
shippable. Because the three stories share a single code path with different branches,
incremental implementation tasks within each phase extend the function from US1 to
cover the US2 and US3 cases.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel (different files, no incomplete dependencies)
- **[Story]**: which user story this task belongs to (US1, US2, US3)
- File paths are absolute from repo root

## Path Conventions

Single-project Rust layout: `src/cli/`, `tests/integration/`. The feature touches
`src/cli/mod.rs`, `src/cli/setup.rs`, one new file under `tests/integration/`, plus a
contract doc update under `specs/001-minimal-sync-daemon/contracts/cli.md`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: nothing to set up. The project is mature; the feature reuses the existing
crate, dependencies, and CI pipeline. This phase intentionally contains no tasks.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: wire the new flag through clap and add the integration-test scaffolding
that all three user-story tests will share. These tasks block the user-story phases
because every story exercises the same flag and reuses the same test shim.

**⚠️ CRITICAL**: no user story work can begin until this phase is complete.

- [X] T001 Extend the `Setup` variant in `src/cli/mod.rs` (`Command::Setup`) with a
  second boolean flag `uninstall_service`. Declare it via clap with
  `#[arg(long, conflicts_with = "install_service")]` so the two flags are mutually
  exclusive (FR-008). Update the doc comment to describe the new flag in one line.
- [X] T002 Update the dispatch arm `Command::Setup { install_service, uninstall_service }`
  in `src/cli/mod.rs` to forward both booleans to `setup::run(...)`. The signature of
  `setup::run` will be updated by T003; until then, this task ships the call-site change
  in a way that compiles against the new signature.
- [X] T003 Extend `pub async fn run` in `src/cli/setup.rs` to accept the new
  `uninstall_service: bool` argument. When `uninstall_service` is true, dispatch to a
  new `uninstall_systemd_unit(config_dir_override)` function (skeleton only — body
  filled in T010). When both flags are false, keep the current "interactive setup not
  yet implemented" error so the no-flag UX is unchanged.
- [X] T004 [P] Add a `tests/integration/common/systemctl_shim.rs` helper (or extend the
  existing common-test module if one exists) that creates a temp directory containing
  an executable shell script named `systemctl` which writes its arguments to a
  log file and exits with a configurable code, then returns `(TempDir, PathBuf)` so the
  test can prepend the directory to `$PATH` and later read back the captured arguments.
  This helper is reused by every integration test in this feature.

**Checkpoint**: clap parses `--uninstall-service`, the binary compiles, and tests can
fake `systemctl`. User-story implementation can begin.

---

## Phase 3: User Story 1 — One-command uninstall of the daemon service (Priority: P1) 🎯 MVP

**Goal**: a user who installed the service can remove it with one command — running
unit gets stopped, disabled, the file is removed, the systemd cache is refreshed, and
the command exits 0 with a one-line confirmation.

**Independent Test**: starting from a state where the unit file is present and the
stub `systemctl` is configured to behave like a host where the unit is active, running
`air-drive setup --uninstall-service` returns exit 0, captures three invocations of the
shim (`--user disable --now air-drive.service` and `--user daemon-reload`), and the
unit file is gone from the temp `$XDG_CONFIG_HOME/systemd/user/`.

### Tests for User Story 1 (write FIRST, ensure they FAIL before implementation)

- [X] T005 [P] [US1] Integration test US1 happy path in
  `tests/integration/setup_uninstall_service.rs`. Setup: temp `$XDG_CONFIG_HOME` with
  a pre-written `systemd/user/air-drive.service` file, stub `systemctl` shim on
  `$PATH` exiting 0. Action: invoke the binary via `assert_cmd` with
  `setup --uninstall-service`. Asserts: exit code 0; the shim log captures
  `disable --now air-drive.service` followed by `daemon-reload`; the unit file no
  longer exists.
- [X] T006 [P] [US1] Integration test US1 mutually-exclusive flags in the same file.
  Action: invoke `setup --install-service --uninstall-service`. Asserts: non-zero exit
  (clap error code), stderr contains "cannot be used with", no shim invocation, no
  filesystem change.

### Implementation for User Story 1

- [X] T010 [US1] Implement `uninstall_systemd_unit(config_dir_override: Option<&Path>)`
  in `src/cli/setup.rs`. Body: resolve `$XDG_CONFIG_HOME` via the same
  `runtime::resolve_paths` + `directories::BaseDirs` chain as `install_systemd_unit`
  (FR-010), compute `unit_path = base.config_dir().join("systemd").join("user").
  join("air-drive.service")`, then execute three steps in order:
  (1) `Command::new("systemctl").args(["--user", "disable", "--now",
  "air-drive.service"]).output()` — for US1, assume success and bubble unexpected
  failures via `Error::Config`;
  (2) `std::fs::remove_file(&unit_path)?` — for US1, assume the file is present;
  (3) `Command::new("systemctl").args(["--user", "daemon-reload"]).output()` — same
  handling as step 1.
  Print a single confirmation line to stderr describing what was removed (FR-009).
  Return `Ok(ExitCode::Ok)` on success.
- [X] T011 [US1] Update the module doc comment at the top of `src/cli/setup.rs` to
  describe the new flag alongside `--install-service`. Two lines max, matching the
  style of the existing doc.

**Checkpoint**: US1 fully functional — the headline scenario works end-to-end. Phase
4 extends the function to handle the edge cases the test doesn't yet exercise.

---

## Phase 4: User Story 2 — Idempotent re-run on a clean system (Priority: P2)

**Goal**: running the flag on a host with no unit file and no loaded unit is a
successful no-op. Scripts can call the command unconditionally.

**Independent Test**: starting from a state where no unit file exists and the stub
`systemctl` is configured to behave like a host where the unit is not loaded (exits
non-zero with stderr "Unit air-drive.service could not be found"), running
`setup --uninstall-service` returns exit 0 in under 1 second and prints a "nothing
to remove" message.

### Tests for User Story 2

- [X] T020 [P] [US2] Integration test US2 no-op on clean host in
  `tests/integration/setup_uninstall_service.rs` (same file as US1, additional test
  function). Setup: temp `$XDG_CONFIG_HOME` with no unit file, stub `systemctl`
  configured to exit non-zero on `disable` (simulating "unit not loaded"). Action:
  invoke `setup --uninstall-service`. Asserts: exit 0; no panic / no surfaced error;
  stderr mentions that nothing was removed; wall-clock under 1 s.
- [X] T021 [P] [US2] Integration test US2 double-invocation in the same file. Action:
  run the command twice in a row against an initially clean state. Asserts: both
  invocations exit 0 with the same stderr.

### Implementation for User Story 2

- [X] T022 [US2] Extend `uninstall_systemd_unit` in `src/cli/setup.rs` to treat the
  `systemctl --user disable --now` failure as a soft warning when the failure mode is
  "unit not loaded": inspect stderr, log a `tracing::info!` line, and continue. Any
  other non-zero exit code stays surfaced as `Error::Config` (a real systemd error the
  user should know about — D3 in research.md).
- [X] T023 [US2] Extend `uninstall_systemd_unit` to tolerate `std::fs::remove_file`
  returning `io::ErrorKind::NotFound`: treat it as success, log
  `tracing::info!("no unit file to remove")`, and continue to the `daemon-reload`
  step. Other `io::Error` variants stay surfaced via the existing
  `From<io::Error> for Error` impl.
- [X] T024 [US2] Adjust the confirmation message in `uninstall_systemd_unit` so the
  no-op path prints a clear "nothing to remove" line and the partial path (file gone
  but daemon-reload ran) prints a "cache refreshed" line. Keeps the single-line
  contract from FR-009.

**Checkpoint**: US1 + US2 both pass. The function is idempotent and safe in
automation scripts.

---

## Phase 5: User Story 3 — Graceful fallback on a non-systemd host (Priority: P3)

**Goal**: when `systemctl` is not on `$PATH`, the command still removes the unit file
if present, logs a clear warning, and exits 0.

**Independent Test**: starting from a state with a stray unit file in the temp
`$XDG_CONFIG_HOME` and an empty `$PATH` (no `systemctl` reachable), running
`setup --uninstall-service` emits a warning, removes the file, and exits 0.

### Tests for User Story 3

- [X] T030 [P] [US3] Integration test US3 missing systemctl, file present in
  `tests/integration/setup_uninstall_service.rs`. Setup: temp `$XDG_CONFIG_HOME` with
  a unit file present, `$PATH` set to a directory that does not contain `systemctl`.
  Action: invoke the binary. Asserts: exit 0; stderr contains "systemctl not found";
  the unit file is gone.
- [X] T031 [P] [US3] Integration test US3 missing systemctl, no file in the same
  file. Setup: temp `$XDG_CONFIG_HOME` with no unit file, `$PATH` without
  `systemctl`. Asserts: exit 0; warning emitted; no filesystem change.

### Implementation for User Story 3

- [X] T032 [US3] Extend `uninstall_systemd_unit` in `src/cli/setup.rs` to detect the
  `Command::output()` failure variant `io::ErrorKind::NotFound` (returned when the
  binary is not on `$PATH`). On detection, emit a `tracing::warn!` line via the
  `tracing` macros already in scope, set a `systemctl_skipped` flag, skip both
  systemctl calls (step 1 and step 3 from T010), and continue to the file-removal
  step (which itself handles the file-absent case via T023).
- [X] T033 [US3] Make sure the final confirmation/warning line clearly states when
  systemd interactions were skipped so the user can verify the partial outcome. Reuses
  the message helper introduced in T024 — add one variant.

**Checkpoint**: all three user stories pass independently. SC-001 through SC-005 are
satisfied.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: documentation alignment and validation walk-through.

- [X] T040 [P] Update `specs/001-minimal-sync-daemon/contracts/cli.md` — section
  `### air-drive setup` — to add the `--uninstall-service` row in the flag table and
  update the USAGE block to `air-drive setup [--install-service | --uninstall-service]`
  per `contracts/cli-uninstall-service.md`.
- [X] T041 [P] Update the doc comment on the `Setup` clap variant in `src/cli/mod.rs`
  (the `/// Install the systemd user unit at ...` block above the `install_service`
  flag) to mention the new flag in the variant-level doc, so `air-drive setup --help`
  surfaces both flags clearly.
- [X] T042 Run `cargo fmt --all` and `cargo clippy --all-targets --all-features -- -D
  warnings`. Fix any new findings introduced by the patch (constitution Quality Gates
  #1 and #2).
- [X] T043 Run `cargo test --all-targets` from the repo root. All three new
  integration tests (T005, T020, T021, T030, T031 and the mutually-exclusive flag test
  T006) must pass, and the existing test suite must remain green.
- [X] T044 Walk through every scenario in `specs/002-uninstall-service-flag/
  quickstart.md` on a real Linux/systemd host (or a Linux container with a user-scope
  systemd available). For each scenario, paste the observed output into the PR
  description as evidence (SC-001 through SC-005).

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: empty — start at Phase 2.
- **Foundational (Phase 2)**: must complete before any user-story phase. T001–T003
  are sequential (they touch the same files in a coherent edit); T004 is independent
  and parallel.
- **User Story 1 (Phase 3)**: depends on Phase 2. The function skeleton from T010
  must compile.
- **User Story 2 (Phase 4)**: depends on Phase 3 — extends the same function with
  edge-case handling. Cannot start before T010 lands.
- **User Story 3 (Phase 5)**: depends on Phase 3 — also extends the same function.
  Can run in parallel with Phase 4 only if the developer is comfortable resolving the
  merge in `uninstall_systemd_unit`.
- **Polish (Phase 6)**: depends on Phases 3–5 being complete. T040 and T041 are
  independent of each other; T042–T044 are sequential.

### User Story Dependencies

- **US1 (P1)**: can start after Foundational. No dependency on US2/US3.
- **US2 (P2)**: piggy-backs on US1's `uninstall_systemd_unit` skeleton. Independently
  testable (its tests target the no-op branch).
- **US3 (P3)**: same — extends US1's skeleton in an orthogonal direction
  (the missing-systemctl branch). Independently testable.

### Within Each User Story

- Tests are written first (T005, T020, T030 …) and MUST fail before the matching
  implementation tasks land.
- Each user story is one coherent extension of `uninstall_systemd_unit` — implement
  the branch, then verify the corresponding tests pass.

### Parallel Opportunities

- T004 is parallel with T001–T003 (different file).
- All `[P]` test tasks within a phase run in parallel — they're separate test
  functions in the same file but they share no mutable state.
- Phases 4 and 5 can run in parallel between two developers if they coordinate on the
  shared function. For a single-developer flow, do them sequentially (P2 then P3).

---

## Parallel Example: User Story 1

```bash
# Once Foundational (T001–T004) is complete, write US1 tests in parallel with the
# scaffolding still fresh in mind:
Task: "T005 [US1] Integration test US1 happy path"
Task: "T006 [US1] Integration test US1 mutually-exclusive flags"

# Both fail (no implementation yet). Then sequentially implement US1:
Task: "T010 [US1] uninstall_systemd_unit happy path"
Task: "T011 [US1] Module doc comment update"
```

---

## Implementation Strategy

### MVP First (User Story 1 only)

1. Phase 2: T001 → T002 → T003 → T004
2. Phase 3: T005, T006 (write tests, watch them fail) → T010 → T011 (tests pass)
3. **STOP and validate**: the happy-path scenario from `quickstart.md` passes on a
   real systemd host. The headline feature is shipped.
4. Tag a checkpoint commit; the MR could stop here if time-pressed.

### Incremental Delivery

1. MVP (US1) → standalone commit / MR.
2. Add US2 → standalone commit / MR (idempotency).
3. Add US3 → standalone commit / MR (graceful degradation).
4. Polish phase → final commit / MR (docs + lint + walk-through).

Each step keeps the code green and adds an observable improvement.

### Single-developer order

Recommended linear order for one developer working a single MR: T001 → T002 → T003 →
T004 → T005 → T006 → T010 → T011 → T020 → T021 → T022 → T023 → T024 → T030 → T031 →
T032 → T033 → T040 → T041 → T042 → T043 → T044.

---

## Notes

- `[P]` tasks touch different files OR different test functions in the same file with
  no shared state.
- Every code change lands with the matching test in the same commit when possible —
  test-first within each user story, but the test and implementation can ship in the
  same commit since both are mechanical.
- All written content stays in English per the project's CLAUDE.md.
- After T043, expect zero new clippy warnings — the new function follows the existing
  `install_systemd_unit` pattern almost line-for-line.
