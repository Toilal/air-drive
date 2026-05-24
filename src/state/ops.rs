//! `pending_operation` table — the queue of atomic operations waiting for the dispatcher.

use std::collections::HashMap;

use rusqlite::params;
use rusqlite::types::Type as SqlType;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::state::items::ItemId;

/// Strongly-typed primary key for a [`PendingOperation`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpId(pub i64);

/// One of the atomic operations the dispatcher can request from the `SyncEngine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Send the local file to Drive.
    Upload,
    /// Fetch the remote file locally (via the staging dir).
    Download,
    /// Delete the local file.
    DeleteLocal,
    /// Delete the remote file.
    DeleteRemote,
    /// Rename or move on the local side.
    RenameLocal,
    /// Rename or move on the remote side (`rclone moveto`).
    RenameRemote,
    /// Create a local directory.
    CreateDirLocal,
    /// Create a remote directory.
    CreateDirRemote,
    /// Mark the item as a conflict and rename the local copy.
    MarkConflict,
}

impl Operation {
    fn as_sql(self) -> &'static str {
        match self {
            Operation::Upload => "upload",
            Operation::Download => "download",
            Operation::DeleteLocal => "delete_local",
            Operation::DeleteRemote => "delete_remote",
            Operation::RenameLocal => "rename_local",
            Operation::RenameRemote => "rename_remote",
            Operation::CreateDirLocal => "create_dir_local",
            Operation::CreateDirRemote => "create_dir_remote",
            Operation::MarkConflict => "mark_conflict",
        }
    }

    fn from_sql(s: &str, col: usize) -> rusqlite::Result<Self> {
        Ok(match s {
            "upload" => Operation::Upload,
            "download" => Operation::Download,
            "delete_local" => Operation::DeleteLocal,
            "delete_remote" => Operation::DeleteRemote,
            "rename_local" => Operation::RenameLocal,
            "rename_remote" => Operation::RenameRemote,
            "create_dir_local" => Operation::CreateDirLocal,
            "create_dir_remote" => Operation::CreateDirRemote,
            "mark_conflict" => Operation::MarkConflict,
            other => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    col,
                    SqlType::Text,
                    format!("unknown pending_operation.op: {other}").into(),
                ));
            }
        })
    }
}

/// Owned snapshot of a `pending_operation` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOperation {
    /// Primary key.
    pub id: OpId,
    /// Item this operation acts on.
    pub sync_item_id: ItemId,
    /// What to do.
    pub op: Operation,
    /// Op-specific JSON-encoded payload (e.g. `{"new_relative_path": "..."}`).
    pub payload: Option<String>,
    /// Number of attempts made so far.
    pub attempts: i64,
    /// Earliest Unix epoch second at which the dispatcher may try this op again.
    pub next_attempt_at: i64,
    /// One-line summary of the most recent failure, if any.
    pub last_error: Option<String>,
    /// When the row was first inserted.
    pub enqueued_at: i64,
}

/// Enqueue a new operation. `next_attempt_at` defaults to `now` so the dispatcher picks
/// it up on the next tick.
pub async fn enqueue(
    conn: &Connection,
    sync_item_id: ItemId,
    op: Operation,
    payload: Option<&str>,
    now: i64,
) -> Result<OpId> {
    let payload = payload.map(str::to_owned);
    conn.call(move |c| {
        c.execute(
            "INSERT INTO pending_operation
                (sync_item_id, op, payload, attempts, next_attempt_at, last_error, enqueued_at)
             VALUES (?1, ?2, ?3, 0, ?4, NULL, ?4)",
            params![sync_item_id.0, op.as_sql(), payload, now],
        )?;
        Ok(OpId(c.last_insert_rowid()))
    })
    .await
    .map_err(Into::into)
}

