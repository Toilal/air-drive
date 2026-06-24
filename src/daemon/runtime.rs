//! Pending-operation dispatcher.
//!
//! Pulls due operations from `pending_operation` and executes them via the
//! configured [`SyncEngine`]. On success the row is removed; on failure the
//! dispatcher applies exponential back-off with ±20 % jitter (1 s → 60 s, max
//! [`MAX_ATTEMPTS`] tries) and reschedules. After [`MAX_ATTEMPTS`] failures the
//! op is parked far in the future ([`PARKED_FOREVER`]) — left in place with its
//! `last_error` for inspection, but no longer reselected — and an error is
//! logged; manual resolution is required to revive it.
//!
//! The dispatcher loop wakes every [`POLL_INTERVAL`] or whenever a new op is
//! enqueued via the wake channel (the reconciler signals on enqueue to skip
//! the polling delay for the common no-op-in-flight case).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::daemon::in_flight::InFlightOps;
use crate::daemon::pause::PauseState;
use crate::drive::http::DriveHttp;
use crate::engine::SyncEngine;
use crate::error::{Error, Result};
use crate::reconcile::fingerprint;
use crate::state::Db;
use crate::state::items;
use crate::state::mapping::MappingId;
use crate::state::meta::{self, BlockedKind};
use crate::state::ops::{self, Operation, PendingOperation};
use crate::state::unix_now;

/// Maximum number of attempts before the dispatcher abandons an op.
pub const MAX_ATTEMPTS: i64 = 10;

/// How often the dispatcher checks for due ops when idle.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Initial backoff (after attempt 1).
const INITIAL_BACKOFF_SECS: i64 = 1;

/// Hard cap on a single backoff delay before jitter.
const MAX_BACKOFF_SECS: i64 = 60;

/// `next_attempt_at` sentinel for an op abandoned after [`MAX_ATTEMPTS`]: far
/// enough in the future (year ~3000) that `next_due` never reselects it, so it
/// stops churning the queue while staying inspectable in `pending_operation`.
const PARKED_FOREVER: i64 = 32_503_680_000;

