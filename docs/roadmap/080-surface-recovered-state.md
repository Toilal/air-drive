# 080 — Surface recovered state

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** state, cli

## Goal

When the daemon recovers from a transient Drive hiccup, reflect that in
`state_meta` / `air-drive status` so the UI and user can tell "blocked" from
"recovered, healthy again".

## Today

`src/state/meta.rs` notes that the "recovered" transition is not wired yet. The
`state_meta` table already carries the `blocked_*` triple and the `last_sync_*`
counters (see [state schema](../dev/state-schema.md)); what's missing is clearing
the blocked state and recording recovery after a successful Drive call following a
transient failure.

## Approach

In the dispatcher / poller paths, on the first successful Drive call after a
transient error, clear `blocked_kind` / `blocked_message` / `blocked_at` and
update the last-sync fields. Expose the resulting healthy state via `status`.

## Acceptance

- After a transient remote failure and a subsequent success, `status` no longer
  reports `blocked`.
- Covered by a unit/integration test simulating hiccup → recovery.
