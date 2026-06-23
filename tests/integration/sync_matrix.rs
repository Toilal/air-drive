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
//! all four cells (12 tests), plus a **combined** batch (`combined_*`) that
//! applies all five operation kinds at once — create, modify, delete, file
//! rename, and folder rename — across the same four cells (4 tests), proving the
//! operations compose without interfering. Shared harness: `converged` /
//! `converged_with_file` / `combined_baseline` seed the baseline, `remote_create`
//! / `remote_modify` / `remote_trash` / `remote_rename` drive the remote side,
//! and `remote_has` / `local_has` / `remote_gone` / `*_tree_is` assert
//! convergence.

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
// combined — every operation at once, across all four cells
//
// A single converged baseline is mutated by FIVE operations in one batch:
//   1. create   new.txt
//   2. modify   mod.txt
//   3. delete   del.txt
//   4. rename   rename-src.txt -> rename-dst.txt   (file rename)
//   5. rename   sub/            -> sub2/            (folder rename, nested file)
// plus keep.txt left untouched as a control. Every cell must land on the same
// final tree (`COMBINED_EXPECTED`), proving the operations compose without
// interfering — e.g. a rename isn't mistaken for a delete+create, an echo of
// one op doesn't undo another.
// ---------------------------------------------------------------------------

/// The tree both sides must converge to after the combined batch.
const COMBINED_EXPECTED: &[(&str, &[u8])] = &[
    ("keep.txt", b"keep-untouched"),
    ("new.txt", b"new-file"),
    ("mod.txt", b"mod-v1"),
    ("rename-dst.txt", b"rename-payload"),
    ("sub2/nested.txt", b"nested-payload"),
];

