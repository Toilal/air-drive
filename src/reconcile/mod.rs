//! Reconciler: turn watcher + remote-change events into atomic `SyncEngine` operations.
//!
//! For the MVP only the **initial** reconciliation pass lives here. It walks
//! the local tree and the remote tree once, reconciles directories (so empty
//! folders propagate and every dir is persisted as a `kind='dir'` `sync_item`),
//! then classifies every leaf as `local-only`, `remote-only`, or `both`, and
//! dispatches `upload` / `download` to the configured [`SyncEngine`] until the
//! queue drains. After convergence it captures a Drive
//! `changes.getStartPageToken` baseline so the continuous-sync loop only sees
//! events that happened *after* the initial pass.
//!
//! Conflict handling on `both` files is intentionally minimal at this stage: if the
//! md5 matches we just record the pair in `sync_item`; if it doesn't, we log and
//! defer the divergence to the continuous-sync conflict path. The integration suite
//! covers the md5-match shortcut.

pub mod conflict;
pub mod continuous;
pub mod fingerprint;
pub mod shortcut;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use crate::drive::http::DriveHttp;
use crate::drive::metadata;
use crate::engine::SyncEngine;
use crate::error::{Error, Result};
use crate::state::Db;
use crate::state::cursor;
use crate::state::items::{self, ItemKind, ItemState, NewSyncItem};
use crate::state::mapping::MappingId;
use crate::state::unix_now;

/// One file under the local watched root, as discovered by [`walk_local`].
#[derive(Debug, Clone)]
struct LocalEntry {
    /// Path relative to the watched root, using `/` as separator.
    relative_path: String,
    /// `true` for directories.
    is_dir: bool,
}

/// One file under the remote root, as discovered by [`walk_remote`].
#[derive(Debug, Clone)]
struct RemoteEntry {
    /// Path relative to the remote root, using `/` as separator.
    relative_path: String,
    /// Drive file ID.
    id: String,
    /// `true` for folders.
    is_dir: bool,
    /// Size in bytes (0 for folders).
    size: i64,
    /// Drive `md5Checksum`; `None` for folders and for native Google Docs.
    md5: Option<String>,
    /// Drive MIME type. Used to tell native Google Docs (`vnd.google-apps.*`) apart
    /// from regular files that simply lack an md5.
    mime_type: String,
}

