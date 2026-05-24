//! `folder_mapping` table — the local↔remote folder pair to be kept in sync.
//!
//! There is exactly one mapping row in this MVP (`id = 1`); `upsert` always writes to
//! that id. Multi-mapping support is a later feature.

use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::state::accounts::AccountId;

/// Strongly-typed primary key for a [`FolderMapping`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingId(pub i64);

/// Snapshot of the `folder_mapping` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderMapping {
    /// Primary key (always `MappingId(1)` in this MVP).
    pub id: MappingId,
    /// Owning account.
    pub account_id: AccountId,
    /// Canonicalised absolute path of the watched local folder.
    pub local_path: String,
    /// Drive file ID of the watched remote folder (immune to renames).
    pub remote_folder_id: String,
    /// Cached display name of the remote folder; may be `None`.
    pub remote_folder_name: Option<String>,
    /// Original `<remote-folder>` CLI argument passed to `air-drive map`
    /// (Drive ID, URL, or `path:` notation). Used by `air-drive start` to
    /// re-resolve and optionally recreate the remote root if it has been
    /// trashed. `None` for rows inserted before the v3 schema migration.
    pub remote_folder_spec: Option<String>,
    /// Unix epoch seconds, set at first insert.
    pub created_at: i64,
}

/// Insert or update the singleton mapping row. Always operates on id = 1.
#[allow(clippy::too_many_arguments)]
pub async fn upsert(
    conn: &Connection,
    account_id: AccountId,
    local_path: &str,
    remote_folder_id: &str,
    remote_folder_name: Option<&str>,
    remote_folder_spec: Option<&str>,
    now: i64,
) -> Result<MappingId> {
    let local_path = local_path.to_owned();
    let remote_folder_id = remote_folder_id.to_owned();
    let remote_folder_name = remote_folder_name.map(str::to_owned);
    let remote_folder_spec = remote_folder_spec.map(str::to_owned);
    conn.call(move |c| {
        c.execute(
            "INSERT INTO folder_mapping
                (id, account_id, local_path, remote_folder_id, remote_folder_name,
                 remote_folder_spec, created_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                 account_id         = excluded.account_id,
                 local_path         = excluded.local_path,
                 remote_folder_id   = excluded.remote_folder_id,
                 remote_folder_name = excluded.remote_folder_name,
                 remote_folder_spec = excluded.remote_folder_spec",
            params![
                account_id.0,
                local_path,
                remote_folder_id,
                remote_folder_name,
                remote_folder_spec,
                now
            ],
        )?;
        Ok(MappingId(1))
    })
    .await
    .map_err(Into::into)
}

/// Update only the `remote_folder_id` of the singleton mapping. Used after the
/// daemon recreates a missing remote root and needs to persist the new Drive
/// ID without touching the other fields (especially `remote_folder_spec`,
/// which is the source of truth for future re-resolutions).
pub async fn update_remote_folder_id(conn: &Connection, new_id: &str) -> Result<()> {
    let new_id = new_id.to_owned();
    conn.call(move |c| {
        c.execute(
            "UPDATE folder_mapping SET remote_folder_id = ?1 WHERE id = 1",
            params![new_id],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Read the singleton mapping if it exists.
pub async fn get_single(conn: &Connection) -> Result<Option<FolderMapping>> {
    conn.call(|c| {
        let res = c.query_row(
            "SELECT id, account_id, local_path, remote_folder_id, remote_folder_name,
                    remote_folder_spec, created_at
             FROM folder_mapping WHERE id = 1",
            [],
            |row| {
                Ok(FolderMapping {
                    id: MappingId(row.get(0)?),
                    account_id: AccountId(row.get(1)?),
                    local_path: row.get(2)?,
                    remote_folder_id: row.get(3)?,
                    remote_folder_name: row.get(4)?,
                    remote_folder_spec: row.get(5)?,
                    created_at: row.get(6)?,
                })
            },
        );
        match res {
            Ok(m) => Ok(Some(m)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Db;
    use crate::state::accounts;
    use crate::state::tests::open_temp;

    async fn open_temp_with_account() -> (tempfile::TempDir, Db, AccountId) {
        let (tmp, db) = open_temp().await;
        let id = accounts::upsert(db.connection(), "alice@gmail.com", 1)
            .await
            .unwrap();
        (tmp, db, id)
    }

    #[tokio::test]
    async fn upsert_then_get() {
        let (_tmp, db, account_id) = open_temp_with_account().await;
        let id = upsert(
            db.connection(),
            account_id,
            "/home/alice/Drive",
            "remote-folder-1",
            Some("My Drive / Sync"),
            Some("path:My Drive/Sync"),
            42,
        )
        .await
        .unwrap();
        assert_eq!(id, MappingId(1));

        let m = get_single(db.connection()).await.unwrap().unwrap();
        assert_eq!(m.local_path, "/home/alice/Drive");
        assert_eq!(m.remote_folder_id, "remote-folder-1");
        assert_eq!(m.remote_folder_name.as_deref(), Some("My Drive / Sync"));
        assert_eq!(m.remote_folder_spec.as_deref(), Some("path:My Drive/Sync"));
        assert_eq!(m.created_at, 42);
    }

    #[tokio::test]
    async fn upsert_replaces_existing_row() {
        let (_tmp, db, account_id) = open_temp_with_account().await;
        upsert(db.connection(), account_id, "/old", "old-id", None, None, 1)
            .await
            .unwrap();
        upsert(
            db.connection(),
            account_id,
            "/new",
            "new-id",
            Some("name"),
            Some("path:new"),
            2,
        )
        .await
        .unwrap();
        let m = get_single(db.connection()).await.unwrap().unwrap();
        assert_eq!(m.local_path, "/new");
        assert_eq!(m.remote_folder_id, "new-id");
        assert_eq!(m.remote_folder_name.as_deref(), Some("name"));
        assert_eq!(m.remote_folder_spec.as_deref(), Some("path:new"));
    }

    #[tokio::test]
    async fn update_remote_folder_id_only_touches_that_column() {
        let (_tmp, db, account_id) = open_temp_with_account().await;
        upsert(
            db.connection(),
            account_id,
            "/local",
            "old-id",
            Some("Sync"),
            Some("path:My Drive/Sync"),
            1,
        )
        .await
        .unwrap();

        update_remote_folder_id(db.connection(), "new-id")
            .await
            .unwrap();

        let m = get_single(db.connection()).await.unwrap().unwrap();
        assert_eq!(m.remote_folder_id, "new-id");
        assert_eq!(m.remote_folder_spec.as_deref(), Some("path:My Drive/Sync"));
        assert_eq!(m.remote_folder_name.as_deref(), Some("Sync"));
        assert_eq!(m.local_path, "/local");
    }
}
