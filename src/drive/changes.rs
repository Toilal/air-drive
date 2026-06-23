//! Drive `changes.list` poller.
//!
//! Long-lived task that wakes every `interval` seconds, drains the full delta
//! since the last cursor (following `nextPageToken` across pages), filters to
//! descendants of the watched root, and emits [`RemoteChange`] events on a tokio
//! mpsc channel. The `newStartPageToken` (emitted by Drive only on the final
//! page) is persisted to `drive_change_cursor` once the whole delta is drained,
//! so a crash mid-tick re-fetches from the old cursor rather than skipping
//! changes.
//!
//! Descendant filtering uses a small in-memory cache (`known_descendant_ids`)
//! seeded with the mapped root. On a cache miss the poller walks the file's
//! `parents` chain (one `files.get` per hop) until it either hits the root
//! (cache: true) or exhausts the chain (cache: false). Folders we see for the
//! first time get added to the cache so subsequent files under them resolve in
//! O(1).

use std::collections::HashMap;
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
/// by `reconcile_remote`. `removed = true` means the file was **permanently
/// deleted or access was lost** (`file` is `None`). A *trash* is NOT a removal:
/// it surfaces as a normal change with `file` present and `file.trashed = true`.
#[derive(Debug, Clone)]
pub struct RemoteChange {
    /// Drive file ID the change refers to.
    pub file_id: String,
    /// `true` for permanent delete / loss of access (NOT a trash — see
    /// [`FileSnapshot::trashed`]).
    pub removed: bool,
    /// Full file resource for non-removal events (id, name, mime, size, md5, parents).
    pub file: Option<FileSnapshot>,
    /// Path of the changed file under the mapped local root, computed by the
    /// poller from the file's parent chain. `None` when `removed` is true or
    /// when the file is outside the watched tree (the poller filters those out
    /// before forwarding; this field is only populated for in-scope creations
    /// and updates).
    pub relative_path: Option<String>,
}

/// One fully-drained poll delta: the in-scope changes plus the
/// `newStartPageToken` to persist *after* they are durably applied. The consumer
/// owns cursor advancement so a failed apply re-fetches the batch next tick
/// rather than losing the change (the cursor is never advanced past unapplied
/// work).
#[derive(Debug, Clone)]
pub struct RemoteBatch {
    /// In-scope changes for this tick, in feed order.
    pub changes: Vec<RemoteChange>,
    /// Cursor to persist once every change above has been applied.
    pub new_token: String,
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
    /// `true` when the file is in Drive's trash. A *trash* surfaces as a normal
    /// change (`removed = false`) with the file still present — distinct from a
    /// permanent delete / loss of access, which surfaces as `removed = true` with
    /// no file. The reconciler treats a trash as a removal of the local copy.
    pub trashed: bool,
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
    tx: mpsc::Sender<RemoteBatch>,
    interval: Duration,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // Cache: Drive file id → relative path under `root_id`. Seeded with the
    // root itself (empty path). Used both to recognise descendants in O(1)
    // after the first visit and to compute the relative path the reconciler
    // needs to place a file on the local filesystem.
    let path_cache: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::from([(
        root_id.clone(),
        String::new(),
    )])));

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

        // Drain every page of the delta in this tick. `newStartPageToken` only
        // appears on the LAST page; a multi-page burst that stopped after one
        // page would re-fetch the same first page forever and never advance.
        let (changes, new_token) = match fetch_changes_since(&http, &token).await {
            Ok(v) => v,
            Err(crate::error::Error::Oauth(msg)) => {
                // OAuth refresh / 401 — persist the blocked flag so the
                // status surface reports "re-link account" and stop polling
                // until the user resolves it. The dispatcher follows the same
                // code path on the next op attempt.
                tracing::error!(error = %msg, "auth failure on changes.list — daemon blocked");
                let _ = crate::state::meta::set_blocked(
                    db.connection(),
                    crate::state::meta::BlockedKind::Auth,
                    &msg,
                    unix_now(),
                )
                .await;
                continue;
            }
            Err(e) => {
                // Transient failures are common (network, 503) and have already
                // survived the HTTP layer's own retry budget. Don't move the
                // cursor — we'll retry on the next tick — but surface the degraded
                // state as a *recoverable* `transient` block so `status` shows
                // "Drive unreachable" rather than a silent stall. The next
                // successful tick clears it.
                tracing::warn!(error = %e, "changes.list failed; marking transient block, will retry next tick");
                let _ = crate::state::meta::set_blocked(
                    db.connection(),
                    crate::state::meta::BlockedKind::Transient,
                    &e.to_string(),
                    unix_now(),
                )
                .await;
                continue;
            }
        };

        // The poll succeeded: Drive is reachable. If a prior tick left a
        // recoverable `transient` block, clear it now (terminal kinds stay).
        match crate::state::meta::clear_if_transient(db.connection()).await {
            Ok(true) => tracing::info!("changes.list succeeded — cleared transient block"),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "failed to clear transient block"),
        }

        let mut batch: Vec<RemoteChange> = Vec::new();
        for c in changes {
            let file_id = c
                .get("fileId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let removed = c.get("removed").and_then(|v| v.as_bool()).unwrap_or(false);
            let file = c.get("file").and_then(parse_snapshot);

            // The watched remote folder itself was deleted on Drive. The
            // daemon can't make further progress; flip to
            // `state_meta.blocked_kind = remote` and skip emitting this
            // change (no sync_item references the root).
            if removed && file_id == root_id {
                tracing::error!(
                    folder = %root_id,
                    "watched remote folder was trashed — daemon is now blocked"
                );
                let _ = crate::state::meta::set_blocked(
                    db.connection(),
                    crate::state::meta::BlockedKind::Remote,
                    "watched remote folder was removed from Drive",
                    unix_now(),
                )
                .await;
                continue;
            }

            let relative_path = if removed {
                // Removed events have no file resource — we trust the daemon's
                // own state (sync_item) to know whether the id was relevant.
                // Always forward, the reconciler filters by sync_item lookup.
                None
            } else if let Some(f) = &file {
                match descendant_path(&http, f, &root_id, &path_cache).await {
                    Some(rel) => Some(rel),
                    None => continue, // out of scope
                }
            } else {
                continue;
            };

            // Track folders we've now identified as descendants so siblings
            // resolve via the cache hit instead of an extra walk.
            if let (Some(f), Some(rel)) = (&file, &relative_path) {
                if f.is_folder() {
                    path_cache.lock().await.insert(f.id.clone(), rel.clone());
                }
            }

            batch.push(RemoteChange {
                file_id,
                removed,
                file,
                relative_path,
            });
        }

        // Hand the whole tick to the consumer, which advances the cursor only
        // after every change is applied. The cursor is NOT persisted here, so a
        // failed apply re-fetches this delta next tick instead of skipping it.
        if tx
            .send(RemoteBatch {
                changes: batch,
                new_token,
            })
            .await
            .is_err()
        {
            tracing::info!("changes consumer closed; poller exiting");
            return Ok(());
        }
    }
}

