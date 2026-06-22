# 060 — Bulk initial reconciliation

- **Priority:** 🟡 medium
- **Status:** Planned
- **Issue:** —
- **Area:** engine, reconcile, observability

## Goal

Make the **initial** reconciliation fast and observable on large mappings (e.g.
a whole-Drive root with hundreds/thousands of files), without changing the
event-driven, per-file model that governs steady-state sync.

## Today

`reconcile::initial` drives the engine **one file at a time**, sequentially:

- each upload is two `rclone` invocations (`copyto` + `lsjson`), each download a
  `files.get` + an `rclone` invocation — so ~1–2 process spawns per file, no
  connection reuse, no parallelism;
- on a 600+ file mapping this is minutes of pure process/round-trip overhead
  before any bandwidth, and it is **completely silent** at the default `warn`
  log level, so an operator who can't see progress assumes a hang and Ctrl-Cs;
- the change-cursor baseline is persisted **last** (correctly — see
  [sync-model](../dev/sync-model.md)), so an interrupted initial pass leaves
  files copied but no cursor, and the next start re-runs the whole pass
  (`No sync state yet`);
- **ignore patterns are not applied to the initial pass at all** —
  `reconcile::initial` never receives `watch.ignore_patterns`, so editor/OS
  scratch files are uploaded on first sync.

## Approach

Keep `reconcile` as the **policy** owner (what to sync, what to ignore, conflict
handling, native-Doc shortcuts, state population) and delegate the **bulk byte
movement** to the engine via two new `SyncEngine` methods:

```rust
async fn bulk_download(&self, remote_root_id: &str, rel_paths: &[String], local_root: &Path) -> Result<()>;
async fn bulk_upload(&self, local_root: &Path, rel_paths: &[String], remote_root_id: &str) -> Result<()>;
```

The `SyncEngine` trait doc is updated: the steady-state contract stays
per-file/atomic (principles II + IV), and a **bootstrap-only** bulk transfer is
explicitly carved out — it is not used by the continuous loop.

- `RcloneEngine`: one `rclone copy` per direction with `--files-from <list>`
  (the exact set `reconcile` computed), `--create-empty-src-dirs`,
  `--transfers`/`--checkers` for parallelism, `--drive-skip-gdocs` on download,
  and `--stats 1s --stats-one-line -v` streamed line-by-line to the `rclone`
  tracing target so progress is visible at `info`. rclone recreates the remote
  folder tree itself, so the per-segment `ensure_remote_folder` dance is dropped
  for the bulk path.
- `HttpEngine`: realises the same intent as a per-file loop over `rel_paths`
  (reusing its existing `upload`/`download`), so the mocked integration suite
  keeps running without rclone.

`reconcile::initial` flow becomes: walk both sides → write native-Doc shortcuts →
compute remote-only / local-only sets (filtered by `watch.ignore_patterns`,
shortcuts excluded) → `bulk_download` + `bulk_upload` → re-walk remote for
authoritative ids/md5 → populate `sync_item` (both-sides md5-equal = synced,
md5-differ = deferred conflict, unchanged) → persist the cursor baseline last.

Progress logging: an `info` summary at start/end of the initial pass and an
`info` "entering continuous sync loop" line in `start`.

## Acceptance

- Initial sync of a multi-hundred-file mapping uses a small, bounded number of
  `rclone` invocations (not O(files)), with visible `info`-level progress.
- `watch.ignore_patterns` are honoured by the initial pass (scratch files are
  neither uploaded nor recorded).
- Both-sides conflict semantics and native-Doc shortcuts are unchanged.
- The cursor baseline is still written only after convergence; an interrupted
  pass remains safely idempotent on re-run.
- Integration suite (mocked Drive, `HttpEngine`) stays green; e2e covers the
  real `rclone` bulk path.
