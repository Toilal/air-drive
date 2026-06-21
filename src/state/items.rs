//! `sync_item` table — known file or folder participating in the sync.
//!
//! One row per logical filesystem object under the mapping. Acts as the source of
//! truth for "what was synced last time" — the reconciler reads it to decide whether
//! an event represents a real change or an echo, and writes it once an operation
//! commits.

use rusqlite::params;
use rusqlite::types::Type as SqlType;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::state::mapping::MappingId;

/// Strongly-typed primary key for a [`SyncItem`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ItemId(pub i64);

/// `kind` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

impl ItemKind {
    fn as_sql(self) -> &'static str {
        match self {
            ItemKind::File => "file",
            ItemKind::Dir => "dir",
        }
    }

    fn from_sql(s: &str, col: usize) -> rusqlite::Result<Self> {
        match s {
            "file" => Ok(ItemKind::File),
            "dir" => Ok(ItemKind::Dir),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                col,
                SqlType::Text,
                format!("unknown sync_item.kind: {other}").into(),
            )),
        }
    }
}

/// `state` enum tracking the item's lifecycle (see data-model state diagram).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// In sync on both sides at the last persisted fingerprint.
    Synced,
    /// Local change observed, not yet uploaded.
    PendingLocal,
    /// Remote change observed, not yet downloaded.
    PendingRemote,
    /// Both sides diverged → conflict was opened.
    Conflict,
    /// Skipped (native Google Docs, symlink, …).
    Skipped,
}

impl ItemState {
    fn as_sql(self) -> &'static str {
        match self {
            ItemState::Synced => "synced",
            ItemState::PendingLocal => "pending_local",
            ItemState::PendingRemote => "pending_remote",
            ItemState::Conflict => "conflict",
            ItemState::Skipped => "skipped",
        }
    }

    fn from_sql(s: &str, col: usize) -> rusqlite::Result<Self> {
        match s {
            "synced" => Ok(ItemState::Synced),
            "pending_local" => Ok(ItemState::PendingLocal),
            "pending_remote" => Ok(ItemState::PendingRemote),
            "conflict" => Ok(ItemState::Conflict),
            "skipped" => Ok(ItemState::Skipped),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                col,
                SqlType::Text,
                format!("unknown sync_item.state: {other}").into(),
            )),
        }
    }
}

/// Owned snapshot of a `sync_item` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncItem {
    /// Primary key.
    pub id: ItemId,
    /// Owning mapping.
    pub mapping_id: MappingId,
    /// POSIX-style path relative to the watched local root.
    pub relative_path: String,
    /// File or directory.
    pub kind: ItemKind,
    /// Drive file ID. `None` during the brief window between a local create and its
    /// first upload.
    pub remote_id: Option<String>,
    /// Size in bytes for files; `None` for dirs.
    pub size: Option<i64>,
    /// Hex MD5 for files; `None` for dirs and items with no Drive MD5.
    pub md5: Option<String>,
    /// Local inode hint for optimistic cache lookups (not authoritative).
    pub local_inode: Option<i64>,
    /// Unix epoch seconds of the last successful sync of this item.
    pub last_synced_at: i64,
    /// Lifecycle state.
    pub state: ItemState,
    /// When `Some`, the item is a **tombstone**: the file was trashed on Drive and
    /// its local copy removed, but the row is kept (with its `remote_id`) so a later
    /// restore re-links to the original path instead of creating a duplicate. The
    /// value is the Unix epoch second of the trash, used by the retention GC.
    pub trashed_at: Option<i64>,
}

/// Owned struct without an id, used when inserting new rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSyncItem {
    /// Owning mapping.
    pub mapping_id: MappingId,
    /// Relative path to the local root.
    pub relative_path: String,
    /// File or directory.
    pub kind: ItemKind,
    /// Drive file ID, when known.
    pub remote_id: Option<String>,
    /// Size in bytes for files.
    pub size: Option<i64>,
    /// Hex MD5 for files.
    pub md5: Option<String>,
    /// Optional local inode hint.
    pub local_inode: Option<i64>,
    /// Unix epoch seconds of the last successful sync.
    pub last_synced_at: i64,
    /// Initial state.
    pub state: ItemState,
}

