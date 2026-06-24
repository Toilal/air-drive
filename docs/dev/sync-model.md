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

Symlinks are classified by `watch::classify_local` according to the
`[watch].symlinks` policy — the single source of truth shared by the live
watcher filter and the `reconcile` tree walkers. Under `skip` (default) a
symlink is ignored; under `follow` it is resolved to its target (a file or
directory), with two guards: a link whose target resolves **outside** the
watched root is skipped, and directory-symlink cycles are broken by tracking
visited canonical paths. See [configuration](../user/configuration.md#symlinks).

When a **new directory** is created, a file dropped into it can land before
`notify` registers the recursive watch on the new subdir, so the file's own
event is never delivered. To avoid silently missing it, `apply_local`'s
`Created(dir)` handling **rescans the new directory** and enqueues every entry
already inside it.

A **within-tree rename** arrives from inotify as a correlated `From`+`To` pair
(matched by cookie) and becomes a single `Renamed` event. A **move out of the
watched tree** — most commonly a desktop file manager moving a file to the
trash — is a lone `From` with no matching `To`. The watcher buffers each `From`
for `RENAME_CORRELATION_TTL` (5 s); a buffered `From` that never gets its `To`
is emitted as a `Deleted` (so the deletion propagates to Drive), driven either
by the next event's sweep or, on an otherwise quiet tree, by a small reaper
task. This keeps "send to trash" on the prompt event-driven path instead of
waiting for the safety-net reconcile.

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

`delete_remote` (and the directory variant) is addressed **by Drive id**, not by
path: the engine PATCHes `trashed=true` or issues a `files.delete`, depending on
[`[sync].remote_deletes`](../user/configuration.md#sync--synchronisation-policy)
(default `trash`, recoverable). It is **not** routed through `rclone` — rclone's
Drive backend addresses by path, so it cannot locate an object given only its id
(the same reason `rename_remote` goes straight to the Drive API). The trashed/
deleted echo that later returns from `changes.list` is dropped by the `in_flight`
tracker, which is consulted before both the `removed` and `trashed` branches of
`apply_remote`.

## Echo suppression

When air-drive uploads a file, Drive's `changes.list` will later report that
very change — and a download we perform triggers a local inotify event. Left
unchecked, these "echoes" would ping-pong forever.

The reconciler consults `sync_item` (the per-path record of last-synced size /
md5 / inode) to recognise a change it caused itself and drop it. The `in_flight`
tracker (`daemon/in_flight.rs`) covers the window where an op is mid-execution.

One subtlety: a freshly **locally-created** file/folder is uploaded, and its
`changes.list` echo can arrive *before* the upload op has written the new Drive
id back onto the `sync_item` (so the by-`remote_id` lookup misses). To avoid
re-importing our own creation (a duplicate / churn), `apply_remote` also checks
the resolved path: if a row already exists there, the change is the echo of a
local create whose remote-id link is still pending, and it is suppressed — the
upload op owns linking the id.

A pure **remote rename / move** of a regular file (same `remote_id`, unchanged
md5, but a different resolved path) would otherwise be indistinguishable from an
echo and dropped. So `apply_remote` checks the resolved path *before* the md5
echo test: a path change on a known file enqueues a `rename_local` (mirroring the
folder and gdoc rename branches) rather than a re-download, and a combined
rename-plus-edit also queues a `download` onto the new path. After our own local
rename, the `rename_remote` op has already rewritten the row's path, so the
returning echo resolves to the same path and is correctly suppressed.

### Folder rename vs file move

A file's resolved path can change for two reasons, and they need opposite
handling. Before treating a path change as a per-file rename, `apply_remote`
compares the change's **parent id** to the directory it tracks at the file's old
parent path:

- **Same parent id** → the *folder* was renamed in place (the child keeps its
  parent, only the name changed). Drive may deliver only the child's change, or
  deliver it before the folder's own change. Moving the child alone would strand
  the emptied old directory and let the watcher re-upload it as a duplicate
  folder. So the daemon enqueues the `rename_local` for the **directory** — an
  idempotent subtree move that carries every child and removes the old path.
- **Different parent id** → a genuine move into another folder → per-file
  `rename_local`.

When a directory `rename_local` finds its destination already populated (a child
downloaded into the new path first, under an eventual-consistency cascade), it
**merges** the source subtree into the destination and drops the emptied source
instead of failing with `ENOTEMPTY`; the DB rewrite drops the stale source row
on a path collision. Together these make a remote folder rename converge however
Drive orders or splits the underlying change events (issue #19).

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

Detection compares three fingerprints: the local file's **current** md5, the
**last-synced** md5 (`sync_item`), and the **remote** md5. A conflict is opened
only when local differs from last-synced *and* from remote. If local already
equals remote, the two sides agree — this is a re-delivery of a change already
applied to disk whose `sync_item` fingerprint isn't persisted yet (the change
feed can hand the same entry back before the `Download` op records the new md5),
so it is treated as a no-op rather than a spurious conflict.

## Failure handling and back-off

The dispatcher retries failed operations with exponential back-off: initial 1 s,
doubling, capped at 60 s, with ±20 % jitter, up to `MAX_ATTEMPTS` (10). After the
cap the op stays in `pending_operation` with `last_error` populated and the
daemon reports `status: blocked`.

`blocked_kind` separates **terminal** failures that need user action — `auth`
(re-link), `remote` (watched folder gone), `mapping` (local path missing) — from
the **recoverable** `transient` kind. When a `changes.list` poll fails past the
HTTP layer's own retry budget (network / 5xx), the poller flips to
`transient` so `status` shows "Drive unreachable" instead of stalling silently.
The first subsequent successful Drive call — a poll, or any dispatcher op —
clears it (`meta::clear_if_transient`, which leaves the terminal kinds in place),
so `status` reports healthy again on its own once Drive recovers.

## Startup catch-up

The event paths only see changes that happen *while the daemon runs*. A change
made while it was stopped is recovered at the next start, from both sides:

- **Remote**: the change poller resumes from the persisted `pageToken`, so every
  Drive delta since the last run is replayed through `apply_remote` (conflicts
  included).
- **Local**: inotify wasn't running while the daemon was down, so a dedicated
  **startup local scan** (`reconcile::startup_local_scan`) diffs the local tree
  against `sync_item` (the last-synced fingerprints) and feeds the differences —
  new/modified/deleted files — through `apply_local` as synthesized watch events.
  It reuses the continuous pipeline's three-way conflict + echo handling and does
  **not** touch the change cursor. It runs on every **restart** (when a change
  cursor already exists); it is skipped on the first sync, where the initial
  reconciliation has just converged everything. A local modify/delete is only
  replayed when the remote is still at the last-synced fingerprint (one
  `files.get`); if both sides drifted while down it is deferred to the change
  poller's conflict handler, so the scan never overwrites a concurrent edit.

## Safety net

`safety_net_interval_seconds` is reserved for a future *periodic* full
reconciliation — a backstop for events dropped while the daemon runs (a missed
inotify event, a poll that errored), constrained to ≥ 5 min so it never becomes
the primary mode (constitution principle II). That periodic loop is not wired
yet; the **startup catch-up** above already covers the common
daemon-was-down case.
