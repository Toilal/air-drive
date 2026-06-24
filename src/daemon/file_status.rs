//! Per-file sync status for the desktop shell overlay.
//!
//! Derives a single status token for an absolute local path from the
//! `sync_item` table, for the control socket's `status-path` query (consumed by
//! the Nautilus extension). Tokens are intentionally coarse — what a file
//! manager needs to pick an emblem:
//!
//! - `synced`   — up to date on both sides.
//! - `pending`  — a local or remote change is queued (the overlay shows it as
//!   "syncing").
//! - `conflict` — a conflict was opened on this path.
//! - `ignored`  — deliberately not synced (native Google Doc shortcut, symlink).
//! - `unknown`  — not tracked, or outside the mapped root.

use std::path::Path;

use crate::state::items::{self, ItemState};
use crate::state::mapping::{self, MappingId};
use crate::state::meta::BlockedKind;
use crate::state::{Db, conflicts, meta, ops};

/// Resolve the overlay status token for `abs` under the mapping rooted at
/// `local_root`. Never errors — any failure (path outside the root, DB error,
/// untracked file) collapses to `"unknown"`, so the overlay simply shows no
/// emblem rather than breaking the file manager.
pub async fn status_token(
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    abs: &Path,
) -> &'static str {
    let Ok(rel) = abs.strip_prefix(local_root) else {
        // Outside the mapped tree (or not absolute): nothing we track.
        return "unknown";
    };
    // The mapped root has no sync_item row of its own; report the mapping's
    // overall state so the file manager can show one emblem on the sync folder.
    if rel.as_os_str().is_empty() {
        return aggregate_token(db).await;
    }
    // `sync_item.relative_path` is POSIX-separated; on Linux this is already the
    // case, but normalise defensively so the lookup matches.
    let rel_str = rel.to_string_lossy().replace('\\', "/");

    match items::get_by_relative_path(db.connection(), mapping_id, &rel_str).await {
        Ok(Some(item)) => token_for_state(item.state),
        // Untracked path, or any read error — degrade to "unknown".
        Ok(None) | Err(_) => "unknown",
    }
}

/// Status tokens for every tracked immediate child of the directory `abs_dir`,
/// as `(child_name, token)` pairs — the bulk form behind the overlay's
/// `status-dir` query, so a folder of N files costs one round-trip.
///
/// Two sources combine:
/// - children that are tracked `sync_item`s directly under `abs_dir` (their
///   per-file token), and
/// - when `abs_dir` is the **parent** of the mapped root, the root folder
///   itself, carrying its [`aggregate_token`] (the root has no `sync_item`).
///
/// A directory unrelated to the mapping yields an empty list (→ no emblems).
pub async fn dir_status(
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    abs_dir: &Path,
) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();

    // The mapped root shown as a child of its parent directory.
    if local_root.parent() == Some(abs_dir) {
        if let Some(name) = local_root.file_name() {
            out.push((
                name.to_string_lossy().into_owned(),
                aggregate_token(db).await,
            ));
        }
    }

    // Tracked children of `abs_dir` when it is the root or inside it.
    if let Ok(rel) = abs_dir.strip_prefix(local_root) {
        let dir_rel = rel.to_string_lossy().replace('\\', "/");
        if let Ok(children) = items::list_child_states(db.connection(), mapping_id, &dir_rel).await
        {
            for (name, state) in children {
                out.push((name, token_for_state(state)));
            }
        }
    }

    out
}

/// Browser URL for the Drive object backing the local path `abs`, or `None`
/// when it isn't tracked (or has no remote id yet). Used by the file-manager
/// "Open in Google Drive" / "Copy Drive link" actions.
///
/// The mapped root resolves to the mapping's remote folder id; any other path
/// resolves to its `sync_item.remote_id`. `https://drive.google.com/open?id=…`
/// is a type-agnostic entry point — Drive redirects files, folders and native
/// Docs to the right viewer.
pub async fn drive_url(
    db: &Db,
    mapping_id: MappingId,
    local_root: &Path,
    abs: &Path,
) -> Option<String> {
    let rel = abs.strip_prefix(local_root).ok()?;
    let remote_id = if rel.as_os_str().is_empty() {
        // The root has no sync_item; use the mapping's remote folder id.
        mapping::get_single(db.connection())
            .await
            .ok()
            .flatten()
            .map(|m| m.remote_folder_id)?
    } else {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        items::get_by_relative_path(db.connection(), mapping_id, &rel_str)
            .await
            .ok()
            .flatten()?
            .remote_id?
    };
    Some(format!("https://drive.google.com/open?id={remote_id}"))
}

