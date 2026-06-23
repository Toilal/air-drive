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
//! Covers the file lifecycle — **create**, **modify**, **delete** — each across
//! all four cells (12 tests total), on a shared harness: `converged` /
//! `converged_with_file` seed the baseline, `remote_create` / `remote_modify` /
//! `remote_trash` drive the remote side, and `remote_has` / `local_has` /
//! `remote_gone` assert convergence. Further scenarios (rename, nested dirs)
//! plug into the same shape.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{ChangeEntry, DaemonProcess, DriveMock, FsFixture, fs_fixture, hex_md5, wait_until};

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
// modify — local-origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modify_local_live() {
    let (mock, root_id, fx, _id) = converged_with_file("m.txt", b"v0").await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    std::fs::write(fx.local_dir.join("m.txt"), b"v1-local-live").unwrap();

    assert!(
        wait_until(T, || async {
            remote_has(&mock, &root_id, "m.txt", b"v1-local-live")
        })
        .await,
        "local modify (live) should update Drive; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn modify_local_startup() {
    let (mock, root_id, fx, _id) = converged_with_file("m.txt", b"v0").await;

    std::fs::write(fx.local_dir.join("m.txt"), b"v1-local-startup").unwrap();
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async {
            remote_has(&mock, &root_id, "m.txt", b"v1-local-startup")
        })
        .await,
        "local modify (startup) should update Drive via the startup scan; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// modify — remote-origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modify_remote_live() {
    let (mock, _root_id, fx, id) = converged_with_file("m.txt", b"v0").await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    remote_modify(&mock, &id, b"v1-remote-live");

    assert!(
        wait_until(T, || async { local_has(&fx, "m.txt", b"v1-remote-live") }).await,
        "remote modify (live) should update local; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn modify_remote_startup() {
    let (mock, _root_id, fx, id) = converged_with_file("m.txt", b"v0").await;

    remote_modify(&mock, &id, b"v1-remote-startup");
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async {
            local_has(&fx, "m.txt", b"v1-remote-startup")
        })
        .await,
        "remote modify (startup) should update local; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// delete — local-origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_local_live() {
    let (mock, _root_id, fx, id) = converged_with_file("d.txt", b"bye").await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    std::fs::remove_file(fx.local_dir.join("d.txt")).unwrap();

    assert!(
        wait_until(T, || async { remote_gone(&mock, &id) }).await,
        "local delete (live) should remove the remote file; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn delete_local_startup() {
    let (mock, _root_id, fx, id) = converged_with_file("d.txt", b"bye").await;

    std::fs::remove_file(fx.local_dir.join("d.txt")).unwrap();
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async { remote_gone(&mock, &id) }).await,
        "local delete (startup) should remove the remote file via the startup scan; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// delete — remote-origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_remote_live() {
    let (mock, _root_id, fx, id) = converged_with_file("d.txt", b"bye").await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    remote_trash(&mock, &id);

    assert!(
        wait_until(T, || async { !fx.local_dir.join("d.txt").exists() }).await,
        "remote trash (live) should remove the local file; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn delete_remote_startup() {
    let (mock, _root_id, fx, id) = converged_with_file("d.txt", b"bye").await;

    remote_trash(&mock, &id);
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async { !fx.local_dir.join("d.txt").exists() }).await,
        "remote trash (startup) should remove the local file; alive? {:?}",
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
    seed_converged(&fx, &root_id, &[]);
    (mock, root_id, fx)
}

/// Like [`converged`] but with a single top-level file already synced on both
/// sides (local copy + Drive copy + a `synced` row). Returns its Drive id so
/// `modify` / `delete` scenarios can mutate the remote side. `rel` is a plain
/// file name (top-level).
async fn converged_with_file(rel: &str, content: &[u8]) -> (DriveMock, String, FsFixture, String) {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let remote_id = mock.insert_file(Some(&root_id), rel, content);
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    fx.populate_local(&[(rel, content)]);
    seed_converged(&fx, &root_id, &[(rel, &remote_id, content)]);
    (mock, root_id, fx, remote_id)
}

/// Replace a remote file's bytes and log the change (a remote-side edit).
fn remote_modify(mock: &DriveMock, remote_id: &str, new_content: &[u8]) {
    let mut st = mock.state.lock().unwrap();
    if let Some(f) = st.files.get_mut(remote_id) {
        f.content = new_content.to_vec();
        f.md5 = hex_md5(new_content);
    }
    st.change_log.push(ChangeEntry {
        file_id: remote_id.to_owned(),
        removed: false,
    });
}

/// Trash a remote file and log the change (a remote-side delete; Drive surfaces
/// a trash as a normal change with `trashed = true`).
fn remote_trash(mock: &DriveMock, remote_id: &str) {
    let mut st = mock.state.lock().unwrap();
    if let Some(f) = st.files.get_mut(remote_id) {
        f.trashed = true;
    }
    st.change_log.push(ChangeEntry {
        file_id: remote_id.to_owned(),
        removed: false,
    });
}

/// True once the remote file `remote_id` is gone (a propagated local delete
/// trashes it via the Drive API, dropping it from the mock's file map).
fn remote_gone(mock: &DriveMock, remote_id: &str) -> bool {
    !mock.state.lock().unwrap().files.contains_key(remote_id)
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
/// mapping as already initialised (no first-time reconciliation on start), plus
/// a `synced` `sync_item` row for each `(relative_path, remote_id, content)` —
/// the already-converged files the `modify` / `delete` scenarios act on.
fn seed_converged(fx: &FsFixture, root_id: &str, files: &[(&str, &str, &[u8])]) {
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
    for (rel, remote_id, content) in files {
        conn.execute(
            "INSERT INTO sync_item \
                (mapping_id, relative_path, kind, remote_id, size, md5, local_inode, \
                 last_synced_at, state) \
             VALUES (1, ?1, 'file', ?2, ?3, ?4, NULL, 0, 'synced')",
            rusqlite::params![rel, remote_id, content.len() as i64, hex_md5(content)],
        )
        .unwrap();
    }
}
