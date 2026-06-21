//! Continuous reconciliation.
//!
//! Two entry points consumed by the daemon's main loop:
//!
//! - [`apply_local`] — `WatchEvent` → SQL inserts into `pending_operation`.
//! - [`apply_remote`] — `RemoteChange` → same, on the Drive side.
//!
//! Both consult `sync_item` to suppress echoes (a change we caused via our own
//! upload) and to represent native Google Docs as local shortcut files
//! ([`crate::reconcile::shortcut`]).
//!
//! The functions are stateless beyond the database — they don't talk to the
//! engine; the dispatcher (`daemon::runtime`) does.

use std::path::Path;

use serde_json::json;

use crate::daemon::in_flight::InFlightOps;
use crate::drive::changes::RemoteChange;
use crate::error::{Error, Result};
use crate::reconcile::shortcut;
use crate::state::Db;
use crate::state::items::{self, ItemId, ItemKind, ItemState, NewSyncItem};
use crate::state::mapping::MappingId;
use crate::state::ops::{self, Operation};
use crate::state::unix_now;
use crate::watch::WatchEvent;

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
                Some(item) if item.state == ItemState::Skipped => {
                    // A shortcut file the daemon wrote for a native Google Doc
                    // (issue #3). It lives only on disk as a pointer — never upload
                    // it back to Drive, and ignore the watcher echo of our own write.
                    return Ok(());
                }
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
                if item.trashed_at.is_some() {
                    // The row is a tombstone: this Deleted event is the echo of our
                    // own local removal after a remote trash. Keep the tombstone
                    // (it anchors a future restore) and do NOT propagate a delete
                    // back to Drive — that would be a feedback loop (#8).
                    return Ok(());
                }
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
        // Permanent delete / loss of access (NOT a trash). The file is gone for
        // good, so remove the local copy and drop the row — no tombstone, since
        // there's nothing to restore.
        if let Some(item) = items::get_by_remote_id(db.connection(), &change.file_id).await? {
            let payload = json!({ "tombstone": false }).to_string();
            ops::enqueue(
                db.connection(),
                item.id,
                Operation::DeleteLocal,
                Some(&payload),
                unix_now(),
            )
            .await?;
        }
        return Ok(());
    }

    let Some(file) = change.file else {
        return Ok(());
    };

    if file.trashed {
        // A trash surfaces as a normal change with the file still present. Treat
        // it as a removal of the local copy. For a file we keep the row as a
        // tombstone so a later restore (an untrash → non-trashed change) re-links
        // to it; a directory is dropped outright (it just re-creates on restore).
        if let Some(item) = items::get_by_remote_id(db.connection(), &change.file_id).await? {
            if item.trashed_at.is_none() {
                let tombstone = matches!(item.kind, ItemKind::File);
                let payload = json!({ "tombstone": tombstone }).to_string();
                ops::enqueue(
                    db.connection(),
                    item.id,
                    Operation::DeleteLocal,
                    Some(&payload),
                    unix_now(),
                )
                .await?;
            }
            // else: already tombstoned — echo of our own trash handling.
        }
        return Ok(());
    }

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
    if shortcut::is_native(&file.mime_type) {
        // Native Google Doc/Sheet/Slide: no md5, no opaque byte stream. Instead of
        // skipping silently we materialise a local shortcut file (issue #3). Its
        // path is the doc's path plus a per-type extension (`Notes` → `Notes.gdoc`),
        // and its `sync_item` is marked `skipped` so the local watcher never tries
        // to upload the pointer and `status` can surface it.
        let base = change
            .relative_path
            .clone()
            .unwrap_or_else(|| file.name.clone());
        let rel = shortcut::relative_path(&base, &file.mime_type);
        match items::get_by_remote_id(db.connection(), &change.file_id).await? {
            None => {
                let new_id = items::insert(
                    db.connection(),
                    &NewSyncItem {
                        mapping_id,
                        relative_path: rel,
                        kind: ItemKind::File,
                        remote_id: Some(file.id.clone()),
                        size: None,
                        md5: None,
                        local_inode: None,
                        last_synced_at: 0,
                        state: ItemState::Skipped,
                    },
                )
                .await?;
                enqueue_write_shortcut(db, new_id, &file.mime_type, &file.id).await?;
            }
            Some(existing) if existing.relative_path != rel => {
                // The doc was renamed on Drive → rename the local shortcut to match.
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
            Some(existing) if existing.trashed_at.is_some() => {
                // Restored from trash → clear the tombstone and re-write the pointer.
                items::clear_trashed(db.connection(), existing.id).await?;
                enqueue_write_shortcut(db, existing.id, &file.mime_type, &file.id).await?;
            }
            Some(_) => { /* unchanged (e.g. a content edit) — the pointer is stable */ }
        }
        return Ok(());
    }
    let Some(remote_md5) = file.md5.clone() else {
        return Ok(());
    };
    let remote_size = file.size.unwrap_or(0);

    match items::get_by_remote_id(db.connection(), &change.file_id).await? {
        Some(item) => {
            // Restore: the row is a tombstone (file was trashed on Drive, local
            // copy removed) and the file is back. Clear the tombstone and
            // re-download to the ORIGINAL path — re-using this row, so no
            // duplicate is created (#8). Checked before echo suppression because
            // a restore usually carries the same md5 the row still remembers.
            if item.trashed_at.is_some() {
                items::clear_trashed(db.connection(), item.id).await?;
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
                return Ok(());
            }
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

/// Enqueue a [`Operation::WriteShortcut`] for a native Google Doc, rendering the
/// pointer body into the op payload so the dispatcher only has to write bytes.
async fn enqueue_write_shortcut(db: &Db, item_id: ItemId, mime: &str, id: &str) -> Result<()> {
    let payload = json!({ "content": shortcut::content(mime, id) }).to_string();
    ops::enqueue(
        db.connection(),
        item_id,
        Operation::WriteShortcut,
        Some(&payload),
        unix_now(),
    )
    .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::changes::FileSnapshot;
    use crate::state::{accounts, mapping};

    const DOC_MIME: &str = "application/vnd.google-apps.document";

    async fn setup() -> (tempfile::TempDir, Db, MappingId) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        let account_id = accounts::upsert(db.connection(), "alice@gmail.com", 1)
            .await
            .unwrap();
        let mapping_id = mapping::upsert(
            db.connection(),
            account_id,
            "/home/alice",
            "root-id",
            None,
            None,
            1,
        )
        .await
        .unwrap();
        (tmp, db, mapping_id)
    }

    fn native_change(id: &str, name: &str, rel: &str) -> RemoteChange {
        RemoteChange {
            file_id: id.to_owned(),
            removed: false,
            file: Some(FileSnapshot {
                id: id.to_owned(),
                name: name.to_owned(),
                mime_type: DOC_MIME.to_owned(),
                size: None,
                md5: None,
                parents: vec!["root-id".to_owned()],
                trashed: false,
            }),
            relative_path: Some(rel.to_owned()),
        }
    }

    #[tokio::test]
    async fn native_doc_creates_skipped_item_and_write_shortcut_op() {
        let (_tmp, db, mapping_id) = setup().await;
        let in_flight = InFlightOps::new();
        apply_remote(
            native_change("doc1", "Notes", "Notes"),
            &db,
            mapping_id,
            Path::new("/home/alice"),
            &in_flight,
        )
        .await
        .unwrap();

        // A skipped, md5-less shortcut item is tracked at the doc path + `.gdoc`.
        let item = items::get_by_relative_path(db.connection(), mapping_id, "Notes.gdoc")
            .await
            .unwrap()
            .expect("shortcut item should exist");
        assert_eq!(item.state, ItemState::Skipped);
        assert_eq!(item.remote_id.as_deref(), Some("doc1"));
        assert_eq!(item.md5, None);

        // A WriteShortcut op carrying a valid JSON pointer body is queued.
        let op = ops::next_due(db.connection(), unix_now() + 1)
            .await
            .unwrap()
            .expect("a write_shortcut op should be queued");
        assert_eq!(op.op, Operation::WriteShortcut);
        let payload: serde_json::Value =
            serde_json::from_str(op.payload.as_deref().unwrap()).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(payload["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            body["url"].as_str().unwrap(),
            "https://docs.google.com/document/d/doc1/edit"
        );
    }

    #[tokio::test]
    async fn native_doc_rename_enqueues_rename_local() {
        let (_tmp, db, mapping_id) = setup().await;
        let in_flight = InFlightOps::new();
        let root = Path::new("/home/alice");
        apply_remote(
            native_change("doc1", "Notes", "Notes"),
            &db,
            mapping_id,
            root,
            &in_flight,
        )
        .await
        .unwrap();
        // Drain the initial WriteShortcut op so the rename op is the next due one.
        let first = ops::next_due(db.connection(), unix_now() + 1)
            .await
            .unwrap()
            .unwrap();
        ops::delete(db.connection(), first.id).await.unwrap();

        // Same doc, renamed on Drive.
        apply_remote(
            native_change("doc1", "Renamed", "Renamed"),
            &db,
            mapping_id,
            root,
            &in_flight,
        )
        .await
        .unwrap();

        let op = ops::next_due(db.connection(), unix_now() + 1)
            .await
            .unwrap()
            .expect("a rename_local op should be queued");
        assert_eq!(op.op, Operation::RenameLocal);
        let payload: serde_json::Value =
            serde_json::from_str(op.payload.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload["new_relative_path"].as_str().unwrap(),
            "Renamed.gdoc"
        );
    }

    #[tokio::test]
    async fn apply_local_does_not_upload_shortcut_files() {
        let (tmp, db, mapping_id) = setup().await;
        let root = tmp.path();
        // A shortcut pointer the daemon already wrote: a `skipped` row + the file.
        items::insert(
            db.connection(),
            &NewSyncItem {
                mapping_id,
                relative_path: "Notes.gdoc".into(),
                kind: ItemKind::File,
                remote_id: Some("doc1".into()),
                size: None,
                md5: None,
                local_inode: None,
                last_synced_at: 0,
                state: ItemState::Skipped,
            },
        )
        .await
        .unwrap();
        let path = root.join("Notes.gdoc");
        tokio::fs::write(&path, "{}\n").await.unwrap();

        // The watcher echo of our own write must NOT enqueue an upload.
        apply_local(WatchEvent::Created(path), &db, mapping_id, root)
            .await
            .unwrap();
        assert!(
            ops::next_due(db.connection(), unix_now() + 1)
                .await
                .unwrap()
                .is_none(),
            "no upload should be queued for a shortcut file"
        );
    }
}
