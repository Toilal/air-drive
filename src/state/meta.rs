//! `state_meta` (singleton row, id = 1) — daemon-level state surfaced by
//! `air-drive status`. Two fields today:
//!
//! - **blocked** — set by any worker (dispatcher, poller) when it hits an
//!   error it can't make progress past. Terminal kinds need user action: an
//!   OAuth refresh failure (`auth`, cleared on `air-drive link`), the watched
//!   remote folder disappearing (`remote`), or the local mapping target
//!   becoming unreadable (`mapping`). The `transient` kind is **recoverable**:
//!   the poller sets it when Drive is briefly unreachable (after the HTTP
//!   layer's own retries) and clears it on the next successful Drive call, so
//!   `air-drive status` can tell "blocked, act now" from "hiccup, recovered".
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
    /// OAuth refresh failed — user must re-link.
    Auth,
    /// Watched remote folder disappeared.
    Remote,
    /// Watched local path missing or unreadable.
    Mapping,
    /// Drive temporarily unreachable (network / 5xx surviving the HTTP retry
    /// budget). Recoverable: cleared automatically on the next successful Drive
    /// call. Distinct from the terminal kinds, which need user action.
    Transient,
}

impl BlockedKind {
    fn as_sql(self) -> &'static str {
        match self {
            BlockedKind::Auth => "auth",
            BlockedKind::Remote => "remote",
            BlockedKind::Mapping => "mapping",
            BlockedKind::Transient => "transient",
        }
    }

    fn from_sql(s: &str) -> Option<Self> {
        Some(match s {
            "auth" => BlockedKind::Auth,
            "remote" => BlockedKind::Remote,
            "mapping" => BlockedKind::Mapping,
            "transient" => BlockedKind::Transient,
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

/// Clear the block flag unconditionally — the daemon believes it can make
/// progress again. Called by `air-drive link` after a fresh token is on disk.
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

/// Clear the block flag ONLY if it is the recoverable [`BlockedKind::Transient`]
/// kind — the signal that Drive was briefly unreachable. Terminal kinds
/// (`auth`, `remote`, `mapping`) need explicit user action and are left in
/// place. Called on every successful Drive call by the poller and dispatcher.
/// Returns `true` if a transient block was actually cleared, so the caller can
/// log the recovery exactly once.
pub async fn clear_if_transient(conn: &Connection) -> Result<bool> {
    conn.call(|c| {
        let rows = c.execute(
            "UPDATE state_meta SET blocked_kind = NULL, blocked_message = NULL, \
                                   blocked_at = NULL \
             WHERE id = 1 AND blocked_kind = 'transient'",
            [],
        )?;
        Ok(rows > 0)
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
    async fn clear_if_transient_clears_only_transient() {
        let (_tmp, db) = open_temp().await;

        // A transient block clears and reports that it did.
        set_blocked(db.connection(), BlockedKind::Transient, "503 x3", 7)
            .await
            .unwrap();
        assert!(clear_if_transient(db.connection()).await.unwrap());
        assert!(get_blocked(db.connection()).await.unwrap().is_none());

        // A terminal auth block is left untouched (needs a re-link).
        set_blocked(db.connection(), BlockedKind::Auth, "revoked", 9)
            .await
            .unwrap();
        assert!(!clear_if_transient(db.connection()).await.unwrap());
        let still = get_blocked(db.connection()).await.unwrap().unwrap();
        assert_eq!(still.kind, BlockedKind::Auth);

        // No-op when nothing is blocked.
        clear_blocked(db.connection()).await.unwrap();
        assert!(!clear_if_transient(db.connection()).await.unwrap());
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