/// Walk `file`'s parent chain (one `files.get` per hop) until we either reach
/// `root_id` (file is in scope) or exhaust the chain (out of scope). Returns
/// the file's path relative to `root_id` when in scope, `None` otherwise.
///
/// As we walk, we cache every (folder-id → relative-path) pair we see so
/// future calls under the same subfolders hit O(1) and so the reconciler can
/// reuse the cached path without re-walking. The cache lock is held only
/// while reading or mutating the map — every HTTP call happens between lock
/// releases, so the poller never blocks other tasks on network I/O.
/// Fetch the full change delta since `start_token`, following `nextPageToken`
/// across pages. Returns the accumulated raw change entries plus the
/// `newStartPageToken` to persist — which Drive only emits on the final page, so
/// the caller must not advance the cursor until the whole delta is drained.
async fn fetch_changes_since(
    http: &DriveHttp,
    start_token: &str,
) -> Result<(Vec<serde_json::Value>, String)> {
    let mut page_token = start_token.to_owned();
    let mut changes = Vec::new();
    loop {
        let body = http
            .get_json(
                "changes",
                &[
                    ("pageToken", page_token.as_str()),
                    ("pageSize", "1000"),
                    (
                        "fields",
                        "nextPageToken,newStartPageToken,changes(fileId,removed,file(id,name,mimeType,size,md5Checksum,parents,trashed))",
                    ),
                ],
            )
            .await?;
        if let Some(arr) = body.get("changes").and_then(|v| v.as_array()) {
            changes.extend(arr.iter().cloned());
        }
        if let Some(next) = body
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
        {
            page_token = next.to_owned();
            continue;
        }
        // Last page: `newStartPageToken` is the cursor for the next tick. Fall
        // back to the token we just used if (unexpectedly) absent.
        let new_start = body
            .get("newStartPageToken")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or(page_token);
        return Ok((changes, new_start));
    }
}