/// Run the initial reconciliation. Caller passes the engine wrapped in `Arc<dyn …>`
/// so swapping rclone for the HTTP engine at the binary level is a no-op for the
/// reconciler.
pub async fn initial(
    http: &DriveHttp,
    engine: Arc<dyn SyncEngine>,
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    remote_root_id: &str,
) -> Result<()> {
    let local_entries = walk_local(local_root)?;
    let remote_entries = walk_remote(http, remote_root_id).await?;

    let local_files: Vec<&LocalEntry> = local_entries.iter().filter(|e| !e.is_dir).collect();
    let remote_files: Vec<&RemoteEntry> = remote_entries.iter().filter(|e| !e.is_dir).collect();

    let remote_by_path: HashMap<&str, &RemoteEntry> = remote_files
        .iter()
        .map(|r| (r.relative_path.as_str(), *r))
        .collect();
    let local_paths: HashSet<&str> = local_files
        .iter()
        .map(|e| e.relative_path.as_str())
        .collect();

    // Cache of remote folder IDs, keyed by relative path. Seeded with "" (the root).
    let mut remote_folder_ids: HashMap<String, String> = HashMap::new();
    remote_folder_ids.insert(String::new(), remote_root_id.to_owned());
    for r in remote_entries.iter().filter(|e| e.is_dir) {
        remote_folder_ids.insert(r.relative_path.clone(), r.id.clone());
    }

    // 0. Reconcile directories first. The union of local + remote dirs is walked
    //    parent-first (a parent path is a strict prefix of its children, so it
    //    sorts before them). Each dir is created on whichever side lacks it and
    //    persisted as a kind='dir' sync_item — this is how *empty* folders
    //    propagate, and it gives folder rename/move (#7) a row to anchor to.
    let mut dir_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for e in local_entries.iter().filter(|e| e.is_dir) {
        dir_paths.insert(e.relative_path.clone());
    }
    for e in remote_entries.iter().filter(|e| e.is_dir) {
        dir_paths.insert(e.relative_path.clone());
    }
    for dir in &dir_paths {
        let (parent_rel, name) = split_parent(dir);
        let remote_id = match remote_folder_ids.get(dir) {
            Some(id) => id.clone(),
            None => {
                // Local-only dir: create it on Drive under its (already-processed)
                // parent, then cache the new id for any children.
                let parent_id = remote_folder_ids
                    .get(parent_rel)
                    .cloned()
                    .unwrap_or_else(|| remote_root_id.to_owned());
                let created = engine.create_dir_remote(&parent_id, name).await?;
                remote_folder_ids.insert(dir.clone(), created.id.clone());
                created.id
            }
        };
        // Remote-only dir: materialise it locally (idempotent if already present).
        tokio::fs::create_dir_all(local_root.join(dir)).await?;
        record_synced_dir(db, mapping_id, dir, &remote_id).await?;
    }

    // 1. Local-only files → upload (creating remote parent folders as needed).
    //    Files matching by md5 (in_both with equal fingerprint) are recorded but NOT
    //    re-uploaded.
    for local in &local_files {
        // A shortcut the daemon wrote for a native Google Doc (issue #3) lives on
        // disk as a pointer file with a `skipped` row — never upload it to Drive.
        if let Some(it) =
            items::get_by_relative_path(db.connection(), mapping_id, &local.relative_path).await?
        {
            if it.state == ItemState::Skipped {
                continue;
            }
        }
        let local_path = local_root.join(&local.relative_path);
        let (size, md5) = fingerprint::compute_local(&local_path).await?;
        let (parent_rel, file_name) = split_parent(&local.relative_path);
        match remote_by_path.get(local.relative_path.as_str()) {
            Some(remote) => {
                if remote.md5.as_deref() == Some(&md5) && remote.size == size {
                    record_synced_item(
                        db,
                        mapping_id,
                        &local.relative_path,
                        Some(remote.id.clone()),
                        size,
                        md5,
                    )
                    .await?;
                } else {
                    tracing::warn!(
                        relative_path = %local.relative_path,
                        local_md5 = %md5,
                        remote_md5 = ?remote.md5,
                        "fingerprint mismatch on both-sides file — deferring to continuous-sync conflict handler"
                    );
                }
            }
            None => {
                let parent_id =
                    ensure_remote_folder(http, &mut remote_folder_ids, remote_root_id, parent_rel)
                        .await?;
                let uploaded = engine.upload(&local_path, &parent_id, file_name).await?;
                record_synced_item(
                    db,
                    mapping_id,
                    &local.relative_path,
                    Some(uploaded.id),
                    size,
                    md5,
                )
                .await?;
            }
        }
    }

    // 2. Remote-only files → download.
    for remote in &remote_files {
        if local_paths.contains(remote.relative_path.as_str()) {
            continue;
        }
        let Some(md5) = remote.md5.clone() else {
            if shortcut::is_native(&remote.mime_type) {
                // Native Google Doc → write a local shortcut pointer instead of
                // downloading bytes that don't exist (issue #3). Idempotent: a
                // pre-existing row (e.g. a re-run) is left untouched.
                let rel = shortcut::relative_path(&remote.relative_path, &remote.mime_type);
                if items::get_by_relative_path(db.connection(), mapping_id, &rel)
                    .await?
                    .is_none()
                {
                    let body = shortcut::content(&remote.mime_type, &remote.id);
                    shortcut::write(&local_root.join(&rel), &body).await?;
                    record_skipped_shortcut(db, mapping_id, &rel, &remote.id).await?;
                }
            } else {
                tracing::info!(
                    relative_path = %remote.relative_path,
                    "skipping remote file with no md5"
                );
            }
            continue;
        };
        let local_path = local_root.join(&remote.relative_path);
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        engine.download(&remote.id, &local_path, local_root).await?;
        record_synced_item(
            db,
            mapping_id,
            &remote.relative_path,
            Some(remote.id.clone()),
            remote.size,
            md5,
        )
        .await?;
    }

    // 3. Persist the changes-cursor baseline AFTER convergence so the continuous loop
    //    doesn't replay events the initial pass already covered.
    let cursor_body = http.get_json("changes/startPageToken", &[]).await?;
    let token = cursor_body
        .get("startPageToken")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Drive("missing startPageToken in response".into()))?
        .to_owned();
    cursor::set(db.connection(), mapping_id, &token, unix_now()).await?;

    Ok(())
}

