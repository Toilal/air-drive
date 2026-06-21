# 030 — Folder rename/move propagation

- **Priority:** 🟠 high
- **Status:** Implemented — pending e2e verification against real Drive
- **Issue:** [#7](https://github.com/Toilal/air-drive/issues/7)
- **Area:** reconcile, state

## Goal

Renaming or moving a folder on either side propagates as a rename/move on the
other (**bidirectional**), instead of being reconstructed as a delete + re-create
— which loses remote IDs, revision history, and wastes transfer.

## Depends on [020 — folders as persistent items](020-propagate-empty-directories.md)

A rename can only be expressed against a `sync_item` row for the folder. Today
folders aren't persisted, so the lookup fails and the rename is lost (see below).
**020 must land first** — issue #7 says so explicitly ("Depends on #1. Once
folders are persistent items").

## Today

`apply_local` already handles `WatchEvent::Renamed { from, to }`
(`reconcile/continuous.rs:114-141`) for **files**: it looks up the source path in
`sync_item` and enqueues `RenameRemote` with a `new_relative_path` payload. The
watcher supplies both paths via inotify `RenameMode::Both`, so identity across the
rename is already known — **`local_inode` is not needed** for this case (it would
only matter if a rename arrived as separate Deleted+Created events).

For a **directory**, the `get_by_relative_path(from)` lookup fails (folders aren't
in `sync_item`), so the event falls through to the "treat as fresh create" branch
— and `Created` on a directory is a no-op (`:42-46`). The rename is silently lost.
On the remote side, `apply_remote` treats `file.is_folder()` as a no-op
(`:182-186`), so a Drive-side folder rename isn't propagated locally either.

## Approach

Once folders are `sync_item` rows (020):

1. **Local → Drive.** A directory `Renamed { from, to }` resolves the folder's
   `sync_item` and enqueues a move against it (engine `move_remote`, already
   implemented via `rclone moveto`). Crucially, inotify emits **no events for
   descendants** of a renamed folder — so rewrite every descendant's
   `relative_path` (those under the `from/` prefix) to the `to/` prefix in a
   **single SQL transaction**, keeping each descendant's `remote_id` (no
   re-upload).
2. **Drive → Local.** A folder rename/move observed via `changes.list` resolves
   the folder by `remote_id`, renames/moves the local directory, and rewrites the
   descendant `relative_path` rows the same way.
3. Decide whether a pure **move** (new parent) and a **rename** (new name) are one
   operation or two; `move_remote` already takes both a new parent id and a new
   name, so a single op can cover both.

## Acceptance

- [x] `mv docs documents` locally renames the folder on Drive — no re-upload
  (`continuous_sync.rs` `us2_7_local_dir_rename_propagates_without_reupload`).
- [x] Renaming/moving a folder on Drive renames/moves it locally
  (`us2_7_remote_dir_rename_propagates_locally`).
- [x] Descendants keep their `remote_id` (no re-upload) and their `relative_path`
  rows are rewritten atomically (`items::rename_subtree`, in a transaction;
  unit-tested incl. a lookalike-prefix `docs2` guard).
- [x] Coverage of a non-trivial multi-level subtree
  (`items::tests::rename_subtree_rewrites_dir_and_all_descendants`).

## Implementation notes

`items::rename_subtree` rewrites the directory row + every descendant
(`old/...` → `new/...`) in one transaction, keeping each `remote_id`. The
dispatcher routes folder ops to it: `RenameRemote` (kind=Dir) after
`engine.move_remote`, and `RenameLocal` after `fs::rename`. `apply_remote`
detects a folder rename/move when a known `remote_id`'s path changed. Echo of a
local `fs::rename` converges harmlessly (a redundant no-op move at worst).

Remaining before deletion of this entry: e2e verification against real Drive
(`RcloneEngine::move_remote` for a folder is exercised only there).
