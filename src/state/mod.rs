//! SQLite-backed persistent state.
//!
//! Layout:
//!
//! - [`Db`] wraps a `tokio_rusqlite::Connection` (async wrapper around `rusqlite` with a
//!   dedicated thread). At open time, the DB is configured with `WAL` + `synchronous =
//!   NORMAL` + `foreign_keys = ON`, the `schema_version` bootstrap runs, and forward-only
//!   migrations are applied (FR-024).
//! - One submodule per logical table — [`accounts`], [`mapping`], [`items`], [`ops`],
//!   [`conflicts`], [`cursor`]. Each exposes typed CRUD helpers as `async fn`.
//!
//! The DB file is created with mode `0600` on Unix so account email and sync state are
//! never world-readable.

pub mod accounts;
pub mod conflicts;
pub mod cursor;
pub mod items;
pub mod mapping;
pub mod meta;
pub mod ops;
pub mod schema;

use std::path::Path;

use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::{Error, Result};

/// Handle on the state DB. Cheap to `Clone` (the inner `Connection` is `Arc`-backed).
#[derive(Clone)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open the state DB at `path`, applying pragmas and any pending migrations.
    ///
    /// Pragmas: `journal_mode = WAL`, `synchronous = NORMAL`, `foreign_keys = ON`.
    ///
    /// On Unix the resulting file is `chmod`ed to `0600` after creation.
    ///
    /// Returns [`Error::Config`] when the on-disk schema version is **newer** than the
    /// binary supports (FR-024 — upgrade required, no downgrade).
    pub async fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).await?;
        conn.call(|c| {
            c.pragma_update(None, "journal_mode", "WAL")?;
            c.pragma_update(None, "synchronous", "NORMAL")?;
            c.pragma_update(None, "foreign_keys", "ON")?;
            c.execute_batch(schema::BOOTSTRAP)?;
            Ok(())
        })
        .await?;

        chmod_owner_only(path)?;

        let db = Db { conn };
        db.migrate_to_latest().await?;
        Ok(db)
    }

    /// Borrow the underlying async connection.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Apply every migration whose target version is strictly above the current
    /// `schema_version`. Refuses to start if the on-disk version is newer than the
    /// binary supports.
    async fn migrate_to_latest(&self) -> Result<()> {
        let current: i64 = self
            .conn
            .call(|c| {
                let v = c.query_row(
                    "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
                Ok(v)
            })
            .await?;

        if current > schema::LATEST_VERSION {
            return Err(Error::Config(format!(
                "state DB schema version {current} is newer than this binary supports \
                 ({}); upgrade air-drive to a newer release",
                schema::LATEST_VERSION
            )));
        }

        let now = unix_now();
        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                for (idx, sql) in schema::MIGRATIONS.iter().enumerate() {
                    let target = (idx + 1) as i64;
                    if target <= current {
                        continue;
                    }
                    tx.execute_batch(sql)?;
                    tx.execute(
                        "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                        params![target, now],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

/// Current Unix epoch in seconds. Centralised so tests can stub it out later if needed.
pub fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Chmod a file to `0600` (owner read+write only). No-op on non-Unix.
#[cfg(unix)]
fn chmod_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn chmod_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) async fn open_temp() -> (tempfile::TempDir, Db) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("state.db")).await.unwrap();
        (tmp, db)
    }

    #[tokio::test]
    async fn open_creates_schema_v1() {
        let (_tmp, db) = open_temp().await;
        let v: i64 = db
            .connection()
            .call(|c| Ok(c.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(v, schema::LATEST_VERSION);
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        let _db1 = Db::open(&path).await.unwrap();
        let db2 = Db::open(&path).await.unwrap();
        let v: i64 = db2
            .connection()
            .call(|c| Ok(c.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(v, schema::LATEST_VERSION);
    }

    #[tokio::test]
    async fn refuses_newer_db_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        {
            let _ = Db::open(&path).await.unwrap();
        }
        // Tamper: bump schema_version above what this binary knows.
        let bumped = schema::LATEST_VERSION + 1;
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                params![bumped, 0],
            )
            .unwrap();
        }
        let err = match Db::open(&path).await {
            Ok(_) => panic!("expected schema-version refusal"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("upgrade air-drive"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn pragmas_are_applied() {
        let (_tmp, db) = open_temp().await;
        let journal: String = db
            .connection()
            .call(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");

        let fk: i64 = db
            .connection()
            .call(|c| Ok(c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn db_file_is_chmod_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        let _db = Db::open(&path).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "actual: {mode:o}");
    }

    #[tokio::test]
    async fn delete_account_cascades_to_dependent_rows() {
        use crate::state::{accounts, conflicts, cursor, items, mapping, ops};
        let (_tmp, db) = open_temp().await;
        let account_id = accounts::upsert(db.connection(), "a@x", 1).await.unwrap();
        let mapping_id = mapping::upsert(db.connection(), account_id, "/l", "rid", None, 1)
            .await
            .unwrap();
        let item_id = items::insert(
            db.connection(),
            &items::NewSyncItem {
                mapping_id,
                relative_path: "f".into(),
                kind: items::ItemKind::File,
                remote_id: None,
                size: None,
                md5: None,
                local_inode: None,
                last_synced_at: 0,
                state: items::ItemState::Synced,
            },
        )
        .await
        .unwrap();
        ops::enqueue(db.connection(), item_id, ops::Operation::Upload, None, 0)
            .await
            .unwrap();
        conflicts::insert(db.connection(), item_id, "f", "f.conflict", 0)
            .await
            .unwrap();
        cursor::set(db.connection(), mapping_id, "tok", 0)
            .await
            .unwrap();

        // Drop the account; everything else cascades.
        db.connection()
            .call(|c| {
                c.execute("DELETE FROM account WHERE id = 1", [])?;
                Ok(())
            })
            .await
            .unwrap();

        assert!(
            mapping::get_single(db.connection())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            items::get_by_relative_path(db.connection(), mapping_id, "f")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            ops::next_due(db.connection(), 9999)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            conflicts::list_unresolved(db.connection())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            cursor::get(db.connection(), mapping_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
