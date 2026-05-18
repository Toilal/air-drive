//! `state_meta` (singleton row, id = 1) — daemon-level state surfaced by
//! `air-drive status`. Two fields today:
//!
//! - **blocked** — set by any worker (dispatcher, poller) when it hits a
//!   non-recoverable error like an OAuth refresh failure (`auth`), the
//!   watched remote folder disappearing (`remote`), or the local mapping
//!   target becoming unreadable (`mapping`). Cleared on `air-drive link`
//!   (auth) or on user resolution.
//! - **last_sync** — bumped by the dispatcher after each successful sync
//!   cycle so the status surface can report "last successful sync at X".
//!
//! Both are persisted because `air-drive status` runs as a separate process
//! from the daemon; the control socket only carries liveness, not state.

use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::Result;

/// Kinds that map to the JSON schema's `last_error.kind` enum. We only emit
/// the subset that the daemon can self-detect; HTTP-derived kinds (`transient`,
/// `quota`, `io`, etc.) live in the schema for future use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    /// OAuth refresh failed — user must re-link (FR-009).
    Auth,
    /// Watched remote folder disappeared (FR-020).
    Remote,
    /// Watched local path missing or unreadable (FR-023).
    Mapping,
}

impl BlockedKind {
    fn as_sql(self) -> &'static str {
        match self {
            BlockedKind::Auth => "auth",
            BlockedKind::Remote => "remote",
            BlockedKind::Mapping => "mapping",
        }
    }

    fn from_sql(s: &str) -> Option<Self> {
        Some(match s {
            "auth" => BlockedKind::Auth,
            "remote" => BlockedKind::Remote,
            "mapping" => BlockedKind::Mapping,
            _ => return None,
        })
    }
}

/// Snapshot of the blocked fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// Why we're blocked.
    pub kind: BlockedKind,
    /// One-line human-readable explanation.
    pub message: String,
    /// Unix epoch seconds at which the block was registered.
    pub at: i64,
}

/// Read the blocked snapshot, if any. Returns `None` when the row's
/// `blocked_kind` column is `NULL` (the "healthy" state).
pub async fn get_blocked(conn: &Connection) -> Result<Option<Blocked>> {
    conn.call(|c| {
        let res = c.query_row(
            "SELECT blocked_kind, blocked_message, blocked_at FROM state_meta WHERE id = 1",
            [],
            |row| {
                let kind: Option<String> = row.get(0)?;
                let message: Option<String> = row.get(1)?;
                let at: Option<i64> = row.get(2)?;
                Ok((kind, message, at))
            },
        );
        match res {
            Ok((Some(kind), Some(message), Some(at))) => match BlockedKind::from_sql(&kind) {
                Some(k) => Ok(Some(Blocked {
                    kind: k,
                    message,
                    at,
                })),
                None => Ok(None),
            },
            Ok(_) => Ok(None),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

/// Mark the daemon as blocked. Idempotent: subsequent calls overwrite the
/// previous reason (good — most-recent block is the most actionable one).
pub async fn set_blocked(
    conn: &Connection,
    kind: BlockedKind,
    message: &str,
    at: i64,
) -> Result<()> {
    let message = message.to_owned();
    conn.call(move |c| {
        c.execute(
            "UPDATE state_meta SET blocked_kind = ?1, blocked_message = ?2, blocked_at = ?3 \
             WHERE id = 1",
            params![kind.as_sql(), message, at],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Clear the block flag — the daemon believes it can make progress again.
/// Called by `air-drive link` after a fresh token is on disk, and at every
/// successful Drive call after a transient hiccup (TODO: not wired yet —
/// today the daemon stays blocked until the user re-links).
pub async fn clear_blocked(conn: &Connection) -> Result<()> {
    conn.call(|c| {
        c.execute(
            "UPDATE state_meta SET blocked_kind = NULL, blocked_message = NULL, \
                                   blocked_at = NULL WHERE id = 1",
            [],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Bump the `last_sync_at` cursor. Called by the dispatcher each time it
/// successfully drains the pending queue. `items_uploaded` / `items_downloaded`
/// counters are incremented for `air-drive status`'s `last_sync` section.
pub async fn record_sync_cycle(
    conn: &Connection,
    at: i64,
    delta_uploaded: i64,
    delta_downloaded: i64,
) -> Result<()> {
    conn.call(move |c| {
        c.execute(
            "UPDATE state_meta SET last_sync_at = ?1, \
                                   items_uploaded = items_uploaded + ?2, \
                                   items_downloaded = items_downloaded + ?3 \
             WHERE id = 1",
            params![at, delta_uploaded, delta_downloaded],
        )?;
        Ok(())
    })
    .await
    .map_err(Into::into)
}

/// Read the (last_sync_at, items_uploaded, items_downloaded) triple. Returns
/// `(None, 0, 0)` when no sync has completed yet.
pub async fn last_sync(conn: &Connection) -> Result<(Option<i64>, i64, i64)> {
    conn.call(|c| {
        let res = c.query_row(
            "SELECT last_sync_at, items_uploaded, items_downloaded FROM state_meta WHERE id = 1",
            [],
            |row| {
                let at: Option<i64> = row.get(0)?;
                let up: i64 = row.get(1)?;
                let down: i64 = row.get(2)?;
                Ok((at, up, down))
            },
        );
        match res {
            Ok(t) => Ok(t),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((None, 0, 0)),
            Err(e) => Err(e.into()),
        }
    })
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::open_temp;

    #[tokio::test]
    async fn get_blocked_returns_none_initially() {
        let (_tmp, db) = open_temp().await;
        assert!(get_blocked(db.connection()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let (_tmp, db) = open_temp().await;
        set_blocked(db.connection(), BlockedKind::Auth, "refresh failed", 1234)
            .await
            .unwrap();
        let b = get_blocked(db.connection()).await.unwrap().unwrap();
        assert_eq!(b.kind, BlockedKind::Auth);
        assert_eq!(b.message, "refresh failed");
        assert_eq!(b.at, 1234);
    }

    #[tokio::test]
    async fn clear_blocked_resets() {
        let (_tmp, db) = open_temp().await;
        set_blocked(db.connection(), BlockedKind::Remote, "gone", 99)
            .await
            .unwrap();
        clear_blocked(db.connection()).await.unwrap();
        assert!(get_blocked(db.connection()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn last_sync_increments_counters() {
        let (_tmp, db) = open_temp().await;
        record_sync_cycle(db.connection(), 100, 3, 1).await.unwrap();
        record_sync_cycle(db.connection(), 200, 2, 5).await.unwrap();
        let (at, up, down) = last_sync(db.connection()).await.unwrap();
        assert_eq!(at, Some(200));
        assert_eq!(up, 5);
        assert_eq!(down, 6);
    }
}
