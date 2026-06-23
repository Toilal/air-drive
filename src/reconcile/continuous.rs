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

use crate::config::SymlinkPolicy;
use crate::daemon::in_flight::InFlightOps;
use crate::drive::changes::RemoteChange;
use crate::error::{Error, Result};
use crate::reconcile::shortcut;
use crate::state::Db;
use crate::state::items::{self, ItemId, ItemKind, ItemState, NewSyncItem};
use crate::state::mapping::MappingId;
use crate::state::ops::{self, Operation};
use crate::state::unix_now;
use crate::watch::{LocalKind, WatchEvent, classify_local};

/// Convert a `WatchEvent` into `pending_operation` rows. `symlinks` decides how
/// symlinks encountered while rescanning a new directory are treated.
pub async fn apply_local(
    event: WatchEvent,
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    symlinks: SymlinkPolicy,
) -> Result<()> {
    match event {
        WatchEvent::Created(p) | WatchEvent::Modified(p) => {
            if p.is_dir() {
                let rel = strip_root(&p, local_root)?;
                enqueue_local_dir(db, mapping_id, &rel).await?;
                // inotify new-dir race: a file created inside a brand-new
                // directory can land before `notify` registers the recursive
                // watch on it, so the file's own Created event is never
                // delivered. Walk the new dir and enqueue everything already
                // inside it (parent-first) so nothing is silently dropped.
                for (child_rel, is_dir) in walk_subtree(&p, local_root, symlinks)? {
                    if is_dir {
                        enqueue_local_dir(db, mapping_id, &child_rel).await?;
                    } else {
                        enqueue_local_file(db, mapping_id, &child_rel).await?;
                    }
                }
                return Ok(());
            }
            if !p.is_file() {
                // Neither a regular file nor a directory — it vanished between the
                // event and now, or it's a special file. Nothing to do.
                return Ok(());
            }
            let rel = strip_root(&p, local_root)?;
            enqueue_local_file(db, mapping_id, &rel).await?;
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
                Some(item) if item.remote_id.is_none() => {
                    // Renamed before its first upload landed: there is nothing on
                    // Drive to move, and a `RenameRemote` would delete the row and
                    // orphan the still-pending `Upload` (which then fails with
                    // "sync_item vanished"). Just repath the row and (re)queue the
                    // upload so the file lands at its new path.
                    items::set_relative_path(db.connection(), item.id, &to_rel).await?;
                    ops::enqueue(
                        db.connection(),
                        item.id,
                        Operation::Upload,
                        None,
                        unix_now(),
                    )
                    .await?;
                }
                Some(item) => {
                    let payload = ops::encode_payload(&ops::RenamePayload {
                        new_relative_path: to_rel.to_string(),
                    });
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
                        symlinks,
                    ))
                    .await;
                }
            }
        }
    }
    Ok(())
}

/// Persist a local directory as a `kind='dir'` item and enqueue its remote
/// creation. Idempotent: a directory we already track is left untouched.
async fn enqueue_local_dir(db: &Db, mapping_id: MappingId, rel: &str) -> Result<()> {
    if items::get_by_relative_path(db.connection(), mapping_id, rel)
        .await?
        .is_none()
    {
        let new_id = items::insert(
            db.connection(),
            &NewSyncItem {
                mapping_id,
                relative_path: rel.to_owned(),
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
    Ok(())
}

/// Enqueue an upload for a local file. A shortcut pointer (native Google Doc) is
/// never uploaded back; a tracked file re-uploads on its existing row; an
/// unknown file is recorded `pending_local` first. No fingerprinting here — the
/// dispatcher hashes once just before uploading and skips a no-op.
async fn enqueue_local_file(db: &Db, mapping_id: MappingId, rel: &str) -> Result<()> {
    match items::get_by_relative_path(db.connection(), mapping_id, rel).await? {
        Some(item) if item.state == ItemState::Skipped => {}
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
                    relative_path: rel.to_owned(),
                    kind: ItemKind::File,
                    remote_id: None,
                    size: None,
                    md5: None,
                    local_inode: None,
                    last_synced_at: 0,
                    state: ItemState::PendingLocal,
                },
            )
            .await?;
            ops::enqueue(db.connection(), new_id, Operation::Upload, None, unix_now()).await?;
        }
    }
    Ok(())
}

