//! Continuous reconciliation.
//!
//! Two entry points consumed by the daemon's main loop:
//!
//! - [`apply_local`] — `WatchEvent` → SQL inserts into `pending_operation`.
//! - [`apply_remote`] — `RemoteChange` → same, on the Drive side.
//!
//! Both consult `sync_item` to suppress echoes (a change we caused via our own
//! upload) and to skip native Google Docs.
//!
//! The functions are stateless beyond the database — they don't talk to the
//! engine; the dispatcher (`daemon::runtime`) does.

use std::path::Path;

use serde_json::json;

use crate::daemon::in_flight::InFlightOps;
use crate::drive::changes::RemoteChange;
use crate::error::{Error, Result};
use crate::state::Db;
use crate::state::items::{self, ItemKind, ItemState, NewSyncItem};
use crate::state::mapping::MappingId;
use crate::state::ops::{self, Operation};
use crate::state::unix_now;
use crate::watch::WatchEvent;

/// Native Google Docs / Sheets / Slides mime prefix. These do not have an md5
/// and cannot be synced as opaque bytes — skipped silently (the poller still
/// observes them; the reconciler is the gate).
const NATIVE_GAPPS_PREFIX: &str = "application/vnd.google-apps.";

/// Convert a `WatchEvent` into `pending_operation` rows.
pub async fn apply_local(
    event: WatchEvent,
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
) -> Result<()> {
    match event {
        WatchEvent::Created(p) | WatchEvent::Modified(p) => {
            if p.is_dir() {
                // Directory: persist as kind='dir' and enqueue a remote create.
                // Idempotent — if we already track it there's nothing to do (covers
                // a Modified on a dir and the watcher echo of a CreateDirLocal we
                // just performed).
                let rel = strip_root(&p, local_root)?;
                if items::get_by_relative_path(db.connection(), mapping_id, &rel)
                    .await?
                    .is_none()
                {
                    let new_id = items::insert(
                        db.connection(),
                        &NewSyncItem {
                            mapping_id,
                            relative_path: rel,
                            kind: ItemKind::Dir,
                            remote_id: None,
                            size: None,
                            md5: None,
                            local_inode: None,
                            last_synced_at: 0,
                            state: ItemState::PendingLocal,
                        },
                    )
                    .await?;
                    ops::enqueue(
                        db.connection(),
                        new_id,
                        Operation::CreateDirRemote,
                        None,
                        unix_now(),
                    )
                    .await?;
                }
                return Ok(());
            }
            if !p.is_file() {
                // Neither a regular file nor a directory — it vanished between the
                // event and now, or it's a special file. Nothing to do.
                return Ok(());
            }
            let rel = strip_root(&p, local_root)?;
            // No fingerprint computation here — the dispatcher will hash the file
            // right before uploading. Doing it twice (once for echo suppression,
            // once for persistence) burns CPU on big files; deferring the check
            // means we may enqueue an upload that turns out to be a no-op, but
            // the dispatcher detects that case via the same md5 comparison and
            // skips the engine call. The fingerprint we persist is then
            // guaranteed to match the bytes we actually pushed.
            match items::get_by_relative_path(db.connection(), mapping_id, &rel).await? {
                Some(item) => {
                    ops::enqueue(
                        db.connection(),
                        item.id,
                        Operation::Upload,
                        None,
                        unix_now(),
                    )
                    .await?;
                }
                None => {
                    let new_id = items::insert(
                        db.connection(),
                        &NewSyncItem {
                            mapping_id,
                            relative_path: rel.clone(),
                            kind: ItemKind::File,
                            remote_id: None,
                            // Leave size/md5 unset — the dispatcher populates them
                            // after the upload completes.
                            size: None,
                            md5: None,
                            local_inode: None,
                            last_synced_at: 0,
                            state: ItemState::PendingLocal,
                        },
                    )
                    .await?;
                    ops::enqueue(db.connection(), new_id, Operation::Upload, None, unix_now())
                        .await?;
                }
            }
        }

        WatchEvent::Deleted(p) => {
            let rel = strip_root(&p, local_root)?;
            // Conflict cleanup: if the deleted path is one side of an open
            // conflict_record, the user just resolved it — drop the row.
            crate::reconcile::conflict::cleanup_on_local_delete(db, &rel).await?;
            if let Some(item) =
                items::get_by_relative_path(db.connection(), mapping_id, &rel).await?
            {
                if item.remote_id.is_some() {
                    ops::enqueue(
                        db.connection(),
                        item.id,
                        Operation::DeleteRemote,
                        None,
                        unix_now(),
                    )
                    .await?;
                } else {
                    // Never made it to Drive — just drop the sync_item.
                    items::delete(db.connection(), item.id).await?;
                }
            }
        }

        WatchEvent::Renamed { from, to } => {
            let from_rel = strip_root(&from, local_root)?;
            let to_rel = strip_root(&to, local_root)?;
            match items::get_by_relative_path(db.connection(), mapping_id, &from_rel).await? {
                Some(item) => {
                    let payload = json!({ "new_relative_path": to_rel }).to_string();
                    ops::enqueue(
                        db.connection(),
                        item.id,
                        Operation::RenameRemote,
                        Some(&payload),
                        unix_now(),
                    )
                    .await?;
                }
                None => {
                    // Rename from outside the watched tree (or the daemon never
                    // saw `from`). Treat as a fresh create at `to`.
                    return Box::pin(apply_local(
                        WatchEvent::Created(to),
                        db,
                        mapping_id,
                        local_root,
                    ))
                    .await;
                }
            }
        }
    }
    Ok(())
}

