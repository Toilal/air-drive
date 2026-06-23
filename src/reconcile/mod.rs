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

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use globset::GlobSet;

use crate::config::SymlinkPolicy;
use crate::drive::http::DriveHttp;
use crate::drive::metadata;
use crate::engine::{BulkDownload, BulkUpload, SyncEngine};
use crate::error::{Error, Result};
use crate::state::Db;
use crate::state::cursor;
use crate::state::items::{self, ItemKind, ItemState, NewSyncItem, SyncItem};
use crate::state::mapping::MappingId;
use crate::state::unix_now;
use crate::watch::{LocalKind, WatchEvent, classify_local};

/// How many times to retry the targeted lookup of an uploaded file the
/// post-sync remote walk didn't see yet (Drive eventual consistency).
const AFTER_WALK_RETRIES: u32 = 4;

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
#[allow(clippy::too_many_arguments)] // wiring collaborators by name is clearer than a struct
pub async fn initial(
    http: &DriveHttp,
    engine: Arc<dyn SyncEngine>,
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    remote_root_id: &str,
    ignore_patterns: &[String],
    symlinks: SymlinkPolicy,
) -> Result<()> {
    let ignore = crate::watch::build_ignore_matcher(ignore_patterns)?;

    let local_entries = walk_local(local_root, symlinks)?;
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
    //    Doing it up front also means every file's remote parent folder exists
    //    (and its id is cached) before the bulk transfer runs.
    let mut dir_paths: BTreeSet<String> = BTreeSet::new();
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

    // 1. Native Google Docs (no md5, `vnd.google-apps.*`) → write a local
    //    shortcut pointer + a `skipped` row, never download bytes that don't
    //    exist (issue #3). Done before the bulk lists are built so these are
    //    excluded from the download set; the on-disk `.gdoc` is excluded from
    //    the upload set below. Idempotent on re-run.
    for remote in &remote_files {
        if remote.md5.is_some() {
            continue;
        }
        if shortcut::is_native(&remote.mime_type) {
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
    }

    // 2. Classify leaf files into the three buckets the bulk transfer / state
    //    pass act on. The engine is a dumb pipe: it only ever moves the exact
    //    paths we put in these lists, so every special case (ignore patterns,
    //    native Docs, conflicts) is resolved *here*, by inclusion/exclusion.
    let mut downloads: Vec<BulkDownload> = Vec::new();
    let mut uploads: Vec<BulkUpload> = Vec::new();
    // remote-only files we'll record as synced once the download succeeds.
    let mut remote_only: Vec<(&str, &str, i64, String)> = Vec::new();

    // Remote-only → download (skip ignored, skip md5-less which §1 handled).
    for remote in &remote_files {
        if local_paths.contains(remote.relative_path.as_str()) {
            continue; // both-sides — handled below
        }
        if is_ignored(&ignore, &remote.relative_path) {
            continue;
        }
        let Some(md5) = remote.md5.clone() else {
            continue; // native Doc / md5-less: handled in §1
        };
        downloads.push(BulkDownload {
            remote_id: remote.id.clone(),
            rel_path: remote.relative_path.clone(),
        });
        remote_only.push((&remote.relative_path, &remote.id, remote.size, md5));
    }

    // Local-only → upload (skip ignored, skip shortcut pointers we wrote).
    for local in &local_files {
        if remote_by_path.contains_key(local.relative_path.as_str()) {
            continue; // both-sides — handled below
        }
        if is_ignored(&ignore, &local.relative_path) {
            continue;
        }
        if let Some(it) =
            items::get_by_relative_path(db.connection(), mapping_id, &local.relative_path).await?
        {
            if it.state == ItemState::Skipped {
                continue; // a `.gdoc` shortcut — never upload it back
            }
        }
        let (parent_rel, name) = split_parent(&local.relative_path);
        let parent_id = remote_folder_ids
            .get(parent_rel)
            .cloned()
            .unwrap_or_else(|| remote_root_id.to_owned());
        uploads.push(BulkUpload {
            rel_path: local.relative_path.clone(),
            remote_parent_id: parent_id,
            name: name.to_owned(),
        });
    }

    // Both-sides files: md5-equal → record synced (no transfer); md5-differ →
    // defer to the continuous-sync conflict handler (unchanged semantics).
    for local in &local_files {
        let Some(remote) = remote_by_path.get(local.relative_path.as_str()) else {
            continue;
        };
        if is_ignored(&ignore, &local.relative_path) {
            continue;
        }
        let local_path = local_root.join(&local.relative_path);
        let (size, md5) = fingerprint::compute_local(&local_path).await?;
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

    // 3. Bulk transfer. One batched call per direction (see `SyncEngine`):
    //    `RcloneEngine` runs a single `rclone copy --files-from` with live
    //    progress; `HttpEngine` loops per file. The list contents already
    //    encode every policy decision from §1–§2.
    tracing::info!(
        downloads = downloads.len(),
        uploads = uploads.len(),
        dirs = dir_paths.len(),
        "initial reconciliation: starting bulk transfer"
    );
    engine
        .bulk_download(&downloads, remote_root_id, local_root)
        .await?;
    engine
        .bulk_upload(&uploads, local_root, remote_root_id)
        .await?;

    // 4. Record state for the transferred files. Remote-only files take the
    //    md5/size/id from the pre-transfer remote walk (the download reproduced
    //    that content locally).
    for (rel, id, size, md5) in remote_only {
        record_synced_item(db, mapping_id, rel, Some(id.to_owned()), size, md5).await?;
    }

    // Local-only files only get their Drive id after the upload, so re-walk the
    // remote once (O(dirs) `files.list`, not O(files) spawns) and match by path.
    //
    // A just-uploaded file can be briefly invisible to `files.list` (Drive
    // eventual consistency); for any still-missing path we retry a targeted
    // lookup under its (already-known) parent folder. Recording it reliably HERE
    // matters: an unrecorded upload reappears as a phantom "new" file in the
    // change feed later and cascades into bugs — e.g. a folder rename downloading
    // the phantom into the new path and then failing the dir rename with
    // ENOTEMPTY (issue #19).
    if !uploads.is_empty() {
        let after = walk_remote(http, remote_root_id).await?;
        let after_by_path: HashMap<&str, &RemoteEntry> = after
            .iter()
            .filter(|e| !e.is_dir)
            .map(|r| (r.relative_path.as_str(), r))
            .collect();
        let mut pending: Vec<&BulkUpload> = Vec::new();
        for up in &uploads {
            match after_by_path
                .get(up.rel_path.as_str())
                .and_then(|r| r.md5.as_ref().map(|m| (*r, m.clone())))
            {
                Some((r, md5)) => {
                    record_synced_item(
                        db,
                        mapping_id,
                        &up.rel_path,
                        Some(r.id.clone()),
                        r.size,
                        md5,
                    )
                    .await?;
                }
                None => pending.push(up),
            }
        }
        // Retry the stragglers with a targeted parent listing, backing off to let
        // Drive's index catch up.
        for attempt in 0..AFTER_WALK_RETRIES {
            if pending.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt + 1))).await;
            let mut still = Vec::new();
            for up in pending {
                match lookup_uploaded(http, &up.remote_parent_id, &up.name).await {
                    Some((id, size, md5)) => {
                        record_synced_item(db, mapping_id, &up.rel_path, Some(id), size, md5)
                            .await?;
                    }
                    None => still.push(up),
                }
            }
            pending = still;
        }
        for up in pending {
            tracing::warn!(
                relative_path = %up.rel_path,
                "uploaded file still not visible after retries — leaving for continuous sync"
            );
        }
    }

    // 5. Persist the changes-cursor baseline AFTER convergence so the continuous loop
    //    doesn't replay events the initial pass already covered.
    let cursor_body = http.get_json("changes/startPageToken", &[]).await?;
    let token = cursor_body
        .get("startPageToken")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Drive("missing startPageToken in response".into()))?
        .to_owned();
    cursor::set(db.connection(), mapping_id, &token, unix_now()).await?;
    tracing::info!("initial reconciliation complete");

    Ok(())
}