/// Daemon dispatcher loop.
///
/// `wake` is signalled by the reconciler whenever it enqueues a new op so the
/// dispatcher doesn't wait the full `POLL_INTERVAL` to react.
#[allow(clippy::too_many_arguments)] // wiring 9 collaborators by name is clearer than a struct
pub async fn run(
    db: Db,
    engine: Arc<dyn SyncEngine>,
    http: DriveHttp,
    local_root: PathBuf,
    remote_root_id: String,
    wake: Arc<Notify>,
    cancel: CancellationToken,
    in_flight: InFlightOps,
    pause: PauseState,
) -> Result<()> {
    loop {
        // Block cooperatively while paused. `wait_for_resume` returns instantly
        // when running, so the running-path overhead is one atomic read.
        if pause.is_paused() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                _ = pause.wait_for_resume() => {}
            }
        }
        // Pause only on a TERMINAL block (`auth` → re-link, `remote` → folder
        // gone, `mapping` → local path gone): those need user action, and the
        // dispatcher can't make progress past them. A recoverable `transient`
        // block (the poller couldn't reach Drive) must NOT pause local→remote
        // work — the ops may succeed, and a successful op clears the transient
        // block itself. Sleeping with a timeout keeps the cancel token responsive.
        if matches!(
            meta::get_blocked(db.connection()).await?,
            Some(b) if b.kind != BlockedKind::Transient
        ) {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(30)) => continue,
            }
        }
        // Drain everything that's due before sleeping.
        loop {
            let Some(op) = ops::next_due(db.connection(), unix_now()).await? else {
                break;
            };
            if op.attempts >= MAX_ATTEMPTS {
                tracing::error!(
                    op_id = op.id.0,
                    attempts = op.attempts,
                    "abandoning operation — manual resolution required"
                );
                // Park it effectively forever so it stops being re-selected every
                // hour (the old `+3600` churned the queue and kept bumping
                // `attempts` unboundedly). It stays in `pending_operation` with its
                // `last_error` for inspection; manual resolution (or a future
                // requeue path) is required to revive it.
                if let Err(e) = ops::mark_attempt(
                    db.connection(),
                    op.id,
                    Some("max attempts reached"),
                    PARKED_FOREVER,
                )
                .await
                {
                    tracing::warn!(op_id = op.id.0, error = %e, "failed to park abandoned op");
                }
                continue;
            }
            match execute(
                &db,
                &engine,
                &http,
                &local_root,
                &remote_root_id,
                &in_flight,
                &op,
            )
            .await
            {
                Ok(()) => {
                    // Bump the last_sync_at + items_uploaded / items_downloaded
                    // counters that `air-drive status` surfaces.
                    // Upload/RenameRemote count as uploaded; Download as
                    // downloaded; deletes don't increment anything (the
                    // conflict + status schema doesn't track them separately).
                    let (delta_up, delta_down) = match op.op {
                        Operation::Upload | Operation::RenameRemote => (1, 0),
                        Operation::Download => (0, 1),
                        _ => (0, 0),
                    };
                    if delta_up != 0 || delta_down != 0 {
                        if let Err(e) = meta::record_sync_cycle(
                            db.connection(),
                            unix_now(),
                            delta_up,
                            delta_down,
                        )
                        .await
                        {
                            tracing::warn!(op_id = op.id.0, error = %e, "failed to record sync cycle");
                        }
                    }
                    // A completed op proves Drive is reachable again — clear any
                    // recoverable `transient` block the poller left, so recovery
                    // is reflected without waiting for the next poll tick.
                    if let Ok(true) = meta::clear_if_transient(db.connection()).await {
                        tracing::info!(op_id = op.id.0, "op succeeded — cleared transient block");
                    }
                    // If the delete fails the op stays `pending` and would
                    // re-execute (a duplicate upload/download/rename) — surface it.
                    if let Err(e) = ops::delete(db.connection(), op.id).await {
                        tracing::warn!(op_id = op.id.0, error = %e, "failed to delete completed op — it may re-run");
                    }
                }
                Err(Error::Oauth(msg)) => {
                    // OAuth failure: refreshing the access token failed or
                    // the server responded 401. Persist the blocked state so
                    // `air-drive status` surfaces it, then push the op far
                    // into the future. The outer loop sees the blocked flag
                    // and sleeps — no point burning retries on auth.
                    tracing::error!(
                        op_id = op.id.0,
                        error = %msg,
                        "auth failure — daemon is now blocked"
                    );
                    if let Err(e) =
                        meta::set_blocked(db.connection(), BlockedKind::Auth, &msg, unix_now())
                            .await
                    {
                        tracing::warn!(op_id = op.id.0, error = %e, "failed to persist auth block");
                    }
                    if let Err(e) = ops::mark_attempt(
                        db.connection(),
                        op.id,
                        Some(&format!("blocked: {msg}")),
                        unix_now() + 3600,
                    )
                    .await
                    {
                        tracing::warn!(op_id = op.id.0, error = %e, "failed to reschedule blocked op");
                    }
                    break; // exit the drain loop; the outer loop catches blocked
                }
                Err(e) => {
                    let delay = backoff_seconds(op.attempts + 1);
                    let next = unix_now() + delay;
                    tracing::warn!(
                        op_id = op.id.0,
                        attempts = op.attempts + 1,
                        retry_in_s = delay,
                        error = %e,
                        "op failed; will retry"
                    );
                    if let Err(e) =
                        ops::mark_attempt(db.connection(), op.id, Some(&e.to_string()), next).await
                    {
                        tracing::warn!(op_id = op.id.0, error = %e, "failed to reschedule failed op");
                    }
                }
            }
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            _ = wake.notified() => {}
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

async fn execute(
    db: &Db,
    engine: &Arc<dyn SyncEngine>,
    http: &DriveHttp,
    local_root: &std::path::Path,
    remote_root_id: &str,
    in_flight: &InFlightOps,
    op: &PendingOperation,
) -> Result<()> {
    let item = items::get_by_id(db.connection(), op.sync_item_id)
        .await?
        .ok_or_else(|| Error::Mapping(format!("sync_item {} vanished", op.sync_item_id.0)))?;

    tracing::debug!(
        op_id = op.id.0,
        op = ?op.op,
        relative_path = %item.relative_path,
        remote_id = item.remote_id.as_deref().unwrap_or("-"),
        "execute op"
    );

    match op.op {
        Operation::Upload => {
            let local = local_root.join(&item.relative_path);
            if !local.is_file() {
                // The file was deleted between enqueue and execution — abandon
                // this op without erroring; the delete event will arrive too.
                return Ok(());
            }
            let (size, md5) = match fingerprint::compute_local(&local).await {
                Ok(v) => v,
                Err(Error::Io(io)) if io.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(Error::Mapping(format!(
                        "EACCES on {} — caller will retry",
                        local.display()
                    )));
                }
                Err(e) => return Err(e),
            };

            // Echo suppression: the reconciler enqueues an Upload for every local
            // Modified/Created event without computing the fingerprint first
            // (cf. `reconcile::continuous::apply_local`). If the file's current
            // hash matches what `sync_item` already records, the event was either
            // a no-op `mv`/`touch` or the watcher-echo of a Download we just
            // performed — skip the engine call entirely.
            if item.md5.as_deref() == Some(&md5) && item.size == Some(size) {
                return Ok(());
            }

            match item.remote_id.as_deref() {
                Some(rid) => {
                    // Mark the existing remote ID as in-flight so the poller's
                    // `apply_remote` skips the change event Drive will emit for
                    // this update. Guard drops at function return → set cleared
                    // automatically, even on the error path.
                    let _guard = in_flight.mark(rid);
                    engine.update(rid, &local).await?;
                    items::update_fingerprint(
                        db.connection(),
                        item.id,
                        Some(size),
                        Some(&md5),
                        unix_now(),
                    )
                    .await?;
                }
                None => {
                    let (parent_rel, name) = split_parent(&item.relative_path);
                    let parent_id = ensure_remote_folder(
                        engine,
                        http,
                        db,
                        item.mapping_id,
                        remote_root_id,
                        parent_rel,
                    )
                    .await?;
                    let rf = engine.upload(&local, &parent_id, name).await?;
                    // Now we know the brand-new remote id — register it as
                    // in-flight before any other state mutation. The poller may
                    // already have noticed the create between `engine.upload`
                    // returning and this `mark` call (a few µs at most); for
                    // that microscopic window the sync_item's UNIQUE
                    // constraint on (mapping_id, relative_path) blocks a
                    // duplicate insert. The mark guarantees subsequent ticks
                    // skip the echo cleanly.
                    let _guard = in_flight.mark(&rf.id);
                    items::set_remote_id(db.connection(), item.id, &rf.id).await?;
                    items::update_fingerprint(
                        db.connection(),
                        item.id,
                        Some(size),
                        Some(&md5),
                        unix_now(),
                    )
                    .await?;
                }
            }
        }

        Operation::Download => {
            let dl: ops::DownloadPayload = ops::decode_payload(&op.payload)?;
            let remote_id = dl.remote_id.as_str();
            let rel = dl.relative_path.as_str();
            let local = local_root.join(rel);
            if let Some(parent) = local.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    if e.kind() == std::io::ErrorKind::StorageFull {
                        // Don't try to write partials when the disk is full.
                        Error::Mapping(format!("ENOSPC creating parent of {}", local.display()))
                    } else {
                        Error::Io(e)
                    }
                })?;
            }
            engine.download(remote_id, &local, local_root).await?;

            let (size, md5) = fingerprint::compute_local(&local).await?;
            items::update_fingerprint(db.connection(), item.id, Some(size), Some(&md5), unix_now())
                .await?;
            // Make sure the sync_item knows the remote_id, in case this was a
            // brand-new remote create.
            if item.remote_id.as_deref() != Some(remote_id) {
                items::set_remote_id(db.connection(), item.id, remote_id).await?;
            }
        }

        Operation::DeleteRemote => {
            if let Some(rid) = &item.remote_id {
                // Mark in-flight so the poller's `removed`/`trashed` echo doesn't
                // re-enqueue a `DeleteLocal` for the item we're already
                // deleting end-to-end.
                let _guard = in_flight.mark(rid);
                let res = match item.kind {
                    items::ItemKind::Dir => engine.remove_dir_remote(rid).await,
                    items::ItemKind::File => engine.delete_remote(rid).await,
                };
                match res {
                    Ok(()) => items::delete(db.connection(), item.id).await?,
                    // The user deleted locally a file they don't own, so Drive
                    // refuses to trash it. Fall back to "Remove from My Drive":
                    // unlink it from the user's folders so it disappears from
                    // their Drive too (the owner keeps their copy). If even that
                    // is refused, stop fighting — mark the item `skipped` so
                    // neither side re-syncs it. Either branch finishes the op (no
                    // retry on a permanent permission error).
                    Err(e) if e.is_insufficient_permissions() => {
                        match crate::drive::metadata::remove_from_my_drive(http, rid).await {
                            Ok(()) => {
                                tracing::info!(
                                    relative_path = %item.relative_path,
                                    "not the owner — removed file from My Drive instead of trashing it"
                                );
                                items::delete(db.connection(), item.id).await?;
                            }
                            Err(e2) => {
                                tracing::warn!(
                                    relative_path = %item.relative_path,
                                    error = %e2,
                                    "not the owner and cannot remove the file from My Drive — leaving \
                                     it on Drive and no longer syncing it; remove it via \
                                     drive.google.com if you want it gone"
                                );
                                items::set_state(
                                    db.connection(),
                                    item.id,
                                    items::ItemState::Skipped,
                                )
                                .await?;
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            } else {
                // Never reached Drive — just drop the row.
                items::delete(db.connection(), item.id).await?;
            }
        }

        Operation::DeleteLocal => {
            let local = local_root.join(&item.relative_path);
            // A directory is removed recursively; its children's sync_items are
            // dropped by their own delete events / the start-up reconcile.
            let res = match item.kind {
                items::ItemKind::Dir => tokio::fs::remove_dir_all(&local).await,
                items::ItemKind::File => tokio::fs::remove_file(&local).await,
            };
            match res {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Io(e)),
            }
            // `tombstone` is set by the reconciler: true for a trash (keep the
            // file row + remote_id so a restore re-links and re-downloads to the
            // original path, avoiding a duplicate — #8), false for a permanent
            // delete / loss of access (drop the row). Directories are never
            // tombstoned regardless.
            let tombstone = ops::decode_payload::<ops::DeleteLocalPayload>(&op.payload)?.tombstone;
            if tombstone && matches!(item.kind, items::ItemKind::File) {
                items::mark_trashed(db.connection(), item.id, unix_now()).await?;
            } else {
                items::delete(db.connection(), item.id).await?;
            }
        }

        Operation::CreateDirRemote => {
            // Idempotent: `ensure_remote_folder` walks the chain, creates only
            // missing segments via the engine, and persists each (including this
            // folder) as a kind='dir' sync_item with its remote_id. Passing the
            // item's full relative path makes the folder itself the leaf segment.
            ensure_remote_folder(
                engine,
                http,
                db,
                item.mapping_id,
                remote_root_id,
                &item.relative_path,
            )
            .await?;
        }

        Operation::CreateDirLocal => {
            let local = local_root.join(&item.relative_path);
            tokio::fs::create_dir_all(&local).await.map_err(Error::Io)?;
            items::set_state(db.connection(), item.id, items::ItemState::Synced).await?;
        }

        Operation::WriteShortcut => {
            // Native Google Doc → local pointer file (issue #3). The reconciler put
            // the rendered JSON body in the payload; we just write it where the
            // shortcut lives. The item keeps `state = skipped` so the local watcher
            // never tries to upload the pointer, and `status` surfaces it.
            let body = ops::decode_payload::<ops::ShortcutPayload>(&op.payload)?.content;
            let local = local_root.join(&item.relative_path);
            crate::reconcile::shortcut::write(&local, &body).await?;
        }

        Operation::RenameRemote => {
            let payload: ops::RenamePayload = ops::decode_payload(&op.payload)?;
            let new_rel = payload.new_relative_path.as_str();
            let Some(rid) = &item.remote_id else {
                // Renamed before the first upload landed — drop op, the
                // create-with-the-new-path will be picked up on the next walk.
                items::delete(db.connection(), item.id).await?;
                return Ok(());
            };
            let (parent_rel, new_name) = split_parent(new_rel);
            let new_parent_id = ensure_remote_folder(
                engine,
                http,
                db,
                item.mapping_id,
                remote_root_id,
                parent_rel,
            )
            .await?;
            // Mark in-flight: Drive emits a change event for the renamed file,
            // and even though our md5 echo check would handle it, suppressing
            // the round-trip saves the poller a redundant `apply_remote` walk.
            let _guard = in_flight.mark(rid);
            engine.move_remote(rid, &new_parent_id, new_name).await?;
            match item.kind {
                // Moving a folder on Drive relocates its whole subtree (the
                // children keep it as parent), so only the folder needs the API
                // call — but every descendant's relative_path must be rewritten.
                items::ItemKind::Dir => {
                    items::rename_subtree(
                        db.connection(),
                        item.mapping_id,
                        &item.relative_path,
                        new_rel,
                    )
                    .await?;
                }
                items::ItemKind::File => {
                    items::set_relative_path(db.connection(), item.id, new_rel).await?;
                }
            }
        }

        Operation::RenameLocal => {
            let payload: ops::RenamePayload = ops::decode_payload(&op.payload)?;
            let new_rel = payload.new_relative_path.as_str();
            let old_rel = item.relative_path.clone();
            let old_local = local_root.join(&old_rel);
            let new_local = local_root.join(new_rel);
            if let Some(parent) = new_local.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(Error::Io)?;
            }
            // Rename the local directory — moves the whole subtree on disk at once.
            match tokio::fs::rename(&old_local, &new_local).await {
                Ok(()) => {}
                // Already moved (e.g. a retry, or the user did it too) — fall through
                // to the DB rewrite so state still converges.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Destination already exists and is non-empty: under a Drive
                // eventual-consistency cascade a child can be downloaded into the
                // *new* path before this rename runs (#19), so a plain rename
                // would `ENOTEMPTY` forever. Merge the source subtree into the
                // destination and drop the emptied source instead — the op then
                // converges. `AlreadyExists` covers backends that report EEXIST.
                Err(e)
                    if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                        || e.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    let (src, dst) = (old_local.clone(), new_local.clone());
                    tokio::task::spawn_blocking(move || merge_move_dir(&src, &dst))
                        .await
                        .map_err(|e| Error::Mapping(format!("merge_move_dir join: {e}")))?
                        .map_err(Error::Io)?;
                }
                Err(e) => return Err(Error::Io(e)),
            }
            items::rename_subtree(db.connection(), item.mapping_id, &old_rel, new_rel).await?;
        }

        // The remaining variant (MarkConflict) is not produced by the current
        // reconciler. We log + drop the op so a stale row from an earlier version
        // doesn't wedge the queue.
        other => {
            tracing::warn!(?other, op_id = op.id.0, "no dispatcher path for op");
        }
    }
    Ok(())
}