/// Walk every descendant of `dir` (a path under `local_root`), returning
/// `(relative_path, is_dir)` sorted parent-first so a directory is always
/// enqueued before its children. Skips the staging directory; symlinks are
/// handled per `symlinks` ([`classify_local`]), mirroring the watcher.
fn walk_subtree(
    dir: &Path,
    local_root: &Path,
    symlinks: SymlinkPolicy,
) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    collect_subtree(dir, local_root, symlinks, &mut visited, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn collect_subtree(
    dir: &Path,
    local_root: &Path,
    symlinks: SymlinkPolicy,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
    out: &mut Vec<(String, bool)>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(Error::Io)?;
        if entry.file_name().to_str() == Some(crate::engine::staging::PARTIAL_DIR) {
            continue;
        }
        let path = entry.path();
        let rel = strip_root(&path, local_root)?;
        match classify_local(&path, local_root, symlinks) {
            Some(LocalKind::Dir) => {
                out.push((rel, true));
                // Loop guard: when following symlinks, descend into each
                // canonical dir at most once so a link cycle can't recurse
                // forever. `Skip` never follows a link, so it descends freely.
                let descend = match symlinks {
                    SymlinkPolicy::Follow => match std::fs::canonicalize(&path) {
                        Ok(canon) => visited.insert(canon),
                        Err(_) => false,
                    },
                    SymlinkPolicy::Skip => true,
                };
                if descend {
                    collect_subtree(&path, local_root, symlinks, visited, out)?;
                }
            }
            Some(LocalKind::File) => out.push((rel, false)),
            None => {}
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
            let payload = ops::encode_payload(&ops::DeleteLocalPayload { tombstone: false });
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
                let payload = ops::encode_payload(&ops::DeleteLocalPayload { tombstone });
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
            None if items::get_by_relative_path(db.connection(), mapping_id, &rel)
                .await?
                .is_some() =>
            {
                // We already track something at this path but not under this
                // remote id: it's the echo of a folder WE just created locally,
                // whose `CreateDirRemote` hasn't linked the remote id yet. Re-
                // creating it locally would churn / risk a duplicate (#21). The
                // pending op owns the remote-id link; suppress the echo.
                tracing::debug!(
                    relative_path = %rel,
                    "suppressing echo of locally-created folder (remote id not yet linked)"
                );
            }
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
                let payload = ops::encode_payload(&ops::RenamePayload {
                    new_relative_path: rel.to_string(),
                });
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
                let payload = ops::encode_payload(&ops::RenamePayload {
                    new_relative_path: rel.to_string(),
                });
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
                let payload = ops::encode_payload(&ops::DownloadPayload {
                    remote_id: file.id.clone(),
                    size: remote_size,
                    md5: remote_md5.clone(),
                    relative_path: item.relative_path.to_string(),
                });
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
            // Rename / move: the file's resolved path changed. Propagate the move
            // locally (mirrors the folder + gdoc rename branches) instead of
            // letting echo suppression swallow a pure rename (same md5). After
            // our OWN local rename, `RenameRemote` has already updated this row's
            // path, so the echo resolves to the same path and skips this branch.
            if let Some(new_rel) = change.relative_path.as_deref()
                && new_rel != item.relative_path
            {
                // Distinguish a parent-folder rename from a genuine file move.
                // When a folder is renamed in place on Drive its children keep
                // the same parent id but their resolved path changes, and Drive
                // may deliver only the child's change (or deliver it before the
                // folder's). Moving the child on its own would strand the emptied
                // old directory and make the local watcher re-upload it as a brand
                // new folder (#19). So if the file's current parent id still
                // matches the directory we track at its OLD parent path, the
                // *directory* was renamed: enqueue the subtree rename for that
                // directory instead. It is idempotent across siblings and carries
                // every child, so the old directory is removed rather than left
                // behind. A genuine move (parent id differs) falls through to the
                // per-file rename below.
                let old_parent = parent_dir(&item.relative_path);
                let new_parent = parent_dir(new_rel);
                if old_parent != new_parent
                    && !old_parent.is_empty()
                    && let Some(parent_id) = file.parents.first()
                    && let Some(dir) =
                        items::get_by_relative_path(db.connection(), mapping_id, old_parent).await?
                    && dir.remote_id.as_deref() == Some(parent_id.as_str())
                {
                    let payload = ops::encode_payload(&ops::RenamePayload {
                        new_relative_path: new_parent.to_string(),
                    });
                    ops::enqueue(
                        db.connection(),
                        dir.id,
                        Operation::RenameLocal,
                        Some(&payload),
                        unix_now(),
                    )
                    .await?;
                    return Ok(());
                }
                let payload = ops::encode_payload(&ops::RenamePayload {
                    new_relative_path: new_rel.to_string(),
                });
                ops::enqueue(
                    db.connection(),
                    item.id,
                    Operation::RenameLocal,
                    Some(&payload),
                    unix_now(),
                )
                .await?;
                // Rename + content edit: also pull the new bytes to the new path
                // (RenameLocal updates the row's path first, so the Download lands
                // there). A pure rename (unchanged md5) skips the Download.
                if item.md5.as_deref() != Some(remote_md5.as_str())
                    || item.size != Some(remote_size)
                {
                    let dl = ops::encode_payload(&ops::DownloadPayload {
                        remote_id: file.id.clone(),
                        size: remote_size,
                        md5: remote_md5.clone(),
                        relative_path: new_rel.to_string(),
                    });
                    ops::enqueue(
                        db.connection(),
                        item.id,
                        Operation::Download,
                        Some(&dl),
                        unix_now(),
                    )
                    .await?;
                }
                return Ok(());
            }
            // Echo suppression: same md5 means this is a notification of our
            // own upload — nothing to do.
            if item.md5.as_deref() == Some(remote_md5.as_str()) && item.size == Some(remote_size) {
                return Ok(());
            }
            // Conflict detection, by comparing the local file's CURRENT md5 to
            // both the last-synced fingerprint and the remote md5:
            //   - local == remote → the on-disk content already matches the
            //     remote. This is a re-delivery of a change we already applied
            //     (the change feed can hand us the same entry again before the
            //     Download's dispatcher has persisted the new fingerprint to
            //     `sync_item`). Not a conflict and nothing to pull — skip, so we
            //     don't open a spurious conflict against bytes that already agree.
            //   - local != last_synced → both sides drifted independently →
            //     open a conflict (rename the local copy, record it), then fall
            //     through to the Download so the remote keeps the canonical name
            //     (Q2: remote keeps canonical).
            //   - otherwise → local untouched, a pure remote update → Download.
            let canonical_local = local_root.join(&item.relative_path);
            if canonical_local.is_file()
                && let Some(last_synced_md5) = item.md5.as_deref()
            {
                match crate::reconcile::fingerprint::compute_local(&canonical_local).await {
                    Ok((_, local_md5)) if local_md5 == remote_md5 => return Ok(()),
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
            let payload = ops::encode_payload(&ops::DownloadPayload {
                remote_id: file.id.clone(),
                size: remote_size,
                md5: remote_md5.clone(),
                relative_path: item.relative_path.to_string(),
            });
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
            // Echo of our own local create: a file we already track at this path
            // but not yet under this remote id (its `Upload` hasn't linked it).
            // Re-downloading it would duplicate/churn the freshly-uploaded file
            // (#21), and inserting a second row at `rel` would hit the unique
            // index. Suppress; the pending upload op owns the remote-id link.
            if items::get_by_relative_path(db.connection(), mapping_id, &rel)
                .await?
                .is_some()
            {
                tracing::debug!(
                    relative_path = %rel,
                    "suppressing echo of locally-created file (remote id not yet linked)"
                );
                return Ok(());
            }
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
            let payload = ops::encode_payload(&ops::DownloadPayload {
                remote_id: file.id.clone(),
                size: remote_size,
                md5: remote_md5.clone(),
                relative_path: rel.to_string(),
            });
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
    let payload = ops::encode_payload(&ops::ShortcutPayload {
        content: shortcut::content(mime, id),
    });
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

/// The parent-directory portion of a `/`-separated relative path, or `""` for a
/// root-level entry (`"a/b/c.txt"` → `"a/b"`, `"c.txt"` → `""`).
fn parent_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
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
        apply_local(
            WatchEvent::Created(path),
            &db,
            mapping_id,
            root,
            SymlinkPolicy::Skip,
        )
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

    #[tokio::test]
    async fn apply_remote_suppresses_echo_of_locally_created_file() {
        let (_tmp, db, mapping_id) = setup().await;
        let in_flight = InFlightOps::new();

        // A local create whose upload hasn't linked the remote id yet: a
        // `pending_local` row at `newdir/note.txt` with no remote_id.
        items::insert(
            db.connection(),
            &NewSyncItem {
                mapping_id,
                relative_path: "newdir/note.txt".into(),
                kind: ItemKind::File,
                remote_id: None,
                size: None,
                md5: None,
                local_inode: None,
                last_synced_at: 0,
                state: ItemState::PendingLocal,
            },
        )
        .await
        .unwrap();

        // The change feed reports that file under a FRESH remote id (the echo of
        // our own upload). It must NOT enqueue a Download or insert a second row
        // (#21) — the pending upload op owns linking the remote id.
        let change = RemoteChange {
            file_id: "remote-new".into(),
            removed: false,
            file: Some(FileSnapshot {
                id: "remote-new".into(),
                name: "note.txt".into(),
                mime_type: "text/plain".into(),
                size: Some(21),
                md5: Some("babb5939d214eedee9136da7913ee59e".into()),
                parents: vec!["dir-id".into()],
                trashed: false,
            }),
            relative_path: Some("newdir/note.txt".into()),
        };
        apply_remote(
            change,
            &db,
            mapping_id,
            Path::new("/home/alice"),
            &in_flight,
        )
        .await
        .unwrap();

        assert!(
            ops::next_due(db.connection(), unix_now() + 1)
                .await
                .unwrap()
                .is_none(),
            "echo of a locally-created file must not enqueue any op"
        );
        let item = items::get_by_relative_path(db.connection(), mapping_id, "newdir/note.txt")
            .await
            .unwrap()
            .expect("the original local item is still tracked");
        assert!(
            item.remote_id.is_none(),
            "the pending upload op owns the remote-id link, not the echo path"
        );
    }
}
