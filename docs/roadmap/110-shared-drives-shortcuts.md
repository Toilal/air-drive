# 110 — Shared Drives & shortcuts

- **Priority:** 🟡 medium
- **Status:** Planned
- **Issue:** [#6](https://github.com/Toilal/air-drive/issues/6)
- **Area:** sync

## Goal

Sync content that lives outside "My Drive": Shared Drives, shared-with-me items,
and Drive shortcuts.

## Today

Mapping targets a folder in the user's own Drive. Shared Drives, the
shared-with-me collection, and shortcut entries are not handled as mapping
sources or as items encountered during reconciliation.

## Approach

Teach the Drive client and reconciler about Shared Drive contexts (the
`driveId` / `supportsAllDrives` parameters), resolve shortcuts to their targets,
and decide how shared-with-me items map locally. `rclone` already understands
these constructs, which helps the current engine.

## Acceptance

- A Shared Drive folder can be used as a mapping target and syncs both ways.
- Shortcuts resolve to their targets rather than syncing as opaque stubs.
