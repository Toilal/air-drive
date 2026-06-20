//! `conflict_record` table — open conflicts surfaced by the status command.

use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::state::items::ItemId;

/// Strongly-typed primary key for a [`ConflictRecord`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictId(pub i64);

/// Snapshot of a `conflict_record` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRecord {
    /// Primary key.
    pub id: ConflictId,
    /// Item that kept the canonical name (the remote version, per the Q2 clarification).
    pub sync_item_id: ItemId,
    /// Canonical relative path retained.
    pub original_relative_path: String,
    /// Companion `.conflict-…` path holding the local-side version.
    pub conflict_relative_path: String,
    /// When the conflict was detected (Unix epoch seconds).
    pub detected_at: i64,
}

/// Open a new conflict record.
pub async fn insert(
    conn: &Connection,
    sync_item_id: ItemId,
    original_relative_path: &str,
    conflict_relative_path: &str,
    detected_at: i64,
) -> Result<ConflictId> {
    let original = original_relative_path.to_owned();
    let conflict = conflict_relative_path.to_owned();
    conn.call(move |c| {
        c.execute(
            "INSERT INTO conflict_record
                (sync_item_id, original_relative_path, conflict_relative_path, detected_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![sync_item_id.0, original, conflict, detected_at],
        )?;
        Ok(ConflictId(c.last_insert_rowid()))
    })
    .await
    .map_err(Into::into)
}

/// List every unresolved conflict in deterministic order (oldest first).
pub async fn list_unresolved(conn: &Connection) -> Result<Vec<ConflictRecord>> {
    conn.call(|c| {
        let mut stmt = c.prepare(
            "SELECT id, sync_item_id, original_relative_path, conflict_relative_path, detected_at
             FROM conflict_record
             ORDER BY detected_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ConflictRecord {
                id: ConflictId(row.get(0)?),
                sync_item_id: ItemId(row.get(1)?),
                original_relative_path: row.get(2)?,
                conflict_relative_path: row.get(3)?,
                detected_at: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
    .map_err(Into::into)
}

/// Remove a resolved conflict.
pub async fn delete(conn: &Connection, id: ConflictId) -> Result<()> {
    conn.call(move |c| {
        c.execute("DELETE FROM conflict_record WHERE id = ?1", params![id.0])?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Db;
    use crate::state::items::{ItemKind, ItemState, NewSyncItem};
    use crate::state::tests::open_temp;
    use crate::state::{accounts, items, mapping};

    async fn open_temp_with_item() -> (tempfile::TempDir, Db, ItemId) {
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
        let item_id = items::insert(
            db.connection(),
            &NewSyncItem {
                mapping_id,
                relative_path: "f".into(),
                kind: ItemKind::File,
                remote_id: None,
                size: None,
                md5: None,
                local_inode: None,
                last_synced_at: 0,
                state: ItemState::Conflict,
            },
        )
        .await
        .unwrap();
        (tmp, db, item_id)
    }

    #[tokio::test]
    async fn insert_list_delete() {
        let (_tmp, db, item_id) = open_temp_with_item().await;
        let cid = insert(
            db.connection(),
            item_id,
            "doc.txt",
            "doc.conflict-20260517T123000Z.txt",
            42,
        )
        .await
        .unwrap();

        let list = list_unresolved(db.connection()).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, cid);
        assert_eq!(list[0].original_relative_path, "doc.txt");

        delete(db.connection(), cid).await.unwrap();
        assert!(list_unresolved(db.connection()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_orders_by_detected_at() {
        let (_tmp, db, item_id) = open_temp_with_item().await;
        insert(db.connection(), item_id, "a", "a.conflict", 200)
            .await
            .unwrap();
        insert(db.connection(), item_id, "b", "b.conflict", 100)
            .await
            .unwrap();
        let list = list_unresolved(db.connection()).await.unwrap();
        let paths: Vec<_> = list
            .iter()
            .map(|c| c.original_relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["b", "a"]);
    }
}
