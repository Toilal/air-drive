//! `account` table — the linked Google Drive account.
//!
//! There is exactly one account row in this MVP (`id = 1`); `upsert` enforces that
//! invariant by always writing to id 1. Multi-account support is a later feature, at
//! which point this module gains a key parameter.

use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::Result;

/// Strongly-typed primary key for an [`Account`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountId(pub i64);

/// Snapshot of the `account` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Primary key (always `AccountId(1)` in this MVP).
    pub id: AccountId,
    /// Email captured from Drive's `about.user.emailAddress`.
    pub email: String,
    /// Unix epoch seconds, set at first insert.
    pub created_at: i64,
    /// Unix epoch seconds, refreshed on each successful re-link.
    pub linked_at: i64,
}

/// Insert or update the singleton account row. Always operates on id = 1.
pub async fn upsert(conn: &Connection, email: &str, linked_at: i64) -> Result<AccountId> {
    let email = email.to_owned();
    conn.call(move |c| {
        c.execute(
            "INSERT INTO account (id, email, created_at, linked_at)
             VALUES (1, ?1, ?2, ?2)
             ON CONFLICT(id) DO UPDATE
                SET email = excluded.email, linked_at = excluded.linked_at",
            params![email, linked_at],
        )?;
        Ok(AccountId(1))
    })
    .await
    .map_err(Into::into)
}

/// Read the singleton account if it exists.
pub async fn get_single(conn: &Connection) -> Result<Option<Account>> {
    conn.call(|c| {
        let res = c.query_row(
            "SELECT id, email, created_at, linked_at FROM account WHERE id = 1",
            [],
            |row| {
                Ok(Account {
                    id: AccountId(row.get(0)?),
                    email: row.get(1)?,
                    created_at: row.get(2)?,
                    linked_at: row.get(3)?,
                })
            },
        );
        match res {
            Ok(a) => Ok(Some(a)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

/// Refresh the `linked_at` timestamp after a successful re-link.
pub async fn touch_linked_at(conn: &Connection, id: AccountId, linked_at: i64) -> Result<()> {
    conn.call(move |c| {
        c.execute(
            "UPDATE account SET linked_at = ?1 WHERE id = ?2",
            params![linked_at, id.0],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::open_temp;

    #[tokio::test]
    async fn upsert_then_get() {
        let (_tmp, db) = open_temp().await;
        let id = upsert(db.connection(), "alice@gmail.com", 1234)
            .await
            .unwrap();
        assert_eq!(id, AccountId(1));

        let a = get_single(db.connection()).await.unwrap().unwrap();
        assert_eq!(a.id, AccountId(1));
        assert_eq!(a.email, "alice@gmail.com");
        assert_eq!(a.created_at, 1234);
        assert_eq!(a.linked_at, 1234);
    }

    #[tokio::test]
    async fn upsert_replaces_existing_row() {
        let (_tmp, db) = open_temp().await;
        upsert(db.connection(), "alice@gmail.com", 1234)
            .await
            .unwrap();
        upsert(db.connection(), "bob@gmail.com", 5678)
            .await
            .unwrap();

        let a = get_single(db.connection()).await.unwrap().unwrap();
        assert_eq!(a.email, "bob@gmail.com");
        // created_at is preserved by ON CONFLICT DO UPDATE.
        assert_eq!(a.created_at, 1234);
        assert_eq!(a.linked_at, 5678);
    }

    #[tokio::test]
    async fn get_returns_none_when_empty() {
        let (_tmp, db) = open_temp().await;
        assert!(get_single(db.connection()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn touch_linked_at_updates() {
        let (_tmp, db) = open_temp().await;
        let id = upsert(db.connection(), "alice@gmail.com", 1).await.unwrap();
        touch_linked_at(db.connection(), id, 999).await.unwrap();
        let a = get_single(db.connection()).await.unwrap().unwrap();
        assert_eq!(a.linked_at, 999);
        assert_eq!(a.created_at, 1);
    }
}
