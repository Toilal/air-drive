# Roadmap

What's planned for air-drive after the MVP. This is a **living document and a
direction, not a commitment** — priorities shift, and the authoritative,
up-to-date list of work is the
[GitHub issue tracker](https://github.com/Toilal/air-drive/issues). Items below
link to their issue where one exists.

The MVP that has shipped: bidirectional event-driven sync, a single Drive
account, one mapped folder pair, the `rclone` engine behind the `SyncEngine`
trait, and a systemd-managed daemon. See [`README.md`](../../README.md) for the
current status.

## Known bugs to fix first

Correctness gaps in the current sync path. These come before new features.

| Priority   | Item                                                                   | Issue |
| ---------- | ---------------------------------------------------------------------- | ----- |
| 🔴 critical | rclone never refreshes its access token — long operations fail with 401 | [#5](https://github.com/Toilal/air-drive/issues/5) |
| 🟠 high     | Folder renames and moves are not propagated to Drive                    | [#7](https://github.com/Toilal/air-drive/issues/7) |
| 🟡 medium   | Restoring a trashed remote file creates a duplicate locally            | [#8](https://github.com/Toilal/air-drive/issues/8) |

## Near-term — close the sync feature gaps

Make single-account, single-mapping sync genuinely complete and trustworthy.

- **Propagate empty directories** in both directions
  ([#1](https://github.com/Toilal/air-drive/issues/1), high) — currently only
  leaves carrying files are reconciled.
- **Handle native Google Docs** (export, `.gdoc` shortcut, or an explicit
  skip-with-UX) ([#3](https://github.com/Toilal/air-drive/issues/3), medium) —
  today `application/vnd.google-apps.*` items are skipped silently
  (see [sync model](../dev/sync-model.md#native-google-docs)).
- **Symlinks**: follow or preserve them instead of dropping silently
  ([#2](https://github.com/Toilal/air-drive/issues/2), low).
- **rclone auto-download**: the post-install download with SHA-256 verification
  to `~/.cache/air-drive/bin/rclone` described in
  [`CLAUDE.md`](../../CLAUDE.md) §VI is not wired yet — the daemon currently
  requires a pre-existing `rclone`.
- **`recovered` state**: surface recovery from a transient Drive hiccup in
  `state_meta` / `status` (placeholder exists, not wired).

## Medium-term — multi-mapping, multi-account, broader Drive

Lift the single-pair restriction and cover more of Drive's surface. The SQLite
schema is already multi-account/multi-mapping by design (rows key off
`account_id` / `mapping_id`); the work is in the CLI and daemon, not the data
model (see [state schema](../dev/state-schema.md)).

- **Multi-mapping support**: lift the singleton-mapping restriction
  ([#13](https://github.com/Toilal/air-drive/issues/13), medium). Unblocks
  persisting `--account-label` (today accepted but dropped).
- **Multi-account**: N linked Drive accounts, each with its own mappings.
- **Shared Drives, shared-with-me, and shortcuts**
  ([#6](https://github.com/Toilal/air-drive/issues/6), medium).
- **Drive-only metadata**: decide and document handling of permissions,
  revisions, and comments
  ([#9](https://github.com/Toilal/air-drive/issues/9), low).
- **Interactive `setup`**: the guided `link → map → start` wizard is stubbed in
  the MVP; users currently drive each subcommand individually.

## Long-term — UI, packaging, and the native engine

The product vision from [`CLAUDE.md`](../../CLAUDE.md).

- **Desktop UI** (Tauri v2): a lightweight tray + settings UI. A `tray-icon` +
  `tao` tray-only fallback is acceptable if Tauri causes platform issues
  (§VI).
- **Desktop shell integration**: overlay icons, context menus, native
  notifications ([#10](https://github.com/Toilal/air-drive/issues/10), low).
- **Cross-platform** (§VI): Linux aarch64, then macOS (aarch64 + x86_64), then
  Windows x86_64 — in priority order.
- **v1.0 self-contained bundles** (§VI): Linux AppImage, macOS `.app`, Windows
  installer, each embedding `rclone` with its MIT notice and a
  `THIRD_PARTY_LICENSES` file.
- **OAuth Production verification** (§V): move the OAuth client from `Testing`
  (7-day refresh-token cap) to `Production` via Google's review, so users stop
  re-consenting weekly. Requires the auth flow's MVP placeholder to be replaced
  before the first public release.
- **Native Rust engine** (`NativeEngine`, §IV): a from-scratch sync engine
  substitutable for `RcloneEngine` behind the `SyncEngine` trait, removing the
  `rclone` subprocess dependency. The long-term goal, not a near-term one.

## How this list is maintained

Per [`CLAUDE.md`](../../CLAUDE.md), docs are updated in the same commit as the
change they describe. When an item here ships, remove it (the code docs and
[`docs/README.md`](../README.md) index should already reflect the new behaviour);
when a new direction is decided, add it with a link to its tracking issue.
