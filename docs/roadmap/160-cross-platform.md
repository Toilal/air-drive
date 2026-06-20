# 160 — Cross-platform builds

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** distribution

## Goal

Run air-drive beyond Linux x86_64, in the priority order set by
[`../../CLAUDE.md`](../../CLAUDE.md) §VI: Linux aarch64, then macOS (aarch64 and
x86_64), then Windows x86_64.

## Today

The MVP targets Linux x86_64 with a systemd user service. The local watcher
abstraction (`notify`) already spans inotify / FSEvents / ReadDirectoryChangesW,
but platform specifics (paths, service management, the `0700`/Unix-mode code in
`src/config/paths.rs`) are Linux-shaped.

## Approach

Port platform-specific pieces (service/autostart integration, path and permission
handling), set up CI build targets per platform, and validate the sync loop on
each. macOS and Windows service/autostart replace the systemd unit.

## Acceptance

- CI produces verified binaries for the target triples in priority order.
- The mocked test suite passes on Linux and macOS at minimum (constitution
  quality gate #3).