/// Insert a new row, returning its generated id.
pub async fn insert(conn: &Connection, item: &NewSyncItem) -> Result<ItemId> {
    let item = item.clone();
    conn.call(move |c| {
        c.execute(
            "INSERT INTO sync_item
                (mapping_id, relative_path, kind, remote_id, size, md5, local_inode,
                 last_synced_at, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                item.mapping_id.0,
                item.relative_path,
                item.kind.as_sql(),
                item.remote_id,
                item.size,
                item.md5,
                item.local_inode,
                item.last_synced_at,
                item.state.as_sql(),
            ],
        )?;
        Ok(ItemId(c.last_insert_rowid()))
    })
    .await
    .map_err(Into::into)
}

/// Look up an item by (mapping, relative path) — the unique index.
pub async fn get_by_relative_path(
    conn: &Connection,
    mapping_id: MappingId,
    relative_path: &str,
) -> Result<Option<SyncItem>> {
    let relative_path = relative_path.to_owned();
    conn.call(move |c| {
        let res = c.query_row(
            "SELECT id, mapping_id, relative_path, kind, remote_id, size, md5, local_inode,
                    last_synced_at, state, trashed_at
             FROM sync_item WHERE mapping_id = ?1 AND relative_path = ?2",
            params![mapping_id.0, relative_path],
            row_to_item,
        );
        match res {
            Ok(item) => Ok(Some(item)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

/// Fetch a single item by primary key.
pub async fn get_by_id(conn: &Connection, id: ItemId) -> Result<Option<SyncItem>> {
    conn.call(move |c| {
        let res = c.query_row(
            "SELECT id, mapping_id, relative_path, kind, remote_id, size, md5, \
                    local_inode, last_synced_at, state, trashed_at \
             FROM sync_item WHERE id = ?1",
            params![id.0],
            row_to_item,
        );
        match res {
            Ok(i) => Ok(Some(i)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

/// Look up a sync item by its Drive `remote_id`.
pub async fn get_by_remote_id(conn: &Connection, remote_id: &str) -> Result<Option<SyncItem>> {
    let remote_id = remote_id.to_owned();
    conn.call(move |c| {
        let res = c.query_row(
            "SELECT id, mapping_id, relative_path, kind, remote_id, size, md5, \
                    local_inode, last_synced_at, state, trashed_at \
             FROM sync_item WHERE remote_id = ?1",
            params![remote_id],
            row_to_item,
        );
        match res {
            Ok(i) => Ok(Some(i)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

/// Update the Drive `remote_id` (set after a successful first-upload).
pub async fn set_remote_id(conn: &Connection, id: ItemId, remote_id: &str) -> Result<()> {
    let remote_id = remote_id.to_owned();
    conn.call(move |c| {
        c.execute(
            "UPDATE sync_item SET remote_id = ?1 WHERE id = ?2",
            params![remote_id, id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Rewrite a sync item's `relative_path` (used by rename ops).
pub async fn set_relative_path(conn: &Connection, id: ItemId, new_path: &str) -> Result<()> {
    let new_path = new_path.to_owned();
    conn.call(move |c| {
        c.execute(
            "UPDATE sync_item SET relative_path = ?1 WHERE id = ?2",
            params![new_path, id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Rewrite the `relative_path` of a directory **and all its descendants** in a
/// single transaction, replacing the leading `old_prefix` with `new_prefix`.
///
/// Used when a folder is renamed or moved: the filesystem `mv` (or Drive move)
/// relocates the whole subtree at once and emits no per-descendant events, so the
/// daemon rewrites the rows itself. Each item keeps its `remote_id` — no re-upload.
pub async fn rename_subtree(
    conn: &Connection,
    mapping_id: MappingId,
    old_prefix: &str,
    new_prefix: &str,
) -> Result<()> {
    let old = old_prefix.to_owned();
    let new = new_prefix.to_owned();
    conn.call(move |c| {
        let tx = c.transaction()?;
        // Collect the directory row plus every descendant (`old/...`). The LIKE
        // uses an explicit ESCAPE so `%` / `_` in names are matched literally.
        let like = format!("{}/%", escape_like(&old));
        let affected: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, relative_path FROM sync_item \
                 WHERE mapping_id = ?1 AND (relative_path = ?2 OR relative_path LIKE ?3 ESCAPE '\\')",
            )?;
            let rows = stmt.query_map(params![mapping_id.0, old, like], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (id, path) in affected {
            // `path` is either exactly `old` or starts with `old/`; splitting at
            // `old.len()` is a valid char boundary since `old` is a prefix.
            let new_path = format!("{}{}", new, &path[old.len()..]);
            tx.execute(
                "UPDATE sync_item SET relative_path = ?1 WHERE id = ?2",
                params![new_path, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Escape `%`, `_` and `\` for use inside a SQL `LIKE` pattern with `ESCAPE '\'`.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Update the (size, md5) fingerprint and `last_synced_at` for an item.
pub async fn update_fingerprint(
    conn: &Connection,
    id: ItemId,
    size: Option<i64>,
    md5: Option<&str>,
    last_synced_at: i64,
) -> Result<()> {
    let md5 = md5.map(str::to_owned);
    conn.call(move |c| {
        c.execute(
            "UPDATE sync_item
             SET size = ?1, md5 = ?2, last_synced_at = ?3
             WHERE id = ?4",
            params![size, md5, last_synced_at, id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Update the lifecycle state of an item.
pub async fn set_state(conn: &Connection, id: ItemId, state: ItemState) -> Result<()> {
    conn.call(move |c| {
        c.execute(
            "UPDATE sync_item SET state = ?1 WHERE id = ?2",
            params![state.as_sql(), id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Delete an item by id. Cascades into `pending_operation` and `conflict_record`.
pub async fn delete(conn: &Connection, id: ItemId) -> Result<()> {
    conn.call(move |c| {
        c.execute("DELETE FROM sync_item WHERE id = ?1", params![id.0])?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Mark an item as a tombstone: its local copy was removed because the file was
/// trashed on Drive, but the row (and its `remote_id`) is kept so a later restore
/// re-links to the original path. `trashed_at` is the Unix epoch second of the
/// trash, consumed by [`gc_tombstones`].
pub async fn mark_trashed(conn: &Connection, id: ItemId, trashed_at: i64) -> Result<()> {
    conn.call(move |c| {
        c.execute(
            "UPDATE sync_item SET trashed_at = ?1 WHERE id = ?2",
            params![trashed_at, id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Clear an item's tombstone (a trashed file was restored on Drive).
pub async fn clear_trashed(conn: &Connection, id: ItemId) -> Result<()> {
    conn.call(move |c| {
        c.execute(
            "UPDATE sync_item SET trashed_at = NULL WHERE id = ?1",
            params![id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Permanently delete tombstones trashed strictly before `cutoff` (a Unix epoch
/// second). Returns the number of rows reclaimed. Live items (`trashed_at IS NULL`)
/// are never touched.
pub async fn gc_tombstones(conn: &Connection, cutoff: i64) -> Result<usize> {
    conn.call(move |c| {
        let n = c.execute(
            "DELETE FROM sync_item WHERE trashed_at IS NOT NULL AND trashed_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    })
    .await
    .map_err(Into::into)
}

/// Materialise every item belonging to a mapping. Allocates a `Vec`; this is fine for
/// the MVP's scale ceiling of ≤ 50 000 items.
pub async fn list_for_mapping(conn: &Connection, mapping_id: MappingId) -> Result<Vec<SyncItem>> {
    conn.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT id, mapping_id, relative_path, kind, remote_id, size, md5, local_inode,
                    last_synced_at, state, trashed_at
             FROM sync_item WHERE mapping_id = ?1
             ORDER BY relative_path",
        )?;
        let rows = stmt.query_map(params![mapping_id.0], row_to_item)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
    .map_err(Into::into)
}

/// List the live (non-tombstoned) skipped items of a mapping, ordered by path.
///
/// Skipped items are things the daemon deliberately does not sync as opaque bytes —
/// today, native Google Docs represented as local shortcut files (issue #3).
/// `air-drive status` surfaces them so they are visible rather than silently absent.
pub async fn list_skipped(conn: &Connection, mapping_id: MappingId) -> Result<Vec<SyncItem>> {
    conn.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT id, mapping_id, relative_path, kind, remote_id, size, md5, local_inode,
                    last_synced_at, state, trashed_at
             FROM sync_item
             WHERE mapping_id = ?1 AND state = 'skipped' AND trashed_at IS NULL
             ORDER BY relative_path",
        )?;
        let rows = stmt.query_map(params![mapping_id.0], row_to_item)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
    .map_err(Into::into)
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncItem> {
    let kind_s: String = row.get(3)?;
    let state_s: String = row.get(9)?;
    Ok(SyncItem {
        id: ItemId(row.get(0)?),
        mapping_id: MappingId(row.get(1)?),
        relative_path: row.get(2)?,
        kind: ItemKind::from_sql(&kind_s, 3)?,
        remote_id: row.get(4)?,
        size: row.get(5)?,
        md5: row.get(6)?,
        local_inode: row.get(7)?,
        last_synced_at: row.get(8)?,
        state: ItemState::from_sql(&state_s, 9)?,
        trashed_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Db;
    use crate::state::tests::open_temp;
    use crate::state::{accounts, mapping};

    async fn open_temp_with_mapping() -> (tempfile::TempDir, Db, MappingId) {
        let (tmp, db) = open_temp().await;
        let account_id = accounts::upsert(db.connection(), "alice@gmail.com", 1)
            .await
            .unwrap();
        let mapping_id = mapping::upsert(
            db.connection(),
            account_id,
            "/home/alice",
            "rid",
            None,
            None,
            1,
        )
        .await
        .unwrap();
        (tmp, db, mapping_id)
    }

    fn sample(mapping_id: MappingId, path: &str) -> NewSyncItem {
        NewSyncItem {
            mapping_id,
            relative_path: path.into(),
            kind: ItemKind::File,
            remote_id: Some("r1".into()),
            size: Some(123),
            md5: Some("deadbeef".into()),
            local_inode: Some(42),
            last_synced_at: 1000,
            state: ItemState::Synced,
        }
    }

    #[tokio::test]
    async fn insert_then_get_by_relative_path() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let id = insert(db.connection(), &sample(mapping_id, "a/b.txt"))
            .await
            .unwrap();
        let item = get_by_relative_path(db.connection(), mapping_id, "a/b.txt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(item.id, id);
        assert_eq!(item.relative_path, "a/b.txt");
        assert_eq!(item.size, Some(123));
        assert_eq!(item.state, ItemState::Synced);
    }

    #[tokio::test]
    async fn rename_subtree_rewrites_dir_and_all_descendants() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let conn = db.connection();
        // A non-trivial subtree under `docs`, plus a sibling that must NOT move.
        for p in [
            "docs",
            "docs/sub",
            "docs/spec.txt",
            "docs/sub/deep.txt",
            "other.txt",
        ] {
            insert(conn, &sample(mapping_id, p)).await.unwrap();
        }

        rename_subtree(conn, mapping_id, "docs", "documents")
            .await
            .unwrap();

        // The whole `docs/...` subtree is rewritten under `documents/...`.
        for p in ["docs", "docs/sub", "docs/spec.txt", "docs/sub/deep.txt"] {
            assert!(
                get_by_relative_path(conn, mapping_id, p)
                    .await
                    .unwrap()
                    .is_none(),
                "{p} should no longer exist"
            );
        }
        for p in [
            "documents",
            "documents/sub",
            "documents/spec.txt",
            "documents/sub/deep.txt",
        ] {
            let it = get_by_relative_path(conn, mapping_id, p)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{p} should exist after rename"));
            // remote_id preserved → no re-upload.
            assert_eq!(it.remote_id.as_deref(), Some("r1"));
        }
        // The sibling outside the subtree is untouched.
        assert!(
            get_by_relative_path(conn, mapping_id, "other.txt")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn mark_then_clear_trashed_roundtrip() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let conn = db.connection();
        let id = insert(conn, &sample(mapping_id, "f.txt")).await.unwrap();
        assert!(
            get_by_id(conn, id)
                .await
                .unwrap()
                .unwrap()
                .trashed_at
                .is_none()
        );
        mark_trashed(conn, id, 1234).await.unwrap();
        assert_eq!(
            get_by_id(conn, id).await.unwrap().unwrap().trashed_at,
            Some(1234)
        );
        clear_trashed(conn, id).await.unwrap();
        assert!(
            get_by_id(conn, id)
                .await
                .unwrap()
                .unwrap()
                .trashed_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn gc_tombstones_reclaims_only_old_ones() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let conn = db.connection();
        let live = insert(conn, &sample(mapping_id, "live.txt")).await.unwrap();
        let old = insert(conn, &sample(mapping_id, "old.txt")).await.unwrap();
        let recent = insert(conn, &sample(mapping_id, "recent.txt"))
            .await
            .unwrap();
        mark_trashed(conn, old, 1000).await.unwrap();
        mark_trashed(conn, recent, 5000).await.unwrap();

        // cutoff 3000: reclaims the old tombstone (1000), keeps the recent (5000)
        // and never touches the live row.
        let reclaimed = gc_tombstones(conn, 3000).await.unwrap();
        assert_eq!(reclaimed, 1);
        assert!(get_by_id(conn, live).await.unwrap().is_some());
        assert!(get_by_id(conn, old).await.unwrap().is_none());
        assert!(get_by_id(conn, recent).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rename_subtree_does_not_touch_lookalike_prefix() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let conn = db.connection();
        // `docs2` shares the `docs` text prefix but is NOT a descendant of `docs`.
        for p in ["docs", "docs/a.txt", "docs2", "docs2/b.txt"] {
            insert(conn, &sample(mapping_id, p)).await.unwrap();
        }

        rename_subtree(conn, mapping_id, "docs", "documents")
            .await
            .unwrap();

        assert!(
            get_by_relative_path(conn, mapping_id, "documents/a.txt")
                .await
                .unwrap()
                .is_some()
        );
        // `docs2` and its child must be left alone (prefix is `docs/`, not `docs`).
        assert!(
            get_by_relative_path(conn, mapping_id, "docs2/b.txt")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn duplicate_relative_path_in_same_mapping_is_rejected() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        insert(db.connection(), &sample(mapping_id, "dup"))
            .await
            .unwrap();
        assert!(
            insert(db.connection(), &sample(mapping_id, "dup"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn update_fingerprint_changes_size_md5() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let id = insert(db.connection(), &sample(mapping_id, "f"))
            .await
            .unwrap();
        update_fingerprint(db.connection(), id, Some(999), Some("cafebabe"), 9999)
            .await
            .unwrap();
        let item = get_by_relative_path(db.connection(), mapping_id, "f")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(item.size, Some(999));
        assert_eq!(item.md5.as_deref(), Some("cafebabe"));
        assert_eq!(item.last_synced_at, 9999);
    }

    #[tokio::test]
    async fn set_state_and_delete() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let id = insert(db.connection(), &sample(mapping_id, "x"))
            .await
            .unwrap();
        set_state(db.connection(), id, ItemState::Conflict)
            .await
            .unwrap();
        let item = get_by_relative_path(db.connection(), mapping_id, "x")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(item.state, ItemState::Conflict);

        delete(db.connection(), id).await.unwrap();
        assert!(
            get_by_relative_path(db.connection(), mapping_id, "x")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_skipped_returns_only_live_skipped_items() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        let conn = db.connection();
        // A synced file, a live skipped shortcut, and a tombstoned skipped one.
        insert(conn, &sample(mapping_id, "regular.txt"))
            .await
            .unwrap();
        let shortcut = NewSyncItem {
            state: ItemState::Skipped,
            ..sample(mapping_id, "Notes.gdoc")
        };
        insert(conn, &shortcut).await.unwrap();
        let trashed = NewSyncItem {
            state: ItemState::Skipped,
            ..sample(mapping_id, "Gone.gdoc")
        };
        let trashed_id = insert(conn, &trashed).await.unwrap();
        mark_trashed(conn, trashed_id, 123).await.unwrap();

        let skipped = list_skipped(conn, mapping_id).await.unwrap();
        let paths: Vec<_> = skipped.iter().map(|i| i.relative_path.as_str()).collect();
        assert_eq!(paths, vec!["Notes.gdoc"]);
    }

    #[tokio::test]
    async fn list_for_mapping_orders_by_path() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        for p in ["c", "a", "b"] {
            insert(db.connection(), &sample(mapping_id, p))
                .await
                .unwrap();
        }
        let items = list_for_mapping(db.connection(), mapping_id).await.unwrap();
        let paths: Vec<_> = items.iter().map(|i| i.relative_path.as_str()).collect();
        assert_eq!(paths, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn corrupt_enum_value_is_rejected() {
        // Sanity check: bypass the CHECK constraint by writing directly with PRAGMA off
        // is not portable; instead, just verify that from_sql returns a typed error.
        let err = ItemKind::from_sql("xxx", 3).unwrap_err();
        assert!(matches!(err, rusqlite::Error::FromSqlConversionFailure(..)));
        let err = ItemState::from_sql("xxx", 9).unwrap_err();
        assert!(matches!(err, rusqlite::Error::FromSqlConversionFailure(..)));
    }
}
