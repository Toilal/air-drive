# Sync model

air-drive's reason to exist is **event-driven synchronization on both sides** —
not periodic polling. This document explains how events on each side become
convergent operations, and how echoes and conflicts are handled. Read
[architecture](architecture.md) first for the module map.

## Two event sources

### Local side — inotify + debounce

The `watch` module wraps `notify` (inotify on Linux). Raw filesystem events are
noisy — a single save can produce several events — so they pass through a
**debouncer** (`watch/debounce.rs`) before becoming `WatchEvent`s.

Files whose **name** matches any `[watch].ignore_patterns` glob are dropped at
this stage: no upload, no rename propagation, no delete propagation. See
[configuration](../user/configuration.md#default-ignore-patterns).

### Remote side — changes.list + pageToken

The `drive::changes` module polls the Drive
[`changes.list`](https://developers.google.com/drive/api/reference/rest/v3/changes/list)
endpoint using a persisted `pageToken`, every `remote_poll_interval` seconds
(clamped to `[10, 60]`). Each delta becomes a `RemoteChange`.

Short polling is used because Drive offers no push mechanism without a public
HTTPS endpoint. The interval budget keeps the daemon well under Drive's quota
(1000 req / 100 s / user). This is still "event-driven" in spirit: the daemon
reacts to *changes*, not to a full re-scan.

## From events to operations

The reconciler (`reconcile::continuous`) has two entry points consumed by the
daemon loop:

- `apply_local(WatchEvent)` — local change → `pending_operation` rows.
- `apply_remote(RemoteChange)` — remote change → `pending_operation` rows.

Both are **stateless beyond the database**: they read/write SQLite and never
talk to the engine. The dispatcher (`daemon::runtime`) is what actually executes
operations. This separation means a crash between "enqueue" and "execute" loses
nothing — the op is durably queued.

The operation vocabulary (the `op` column of `pending_operation`):

```
upload            download
delete_local      delete_remote
rename_local      rename_remote
create_dir_local  create_dir_remote
mark_conflict
```

## Echo suppression

When air-drive uploads a file, Drive's `changes.list` will later report that
very change — and a download we perform triggers a local inotify event. Left
unchecked, these "echoes" would ping-pong forever.

The reconciler consults `sync_item` (the per-path record of last-synced size /
md5 / inode) to recognise a change it caused itself and drop it. The `in_flight`
tracker (`daemon/in_flight.rs`) covers the window where an op is mid-execution.

## Native Google Docs

Native Google formats (`application/vnd.google-apps.*` — Docs, Sheets, Slides,
Drawings, …) have no md5 and no byte stream Drive will hand us, so they cannot be
synced as opaque files. Instead of leaving them invisible, the daemon writes a
**local shortcut file**: a small JSON pointer carrying the doc's web URL and Drive
id (mirroring the Google Drive desktop client's `.gdoc`/`.gsheet`/`.gslides`
files). The mime → extension/URL mapping lives in `reconcile/shortcut.rs`.

- **Naming**: the pointer sits at the doc's path plus a per-type extension, e.g. a
  Google Doc named `Notes` becomes `Notes.gdoc`; a Sheet `Budget` → `Budget.gsheet`.
  Unknown native types fall back to `.glink`.
- **One-directional**: shortcuts are written, renamed, and removed to track the
  remote doc, but **never uploaded back**. Their `sync_item` rows carry
  `state = skipped`, which both surfaces them in `air-drive status` (the `skipped`
  block) and tells `apply_local` to ignore the on-disk pointer instead of treating
  it as a regular file to upload.
- **Flow**: `apply_remote` (continuous) and the initial reconciliation pass detect
  a native doc, persist the `skipped` row, and write the pointer — the continuous
  path via a queued `write_shortcut` operation (the dispatcher renders the payload
  to disk), the initial pass synchronously. A rename on Drive enqueues a
  `rename_local` to move the pointer; a trash/delete flows through the normal
  `delete_local` path (matched by `remote_id`), removing the pointer. A content
  edit of the doc is a no-op — the pointer URL is stable.

## Initial reconciliation

The one-shot initial pass lives in `reconcile/mod.rs` and runs automatically on
the first `start` of a mapping, i.e. when the Drive change cursor is empty (on an
interactive terminal the daemon confirms first; see
[CLI reference](../user/cli.md#start)). It:

1. Walks the local tree and the remote tree once.
2. Reconciles directories on both sides (so empty folders propagate and every
   remote parent folder exists, with its id cached, before any file moves).
3. Writes a local shortcut + `skipped` row for each native Google Doc.
4. Classifies every leaf as `local-only`, `remote-only`, or `both`, dropping any
   whose **name** matches a `[watch].ignore_patterns` glob — the same filter the
   continuous watcher applies, so a pattern means the same thing during bootstrap
   and steady state.
5. Moves the `local-only` and `remote-only` sets in **two batched transfers** via
   [`SyncEngine::bulk_upload`] / `bulk_download` (see
   [architecture](architecture.md#sync-engine)), then re-walks the remote once to
   record the uploaded files' Drive ids in `sync_item`.
6. Captures a Drive `changes.getStartPageToken` baseline **last**, so the
   continuous loop only sees events that happened *after* the initial pass.

For `both` files, an md5 match is recorded directly in `sync_item`; a mismatch
is deferred to the continuous-sync conflict path. The reconciler owns every
policy decision here — the engine only moves the exact paths it is handed, so the
bulk transfer can never sync an ignored, conflicting, or native-Doc file.

Persisting the cursor last means an interrupted initial pass leaves files copied
but no cursor; the next `start` simply re-runs the (idempotent) pass — md5-equal
files are recorded without re-transfer.

## Conflicts

A conflict is a file modified on **both** sides between syncs. When detected, the
reconciler enqueues `mark_conflict` and records a row in `conflict_record` with
the original path and a derived `conflict_relative_path` (a renamed sibling), so
neither edit is lost. Conflicts are surfaced via `air-drive status`.

## Failure handling and back-off

The dispatcher retries failed operations with exponential back-off: initial 1 s,
doubling, capped at 60 s, with ±20 % jitter, up to `MAX_ATTEMPTS` (10). After the
cap the op stays in `pending_operation` with `last_error` populated and the
daemon reports `status: blocked` (`blocked_kind` ∈ `auth`, `remote`, `mapping`).

## Safety net

A periodic full reconciliation (`safety_net_interval_seconds`, ≥ 5 min) catches
anything the event paths missed (a dropped inotify event, a poll that errored).
It is a backstop, not the primary mechanism — see
[`../../CLAUDE.md`](../../CLAUDE.md) §II.