/// Overall sync state for the whole mapping, used for the emblem on the mapped
/// root folder. Resolves to:
///
/// - `conflict` — an unresolved conflict exists, or the daemon is blocked on a
///   terminal condition (auth/remote/mapping) that needs the user.
/// - `pending` — work is queued, or the daemon is in a recoverable transient
///   block (Drive temporarily unreachable).
/// - `synced` — nothing outstanding.
///
/// Uses cheap aggregate queries (a conflict list, the singleton block row, and a
/// grouped op count) rather than scanning every `sync_item`. Any error degrades
/// toward `synced` so the root never shows a spurious alarm.
pub async fn aggregate_token(db: &Db) -> &'static str {
    let conn = db.connection();

    let has_conflict = conflicts::list_unresolved(conn)
        .await
        .map(|c| !c.is_empty())
        .unwrap_or(false);
    if has_conflict {
        return "conflict";
    }

    if let Ok(Some(blocked)) = meta::get_blocked(conn).await {
        return match blocked.kind {
            // Recoverable: the daemon is retrying, not stuck.
            BlockedKind::Transient => "pending",
            // Terminal: needs the user (re-link, re-map, lost remote).
            BlockedKind::Auth | BlockedKind::Remote | BlockedKind::Mapping => "conflict",
        };
    }

    let has_pending = ops::count_by_op(conn)
        .await
        .map(|counts| counts.values().sum::<i64>() > 0)
        .unwrap_or(false);
    if has_pending { "pending" } else { "synced" }
}

