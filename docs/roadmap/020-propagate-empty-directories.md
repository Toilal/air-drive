# 020 — Propagate empty directories (folders as persistent items)

- **Priority:** 🟠 high
- **Status:** Planned
- **Issue:** [#1](https://github.com/Toilal/air-drive/issues/1)
- **Area:** reconcile, state

## Goal

Empty directories are created and deleted on both sides, not just directories
that happen to contain files. As a side effect, directories become **first-class
`sync_item` rows** (`kind = 'dir'`) — the foundation that
[030 — folder rename/move](030-folder-rename-move-propagation.md) builds on.

## Today

Reconciliation is driven by leaf files. A directory create is an explicit no-op
(`reconcile/continuous.rs:42-46`, `:182-186`): folders "materialise implicitly
when their first file syncs". So an empty folder created on either side never
appears on the other, and — because folders aren't in `sync_item` — there is no
row to anchor a future rename/move against.

## Approach

Treat directories as first-class `sync_item` rows (`kind = 'dir'`) throughout the
reconciler:

- `apply_local` on a directory `Created` / `Deleted` inserts/removes a
  `kind='dir'` row and enqueues `create_dir_remote` / `delete_remote`.
- `apply_remote` on `file.is_folder()` inserts/removes the row and enqueues
  `create_dir_local` / `delete_local`.
- The op vocabulary already has `create_dir_local` / `create_dir_remote` (see
  [state schema](../dev/state-schema.md) and [sync model](../dev/sync-model.md));
  wire them through the dispatcher to the engine.

## Acceptance

- An empty directory created on either side appears on the other; deleting it
  propagates too.
- Directories are persisted as `sync_item` rows with `kind='dir'` and a
  `remote_id`.
- Covered by an integration test.

## Unblocks

[030 — folder rename/move](030-folder-rename-move-propagation.md): a rename can
only target a folder that exists as a `sync_item` row, which this delivers.