/// Match a relative path against the watcher's ignore globs — by **file name**
/// only, mirroring [`crate::watch`]'s steady-state behaviour so a pattern means
/// the same thing during bootstrap and during continuous sync.
fn is_ignored(matcher: &GlobSet, rel_path: &str) -> bool {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    matcher.is_match(name)
}

/// Replay local changes that occurred while the daemon was stopped: diff the
/// local tree against the last-synced fingerprints in `sync_item` and feed the
/// differences through [`continuous::apply_local`] as synthesized watch events.
/// The change cursor is intentionally **not** touched — remote-side offline
/// changes are recovered by the change poller from the persisted cursor.
///
/// A local modify/delete is only propagated when the **remote** is still at the
/// last-synced fingerprint (verified with one `files.get`). If both sides
/// drifted while down it is left to the change poller's conflict handler, so the
/// scan can never overwrite a concurrent remote edit.
///
/// Runs on every daemon start. On the very first start it is a near-no-op: the
/// initial reconciliation already converged every path. Returns the number of
/// replayed events (for logging).
pub async fn startup_local_scan(
    http: &DriveHttp,
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    ignore_patterns: &[String],
    symlinks: SymlinkPolicy,
) -> Result<usize> {
    let ignore = crate::watch::build_ignore_matcher(ignore_patterns)?;
    let local_entries = walk_local(local_root, symlinks)?;
    let local_file_paths: HashSet<&str> = local_entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.relative_path.as_str())
        .collect();

    let mut replayed = 0usize;

    // New or modified local entries → Created / Modified events.
    for entry in &local_entries {
        if is_ignored(&ignore, &entry.relative_path) {
            continue;
        }
        let abs = local_root.join(&entry.relative_path);
        let existing =
            items::get_by_relative_path(db.connection(), mapping_id, &entry.relative_path).await?;

        if entry.is_dir {
            // A directory we don't track yet → propagate its creation (empty
            // dirs included). A known dir needs nothing.
            if existing.is_none() {
                continuous::apply_local(
                    WatchEvent::Created(abs),
                    db,
                    mapping_id,
                    local_root,
                    symlinks,
                )
                .await?;
                replayed += 1;
            }
            continue;
        }

        match existing {
            // Shortcut pointer for a native Google Doc — never re-upload it.
            Some(item) if item.state == ItemState::Skipped => {}
            // Tracked file: replay only if its content actually drifted from the
            // last-synced fingerprint, AND the remote hasn't drifted too (else
            // it's a both-sides conflict — leave it to the poller).
            Some(item) => {
                let (size, md5) = fingerprint::compute_local(&abs).await?;
                let changed_locally =
                    item.md5.as_deref() != Some(md5.as_str()) || item.size != Some(size);
                if changed_locally && remote_matches_synced(http, &item).await {
                    continuous::apply_local(
                        WatchEvent::Modified(abs),
                        db,
                        mapping_id,
                        local_root,
                        symlinks,
                    )
                    .await?;
                    replayed += 1;
                } else if changed_locally {
                    tracing::info!(
                        relative_path = %item.relative_path,
                        "startup scan: local edit + remote drift — deferring to conflict handler"
                    );
                }
            }
            // Brand-new local file created while the daemon was down.
            None => {
                continuous::apply_local(
                    WatchEvent::Created(abs),
                    db,
                    mapping_id,
                    local_root,
                    symlinks,
                )
                .await?;
                replayed += 1;
            }
        }
    }

    // Files we had synced that vanished locally while down → Deleted events,
    // unless the remote moved on (delete-vs-edit conflict → defer to the poller).
    for item in items::list_for_mapping(db.connection(), mapping_id).await? {
        if item.kind != ItemKind::File
            || item.state == ItemState::Skipped
            || item.trashed_at.is_some()
            || local_file_paths.contains(item.relative_path.as_str())
            || is_ignored(&ignore, &item.relative_path)
        {
            continue;
        }
        if !remote_matches_synced(http, &item).await {
            tracing::info!(
                relative_path = %item.relative_path,
                "startup scan: local delete + remote drift — deferring to conflict handler"
            );
            continue;
        }
        let abs = local_root.join(&item.relative_path);
        continuous::apply_local(
            WatchEvent::Deleted(abs),
            db,
            mapping_id,
            local_root,
            symlinks,
        )
        .await?;
        replayed += 1;
    }

    Ok(replayed)
}

