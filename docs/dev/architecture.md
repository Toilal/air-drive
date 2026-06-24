# Architecture

air-drive is a small Rust daemon that keeps a local folder and a Google Drive
folder in sync, reacting to events on **both** sides. This document maps the
codebase and explains how the pieces fit together. For the *why* behind the
design, see [`../../CLAUDE.md`](../../CLAUDE.md).

## High-level picture

```
            ┌──────────────────────── air-drive daemon ────────────────────────┐
            │                                                                   │
  local FS  │   watch (notify)  ──debounce──▶                                   │
  ─────────▶│                                  reconcile::continuous            │
            │                                  (apply_local / apply_remote)     │
  Drive API │   drive::changes  ──poll──────▶        │                          │
  ─────────▶│   (changes.list + pageToken)           ▼                          │
            │                                  pending_operation (SQLite)       │
            │                                        │                          │
            │                                        ▼                          │
            │                            daemon::runtime (dispatcher)           │
            │                                        │                          │
            │                                        ▼                          │
            │                            engine: SyncEngine (rclone)  ─────────▶│──▶ Drive
            └───────────────────────────────────────────────────────────────────┘
```

Everything is coordinated behind a single
`tokio_util::sync::CancellationToken`. `SIGTERM` / `SIGINT` flip the token, the
loops drain whatever is in flight, and `daemon::run` returns cleanly.

## Module layout (`src/`)

| Module        | Responsibility                                                                                  |
| ------------- | ----------------------------------------------------------------------------------------------- |
| `cli/`        | Argument parsing (`clap`) and one handler per subcommand. Resolves paths + config, then dispatches. |
| `config/`     | `config.toml` schema (`mod.rs`), XDG path resolution (`paths.rs`), comment-preserving auto-migration (`migrate.rs`). |
| `daemon/`     | Orchestration: the event loop (`mod.rs`), the op dispatcher (`runtime.rs`), single-instance lock (`lock.rs`), the control socket (`pause.rs`: pause/resume + `status-path` per-file status + `subscribe` live-activity stream), per-file status derivation for the shell overlay (`file_status.rs`), in-flight op tracking (`in_flight.rs`). |
| `drive/`      | Google Drive REST client: OAuth (`auth.rs`), HTTP plumbing (`http.rs`), `changes.list` polling (`changes.rs`), metadata helpers (`metadata.rs`). |
| `engine/`     | The `SyncEngine` trait (`mod.rs`) and its implementations: `RcloneEngine` (`rclone.rs`), an HTTP-based engine (`http.rs`), rclone binary resolution (`rclone_path.rs`), verified auto-download (`rclone_download.rs`), staging dirs (`staging.rs`). |
| `reconcile/`  | Turns events into operations: the one-shot initial pass (`mod.rs`), continuous `apply_local`/`apply_remote` (`continuous.rs`), conflict handling (`conflict.rs`), content fingerprinting (`fingerprint.rs`), native-doc shortcut files (`shortcut.rs`). |
| `state/`      | SQLite persistence: schema + migrations (`schema.rs`), and typed accessors per table (`accounts.rs`, `mapping.rs`, `items.rs`, `ops.rs`, `cursor.rs`, `conflicts.rs`, `meta.rs`). |
| `watch/`      | Local filesystem watcher (`notify`) and debouncer (`debounce.rs`).                              |
| `observability.rs` | Logging / tracing setup.                                                                    |
| `error.rs`    | The crate-wide `Error` / `Result` types (`thiserror`).                                           |

## The four concurrent loops

`daemon::run` (`src/daemon/mod.rs`) wires up four cooperating tasks:

1. **Watcher** (`watch`) — receives `notify` events, debounces them, and emits
   `WatchEvent`s.
2. **Change poller** (`drive::changes`) — polls Drive `changes.list` every
   `remote_poll_interval` (clamped `[10, 60] s`) and emits `RemoteChange`s.
3. **Reconciler** (`reconcile::continuous`) — consumes both event streams,
   consults `sync_item` to suppress echoes, and writes rows into
   `pending_operation`. It is stateless beyond the database; it never talks to
   the engine.
4. **Dispatcher** (`daemon::runtime`) — pulls due rows from `pending_operation`
   and executes them via the `SyncEngine`. On success the row is deleted; on
   failure it applies exponential back-off (1 s → 60 s, ±20 % jitter, max 10
   attempts) and reschedules. After the attempt cap the op is left in place with
   its `last_error` set and surfaces as `status: blocked`.

The reconciler signals the dispatcher on a wake channel whenever it enqueues an
op, so the dispatcher reacts immediately instead of waiting out its poll
interval.

A periodic **safety-net** reconciliation (≥ 5 min, `safety_net_interval_seconds`)
guards against missed events. It is a guard, never the primary path — the
event-driven loops above are.

## Pluggable engine

<a id="sync-engine"></a>
Application code never touches `rclone`'s CLI directly; it depends on the
`SyncEngine` trait in `engine/mod.rs`. `RcloneEngine` drives the `rclone` binary
via `tokio::process::Command`. A native Rust engine (`NativeEngine`) is the
long-term goal and must be substitutable without touching the rest of the
daemon. See [`../../CLAUDE.md`](../../CLAUDE.md) §IV.

The trait's steady-state surface is **per-file** (`upload`, `download`,
`move_remote`, `delete_remote`, …) — that granularity is what the event-driven
loop is built on, and the continuous loop uses nothing else. The one exception is
**bootstrap**: `bulk_download` / `bulk_upload` move a pre-computed *set* of files
in a single batched transfer, used only by the [initial
reconciliation](sync-model.md#initial-reconciliation) where O(files) process
spawns would dominate. `RcloneEngine` realises them as one `rclone copy
--files-from` per direction (with `--transfers`/`--checkers` parallelism and
`--stats` progress streamed line-by-line to the `rclone` tracing target, visible
at `info`); `HttpEngine` as a per-file loop. The reconciler still owns all policy
— the engine only moves the exact paths it is handed.

## State

All sync state — Drive `pageToken`, mapping, sync items, pending operations,
conflicts, accounts — is persisted to SQLite, never held in memory only. See the
[state schema](state-schema.md).

## Related reading

- [Sync model](sync-model.md) — how events become convergent operations.
- [State schema](state-schema.md) — the tables behind it all.
