//! Continuous reconciliation (T053).
//!
//! Two entry points consumed by the daemon's main loop:
//!
//! - [`apply_local`] — `WatchEvent` → SQL inserts into `pending_operation`.
//! - [`apply_remote`] — `RemoteChange` → same, on the Drive side.
//!
//! Both consult `sync_item` to suppress echoes (a change we caused via our own
//! upload) and to skip native Google Docs (FR-011).
//!
//! The functions are stateless beyond the database — they don't talk to the
//! engine; the dispatcher (`daemon::runtime`) does.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::drive::changes::RemoteChange;
use crate::error::{Error, Result};
use crate::reconcile::fingerprint;
use crate::state::Db;
use crate::state::items::{self, ItemKind, ItemState, NewSyncItem};
use crate::state::mapping::MappingId;
use crate::state::ops::{self, Operation};
use crate::state::unix_now;
use crate::watch::WatchEvent;

/// Native Google Docs / Sheets / Slides mime prefix. These do not have an md5
/// and cannot be synced as opaque bytes — skipped silently per FR-011 (the
/// poller still observes them; the reconciler is the gate).
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
            if !p.is_file() {
                // Directory create — nothing to do; folders are materialised
                // implicitly when their first file syncs.
                return Ok(());
            }
            let rel = strip_root(&p, local_root)?;
            // EACCES (FR-021): a permission denied is logged + retried on the
            // next safety-net cycle. We surface as a warn here so the daemon
            // doesn't crash; the file simply doesn't get queued this tick.
            let (size, md5) = match fingerprint::compute_local(&p).await {
                Ok(v) => v,
                Err(Error::Io(io)) if io.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!(path = %p.display(), "EACCES on local read; will retry");
                    return Ok(());
                }
                Err(e) => return Err(e),
            };

            match items::get_by_relative_path(db.connection(), mapping_id, &rel).await? {
                Some(item) => {
                    // Echo suppression: if our fingerprint matches what's already
                    // recorded, the modify event was either a no-op or the echo
                    // of a download we just performed.
                    if item.md5.as_deref() == Some(&md5) && item.size == Some(size) {
                        return Ok(());
                    }
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
                            size: Some(size),
                            md5: Some(md5),
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
/// Google Docs (FR-011) and our own echoes (md5 match).
pub async fn apply_remote(
    change: RemoteChange,
    db: &Db,
    mapping_id: MappingId,
    _local_root: &Path,
) -> Result<()> {
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
        // Folder creation on the remote side: we don't pre-create the local
        // counterpart; it materialises when a file inside it is downloaded.
        return Ok(());
    }
    if file.mime_type.starts_with(NATIVE_GAPPS_PREFIX) {
        tracing::info!(
            id = %file.id,
            name = %file.name,
            mime = %file.mime_type,
            "skipping native Google Docs file (FR-011)"
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
            // Brand-new remote file. Insert sync_item + enqueue Download.
            // Relative path heuristic for the MVP: just the file's name. A
            // deeper hierarchy needs parent walking (Phase 4 follow-up).
            let new_id = items::insert(
                db.connection(),
                &NewSyncItem {
                    mapping_id,
                    relative_path: file.name.clone(),
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
                "relative_path": file.name,
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

#[allow(dead_code)] // future use by conflict path
fn parent_dir(rel: &str) -> &str {
    match rel.rsplit_once('/') {
        Some((p, _)) => p,
        None => "",
    }
}

#[allow(dead_code)]
fn _unused(_p: PathBuf) {} // placeholder kept while continuous reconciler grows
