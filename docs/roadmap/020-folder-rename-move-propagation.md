# 020 — Folder rename/move propagation

- **Priority:** 🟠 high
- **Status:** Planned
- **Issue:** [#7](https://github.com/Toilal/air-drive/issues/7)
- **Area:** reconcile, state

## Goal

Renaming or moving a folder on one side propagates as a rename/move on the other,
instead of being reconstructed as a delete + re-create (which loses remote IDs,
revision history, and wastes transfer).

## Today

The reconciler handles file-level operations and the op vocabulary already
includes `rename_local` / `rename_remote` (see
[sync model](../dev/sync-model.md) and [state schema](../dev/state-schema.md)).
Directory renames/moves are not detected or propagated as moves.

## Approach

Detect directory identity across a rename (the `sync_item.local_inode` column and
the remote folder ID are the anchors) and emit `rename_remote` / `rename_local`
for the folder rather than tearing down and rebuilding the subtree.

## Acceptance

- Renaming/moving a folder locally renames/moves it on Drive (same Drive IDs
  preserved), and vice versa.
- Covered by an engine integration test.