/// Walk `parent_rel` segment by segment under `root_id`, creating missing Drive
/// folders via the engine and persisting each as a `kind='dir'` `sync_item`.
///
/// This is the single, idempotent folder-creation path: an existing segment is
/// reused (no duplicate on Drive), a missing one is created through
/// `engine.create_dir_remote`, and every segment is recorded in `sync_item` so a
/// later rename/move (#7) or delete has a row to anchor to. For the MVP we
/// re-walk the chain every time; the test set is small. Caching is a follow-up.
async fn ensure_remote_folder(
    engine: &Arc<dyn SyncEngine>,
    http: &DriveHttp,
    db: &Db,
    mapping_id: MappingId,
    root_id: &str,
    parent_rel: &str,
) -> Result<String> {
    if parent_rel.is_empty() {
        return Ok(root_id.to_owned());
    }
    let mut current = root_id.to_owned();
    let mut cumulative = String::new();
    for seg in parent_rel.split('/').filter(|s| !s.is_empty()) {
        cumulative = if cumulative.is_empty() {
            seg.to_owned()
        } else {
            format!("{cumulative}/{seg}")
        };
        let children = crate::drive::metadata::list_children(http, &current).await?;
        let existing = children
            .into_iter()
            .find(|c| c.is_folder() && c.name == seg);
        current = match existing {
            Some(f) => f.id,
            None => engine.create_dir_remote(&current, seg).await?.id,
        };
        persist_dir(db, mapping_id, &cumulative, &current).await?;
    }
    Ok(current)
}