/// Is `item`'s remote file still at the last-synced md5 (the remote side hasn't
/// changed since we last synced)? A missing/404 remote, a fetch error, or a
/// missing id all return `false` — meaning "can't confirm the remote is
/// unchanged", so the caller defers to the change poller instead of acting.
async fn remote_matches_synced(http: &DriveHttp, item: &SyncItem) -> bool {
    let Some(remote_id) = item.remote_id.as_deref() else {
        return false;
    };
    match metadata::get_file_raw(http, remote_id, "md5Checksum").await {
        Ok(v) => v.get("md5Checksum").and_then(|x| x.as_str()) == item.md5.as_deref(),
        Err(_) => false,
    }
}

/// Walk the local tree, returning every file and directory beneath `root` (excluding
/// the root itself and any `.air-drive-partial/` staging artefacts). `policy`
/// decides how symlinks are treated ([`classify_local`]).
fn walk_local(root: &Path, policy: SymlinkPolicy) -> Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    collect_local(root, root, policy, &mut visited, &mut out)?;
    out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(out)
}

fn collect_local(
    root: &Path,
    dir: &Path,
    policy: SymlinkPolicy,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<LocalEntry>,
) -> Result<()> {
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
        match classify_local(&path, root, policy) {
            Some(LocalKind::Dir) => {
                out.push(LocalEntry {
                    relative_path: rel.clone(),
                    is_dir: true,
                });
                // Loop guard: when following symlinks, a directory link can point
                // back into the tree and cycle. Descend into each canonical dir
                // at most once. In `Skip` mode no link is followed, so there is
                // no cycle and we descend unconditionally (no canonicalize cost).
                let descend = match policy {
                    SymlinkPolicy::Follow => match std::fs::canonicalize(&path) {
                        Ok(canon) => visited.insert(canon),
                        Err(_) => false,
                    },
                    SymlinkPolicy::Skip => true,
                };
                if descend {
                    collect_local(root, &path, policy, visited, out)?;
                }
            }
            Some(LocalKind::File) => {
                out.push(LocalEntry {
                    relative_path: rel,
                    is_dir: false,
                });
            }
            // Symlink under Skip, escaping/broken link under Follow, or a special
            // file — skipped.
            None => {}
        }
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
            // Reject attacker-controlled names that aren't a single safe path
            // component (`..`, `a/b`, …) before they're joined into a path the
            // engine writes under `local_root` — they could otherwise escape it.
            if !metadata::is_safe_name(&c.name) {
                tracing::warn!(
                    name = %c.name,
                    "skipping remote entry with an unsafe path component"
                );
                continue;
            }
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

/// Look up a just-uploaded file directly under its (known) Drive parent folder,
/// returning `(id, size, md5)` when it is present with an md5. Used to record an
/// upload the post-sync remote walk missed because of Drive eventual
/// consistency. Any error / absence yields `None` (the caller retries or warns).
async fn lookup_uploaded(
    http: &DriveHttp,
    parent_id: &str,
    name: &str,
) -> Option<(String, i64, String)> {
    let children = metadata::list_children(http, parent_id).await.ok()?;
    let child = children
        .into_iter()
        .find(|c| !c.is_folder() && c.name == name)?;
    let md5 = child.md5?;
    Some((child.id, child.size.unwrap_or(0), md5))
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
    // Idempotent: an interrupted initial pass (cursor not yet written) may have
    // already recorded this row. On re-run, leave the existing one in place
    // rather than hitting the (mapping_id, relative_path) unique constraint.
    if items::get_by_relative_path(db.connection(), mapping_id, relative_path)
        .await?
        .is_some()
    {
        return Ok(());
    }
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
    // Idempotent on re-run of an interrupted initial pass — see `record_synced_dir`.
    if items::get_by_relative_path(db.connection(), mapping_id, relative_path)
        .await?
        .is_some()
    {
        return Ok(());
    }
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
        let entries = walk_local(tmp.path(), SymlinkPolicy::Skip).unwrap();
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
        let entries = walk_local(tmp.path(), SymlinkPolicy::Skip).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
        assert!(paths.contains(&"a/b/c/leaf.txt"), "got {paths:?}");
    }

    #[test]
    fn is_ignored_matches_on_file_name_at_any_depth() {
        let patterns: Vec<String> = ["*.swp", ".DS_Store"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let m = crate::watch::build_ignore_matcher(&patterns).unwrap();

        // Matches by base name, regardless of directory depth.
        assert!(is_ignored(&m, ".DS_Store"));
        assert!(is_ignored(&m, "docs/notes/.DS_Store"));
        assert!(is_ignored(&m, "deep/dir/scratch.swp"));

        // Non-ignored files are kept.
        assert!(!is_ignored(&m, "keep.txt"));
        assert!(!is_ignored(&m, "docs/keep.txt"));
        // The pattern matches the name, not a path segment.
        assert!(!is_ignored(&m, "swp/keep.txt"));
    }
}