/// Map a persisted [`ItemState`] to its overlay token.
fn token_for_state(state: ItemState) -> &'static str {
    match state {
        ItemState::Synced => "synced",
        ItemState::PendingLocal | ItemState::PendingRemote => "pending",
        ItemState::Conflict => "conflict",
        ItemState::Skipped => "ignored",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::items::{ItemKind, NewSyncItem};

    fn item(rel: &str, state: ItemState) -> NewSyncItem {
        NewSyncItem {
            mapping_id: MappingId(1),
            relative_path: rel.to_owned(),
            kind: ItemKind::File,
            remote_id: Some("rid".into()),
            size: Some(1),
            md5: Some("m".into()),
            local_inode: None,
            last_synced_at: 0,
            state,
        }
    }

    #[test]
    fn state_token_mapping() {
        assert_eq!(token_for_state(ItemState::Synced), "synced");
        assert_eq!(token_for_state(ItemState::PendingLocal), "pending");
        assert_eq!(token_for_state(ItemState::PendingRemote), "pending");
        assert_eq!(token_for_state(ItemState::Conflict), "conflict");
        assert_eq!(token_for_state(ItemState::Skipped), "ignored");
    }

    #[tokio::test]
    async fn status_token_resolves_tracked_paths() {
        use crate::state::{accounts, mapping};

        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        let account_id = accounts::upsert(db.connection(), "a@b.com", 1)
            .await
            .unwrap();
        // `upsert` always yields MappingId(1), matching the seeded items below.
        mapping::upsert(
            db.connection(),
            account_id,
            "/home/u/Drive",
            "rid",
            None,
            None,
            1,
        )
        .await
        .unwrap();
        let root = Path::new("/home/u/Drive");
        items::insert(db.connection(), &item("a/synced.txt", ItemState::Synced))
            .await
            .unwrap();
        items::insert(
            db.connection(),
            &item("a/pending.txt", ItemState::PendingLocal),
        )
        .await
        .unwrap();
        items::insert(db.connection(), &item("doc.gdoc", ItemState::Skipped))
            .await
            .unwrap();

        let mid = MappingId(1);
        assert_eq!(
            status_token(&db, mid, root, &root.join("a/synced.txt")).await,
            "synced"
        );
        assert_eq!(
            status_token(&db, mid, root, &root.join("a/pending.txt")).await,
            "pending"
        );
        assert_eq!(
            status_token(&db, mid, root, &root.join("doc.gdoc")).await,
            "ignored"
        );
        // Untracked file under the root.
        assert_eq!(
            status_token(&db, mid, root, &root.join("a/unknown.txt")).await,
            "unknown"
        );
        // Path outside the mapped root.
        assert_eq!(
            status_token(&db, mid, root, Path::new("/etc/passwd")).await,
            "unknown"
        );
        // The root itself reports the aggregate — nothing queued/conflicting here.
        assert_eq!(status_token(&db, mid, root, root).await, "synced");
    }

    /// Seed an account + the singleton mapping so foreign keys are satisfied.
    async fn seed_mapping(db: &Db) {
        use crate::state::{accounts, mapping};
        let account_id = accounts::upsert(db.connection(), "a@b.com", 1)
            .await
            .unwrap();
        mapping::upsert(
            db.connection(),
            account_id,
            "/home/u/Drive",
            "rid",
            None,
            None,
            1,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dir_status_lists_immediate_children_and_root_aggregate() {
        use crate::state::items::ItemKind;

        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        seed_mapping(&db).await;
        let root = Path::new("/home/u/Drive");
        let mid = MappingId(1);

        items::insert(db.connection(), &item("a.txt", ItemState::Synced))
            .await
            .unwrap();
        items::insert(db.connection(), &item("doc.gdoc", ItemState::Skipped))
            .await
            .unwrap();
        items::insert(db.connection(), &item("sub/b.txt", ItemState::PendingLocal))
            .await
            .unwrap();
        // A tracked subdirectory.
        items::insert(
            db.connection(),
            &NewSyncItem {
                mapping_id: mid,
                relative_path: "sub".into(),
                kind: ItemKind::Dir,
                remote_id: Some("d".into()),
                size: None,
                md5: None,
                local_inode: None,
                last_synced_at: 0,
                state: ItemState::Synced,
            },
        )
        .await
        .unwrap();

        // Root view: immediate children only (not sub/b.txt).
        let mut top = dir_status(&db, mid, root, root).await;
        top.sort();
        assert_eq!(
            top,
            vec![
                ("a.txt".to_string(), "synced"),
                ("doc.gdoc".to_string(), "ignored"),
                ("sub".to_string(), "synced"),
            ]
        );

        // Subdirectory view.
        let sub = dir_status(&db, mid, root, &root.join("sub")).await;
        assert_eq!(sub, vec![("b.txt".to_string(), "pending")]);

        // Parent of the mapped root → the root itself with its aggregate.
        let parent = dir_status(&db, mid, root, Path::new("/home/u")).await;
        assert_eq!(parent, vec![("Drive".to_string(), "synced")]);

        // Unrelated directory → nothing.
        assert!(
            dir_status(&db, mid, root, Path::new("/etc"))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn drive_url_resolves_root_and_tracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        seed_mapping(&db).await; // remote_folder_id = "rid"
        let root = Path::new("/home/u/Drive");
        let mid = MappingId(1);
        items::insert(db.connection(), &item("a.txt", ItemState::Synced))
            .await
            .unwrap(); // remote_id = "rid"

        // Root → the mapping's remote folder id.
        assert_eq!(
            drive_url(&db, mid, root, root).await.as_deref(),
            Some("https://drive.google.com/open?id=rid")
        );
        // Tracked file → its remote id.
        assert_eq!(
            drive_url(&db, mid, root, &root.join("a.txt"))
                .await
                .as_deref(),
            Some("https://drive.google.com/open?id=rid")
        );
        // Untracked / outside → None.
        assert!(
            drive_url(&db, mid, root, &root.join("nope.txt"))
                .await
                .is_none()
        );
        assert!(drive_url(&db, mid, root, Path::new("/etc")).await.is_none());
    }

    #[tokio::test]
    async fn aggregate_is_synced_when_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        seed_mapping(&db).await;
        assert_eq!(aggregate_token(&db).await, "synced");
    }

    #[tokio::test]
    async fn aggregate_is_pending_with_queued_op() {
        use crate::state::ops::{self, Operation};
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        seed_mapping(&db).await;
        let id = items::insert(db.connection(), &item("a.txt", ItemState::PendingLocal))
            .await
            .unwrap();
        ops::enqueue(db.connection(), id, Operation::Upload, None, 0)
            .await
            .unwrap();
        assert_eq!(aggregate_token(&db).await, "pending");
    }

    #[tokio::test]
    async fn aggregate_is_conflict_when_blocked_on_auth() {
        use crate::state::meta;
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        seed_mapping(&db).await;
        meta::set_blocked(db.connection(), BlockedKind::Auth, "re-link needed", 1)
            .await
            .unwrap();
        assert_eq!(aggregate_token(&db).await, "conflict");
    }

    #[tokio::test]
    async fn aggregate_is_pending_on_transient_block() {
        use crate::state::meta;
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        seed_mapping(&db).await;
        meta::set_blocked(db.connection(), BlockedKind::Transient, "drive 503", 1)
            .await
            .unwrap();
        assert_eq!(aggregate_token(&db).await, "pending");
    }
}
