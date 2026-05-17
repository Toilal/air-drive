//! Drive `changes.list` poller (T052).
//!
//! Long-lived task that wakes every `interval` seconds, fetches the delta since
//! the last cursor, filters to descendants of the watched root, and emits
//! [`RemoteChange`] events on a tokio mpsc channel. The new
//! `newStartPageToken` is persisted to `drive_change_cursor` after every page
//! so a crash mid-loop doesn't replay events on restart.
//!
//! Descendant filtering uses a small in-memory cache (`known_descendant_ids`)
//! seeded with the mapped root. On a cache miss the poller walks the file's
//! `parents` chain (one `files.get` per hop) until it either hits the root
//! (cache: true) or exhausts the chain (cache: false). Folders we see for the
//! first time get added to the cache so subsequent files under them resolve in
//! O(1).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::drive::http::DriveHttp;
use crate::drive::metadata;
use crate::error::Result;
use crate::state::Db;
use crate::state::cursor;
use crate::state::mapping::MappingId;
use crate::state::unix_now;

/// One Drive change that reached the daemon — emitted by the poller, consumed
/// by `reconcile_remote`. `removed = true` means the file was trashed; `file`
/// is `None` in that case.
#[derive(Debug, Clone)]
pub struct RemoteChange {
    /// Drive file ID the change refers to.
    pub file_id: String,
    /// `true` for delete / trash events.
    pub removed: bool,
    /// Full file resource for non-removal events (id, name, mime, size, md5, parents).
    pub file: Option<FileSnapshot>,
}

/// Snapshot of a Drive file as returned by `changes.list`. Mirrors the relevant
/// subset of [`crate::drive::metadata::DriveFileMeta`] plus the parents list
/// (needed for descendant filtering).
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    /// Drive file ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// MIME type, including the special `application/vnd.google-apps.*` family.
    pub mime_type: String,
    /// Size in bytes when reported.
    pub size: Option<i64>,
    /// Hex md5; `None` for folders + native Google Docs.
    pub md5: Option<String>,
    /// Parent folder IDs (may be empty after a parent-detach).
    pub parents: Vec<String>,
}

impl FileSnapshot {
    /// `true` for folders (`mime_type == application/vnd.google-apps.folder`).
    pub fn is_folder(&self) -> bool {
        self.mime_type == metadata::FOLDER_MIME
    }
}

/// Spawn the poller as a tokio task. Returns the JoinHandle so the daemon can
/// await it during shutdown. The poller stops cleanly when `cancel` fires.
pub async fn run(
    http: DriveHttp,
    db: Db,
    mapping_id: MappingId,
    root_id: String,
    tx: mpsc::Sender<RemoteChange>,
    interval: Duration,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // `known_descendant_ids` caches "this id is somewhere under the watched
    // root". Seeded with the root itself; folders get added as we discover them.
    let descendants: Arc<Mutex<HashSet<String>>> =
        Arc::new(Mutex::new(HashSet::from([root_id.clone()])));

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(interval) => {}
        }

        let Some(token) = cursor::get(db.connection(), mapping_id).await? else {
            // Initial sync hasn't run yet (or no mapping). Skip this round.
            continue;
        };

        let body = match http
            .get_json(
                "changes",
                &[
                    ("pageToken", token.as_str()),
                    (
                        "fields",
                        "newStartPageToken,changes(fileId,removed,file(id,name,mimeType,size,md5Checksum,parents))",
                    ),
                ],
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                // Transient failures are common (network, 503). Don't move the
                // cursor — we'll retry on the next tick.
                tracing::warn!(error = %e, "changes.list failed; will retry next tick");
                continue;
            }
        };

        let new_token = body
            .get("newStartPageToken")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or(token.clone());

        let changes = body
            .get("changes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for c in changes {
            let file_id = c
                .get("fileId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let removed = c.get("removed").and_then(|v| v.as_bool()).unwrap_or(false);
            let file = c.get("file").and_then(parse_snapshot);

            let in_scope = if removed {
                // Removed events have no file resource — we trust the daemon's
                // own state (sync_item) to know whether the id was relevant.
                // Always forward, the reconciler filters by sync_item lookup.
                true
            } else if let Some(f) = &file {
                is_descendant(&http, f, &root_id, &descendants).await
            } else {
                false
            };
            if !in_scope {
                continue;
            }

            // Track folders we've now identified as descendants so siblings
            // resolve via the cache hit instead of an extra walk.
            if let Some(f) = &file {
                if f.is_folder() {
                    descendants.lock().await.insert(f.id.clone());
                }
            }

            if tx
                .send(RemoteChange {
                    file_id,
                    removed,
                    file,
                })
                .await
                .is_err()
            {
                tracing::info!("changes consumer closed; poller exiting");
                return Ok(());
            }
        }

        if let Err(e) = cursor::set(db.connection(), mapping_id, &new_token, unix_now()).await {
            tracing::warn!(error = %e, "failed to persist new cursor");
        }
    }
}

