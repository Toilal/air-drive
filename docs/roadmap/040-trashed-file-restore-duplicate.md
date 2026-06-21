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

## Trash semantics (confirmed against the Drive API)

`changes.list` distinguishes two things the code previously conflated:

- **Trash** (`files.update trashed=true`): a normal change, `removed=false`, the
  **file still present with `file.trashed=true`** (reversible 30 days).
- **Permanent delete / loss of access** (`files.delete`): `removed=true`, no file.

The pre-existing code read only `removed`, so real trashes weren't detected at
all. This fix requests `trashed` in the `changes` query, exposes it on
`FileSnapshot`, and branches on it.

## Approach (implemented)

Tombstones, per the issue's proposed direction:

- **Schema v4** adds `sync_item.trashed_at` (nullable Unix epoch).
- **Trash** (`file.trashed=true`, live row): `apply_remote` enqueues a
  `DeleteLocal{tombstone:true}`; the dispatcher removes the local file but
  **keeps the row as a tombstone** (`mark_trashed`), preserving `remote_id` and
  the original `relative_path`. Directories aren't tombstoned (they re-create).
- **Restore** (untrash → `file.trashed=false` on a tombstone): `clear_trashed` +
  re-download to the original path, re-using the row (checked *before* echo
  suppression and *before* the folder/normal-file branches).
- **Permanent delete** (`removed=true`): `DeleteLocal{tombstone:false}` → drop the
  row, since there's nothing to restore.
- `apply_local` ignores a `Deleted` event on a tombstone (the echo of our own
  local removal) so the trash isn't bounced back to Drive as a delete.
- A start-up GC (`gc_tombstones`) reclaims tombstones older than 30 days.

## Acceptance

- [x] Trash (real `trashed=true`) removes the local copy; restore (`trashed=false`)
  brings it back at the original path with no duplicate row (`continuous_sync.rs`
  `us2_8_trash_then_restore_no_duplicate`).
- [x] A permanent delete (`removed=true`) drops the row with no tombstone
  (`us2_9_permanent_delete_drops_row_without_tombstone`).
- [x] Tombstone lifecycle + retention GC unit-tested
  (`items::tests::{mark_then_clear_trashed_roundtrip, gc_tombstones_reclaims_only_old_ones}`).

Remaining before deletion of this entry: e2e verification against real Drive
(confirm a web-UI trash surfaces as `trashed=true` and an untrash as the restore
path — the mock now models this, but only real Drive proves the field wiring).
