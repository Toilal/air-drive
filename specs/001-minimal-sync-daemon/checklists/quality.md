# Quality Checklist: Minimal Sync Daemon

**Purpose**: Master pre-implementation sweep validating that the requirements in `spec.md`
are complete, clear, consistent, and measurable across four domains: data integrity /
reliability, security, CLI contract clarity, and non-functional / performance budgets.
**Created**: 2026-05-11
**Feature**: [spec.md](../spec.md)

**Note**: These items test the *requirements writing*, not the implementation. Each item
asks whether the spec defines something well, not whether code behaves correctly.

## Requirement Completeness

- [x] CHK001 - Are unlink/revoke requirements specified so a user can disconnect the linked Drive account? [Gap]
- [x] CHK002 - Are requirements defined for handling the watched **remote folder being deleted on Drive** while the daemon is running? [Coverage, Spec §Edge Cases]
- [x] CHK003 - Are requirements defined for **local files that become unreadable** (perms changed mid-flight, EACCES at read time)? [Gap]
- [x] CHK004 - Are requirements documented for **disk-full conditions during a download in progress** (vs. the existing edge case which only covers files larger than free space at start)? [Coverage, Spec §Edge Cases]
- [x] CHK005 - Are the OAuth **scopes** explicitly enumerated in the spec (`drive.file` + `drive.metadata.readonly`), not only in research? Principle of least privilege deserves a top-level requirement. [Gap, Spec §FR-001]
- [x] CHK006 - Is the **service manager** specified for "installable as a user-level service" (systemd user unit, launchd, …)? [Gap, Spec §FR-014]
- [x] CHK007 - Are requirements defined for what happens when the user-supplied **OAuth client_id override is invalid or unauthorised**? [Gap, Spec §FR-001 + §Clarifications]
- [x] CHK008 - Are requirements specified for a folder mapping whose `local_path` has been **deleted or moved between runs**? [Gap, Spec §FR-002]
- [x] CHK009 - Is there a requirement for **migrating the state DB schema** when the user upgrades the binary (and a behaviour when downgrading)? [Gap]
- [x] CHK010 - Is there a requirement covering the **initial-sync failure mode** (partial upload then crash) and the expected state on restart? [Coverage, Recovery Flow, Gap]

## Requirement Clarity

- [x] CHK011 - Is "matching files" quantified in the initial-sync scenario — same name + identical content fingerprint, or something weaker? [Ambiguity, Spec §US1.5 + §FR-005]
- [x] CHK012 - Is "restrictive permissions" quantified with explicit mode bits (e.g., `0600`) rather than left to interpretation? [Ambiguity, Spec §FR-016]
- [x] CHK013 - Is the **conflict-suffix timestamp** format specified with explicit precision and time zone (e.g., `YYYYMMDDTHHMMSSZ` second-precision UTC)? [Ambiguity, Spec §FR-006]
- [x] CHK014 - Is "nominal network conditions" (SC-003 / SC-004) defined with measurable bounds (loss %, latency, bandwidth)? Otherwise the success criterion is not testable. [Ambiguity, Spec §SC-003, §SC-004]
- [x] CHK015 - Is "steady state" (SC-008) defined — idle? idle + occasional changes? a specific event rate? Otherwise the 10% quota budget cannot be checked. [Ambiguity, Spec §SC-008]
- [x] CHK016 - Is "machine-readable form" in FR-008 explicitly named **JSON** (with a reference to the schema), or only implied? [Clarity, Spec §FR-008]
- [x] CHK017 - Is "consistent state" (FR-010) defined in implementable terms — what specific invariants survive a crash? [Clarity, Spec §FR-010]
- [x] CHK018 - Is "half-written files" defined operationally — does the spec forbid exposing a partial download under its final name? [Clarity, Spec §FR-010]
- [x] CHK019 - Are the **resume-from-pause semantics** specified — does the daemon replay events that occurred while paused, or only converge from the new state? [Ambiguity, Spec §FR-015 + §Edge Cases "Sync paused"]
- [x] CHK020 - Are **exponential-retry parameters** quantified (initial delay, max delay, jitter range, max attempts before giving up)? [Gap, Spec §FR-012]
- [x] CHK021 - Is the precise meaning of "skip" for native Google Docs defined — leave them invisible to the local tree? Place a `.gdoc` shortcut? Refuse to download but record an item row? [Clarity, Spec §FR-011]

## Requirement Consistency