/// Record a folder as a `kind='dir'` `sync_item` (insert if absent, otherwise
/// ensure its `remote_id` is set). Keyed by `(mapping_id, relative_path)`, whose
/// UNIQUE index makes a racing insert from the poller's `apply_remote` fail
/// rather than create a duplicate.
async fn persist_dir(db: &Db, mapping_id: MappingId, rel: &str, remote_id: &str) -> Result<()> {
    match items::get_by_relative_path(db.connection(), mapping_id, rel).await? {
        Some(existing) => {
            if existing.remote_id.as_deref() != Some(remote_id) {
                items::set_remote_id(db.connection(), existing.id, remote_id).await?;
            }
        }
        None => {
            items::insert(
                db.connection(),
                &items::NewSyncItem {
                    mapping_id,
                    relative_path: rel.to_owned(),
                    kind: items::ItemKind::Dir,
                    remote_id: Some(remote_id.to_owned()),
                    size: None,
                    md5: None,
                    local_inode: None,
                    last_synced_at: unix_now(),
                    state: items::ItemState::Synced,
                },
            )
            .await?;
        }
    }
    Ok(())
}

fn split_parent(rel: &str) -> (&str, &str) {
    match rel.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", rel),
    }
}

/// Move every entry of `src` into `dst` (which already exists), recursing into
/// subdirectories that exist on both sides, then remove the emptied `src`. Used
/// when a directory `RenameLocal` finds its destination already populated — a
/// file having been downloaded into the new path before the rename ran, under a
/// Drive eventual-consistency cascade (#19). A plain `fs::rename` `ENOTEMPTY`s
/// there; merging converges instead. Overwriting a file with the byte-identical
/// copy that already arrived is harmless (same Drive object).
///
/// Synchronous (`std::fs`) so it can recurse without boxing; the caller runs it
/// on a blocking thread.
fn merge_move_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() && to.is_dir() {
            merge_move_dir(&from, &to)?;
        } else {
            // File / symlink, or a directory whose destination doesn't exist yet:
            // a plain rename moves it (overwriting a destination file).
            std::fs::rename(&from, &to)?;
        }
    }
    std::fs::remove_dir(src)
}

