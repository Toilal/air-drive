# 090 — Symlink handling

- **Priority:** ⚪ low
- **Status:** Planned
- **Issue:** [#2](https://github.com/Toilal/air-drive/issues/2)
- **Area:** watch

## Goal

Symlinks under the watched root get a defined behaviour — followed or preserved —
instead of being dropped silently.

## Today

The watcher silently ignores symlinks, so a symlinked file or directory is
neither synced nor reported.

## Approach (to decide)

Choose a policy (and likely make it configurable under `[watch]`):

- **Follow** — sync the link target's contents.
- **Preserve** — record the link itself where the destination format allows.

Guard against symlink loops and links pointing outside the mapped root.

## Acceptance

- Symlinks have a documented, predictable outcome.
- No infinite loop on a self-referential or cyclic link.
