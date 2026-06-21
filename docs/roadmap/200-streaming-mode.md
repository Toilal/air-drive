# 200 — Streaming mode (on-demand virtual files)

- **Priority:** —
- **Status:** Planned (long-term)
- **Issue:** —
- **Area:** engine, watch, distribution

## Goal

Offer a **streaming** sync mode alongside today's full-mirror mode: files in the
mapped folder appear in the file manager with their real name, size, and tree
position, but their bytes are **not** downloaded until something opens them.
Pinning a file (or folder) forces it local for offline use. This is the on-demand
model the official Google Drive for Desktop client defaults to ("stream files"),
and it is what makes syncing a multi-hundred-GB Drive practical without that much
free disk.

## Today

air-drive only does **full mirror**: every file under the mapped subtree is
downloaded in full to local disk, because the engine is `rclone bisync`, which
reconciles two concrete file trees. A Drive of N bytes needs N bytes locally.
There are no placeholder/virtual files and no per-file "available offline"
toggle. See [sync-model](../dev/sync-model.md) and the mirror-vs-stream note in
[configuration](../user/configuration.md).

## Approach

Streaming needs a virtual filesystem that materialises file content lazily, which
is a different mechanism per platform:

- **Linux (first target):** a **FUSE** mount exposing the mapped tree. Directory
  listings and metadata come from `sync_item` (already persisted); a `read()` on
  a not-yet-local file triggers an on-demand fetch (range requests where the
  backend supports them), with an LRU byte cache under the cache dir. Evaluate
  `fuser` (MIT) for the FUSE bindings, and whether `rclone mount`'s VFS can be
  driven behind the `SyncEngine` boundary as an MVP shortcut before a native
  implementation.
- **macOS / Windows (to decide):** the OS-native placeholder APIs — macOS
  **File Provider**, Windows **Cloud Files / placeholder** API (the same surface
  OneDrive "Files On-Demand" uses). These are not FUSE and will need separate
  per-OS backends behind a shared trait. Scope and sequencing TBD; Linux/FUSE
  proves the model first.

Cross-cutting decisions to settle in the spec before coding:

- **Mode selection & coexistence** with mirror mode (per-mapping `config.toml`
  key? a `pin`/`unpin` CLI verb? what happens to already-mirrored files when a
  mapping switches mode?).
- **Interaction with the watcher and the reconciler**: a streamed (non-local)
  file must not look like a local deletion to be propagated to Drive.
- **Native Google Docs**: streaming does not change the shortcut-file model
  (#3, shipped) — native docs have no bytes to stream either way.
- **Relation to the [native engine](190-native-engine.md)**: on-demand fetch may
  be cleaner to build on `NativeEngine` than to bolt onto the rclone subprocess;
  decide whether 200 depends on 190 or ships a FUSE-over-rclone-mount MVP first.

This is a large, long-term feature and a genuine product differentiator; it is
not a near-term item.

## Acceptance

- On Linux, a mapped folder can be mounted in streaming mode: files are visible
  but occupy ~no disk until opened; opening one fetches its bytes transparently.
- A file (or folder) can be **pinned** to force it local, and **unpinned** to
  release the bytes while keeping the placeholder.
- A non-local placeholder is never mistaken for a local deletion by the
  reconciler (no spurious `delete_remote`).
- Mirror mode remains the default and is unaffected; the streaming backend sits
  behind a trait so macOS/Windows backends can be added without touching the
  daemon core.
