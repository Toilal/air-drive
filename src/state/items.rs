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
                    last_synced_at, state
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
                    local_inode, last_synced_at, state \
             FROM sync_item WHERE id = ?1",
            params![id.0],
            |row| {
                Ok(SyncItem {
                    id: ItemId(row.get(0)?),
                    mapping_id: MappingId(row.get(1)?),
                    relative_path: row.get(2)?,
                    kind: ItemKind::from_sql(&row.get::<_, String>(3)?, 3)?,
                    remote_id: row.get(4)?,
                    size: row.get(5)?,
                    md5: row.get(6)?,
                    local_inode: row.get(7)?,
                    last_synced_at: row.get(8)?,
                    state: ItemState::from_sql(&row.get::<_, String>(9)?, 9)?,
                })
            },
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
                    local_inode, last_synced_at, state \
             FROM sync_item WHERE remote_id = ?1",
            params![remote_id],
            |row| {
                Ok(SyncItem {
                    id: ItemId(row.get(0)?),
                    mapping_id: MappingId(row.get(1)?),
                    relative_path: row.get(2)?,
                    kind: ItemKind::from_sql(&row.get::<_, String>(3)?, 3)?,
                    remote_id: row.get(4)?,
                    size: row.get(5)?,
                    md5: row.get(6)?,
                    local_inode: row.get(7)?,
                    last_synced_at: row.get(8)?,
                    state: ItemState::from_sql(&row.get::<_, String>(9)?, 9)?,
                })
            },
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

/// Materialise every item belonging to a mapping. Allocates a `Vec`; this is fine for
/// the MVP's scale ceiling of ≤ 50 000 items.
pub async fn list_for_mapping(conn: &Connection, mapping_id: MappingId) -> Result<Vec<SyncItem>> {
    conn.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT id, mapping_id, relative_path, kind, remote_id, size, md5, local_inode,
                    last_synced_at, state
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