/// Pull the next due operation, or `None` if the queue is empty / nothing is due yet.
pub async fn next_due(conn: &Connection, now: i64) -> Result<Option<PendingOperation>> {
    conn.call(move |c| {
        let res = c.query_row(
            "SELECT id, sync_item_id, op, payload, attempts, next_attempt_at, last_error,
                    enqueued_at
             FROM pending_operation
             WHERE next_attempt_at <= ?1
             ORDER BY next_attempt_at ASC, id ASC
             LIMIT 1",
            params![now],
            row_to_pending,
        );
        match res {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

/// Record an attempt outcome. Bumps `attempts`, sets `last_error`, and schedules the
/// next retry.
pub async fn mark_attempt(
    conn: &Connection,
    id: OpId,
    last_error: Option<&str>,
    next_attempt_at: i64,
) -> Result<()> {
    let last_error = last_error.map(str::to_owned);
    conn.call(move |c| {
        c.execute(
            "UPDATE pending_operation
             SET attempts = attempts + 1,
                 last_error = ?1,
                 next_attempt_at = ?2
             WHERE id = ?3",
            params![last_error, next_attempt_at, id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Remove a finished operation.
pub async fn delete(conn: &Connection, id: OpId) -> Result<()> {
    conn.call(move |c| {
        c.execute("DELETE FROM pending_operation WHERE id = ?1", params![id.0])?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Count pending operations grouped by [`Operation`] — used by the status command.
pub async fn count_by_op(conn: &Connection) -> Result<HashMap<Operation, i64>> {
    conn.call(|c| {
        let mut stmt = c.prepare("SELECT op, COUNT(*) FROM pending_operation GROUP BY op")?;
        let rows = stmt.query_map([], |row| {
            let op_s: String = row.get(0)?;
            let n: i64 = row.get(1)?;
            let op = Operation::from_sql(&op_s, 0)?;
            Ok((op, n))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (op, n) = row?;
            out.insert(op, n);
        }
        Ok(out)
    })
    .await
    .map_err(Into::into)
}

fn row_to_pending(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingOperation> {
    let op_s: String = row.get(2)?;
    Ok(PendingOperation {
        id: OpId(row.get(0)?),
        sync_item_id: ItemId(row.get(1)?),
        op: Operation::from_sql(&op_s, 2)?,
        payload: row.get(3)?,
        attempts: row.get(4)?,
        next_attempt_at: row.get(5)?,
        last_error: row.get(6)?,
        enqueued_at: row.get(7)?,
    })
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
        let mapping_id =
            mapping::upsert(db.connection(), account_id, "/home/alice", "rid", None, None, 1)
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
                state: ItemState::PendingLocal,
            },
        )
        .await
        .unwrap();
        (tmp, db, item_id)
    }

    #[tokio::test]
    async fn enqueue_then_next_due() {
        let (_tmp, db, item_id) = open_temp_with_item().await;
        let op_id = enqueue(db.connection(), item_id, Operation::Upload, None, 100)
            .await
            .unwrap();
        let due = next_due(db.connection(), 200).await.unwrap().unwrap();
        assert_eq!(due.id, op_id);
        assert_eq!(due.op, Operation::Upload);
        assert_eq!(due.attempts, 0);
    }

    #[tokio::test]
    async fn next_due_skips_future_ops() {
        let (_tmp, db, item_id) = open_temp_with_item().await;
        enqueue(db.connection(), item_id, Operation::Upload, None, 1000)
            .await
            .unwrap();
        assert!(next_due(db.connection(), 500).await.unwrap().is_none());
        assert!(next_due(db.connection(), 1000).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn mark_attempt_bumps_counter_and_reschedules() {
        let (_tmp, db, item_id) = open_temp_with_item().await;
        let op_id = enqueue(db.connection(), item_id, Operation::Upload, None, 0)
            .await
            .unwrap();
        mark_attempt(db.connection(), op_id, Some("boom"), 500)
            .await
            .unwrap();
        let due = next_due(db.connection(), 500).await.unwrap().unwrap();
        assert_eq!(due.attempts, 1);
        assert_eq!(due.last_error.as_deref(), Some("boom"));
        assert_eq!(due.next_attempt_at, 500);
    }

    #[tokio::test]
    async fn count_by_op_aggregates() {
        let (_tmp, db, item_id) = open_temp_with_item().await;
        enqueue(db.connection(), item_id, Operation::Upload, None, 0)
            .await
            .unwrap();
        enqueue(db.connection(), item_id, Operation::Upload, None, 0)
            .await
            .unwrap();
        enqueue(db.connection(), item_id, Operation::Download, None, 0)
            .await
            .unwrap();

        let counts = count_by_op(db.connection()).await.unwrap();
        assert_eq!(counts.get(&Operation::Upload).copied().unwrap_or(0), 2);
        assert_eq!(counts.get(&Operation::Download).copied().unwrap_or(0), 1);
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let (_tmp, db, item_id) = open_temp_with_item().await;
        let op_id = enqueue(db.connection(), item_id, Operation::Upload, None, 0)
            .await
            .unwrap();
        delete(db.connection(), op_id).await.unwrap();
        assert!(next_due(db.connection(), 1).await.unwrap().is_none());
    }
}