/// Walk the local tree, returning every file and directory beneath `root` (excluding
/// the root itself and any `.air-drive-partial/` staging artefacts).
fn walk_local(root: &Path) -> Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    collect_local(root, root, &mut out)?;
    out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(out)
}

fn collect_local(root: &Path, dir: &Path, out: &mut Vec<LocalEntry>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == crate::engine::staging::PARTIAL_DIR {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|e| Error::Mapping(format!("strip_prefix: {e}")))?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            out.push(LocalEntry {
                relative_path: rel.clone(),
                is_dir: true,
            });
            collect_local(root, &path, out)?;
        } else if metadata.is_file() {
            out.push(LocalEntry {
                relative_path: rel,
                is_dir: false,
            });
        }
        // Symlinks and special files: skipped silently.
    }
    Ok(())
}

/// Walk the remote tree under `root_id` via `list_children` calls in BFS order.
///
/// `list_children` now requests `size`+`md5Checksum` alongside id/name/mime in a
/// single `files.list` call, so each leaf needs zero follow-up requests. The walk
/// costs exactly one `files.list` per directory.
async fn walk_remote(http: &DriveHttp, root_id: &str) -> Result<Vec<RemoteEntry>> {
    let mut out = Vec::new();
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    queue.push_back((root_id.to_owned(), String::new()));
    while let Some((parent_id, parent_path)) = queue.pop_front() {
        let children = metadata::list_children(http, &parent_id).await?;
        for c in children {
            let rel = if parent_path.is_empty() {
                c.name.clone()
            } else {
                format!("{parent_path}/{}", c.name)
            };
            if c.is_folder() {
                out.push(RemoteEntry {
                    relative_path: rel.clone(),
                    id: c.id.clone(),
                    is_dir: true,
                    size: 0,
                    md5: None,
                    mime_type: c.mime_type,
                });
                queue.push_back((c.id, rel));
            } else {
                out.push(RemoteEntry {
                    relative_path: rel,
                    id: c.id,
                    is_dir: false,
                    size: c.size.unwrap_or(0),
                    md5: c.md5,
                    mime_type: c.mime_type,
                });
            }
        }
    }
    out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(out)
}

/// Walk `parent_rel` segment by segment, creating Drive folders as needed. Returns
/// the Drive folder ID corresponding to `parent_rel`. Caches results so a deep tree
/// does at most O(depth) folder creations.
async fn ensure_remote_folder(
    http: &DriveHttp,
    folder_ids_by_path: &mut HashMap<String, String>,
    root_id: &str,
    rel: &str,
) -> Result<String> {
    if rel.is_empty() {
        return Ok(root_id.to_owned());
    }
    if let Some(id) = folder_ids_by_path.get(rel) {
        return Ok(id.clone());
    }
    let segments: Vec<&str> = rel.split('/').collect();
    let mut current_parent_id = root_id.to_owned();
    let mut current_path = String::new();
    for seg in segments {
        if !current_path.is_empty() {
            current_path.push('/');
        }
        current_path.push_str(seg);
        if let Some(id) = folder_ids_by_path.get(&current_path) {
            current_parent_id = id.clone();
            continue;
        }
        let existing = metadata::list_children(http, &current_parent_id)
            .await?
            .into_iter()
            .find(|c| c.is_folder() && c.name == seg);
        let id = match existing {
            Some(c) => c.id,
            None => {
                let created = metadata::create_folder(http, &current_parent_id, seg).await?;
                created.id
            }
        };
        folder_ids_by_path.insert(current_path.clone(), id.clone());
        current_parent_id = id;
    }
    Ok(current_parent_id)
}

