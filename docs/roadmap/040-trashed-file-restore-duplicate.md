# 040 — Trashed-file restore duplicate

- **Priority:** 🟡 medium
- **Status:** Implemented — pending e2e verification against real Drive
- **Issue:** [#8](https://github.com/Toilal/air-drive/issues/8)
- **Area:** reconcile, state

## Goal

Restoring a file from the Drive trash reconciles cleanly back to its original
local path instead of producing a duplicate.

## Today

When a remote file is trashed and later restored, the change feed surfaces it in
a way the reconciler treats as a new item, creating a second local copy alongside
the original.

## Approach (implemented)

Tombstones, per the issue's proposed direction:

- **Schema v4** adds `sync_item.trashed_at` (nullable Unix epoch).
- On a remote trash (`DeleteLocal`, kind=File), the dispatcher removes the local
  file but **keeps the row as a tombstone** (`mark_trashed`) instead of deleting
  it — preserving its `remote_id` and original `relative_path`.
- `apply_remote` recognises a restore: a non-removed event whose `remote_id`
  matches a tombstone → `clear_trashed` + re-download to the original path,
  re-using the row (checked *before* echo suppression).
- `apply_local` ignores a `Deleted` event on a tombstone (the echo of our own
  local removal) so the trash isn't bounced back to Drive as a delete.
- A start-up GC (`gc_tombstones`) reclaims tombstones older than 30 days.

## Acceptance

- [x] Trash then restore on Drive leaves exactly one local copy at the original
  path, with no duplicate `sync_item` row (`continuous_sync.rs`
  `us2_8_trash_then_restore_no_duplicate`).
- [x] Tombstone lifecycle + retention GC unit-tested
  (`items::tests::{mark_then_clear_trashed_roundtrip, gc_tombstones_reclaims_only_old_ones}`).

## Caveat to verify in e2e

The code (pre-existing) treats `changes.list` `removed = true` as the trash
signal. A real Drive *trash* may instead surface as `file.trashed = true` with
`removed = false` (the `changes` query doesn't even request `trashed`). If so,
trash detection itself needs a separate fix — file a follow-up. This entry only
covers the **restore de-duplication** once a trash is detected; e2e against real
Drive should confirm the trash-detection assumption before deletion.
