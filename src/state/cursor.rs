//! `drive_change_cursor` table — the `pageToken` we hand to Drive `changes.list` next.

use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::state::mapping::MappingId;

/// Read the persisted page token for the given mapping, if any.
pub async fn get(conn: &Connection, mapping_id: MappingId) -> Result<Option<String>> {
    conn.call(move |c| {
        let res = c.query_row(
            "SELECT page_token FROM drive_change_cursor WHERE id = 1 AND mapping_id = ?1",
            params![mapping_id.0],
            |row| row.get::<_, String>(0),
        );
        match res {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

/// Insert or update the singleton cursor for the given mapping.
pub async fn set(
    conn: &Connection,
    mapping_id: MappingId,
    page_token: &str,
    now: i64,
) -> Result<()> {
    let page_token = page_token.to_owned();
    conn.call(move |c| {
        c.execute(
            "INSERT INTO drive_change_cursor (id, mapping_id, page_token, updated_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 mapping_id = excluded.mapping_id,
                 page_token = excluded.page_token,
                 updated_at = excluded.updated_at",
            params![mapping_id.0, page_token, now],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
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
        let mapping_id =
            mapping::upsert(db.connection(), account_id, "/home/alice", "rid", None, 1)
                .await
                .unwrap();
        (tmp, db, mapping_id)
    }

    #[tokio::test]
    async fn get_is_empty_initially() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        assert!(get(db.connection(), mapping_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let (_tmp, db, mapping_id) = open_temp_with_mapping().await;
        set(db.connection(), mapping_id, "token-1", 100)
            .await
            .unwrap();
        assert_eq!(
            get(db.connection(), mapping_id).await.unwrap().as_deref(),
            Some("token-1")
        );

        set(db.connection(), mapping_id, "token-2", 200)
            .await
            .unwrap();
        assert_eq!(
            get(db.connection(), mapping_id).await.unwrap().as_deref(),
            Some("token-2")
        );
    }
}