/// Split a relative path into `(parent_dir, file_name)`. `"a/b/c.txt"` → `("a/b", "c.txt")`,
/// `"top.txt"` → `("", "top.txt")`.
fn split_parent(rel: &str) -> (&str, &str) {
    match rel.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", rel),
    }
}

async fn record_synced_dir(
    db: &Db,
    mapping_id: MappingId,
    relative_path: &str,
    remote_id: &str,
) -> Result<()> {
    items::insert(
        db.connection(),
        &NewSyncItem {
            mapping_id,
            relative_path: relative_path.to_owned(),
            kind: ItemKind::Dir,
            remote_id: Some(remote_id.to_owned()),
            size: None,
            md5: None,
            local_inode: None,
            last_synced_at: unix_now(),
            state: ItemState::Synced,
        },
    )
    .await?;
    Ok(())
}

/// Record a native Google Doc shortcut as a `skipped`, md5-less file `sync_item`.
/// The on-disk pointer is written by the caller; this row keeps the daemon from
/// uploading it back and lets `air-drive status` surface it (issue #3).
async fn record_skipped_shortcut(
    db: &Db,
    mapping_id: MappingId,
    relative_path: &str,
    remote_id: &str,
) -> Result<()> {
    items::insert(
        db.connection(),
        &NewSyncItem {
            mapping_id,
            relative_path: relative_path.to_owned(),
            kind: ItemKind::File,
            remote_id: Some(remote_id.to_owned()),
            size: None,
            md5: None,
            local_inode: None,
            last_synced_at: unix_now(),
            state: ItemState::Skipped,
        },
    )
    .await?;
    Ok(())
}

async fn record_synced_item(
    db: &Db,
    mapping_id: MappingId,
    relative_path: &str,
    remote_id: Option<String>,
    size: i64,
    md5: String,
) -> Result<()> {
    items::insert(
        db.connection(),
        &NewSyncItem {
            mapping_id,
            relative_path: relative_path.to_owned(),
            kind: ItemKind::File,
            remote_id,
            size: Some(size),
            md5: Some(md5),
            local_inode: None,
            last_synced_at: unix_now(),
            state: ItemState::Synced,
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_parent_root_vs_nested() {
        assert_eq!(split_parent("a.txt"), ("", "a.txt"));
        assert_eq!(split_parent("dir/a.txt"), ("dir", "a.txt"));
        assert_eq!(split_parent("a/b/c.txt"), ("a/b", "c.txt"));
    }

    #[test]
    fn walk_local_skips_partial_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("real.txt"), b"r").unwrap();
        let partial = tmp.path().join(crate::engine::staging::PARTIAL_DIR);
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("op-1"), b"stale").unwrap();
        let entries = walk_local(tmp.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
        assert!(names.contains(&"real.txt"));
        assert!(
            !names
                .iter()
                .any(|n| n.contains(crate::engine::staging::PARTIAL_DIR)),
            "partial entries should be filtered out: {names:?}"
        );
    }

    #[test]
    fn walk_local_lists_nested_paths_with_forward_slashes() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("leaf.txt"), b"L").unwrap();
        let entries = walk_local(tmp.path()).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
        assert!(paths.contains(&"a/b/c/leaf.txt"), "got {paths:?}");
    }
}
