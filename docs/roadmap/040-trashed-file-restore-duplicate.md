# 040 — Trashed-file restore duplicate

- **Priority:** 🟡 medium
- **Status:** Planned
- **Issue:** [#8](https://github.com/Toilal/air-drive/issues/8)
- **Area:** reconcile, state

## Goal

Restoring a file from the Drive trash reconciles cleanly back to its original
local path instead of producing a duplicate.

## Today

When a remote file is trashed and later restored, the change feed surfaces it in
a way the reconciler treats as a new item, creating a second local copy alongside
the original.

## Approach

Recognise a restore via the file's persistent Drive ID in `sync_item.remote_id`
(see [state schema](../dev/state-schema.md)) and re-link it to the existing
`sync_item` row rather than enqueuing a fresh download to a new path. Handle the
trash → restore transition explicitly in `reconcile::continuous`.

## Acceptance

- Trash then restore on Drive leaves exactly one local copy at the original path.
- Covered by an integration test exercising the trash/restore transition.
