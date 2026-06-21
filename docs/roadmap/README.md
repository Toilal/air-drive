# Roadmap

What's planned for air-drive after the MVP. This is a **living document and a
direction, not a commitment** — the authoritative, up-to-date list is the
[GitHub issue tracker](https://github.com/Toilal/air-drive/issues).

Each feature has its **own file**, prefixed with a three-digit number giving the
intended order (lower = sooner). Numbers leave gaps of 10 (`010`, `020`, …) so a
new item can slot in without renumbering the rest. When an item ships, remove its
file and make sure the behaviour is reflected in the relevant code docs
([`../user/`](../user/), [`../dev/`](../dev/)).

The MVP that has shipped: bidirectional event-driven sync, a single Drive
account, one mapped folder pair, the `rclone` engine behind the `SyncEngine`
trait, and a systemd-managed daemon. See [`../../README.md`](../../README.md) for
the current status.

## Order

### Correctness — fix first

| #   | Feature                                                              | Priority    | Issue |
| --- | ------------------------------------------------------------------- | ----------- | ----- |
| 010 | [rclone access-token refresh](010-rclone-token-refresh.md)          | 🔴 critical | [#5](https://github.com/Toilal/air-drive/issues/5) |
| 020 | [Propagate empty directories (folders as items)](020-propagate-empty-directories.md) | 🟠 high | [#1](https://github.com/Toilal/air-drive/issues/1) |
| 030 | [Folder rename/move propagation](030-folder-rename-move-propagation.md) | 🟠 high  | [#7](https://github.com/Toilal/air-drive/issues/7) |
| 040 | [Trashed-file restore duplicate](040-trashed-file-restore-duplicate.md) | 🟡 medium | [#8](https://github.com/Toilal/air-drive/issues/8) |

### Near-term — complete single-account sync

| #   | Feature                                                       | Priority  | Issue |
| --- | ------------------------------------------------------------ | --------- | ----- |
| 050 | [rclone auto-download](050-rclone-auto-download.md)          | —         | —     |
| 060 | [Native Google Docs handling](060-native-google-docs.md)    | 🟡 medium | [#3](https://github.com/Toilal/air-drive/issues/3) |
| 070 | [Interactive setup wizard](070-interactive-setup.md)        | —         | —     |
| 080 | [Surface recovered state](080-surface-recovered-state.md)   | —         | —     |
| 090 | [Symlink handling](090-symlinks.md)                         | ⚪ low     | [#2](https://github.com/Toilal/air-drive/issues/2) |

### Medium-term — multi-mapping, multi-account, broader Drive

| #   | Feature                                                       | Priority  | Issue |
| --- | ------------------------------------------------------------ | --------- | ----- |
| 100 | [Multi-mapping support](100-multi-mapping.md)               | 🟡 medium | [#13](https://github.com/Toilal/air-drive/issues/13) |
| 110 | [Shared Drives & shortcuts](110-shared-drives-shortcuts.md) | 🟡 medium | [#6](https://github.com/Toilal/air-drive/issues/6) |
| 120 | [Multi-account](120-multi-account.md)                       | —         | —     |
| 130 | [Drive-only metadata](130-drive-only-metadata.md)          | ⚪ low     | [#9](https://github.com/Toilal/air-drive/issues/9) |

### Long-term — UI, packaging, native engine

| #   | Feature                                                              | Priority | Issue |
| --- | ------------------------------------------------------------------- | -------- | ----- |
| 140 | [Desktop UI (Tauri)](140-desktop-ui.md)                            | —        | —     |
| 150 | [Desktop shell integration](150-desktop-shell-integration.md)      | ⚪ low    | [#10](https://github.com/Toilal/air-drive/issues/10) |
| 160 | [Cross-platform builds](160-cross-platform.md)                     | —        | —     |
| 170 | [v1.0 self-contained bundles](170-v1-bundles.md)                   | —        | —     |
| 180 | [OAuth Production verification](180-oauth-production.md)           | —        | —     |
| 190 | [Native Rust engine](190-native-engine.md)                        | —        | —     |

## Maintaining this folder

Per [`../../CLAUDE.md`](../../CLAUDE.md), docs are updated in the same commit as
the change they describe. Add a new feature as its own `NNN-slug.md` file and a
row in the table above; when a feature ships, delete its file and drop the row.
Keep the numbering gapped so reordering rarely needs a rename.
