# 150 — Desktop shell integration

- **Priority:** ⚪ low
- **Status:** Planned
- **Issue:** [#10](https://github.com/Toilal/air-drive/issues/10)
- **Area:** desktop-integration

## Goal

Integrate sync state into the desktop shell: per-file overlay icons, file-manager
context-menu actions, and native notifications.

## Today

No shell integration exists. Builds on [140 — desktop UI](140-desktop-ui.md) and
is inherently platform-specific.

## Approach

Per-platform work: overlay-icon providers (e.g. Nautilus/Dolphin extensions on
Linux), context-menu entries for common actions, and native notifications for
events like conflicts or blocked state. Driven by the daemon's state surface.

## Acceptance

- Synced/syncing/conflict overlay icons appear in a supported file manager.
- Notifications fire on conflict and on transition to/from blocked.