#[tokio::test]
async fn combined_local_live() {
    let (mock, root_id, fx, _ids) = combined_baseline().await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    apply_local_combined(&fx);

    assert!(
        wait_until(T, || async {
            remote_tree_is(&mock, &root_id, COMBINED_EXPECTED)
        })
        .await,
        "local combined batch (live) should converge Drive to the expected tree; \
         got {:?}; alive? {:?}",
        remote_tree(&mock, &root_id),
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn combined_local_startup() {
    let (mock, root_id, fx, _ids) = combined_baseline().await;

    apply_local_combined(&fx);
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async {
            remote_tree_is(&mock, &root_id, COMBINED_EXPECTED)
        })
        .await,
        "local combined batch (startup) should converge Drive via the startup scan; \
         got {:?}; alive? {:?}",
        remote_tree(&mock, &root_id),
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn combined_remote_live() {
    let (mock, root_id, fx, ids) = combined_baseline().await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    apply_remote_combined(&mock, &root_id, &ids);

    assert!(
        wait_until(T, || async { local_tree_is(&fx, COMBINED_EXPECTED) }).await,
        "remote combined batch (live) should converge local; got {:?}; alive? {:?}",
        fx.walk_local(),
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn combined_remote_startup() {
    let (mock, root_id, fx, ids) = combined_baseline().await;

    apply_remote_combined(&mock, &root_id, &ids);
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    assert!(
        wait_until(T, || async { local_tree_is(&fx, COMBINED_EXPECTED) }).await,
        "remote combined batch (startup) should converge local via the cursor; \
         got {:?}; alive? {:?}",
        fx.walk_local(),
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

/// Drive ids of the baseline entries the combined scenario mutates remotely.
struct CombinedIds {
    mod_id: String,
    del_id: String,
    rename_id: String,
    dir_id: String,
}

/// Seed a converged baseline carrying the six entries the combined batch acts
/// on: a control file, a file to modify, a file to delete, a file to rename, and
/// a subfolder (with a nested file) to rename. Returns the mutated entries' ids.
async fn combined_baseline() -> (DriveMock, String, FsFixture, CombinedIds) {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    let keep_id = mock.insert_file(Some(&root_id), "keep.txt", b"keep-untouched");
    let mod_id = mock.insert_file(Some(&root_id), "mod.txt", b"mod-v0");
    let del_id = mock.insert_file(Some(&root_id), "del.txt", b"del-v0");
    let rename_id = mock.insert_file(Some(&root_id), "rename-src.txt", b"rename-payload");
    let dir_id = mock.insert_folder(Some(&root_id), "sub");
    let nested_id = mock.insert_file(Some(&dir_id), "nested.txt", b"nested-payload");

    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    fx.populate_local(&[
        ("keep.txt", b"keep-untouched"),
        ("mod.txt", b"mod-v0"),
        ("del.txt", b"del-v0"),
        ("rename-src.txt", b"rename-payload"),
        ("sub/nested.txt", b"nested-payload"),
    ]);
    seed_converged_with_dirs(
        &fx,
        &root_id,
        &[("sub", &dir_id)],
        &[
            ("keep.txt", &keep_id, b"keep-untouched"),
            ("mod.txt", &mod_id, b"mod-v0"),
            ("del.txt", &del_id, b"del-v0"),
            ("rename-src.txt", &rename_id, b"rename-payload"),
            ("sub/nested.txt", &nested_id, b"nested-payload"),
        ],
    );
    (
        mock,
        root_id,
        fx,
        CombinedIds {
            mod_id,
            del_id,
            rename_id,
            dir_id,
        },
    )
}

/// Apply the five-operation batch to the local tree (the local-origin cells).
fn apply_local_combined(fx: &FsFixture) {
    std::fs::write(fx.local_dir.join("new.txt"), b"new-file").unwrap();
    std::fs::write(fx.local_dir.join("mod.txt"), b"mod-v1").unwrap();
    std::fs::remove_file(fx.local_dir.join("del.txt")).unwrap();
    std::fs::rename(
        fx.local_dir.join("rename-src.txt"),
        fx.local_dir.join("rename-dst.txt"),
    )
    .unwrap();
    std::fs::rename(fx.local_dir.join("sub"), fx.local_dir.join("sub2")).unwrap();
}

/// Apply the five-operation batch to the mock Drive (the remote-origin cells).
fn apply_remote_combined(mock: &DriveMock, root_id: &str, ids: &CombinedIds) {
    remote_create(mock, root_id, "new.txt", b"new-file");
    remote_modify(mock, &ids.mod_id, b"mod-v1");
    remote_trash(mock, &ids.del_id);
    remote_rename(mock, &ids.rename_id, "rename-dst.txt");
    remote_rename(mock, &ids.dir_id, "sub2");
}

/// Rename a remote file or folder in place (same id, new name) and log the
/// change, mirroring a Drive `files.update` that only touches the name.
fn remote_rename(mock: &DriveMock, remote_id: &str, new_name: &str) {
    let mut st = mock.state.lock().unwrap();
    if let Some(f) = st.files.get_mut(remote_id) {
        f.name = new_name.to_owned();
    }
    st.change_log.push(ChangeEntry {
        file_id: remote_id.to_owned(),
        removed: false,
    });
}

/// Snapshot of the remote file tree under `root_id` as sorted `(path, bytes)`,
/// excluding trashed entries (a propagated delete trashes rather than unlinks).
fn remote_tree(mock: &DriveMock, root_id: &str) -> Vec<(String, Vec<u8>)> {
    mock.state
        .lock()
        .unwrap()
        .descendants(root_id)
        .into_iter()
        .filter(|(_, f)| !f.trashed)
        .map(|(p, f)| (p, f.content))
        .collect()
}

/// True once the remote tree matches `expected` exactly (as a set).
fn remote_tree_is(mock: &DriveMock, root_id: &str, expected: &[(&str, &[u8])]) -> bool {
    tree_eq(&remote_tree(mock, root_id), expected)
}

/// True once the local tree matches `expected` exactly (as a set).
fn local_tree_is(fx: &FsFixture, expected: &[(&str, &[u8])]) -> bool {
    tree_eq(&fx.walk_local(), expected)
}

/// Order-insensitive equality between an observed `(path, bytes)` tree and the
/// expected `(path, bytes)` set.
fn tree_eq(actual: &[(String, Vec<u8>)], expected: &[(&str, &[u8])]) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    let mut a: Vec<(&str, &[u8])> = actual
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let mut e: Vec<(&str, &[u8])> = expected.to_vec();
    a.sort();
    e.sort();
    a == e
}

/// Seed a converged baseline with files AND directory rows (the latter anchor a
/// folder rename/move). `dirs` is `(relative_path, remote_id)`; `files` is
/// `(relative_path, remote_id, content)`. Delegates the account/mapping/cursor
/// boilerplate to the same SQL as [`seed_converged`].
fn seed_converged_with_dirs(
    fx: &FsFixture,
    root_id: &str,
    dirs: &[(&str, &str)],
    files: &[(&str, &str, &[u8])],
) {
    seed_converged(fx, root_id, files);
    let conn = rusqlite::Connection::open(fx.state_db_path()).expect("open state.db");
    for (rel, remote_id) in dirs {
        conn.execute(
            "INSERT INTO sync_item \
                (mapping_id, relative_path, kind, remote_id, size, md5, local_inode, \
                 last_synced_at, state) \
             VALUES (1, ?1, 'dir', ?2, NULL, NULL, NULL, 0, 'synced')",
            rusqlite::params![rel, remote_id],
        )
        .unwrap();
    }
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