async fn descendant_path(
    http: &DriveHttp,
    file: &FileSnapshot,
    _root_id: &str,
    cache: &Arc<Mutex<HashMap<String, String>>>,
) -> Option<String> {
    let first_parent = file.parents.first()?.clone();
    // Walk up, collecting (id, name) for every folder above the file that we
    // don't already have cached. The chain is in "child-first" order — we
    // reverse it once we know the file is in scope.
    let mut chain: Vec<(String, String)> = Vec::new();
    let mut current_id = first_parent;

    loop {
        // Cache hit on the current parent?
        let cached_prefix: Option<String> = {
            let guard = cache.lock().await;
            guard.get(&current_id).cloned()
        };
        if let Some(prefix) = cached_prefix {
            return assemble_path(&prefix, &chain, &file.name, cache).await;
        }

        // Cache miss — fetch this parent's own (name, parents) and walk further up.
        let raw = metadata::get_file_raw(http, &current_id, "id,name,parents")
            .await
            .ok()?;
        let name = raw.get("name").and_then(|v| v.as_str())?.to_owned();
        let next_parents: Vec<String> = raw
            .get("parents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        chain.push((current_id.clone(), name));
        let Some(next) = next_parents.into_iter().next() else {
            // Walked off the top without hitting `root_id` → out of scope.
            return None;
        };
        current_id = next;
    }
}

/// Glue helper for [`descendant_path`]. Builds the final relative path from a
/// cached prefix, the walked-but-uncached chain (child-first order), and the
/// file's own name. Promotes the walked entries into the cache so the next
/// call under the same subtree resolves in one hop.
async fn assemble_path(
    cached_prefix: &str,
    chain: &[(String, String)],
    file_name: &str,
    cache: &Arc<Mutex<HashMap<String, String>>>,
) -> Option<String> {
    // Reject any attacker-controlled name that isn't a single safe path
    // component before it can escape the mapped root (e.g. `..`, `a/b`). The
    // cached prefix is trusted: it only ever holds names that passed this gate.
    if !metadata::is_safe_name(file_name)
        || chain.iter().any(|(_, name)| !metadata::is_safe_name(name))
    {
        tracing::warn!(
            name = %file_name,
            "skipping Drive entry whose path contains an unsafe component"
        );
        return None;
    }
    // Build path piece by piece. `chain` is child-first; the final path goes
    // prefix / chain[last].name / chain[last-1].name / ... / chain[0].name / file_name.
    let mut parts: Vec<&str> = Vec::with_capacity(chain.len() + 2);
    if !cached_prefix.is_empty() {
        parts.push(cached_prefix);
    }
    let walked: Vec<&str> = chain
        .iter()
        .rev()
        .map(|(_id, name)| name.as_str())
        .collect();
    parts.extend(walked);
    parts.push(file_name);
    let path = parts.join("/");

    // Cache every intermediate folder we passed through.
    let mut guard = cache.lock().await;
    let mut acc = cached_prefix.to_owned();
    for (id, name) in chain.iter().rev() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(name);
        guard.insert(id.clone(), acc.clone());
    }
    Some(path)
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
    let trashed = v.get("trashed").and_then(|x| x.as_bool()).unwrap_or(false);
    Some(FileSnapshot {
        id,
        name,
        mime_type,
        size,
        md5,
        parents,
        trashed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_changes_since_follows_next_page_token() {
        use crate::drive::auth::StaticToken;
        use wiremock::matchers::{method, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Page 1 (pageToken=start): one change + a nextPageToken, NO newStartPageToken.
        Mock::given(method("GET"))
            .and(query_param("pageToken", "start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nextPageToken": "p2",
                "changes": [{ "fileId": "a", "removed": false }],
            })))
            .mount(&server)
            .await;
        // Page 2 (pageToken=p2): the last page — newStartPageToken, no nextPageToken.
        Mock::given(method("GET"))
            .and(query_param("pageToken", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "newStartPageToken": "final",
                "changes": [{ "fileId": "b", "removed": false }],
            })))
            .mount(&server)
            .await;

        let http = DriveHttp::with_bases(
            Arc::new(StaticToken::new("t")),
            format!("{}/drive/v3", server.uri()),
            format!("{}/upload/drive/v3", server.uri()),
        )
        .unwrap();

        let (changes, new_token) = fetch_changes_since(&http, "start").await.unwrap();
        assert_eq!(changes.len(), 2, "both pages must be accumulated");
        assert_eq!(changes[0]["fileId"], "a");
        assert_eq!(changes[1]["fileId"], "b");
        assert_eq!(
            new_token, "final",
            "cursor must be the last page's newStartPageToken"
        );
    }

    #[tokio::test]
    async fn assemble_path_rejects_unsafe_components() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        // A traversal in the leaf name is refused (no path returned, nothing cached).
        let chain = vec![("dir-id".to_owned(), "sub".to_owned())];
        assert_eq!(assemble_path("", &chain, "../escape", &cache).await, None);
        // A traversal in a walked folder name is refused too.
        let evil_chain = vec![("dir-id".to_owned(), "..".to_owned())];
        assert_eq!(assemble_path("", &evil_chain, "ok.txt", &cache).await, None);
        assert!(
            cache.lock().await.is_empty(),
            "nothing unsafe may be cached"
        );
        // A clean path assembles and caches normally.
        assert_eq!(
            assemble_path("", &chain, "ok.txt", &cache).await,
            Some("sub/ok.txt".to_owned())
        );
    }

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
            trashed: false,
        };
        assert!(folder.is_folder());
    }
}