/// Walk `file`'s parent chain (one `files.get` per hop) until we either reach
/// `root_id` or exhaust the chain. Adds intermediate folder IDs to the cache so
/// future calls hit O(1).
async fn is_descendant(
    http: &DriveHttp,
    file: &FileSnapshot,
    root_id: &str,
    cache: &Arc<Mutex<HashSet<String>>>,
) -> bool {
    let mut to_check: Vec<String> = file.parents.clone();
    let mut visited: HashMap<String, ()> = HashMap::new();
    let mut path: Vec<String> = Vec::new();

    while let Some(parent_id) = to_check.pop() {
        if visited.contains_key(&parent_id) {
            continue;
        }
        visited.insert(parent_id.clone(), ());

        {
            let cache_guard = cache.lock().await;
            if cache_guard.contains(&parent_id) {
                // Found a known descendant ancestor. Promote every folder we
                // walked through.
                drop(cache_guard);
                let mut cache_guard = cache.lock().await;
                for id in path.into_iter() {
                    cache_guard.insert(id);
                }
                return parent_id == root_id || cache_guard.contains(root_id);
            }
        }

        if parent_id == root_id {
            // Reached the root directly. Promote the walked folders.
            let mut cache_guard = cache.lock().await;
            for id in path.into_iter() {
                cache_guard.insert(id);
            }
            cache_guard.insert(root_id.to_owned());
            return true;
        }

        // Cache miss — fetch this parent's parents.
        let raw = match metadata::get_file_raw(http, &parent_id, "id,parents").await {
            Ok(v) => v,
            Err(_) => return false,
        };
        let next_parents: Vec<String> = raw
            .get("parents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if next_parents.is_empty() {
            // Reached a parentless node that isn't our root → out of scope.
            return false;
        }
        path.push(parent_id);
        to_check.extend(next_parents);
    }
    false
}

/// Parse a `file` JSON value from a `changes.list` entry into a [`FileSnapshot`].
fn parse_snapshot(v: &serde_json::Value) -> Option<FileSnapshot> {
    let id = v.get("id")?.as_str()?.to_owned();
    let name = v.get("name")?.as_str()?.to_owned();
    let mime_type = v
        .get("mimeType")
        .and_then(|x| x.as_str())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let size = v.get("size").and_then(|x| match x {
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        serde_json::Value::Number(n) => n.as_i64(),
        _ => None,
    });
    let md5 = v
        .get("md5Checksum")
        .and_then(|x| x.as_str())
        .map(str::to_owned);
    let parents: Vec<String> = v
        .get("parents")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some(FileSnapshot {
        id,
        name,
        mime_type,
        size,
        md5,
        parents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_snapshot_extracts_full_record() {
        let v = serde_json::json!({
            "id": "f1",
            "name": "n.txt",
            "mimeType": "text/plain",
            "size": "42",
            "md5Checksum": "deadbeef",
            "parents": ["p1", "p2"],
        });
        let s = parse_snapshot(&v).unwrap();
        assert_eq!(s.id, "f1");
        assert_eq!(s.size, Some(42));
        assert_eq!(s.md5.as_deref(), Some("deadbeef"));
        assert_eq!(s.parents, vec!["p1".to_owned(), "p2".to_owned()]);
    }

    #[test]
    fn parse_snapshot_tolerates_missing_optional_fields() {
        let v = serde_json::json!({"id": "f1", "name": "n.txt"});
        let s = parse_snapshot(&v).unwrap();
        assert!(s.size.is_none() && s.md5.is_none() && s.parents.is_empty());
    }

    #[test]
    fn parse_snapshot_rejects_missing_required() {
        assert!(parse_snapshot(&serde_json::json!({"name": "x"})).is_none());
        assert!(parse_snapshot(&serde_json::json!({"id": "x"})).is_none());
    }

    #[test]
    fn snapshot_is_folder_detects_mime() {
        let folder = FileSnapshot {
            id: "x".into(),
            name: "f".into(),
            mime_type: metadata::FOLDER_MIME.into(),
            size: None,
            md5: None,
            parents: vec![],
        };
        assert!(folder.is_folder());
    }
}