/// Exponential backoff with jitter. `attempt` is 1-indexed: 1, 2, 4, 8, ... s.
fn backoff_seconds(attempt: i64) -> i64 {
    let base = INITIAL_BACKOFF_SECS << ((attempt - 1).clamp(0, 6) as u32);
    let capped = base.min(MAX_BACKOFF_SECS);
    let jitter_range = (capped / 5).max(1);
    let mut rng = rand::thread_rng();
    let delta: i64 = rng.gen_range(-jitter_range..=jitter_range);
    (capped + delta).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_climbs_then_caps() {
        // Attempt 1 lands somewhere around 1 s.
        let a1 = backoff_seconds(1);
        assert!((1..=2).contains(&a1), "got {a1}");
        // Attempt 7+ caps at ~60 s ± jitter.
        for n in 7..=10 {
            let b = backoff_seconds(n);
            assert!((48..=72).contains(&b), "attempt {n}: {b}");
        }
    }

    #[test]
    fn split_parent_works() {
        assert_eq!(split_parent("a.txt"), ("", "a.txt"));
        assert_eq!(split_parent("dir/a.txt"), ("dir", "a.txt"));
        assert_eq!(split_parent("a/b/c.txt"), ("a/b", "c.txt"));
    }

    #[test]
    fn merge_move_dir_merges_into_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("docs");
        let dst = tmp.path().join("documents");
        // Source subtree (the pre-rename local copy).
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("spec.txt"), b"payload").unwrap();
        std::fs::write(src.join("sub/nested.txt"), b"nested").unwrap();
        // Destination already exists with a byte-identical overlapping file — the
        // child the cascade downloaded into the new path before the rename ran.
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("spec.txt"), b"payload").unwrap();

        merge_move_dir(&src, &dst).unwrap();

        assert!(!src.exists(), "emptied source dir must be removed");
        assert_eq!(std::fs::read(dst.join("spec.txt")).unwrap(), b"payload");
        assert_eq!(
            std::fs::read(dst.join("sub/nested.txt")).unwrap(),
            b"nested"
        );
    }
}
