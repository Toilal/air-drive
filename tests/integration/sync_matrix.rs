//! Sync test matrix: each scenario across the four cells
//! `{local-origin, remote-origin} × {live, startup}`.
//!
//! - **live** — the change happens while the daemon is running (caught by the
//!   inotify watcher locally, or the `changes.list` poller remotely).
//! - **startup** — the change is made while the daemon is *stopped* and must be
//!   recovered on the next start: a remote change via the persisted change
//!   cursor, a local change via the startup local scan
//!   ([`air_drive::reconcile::startup_local_scan`]).
//!
//! The mapping is seeded already-converged (account + mapping + a `'0'` change
//! cursor), so `start` never re-runs the first-time initial reconciliation —
//! exactly the "daemon was running before, now restarted" baseline the startup
//! cells need.
//!
//! This file is the harness + the `create` scenario as the worked example;
//! `modify` / `delete` extend by the same four-cell shape, reusing
//! [`converged`], [`remote_create`]-style actors, and the `*_has` assertions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{ChangeEntry, DaemonProcess, DriveMock, FsFixture, fs_fixture, wait_until};

/// Generous convergence budget — these poll the mocked Drive REST API.
const T: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// create — local-origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_local_live() {
    let (mock, root_id, fx) = converged().await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Change while the daemon runs → inotify watcher.
    std::fs::write(fx.local_dir.join("a.txt"), b"create-local-live").unwrap();

    assert!(
        wait_until(T, || async {
            remote_has(&mock, &root_id, "a.txt", b"create-local-live")
        })
        .await,
        "local create (live) should reach Drive; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn create_local_startup() {
    let (mock, root_id, fx) = converged().await;

    // Change while the daemon is DOWN → recovered by the startup local scan.
    std::fs::write(fx.local_dir.join("a.txt"), b"create-local-startup").unwrap();
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async {
            remote_has(&mock, &root_id, "a.txt", b"create-local-startup")
        })
        .await,
        "local create (startup) should reach Drive via the startup scan; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// create — remote-origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_remote_live() {
    let (mock, root_id, fx) = converged().await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Change while the daemon runs → changes.list poller.
    remote_create(&mock, &root_id, "b.txt", b"create-remote-live");

    assert!(
        wait_until(T, || async {
            local_has(&fx, "b.txt", b"create-remote-live")
        })
        .await,
        "remote create (live) should reach local; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn create_remote_startup() {
    let (mock, root_id, fx) = converged().await;

    // Change while the daemon is DOWN → recovered by the poller from the cursor.
    remote_create(&mock, &root_id, "b.txt", b"create-remote-startup");
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async {
            local_has(&fx, "b.txt", b"create-remote-startup")
        })
        .await,
        "remote create (startup) should reach local; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Spin up a mock + fixture wired to an already-converged, initialised mapping
/// (account + mapping + `'0'` cursor, no files). Returns `(mock, root_id, fx)`.
async fn converged() -> (DriveMock, String, FsFixture) {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_converged(&fx, &root_id);
    (mock, root_id, fx)
}

/// Create a file on the mock Drive AND log it in the change feed, so a polling
/// daemon picks it up (mirrors a real Drive create surfacing in `changes.list`).
fn remote_create(mock: &DriveMock, parent_id: &str, name: &str, content: &[u8]) {
    let id = mock.insert_file(Some(parent_id), name, content);
    mock.state.lock().unwrap().change_log.push(ChangeEntry {
        file_id: id,
        removed: false,
    });
}

/// True when the mock holds `rel` under `root_id` with exactly `content`.
fn remote_has(mock: &DriveMock, root_id: &str, rel: &str, content: &[u8]) -> bool {
    mock.state
        .lock()
        .unwrap()
        .descendants(root_id)
        .iter()
        .any(|(p, f)| p == rel && f.content == content)
}

/// True when the local file `rel` exists with exactly `content`.
fn local_has(fx: &FsFixture, rel: &str, content: &[u8]) -> bool {
    std::fs::read(fx.local_dir.join(rel))
        .map(|b| b == content)
        .unwrap_or(false)
}

/// Seed account + mapping + a `'0'` change cursor so the daemon treats the
/// mapping as already initialised (no first-time reconciliation on start).
fn seed_converged(fx: &FsFixture, root_id: &str) {
    let conn = rusqlite::Connection::open(fx.state_db_path()).expect("open state.db");
    conn.execute_batch(air_drive::state::schema::BOOTSTRAP)
        .expect("bootstrap schema_version");
    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for (idx, sql) in air_drive::state::schema::MIGRATIONS.iter().enumerate() {
        let target = (idx + 1) as i64;
        if target <= current {
            continue;
        }
        conn.execute_batch(sql).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![target, 0i64],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT OR REPLACE INTO account (id, email, created_at, linked_at) VALUES (1, 'm@x', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO folder_mapping \
            (id, account_id, local_path, remote_folder_id, remote_folder_name, created_at) \
         VALUES (1, 1, ?1, ?2, NULL, 0)",
        rusqlite::params![fx.local_dir.to_string_lossy(), root_id],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO drive_change_cursor (mapping_id, page_token, updated_at) \
         VALUES (1, '0', 0)",
        [],
    )
    .unwrap();
}
