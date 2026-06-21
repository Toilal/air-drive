# 020 — Propagate empty directories (folders as persistent items)

- **Priority:** 🟠 high
- **Status:** In progress — engine + continuous sync done; initial reconciliation pending
- **Issue:** [#1](https://github.com/Toilal/air-drive/issues/1)
- **Area:** reconcile, state, engine

## Goal

Empty directories are created and deleted on both sides, not just directories
that happen to contain files. As a side effect, **every** directory becomes a
first-class `sync_item` row (`kind = 'dir'`) with a `remote_id` — the foundation
[030 — folder rename/move](030-folder-rename-move-propagation.md) builds on.

## Today

- Reconciliation is driven by leaf files. A directory `Created` is an explicit
  no-op (`reconcile/continuous.rs:42-46`); a remote folder is a no-op too
  (`:182-186`). An empty folder never propagates.
- Nested uploads *do* create parent folders on Drive, but via
  `ensure_remote_folder(http, …)` in the dispatcher (`daemon/runtime.rs:247-248`)
  — **`DriveHttp` REST, outside the `SyncEngine` trait** — and those folders are
  **not** persisted as `sync_item` rows. So there is no row to anchor a rename.
- The `SyncEngine` trait has no directory method (only upload/update/download/
  move_remote/delete_remote). `ItemKind::{File,Dir}` and
  `Operation::{CreateDirLocal,CreateDirRemote,DeleteLocal,DeleteRemote}` already
  exist; `CreateDir*` currently fall through the dispatcher's `other =>` arm.

## Approach

### Engine trait (decision: go through the engine, per principle IV)

Add to `SyncEngine`:

- `async fn create_dir_remote(&self, parent_id: &str, name: &str) -> Result<RemoteFile>`
- `async fn remove_dir_remote(&self, remote_id: &str) -> Result<()>`

Implement in both engines:

- **RcloneEngine**: `rclone mkdir airdrive:<name> --drive-root-folder-id <parent>`
  (then `lsjson` to capture the new folder id); removal via `rclone rmdir`/`purge`
  (`rclone delete` does not remove a directory).
- **HttpEngine**: `files.create` with `mimeType application/vnd.google-apps.folder`;
  removal via `files.delete` (works for folders by id).

Unify the implicit path: `ensure_remote_folder` should create missing segments via
`engine.create_dir_remote` and **persist each created folder** as a `sync_item`
`kind='dir'`, so folders made during a nested upload are anchored too (required by
030). Avoid double-creation between the implicit path and explicit `CreateDirRemote`
ops (look up `sync_item` first).

### Reconciler

- `apply_local`: a directory `Created` inserts a `kind='dir'` row and enqueues
  `CreateDirRemote`; a directory `Deleted` enqueues `DeleteRemote` (the dir's
  descendants are handled by their own events / the delete cascade).
- `apply_remote`: `file.is_folder()` inserts/removes a `kind='dir'` row and
  enqueues `CreateDirLocal` / `DeleteLocal`.

### Dispatcher (`daemon/runtime.rs`)

- `CreateDirRemote`: call `engine.create_dir_remote`, then `set_remote_id` on the
  item.
- `CreateDirLocal`: `tokio::fs::create_dir_all` (local ops already run in the
  dispatcher, not the engine).
- `DeleteLocal` / `DeleteRemote`: branch on `item.kind` — `remove_dir_all` vs
  `remove_file` locally, `remove_dir_remote` vs `delete_remote` on Drive.

### Ordering

A parent dir must be created before its children, and children deleted before the
parent. The dispatcher runs ops oldest-first; enqueue order from the reconciler
plus the existing retry/backoff should converge, but verify nested-create and
nested-delete ordering explicitly (a child op may need to retry until its parent
dir exists).

### Initial reconciliation (`reconcile/mod.rs`)

`walk_local` / `walk_remote` already classify directories. Extend the initial pass
to create missing empty directories on both sides and persist their rows, so a
first sync of a tree with empty folders converges.

## Acceptance

- [x] An empty directory created on either side appears on the other (continuous
  sync); deleting it locally trashes the Drive folder.
- [x] Every directory (explicit or created during a nested upload) is persisted as
  a `sync_item` `kind='dir'` with a `remote_id` (via `ensure_remote_folder` +
  `persist_dir`).
- [x] `SyncEngine` gains `create_dir_remote` + `remove_dir_remote`, implemented for
  both `RcloneEngine` and `HttpEngine`.
- [x] Integration tests: empty-dir create both directions + local dir delete
  (`continuous_sync.rs` `us2_6_*`).
- [ ] **Initial reconciliation** (`reconcile/mod.rs`): a first sync of a tree
  containing empty folders creates them on both sides. ← step 3, remaining.
- [ ] Remote-side dir delete → local `remove_dir_all` (code path exists via
  `apply_remote` `removed` → `DeleteLocal`; add an integration test in step 3).

## Unblocks

[030 — folder rename/move](030-folder-rename-move-propagation.md): a rename targets
a folder that now exists as a `sync_item` row.
