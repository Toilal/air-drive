# 140 — Desktop UI (Tauri)

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** ui

## Goal

A lightweight desktop UI — tray icon plus a settings/status window — on top of the
daemon, per [`../../CLAUDE.md`](../../CLAUDE.md) §VI.

## Today

There is no UI; the daemon is driven entirely through the CLI and `status`. The
control socket and `status --json` already give a UI a clean integration surface
(see [CLI reference](../user/cli.md) and [architecture](../dev/architecture.md)).

## Approach

Build the UI with **Tauri v2** (Rust backend + system webview). It talks to the
running daemon over the existing control socket and `status` rather than
reimplementing logic. A tray-only fallback built on `tray-icon` + `tao` is
acceptable if Tauri causes issues on a given platform.

## Acceptance

- Tray shows sync state at a glance (synced / syncing / blocked / paused).
- A window exposes account, mapping, and pause/resume.
- No sync logic duplicated in the UI layer.