- [x] CHK022 - Does US3.3 ("crashed or rebooted mid-sync") align with SC-005 (covers SIGTERM and "cleanly rebooted")? **SIGKILL / `kill -9`** is named in the acceptance scenario but not in the success criterion. [Conflict, Spec §US3.3 vs §SC-005]
- [x] CHK023 - Is the **delete-vs-edit conflict** outcome consistent with FR-006? FR-006 says both versions are preserved (one canonical name + one suffixed); the delete-vs-edit edge case says "preserve the edited version, suppress the delete" — only one file remains, so no `.conflict-` file. Which side keeps which name? [Conflict, Spec §FR-006 vs §Edge Cases]
- [x] CHK024 - Are the rate-limit requirements consistent between FR-012 (transient back-off) and SC-008 (10 % steady-state budget)? Does back-off count against the budget? [Consistency, Spec §FR-012 + §SC-008]
- [x] CHK025 - Is the **conflict-record lifecycle** fully specified — FR-006 says "surfaced until the user resolves it by deleting or renaming the offending file", but no FR defines how the daemon detects that resolution to clear the record. [Gap, Spec §FR-006]

## Acceptance Criteria Quality

- [x] CHK026 - Can SC-002 ("≤ 5 min on 50 Mbps") be objectively measured given the spec does not define what counts as "start" (CLI invocation? lock acquired? first byte transferred?) and "end" (queue empty? idle state? `status` reports idle)? [Measurability, Spec §SC-002]
- [x] CHK027 - Can SC-005 ("zero lost edits, zero corrupted files") be verified without definitions of **"lost edit"** and **"corrupted file"** in the spec? [Measurability, Spec §SC-005]
- [x] CHK028 - Can SC-007 (200 MB resident over 7 days) be verified without stated assumptions about file count and event rate? [Measurability, Spec §SC-007]
- [x] CHK029 - Do the p95 latency targets (SC-003, SC-004) state a **minimum sample size** required before the percentile is meaningful? [Measurability, Gap, Spec §SC-003 + §SC-004]

## Scenario Coverage

- [x] CHK030 - Are recovery requirements defined for an **OAuth refresh failure caused by a transient outage** (Google 5xx) vs. real revocation (400 invalid_grant)? FR-009 covers revocation only. [Coverage, Spec §FR-009]
- [x] CHK031 - Are requirements defined for **two local edits during a single debounce window** (e.g., editor saves twice in 100 ms)? Spec says debounce coalesces — but is the *last* state guaranteed to be the one propagated? [Coverage, Gap]
- [x] CHK032 - Are requirements defined for **a folder created locally then deleted before its first sync cycle**? [Coverage, Edge Case, Gap]
- [x] CHK033 - Are requirements specified for **clock-skew larger than the conflict-detection window** (e.g., user's clock is 10 minutes off)? Spec says reconciliation MUST rely on content fingerprints, but the conflict-suffix timestamp uses the wall clock — is that acceptable? [Coverage, Spec §Edge Cases "Clock skew" + §FR-006]

## Edge Case Coverage

- [x] CHK034 - Are requirements defined for **filename characters that are valid on Drive but invalid locally** (or vice-versa) beyond the existing case-collision item — e.g., `:`, `\`, control characters, leading/trailing spaces, names ending in `.`? [Coverage, Spec §Edge Cases]
- [x] CHK035 - Is the **`air-drive map` behaviour with a non-existent local path** specified in the spec (auto-create vs. error)? Contracts say "created if missing" but the spec is silent. [Gap, Spec §FR-002 vs §contracts/cli.md]

## Non-Functional Requirements

- [x] CHK036 - Are **observability requirements** (default log level, structured fields, what is logged at info vs. debug) part of the spec or only of research? [Gap, only in research.md §9]
- [x] CHK037 - Are **upgrade and downgrade** requirements specified for the binary (running an older binary against a newer state.db)? [Gap]

## Dependencies & Assumptions

- [x] CHK038 - Is the assumption that the **rclone binary is reachable** (either pre-installed, in the cache, or downloadable from `downloads.rclone.org`) listed in the spec assumptions? Currently only in research. [Assumption, only in research.md §5]
- [x] CHK039 - Is the dependency on **Google's `changes.list` `newStartPageToken` semantics** (specifically that the token is monotonic and that no changes are lost when we persist it after each page) surfaced as an explicit dependency in the spec? [Dependency, Gap]
- [x] CHK040 - Is the assumption that **the user's clock is broadly correct** stated, given the conflict-suffix uses a UTC timestamp? A clock years off would produce confusing names but not data loss — clarify the intent. [Assumption, Spec §FR-006]

## Notes

- Check items off as the spec is amended: `[x]`
- Add inline comments under an item when a Gap/Ambiguity/Conflict is resolved.
- Items are sequential CHK001..CHK040.
- Traceability: 38 / 40 items reference a `[Spec §…]` section or a `[Gap]` / `[Ambiguity]` /
  `[Conflict]` / `[Assumption]` marker — well above the 80 % floor.
- Priority for the next iteration of `spec.md` (in author judgement, not strict): **CHK022,
  CHK023, CHK020, CHK025, CHK026, CHK027, CHK013, CHK012** — these mix data-integrity
  ambiguity with measurability gaps in the success criteria and would most reduce
  implementation risk.
