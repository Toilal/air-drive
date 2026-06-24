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

Linux/GNOME first (Ubuntu default = Nautilus). A `nautilus-python`
`InfoProvider` extension paints a per-file emblem; it asks the daemon for each
file's status over the **control socket** (a `status-path <abs>` query), so the
extension stays decoupled from the SQLite schema. Emblems map from the
`sync_item` state: `synced` → `emblem-default`, `syncing`/`pending` →
`emblem-synchronizing`, `conflict` → `emblem-important`.

Progress:

- ✅ `air-drive shell install/uninstall/status` — detects platform + file
  manager, installs the `python3-nautilus` bridge via the host package manager
  (or prints the command), deploys the bundled extension under
  `~/.local/share/nautilus-python/extensions/`. The extension is shipped and
  defensive (no emblem until the daemon answers).

- ✅ Daemon: `status-path <abs>` control-socket query deriving the per-file
  token (`synced`/`pending`/`conflict`/`ignored`/`unknown`) from `sync_item`,
  plus an aggregate token for the mapped root (`daemon/file_status.rs`).
- ✅ Emblems confirmed rendering on Nautilus 50 (GTK4), files **and** the root
  folder (viewed from its parent).
- ✅ Live refresh: a `subscribe` control-socket stream emits `changed` on every
  sync activity; the extension invalidates the files it has emblemed so Nautilus
  re-queries them — no manual refresh.

Remaining:

- Bulk `status-dir` query + extension-side caching so a folder of N files is one
  round-trip instead of N (perf for large folders).
- Context-menu actions and native notifications (conflict, blocked transitions).
- Later: native (C) extension for the v1.0 bundle to drop the Python dep;
  Dolphin/Nemo providers.

## Acceptance

- Synced/syncing/conflict overlay icons appear in a supported file manager.
- Notifications fire on conflict and on transition to/from blocked.