/// Convert a `RemoteChange` into `pending_operation` rows. Filters native
/// Google Docs, our own in-flight ops ([`InFlightOps`]), and post-echo md5
/// matches.
pub async fn apply_remote(
    change: RemoteChange,
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    in_flight: &InFlightOps,
) -> Result<()> {
    // First gate: is this an echo of a write the dispatcher is performing
    // right now? Eliminates the race between `engine.update` returning and
    // the dispatcher persisting the new fingerprint to `sync_item`.
    if in_flight.contains(&change.file_id) {
        tracing::debug!(file_id = %change.file_id, "skipping in-flight echo");
        return Ok(());
    }

    if change.removed {
        if let Some(item) = items::get_by_remote_id(db.connection(), &change.file_id).await? {
            ops::enqueue(
                db.connection(),
                item.id,
                Operation::DeleteLocal,
                None,
                unix_now(),
            )
            .await?;
        }
        return Ok(());
    }

    let Some(file) = change.file else {
        return Ok(());
    };

    if file.is_folder() {
        let rel = change
            .relative_path
            .clone()
            .unwrap_or_else(|| file.name.clone());
        match items::get_by_remote_id(db.connection(), &change.file_id).await? {
            None => {
                // Brand-new folder → persist as kind='dir' and enqueue a local mkdir.
                let new_id = items::insert(
                    db.connection(),
                    &NewSyncItem {
                        mapping_id,
                        relative_path: rel,
                        kind: ItemKind::Dir,
                        remote_id: Some(file.id.clone()),
                        size: None,
                        md5: None,
                        local_inode: None,
                        last_synced_at: 0,
                        state: ItemState::PendingLocal,
                    },
                )
                .await?;
                ops::enqueue(
                    db.connection(),
                    new_id,
                    Operation::CreateDirLocal,
                    None,
                    unix_now(),
                )
                .await?;
            }
            Some(existing) if existing.relative_path != rel => {
                // Known folder whose path changed on Drive → renamed or moved.
                // Propagate locally; the descendants follow on disk and are
                // rewritten in the DB by the dispatcher (no per-child events).
                let payload = json!({ "new_relative_path": rel }).to_string();
                ops::enqueue(
                    db.connection(),
                    existing.id,
                    Operation::RenameLocal,
                    Some(&payload),
                    unix_now(),
                )
                .await?;
            }
            Some(_) => { /* same path — echo of a folder we created; nothing to do */ }
        }
        return Ok(());
    }
    if file.mime_type.starts_with(NATIVE_GAPPS_PREFIX) {
        tracing::info!(
            id = %file.id,
            name = %file.name,
            mime = %file.mime_type,
            "skipping native Google Docs file"
        );
        return Ok(());
    }
    let Some(remote_md5) = file.md5.clone() else {
        return Ok(());
    };
    let remote_size = file.size.unwrap_or(0);

    match items::get_by_remote_id(db.connection(), &change.file_id).await? {
        Some(item) => {
            // Echo suppression: same md5 means this is a notification of our
            // own upload — nothing to do.
            if item.md5.as_deref() == Some(remote_md5.as_str()) && item.size == Some(remote_size) {
                return Ok(());
            }
            // Conflict detection: if the local file's CURRENT md5
            // differs from the last-synced fingerprint, both sides drifted
            // independently. Open a conflict — rename the local copy, insert
            // a conflict_record, then proceed with the Download so the remote
            // version takes the canonical name (Q2: remote keeps canonical).
            let canonical_local = local_root.join(&item.relative_path);
            if canonical_local.is_file()
                && let Some(last_synced_md5) = item.md5.as_deref()
            {
                match crate::reconcile::fingerprint::compute_local(&canonical_local).await {
                    Ok((_, local_md5)) if local_md5 != last_synced_md5 => {
                        // Both sides changed since the last sync → conflict.
                        crate::reconcile::conflict::open_conflict(
                            db,
                            item.id,
                            &canonical_local,
                            &item.relative_path,
                            local_root,
                            unix_now(),
                        )
                        .await?;
                    }
                    Ok(_) => { /* local untouched — pure remote update */ }
                    Err(e) => tracing::warn!(
                        error = %e,
                        path = %canonical_local.display(),
                        "could not fingerprint local for conflict check; treating as remote-only update"
                    ),
                }
            }
            // Real divergence — enqueue a Download to pull the new content.
            let payload = json!({
                "remote_id": file.id,
                "size": remote_size,
                "md5": remote_md5,
                "relative_path": item.relative_path,
            })
            .to_string();
            ops::enqueue(
                db.connection(),
                item.id,
                Operation::Download,
                Some(&payload),
                unix_now(),
            )
            .await?;
        }
        None => {
            // Brand-new remote file. The poller already walked the parent chain
            // and packaged the full relative path into `change.relative_path`,
            // so a nested create (`docs/spec.txt` on Drive) lands at the right
            // place locally rather than being flattened to `<root>/spec.txt`.
            let rel = change
                .relative_path
                .clone()
                .unwrap_or_else(|| file.name.clone());
            let new_id = items::insert(
                db.connection(),
                &NewSyncItem {
                    mapping_id,
                    relative_path: rel.clone(),
                    kind: ItemKind::File,
                    remote_id: Some(file.id.clone()),
                    size: Some(remote_size),
                    md5: Some(remote_md5.clone()),
                    local_inode: None,
                    last_synced_at: 0,
                    state: ItemState::PendingLocal,
                },
            )
            .await?;
            let payload = json!({
                "remote_id": file.id,
                "size": remote_size,
                "md5": remote_md5,
                "relative_path": rel,
            })
            .to_string();
            ops::enqueue(
                db.connection(),
                new_id,
                Operation::Download,
                Some(&payload),
                unix_now(),
            )
            .await?;
        }
    }
    Ok(())
}

fn strip_root(absolute: &Path, root: &Path) -> Result<String> {
    let rel = absolute
        .strip_prefix(root)
        .map_err(|e| Error::Mapping(format!("strip_prefix: {e}")))?;
    Ok(rel
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/"))
}
