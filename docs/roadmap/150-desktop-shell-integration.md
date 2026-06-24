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

- ✅ Bulk `status-dir` query + extension-side per-directory cache: a folder of N
  files is one round-trip, not N.
- ✅ Context-menu actions (`Nautilus.MenuProvider`): Open in Google Drive / Copy
  Drive link / Open Google Doc, and Pause/Resume in a folder background.
  Localised en/fr/es. Backed by `drive-url` + `pause-state` control commands.
- ✅ Double-click a native-Doc shortcut opens it in the browser, via a MIME type
  + desktop handler (`air-drive open-shortcut`) registered by `shell install`.

Remaining:

- Native notifications on conflict and on transition to/from blocked. The daemon
  already has the signals (`conflict_record`, `state_meta.blocked_kind`); needs a
  notifier (libnotify / `notify-send` / the `notify-rust` crate) wired to those
  transitions.

Deferred to **[170 — v1.0 bundles](170-v1-bundles.md)** (packaging):

- A **native extension** (drop the `python3-nautilus` runtime dep). Decided C
  over Rust for the glue — `libnautilus-extension` has no Rust bindings, so a
  Rust `cdylib` means hand-written GObject-interface FFI for throwaway glue;
  C is the documented ~200-line path (logic still delegated to the `air-drive`
  binary over the socket). **Belongs in packaging**: native `.so`s load only
  from the *system* extensions dir (`/usr/lib/<arch>/nautilus/extensions-4/`,
  root-only) — there is no user-local native-extension dir, so it cannot ship
  via the current `~/.local` `shell install`; it must be installed system-wide
  by the `.deb`/AppImage.

Later:

- Dolphin (KDE) / Nemo providers.

## Acceptance

- Synced/syncing/conflict overlay icons appear in a supported file manager.
- Notifications fire on conflict and on transition to/from blocked.
