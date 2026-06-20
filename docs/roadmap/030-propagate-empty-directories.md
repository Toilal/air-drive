# 030 — Propagate empty directories

- **Priority:** 🟠 high
- **Status:** Planned
- **Issue:** [#1](https://github.com/Toilal/air-drive/issues/1)
- **Area:** reconcile, sync

## Goal

Empty directories are created and deleted on both sides, not just directories
that happen to contain files.

## Today

Reconciliation is driven by leaf files; a directory with no files is effectively
invisible to the sync path. Creating an empty folder locally doesn't create it on
Drive, and vice versa.

## Approach

Treat directories as first-class `sync_item` rows (`kind = 'dir'`) throughout the
reconciler, emitting `create_dir_local` / `create_dir_remote` and the matching
deletes regardless of whether the directory has children. See the op vocabulary
in [sync model](../dev/sync-model.md).

## Acceptance

- An empty directory created on either side appears on the other.
- Deleting it propagates too.
- Covered by an integration test.
