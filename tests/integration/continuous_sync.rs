//! Integration tests for continuous bidirectional sync.
//!
//! These tests exercise the `air-drive` binary's `start` command (without
//! `--initial-sync` because the cursor is pre-seeded) and assert that on-going
//! edits flow through both directions within the latency targets.
//!
//! Convention: every test stages the daemon-runs-against-already-converged state by
//! seeding `account`, `folder_mapping`, `drive_change_cursor`, and `sync_item` rows
//! before spawning the daemon. The daemon then enters the continuous loop (no
//! initial reconciliation needed).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::{DaemonProcess, DriveMock, FsFixture, fs_fixture, wait_until};

const T_LOCAL_TO_REMOTE: Duration = Duration::from_secs(15);
const T_REMOTE_TO_LOCAL: Duration = Duration::from_secs(60);
const T_RECOVERY: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// local create propagates to Drive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_1_local_create_propagates() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Create a brand-new local file. The daemon's watcher should pick it up,
    // debounce, and enqueue an upload.
    std::fs::write(fx.local_dir.join("hello.txt"), b"hello continuous").unwrap();

    let converged = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.descendants(&root_id)
            .iter()
            .any(|(p, f)| p == "hello.txt" && f.content == b"hello continuous")
    })
    .await;
    assert!(
        converged,
        "hello.txt should have reached Drive within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// local modify propagates (md5 changes on Drive)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_1_local_modify_propagates() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let original = b"v1 -- original";
    let remote_id = mock.insert_file(Some(&root_id), "report.txt", original);

    let fx = fs_fixture();
    fx.populate_local(&[("report.txt", original)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "report.txt",
            remote_id: &remote_id,
            content: original,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    let new_content = b"v2 -- edited locally";
    std::fs::write(fx.local_dir.join("report.txt"), new_content).unwrap();

    let new_md5 = common::hex_md5(new_content);
    let converged = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.files
            .get(&remote_id)
            .map(|f| f.md5 == new_md5)
            .unwrap_or(false)
    })
    .await;
    assert!(
        converged,
        "Drive should see the new md5 within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// local delete propagates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_1_local_delete_propagates() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let content = b"about to disappear";
    let remote_id = mock.insert_file(Some(&root_id), "ephemeral.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("ephemeral.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "ephemeral.txt",
            remote_id: &remote_id,
            content,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    std::fs::remove_file(fx.local_dir.join("ephemeral.txt")).unwrap();

    let gone = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        !st.files.contains_key(&remote_id)
    })
    .await;
    assert!(
        gone,
        "remote file should be trashed within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// local rename propagates via `moveto`, NOT a re-upload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_3_local_rename_uses_moveto_not_reupload() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let content = b"renaming test payload";
    let remote_id = mock.insert_file(Some(&root_id), "old-name.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("old-name.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "old-name.txt",
            remote_id: &remote_id,
            content,
        }],
    );

    let uploads_before = mock.upload_count().await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    std::fs::rename(
        fx.local_dir.join("old-name.txt"),
        fx.local_dir.join("new-name.txt"),
    )
    .unwrap();

    let renamed = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.files
            .get(&remote_id)
            .map(|f| f.name == "new-name.txt")
            .unwrap_or(false)
    })
    .await;
    assert!(
        renamed,
        "rename should reach Drive within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );

    let uploads_after = mock.upload_count().await;
    assert_eq!(
        uploads_after, uploads_before,
        "a rename MUST NOT trigger files.create — that would re-upload the bytes. \
         before={uploads_before}, after={uploads_after}"
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// moving a file across subfolders propagates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_4_subfolder_move_propagates() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let dir_a = mock.insert_folder(Some(&root_id), "A");
    let dir_b = mock.insert_folder(Some(&root_id), "B");
    let content = b"moving payload";
    let remote_id = mock.insert_file(Some(&dir_a), "movable.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("A/movable.txt", content)]);
    std::fs::create_dir_all(fx.local_dir.join("B")).unwrap();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state_with_dirs(
        &fx,
        &mock,
        &root_id,
        &[("A", dir_a.as_str()), ("B", dir_b.as_str())],
        &[SyncedItem {
            relative_path: "A/movable.txt",
            remote_id: &remote_id,
            content,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    std::fs::rename(
        fx.local_dir.join("A/movable.txt"),
        fx.local_dir.join("B/movable.txt"),
    )
    .unwrap();

    let moved = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.files
            .get(&remote_id)
            .map(|f| f.parent_id.as_deref() == Some(dir_b.as_str()))
            .unwrap_or(false)
    })
    .await;
    assert!(
        moved,
        "subfolder move should reach Drive within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// remote create propagates locally
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_2_remote_create_propagates_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    let payload = b"appeared on Drive while daemon was running";
    mock.insert_file(Some(&root_id), "remote-create.txt", payload);

    let appeared = wait_until(T_REMOTE_TO_LOCAL, || async {
        std::fs::read(fx.local_dir.join("remote-create.txt"))
            .map(|got| got == payload)
            .unwrap_or(false)
    })
    .await;
    assert!(
        appeared,
        "remote create should land locally within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// nested remote create propagates to the matching local subfolder
// (regression: the poller used to flatten the path to `<file_name>` instead
// of walking the parent chain back to the watched root)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_2_nested_remote_create_propagates_with_full_path() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let docs = mock.insert_folder(Some(&root_id), "docs");
    let notes = mock.insert_folder(Some(&docs), "notes");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Create a file two levels deep on Drive. It must land at
    // `<local_dir>/docs/notes/alpha.md`, NOT `<local_dir>/alpha.md`.
    let payload = b"deep payload";
    mock.insert_file(Some(&notes), "alpha.md", payload);

    let landed = wait_until(T_REMOTE_TO_LOCAL, || async {
        std::fs::read(fx.local_dir.join("docs/notes/alpha.md"))
            .map(|got| got == payload)
            .unwrap_or(false)
    })
    .await;
    assert!(
        landed,
        "nested remote create should land at docs/notes/alpha.md within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );

    // And NOT at the flattened path.
    assert!(
        !fx.local_dir.join("alpha.md").exists(),
        "regression: file ended up flattened at the root instead of under docs/notes/"
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// remote modify propagates locally
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_2_remote_modify_propagates_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let original = b"v1";
    let remote_id = mock.insert_file(Some(&root_id), "doc.txt", original);

    let fx = fs_fixture();
    fx.populate_local(&[("doc.txt", original)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "doc.txt",
            remote_id: &remote_id,
            content: original,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Edit the file on the mock — simulates a Drive web-UI edit.
    let new_payload = b"v2 -- modified on Drive";
    {
        let mut st = mock.state.lock().unwrap();
        let f = st.files.get_mut(&remote_id).unwrap();
        f.content = new_payload.to_vec();
        f.md5 = common::hex_md5(new_payload);
        let id = f.id.clone();
        st.change_log.push(common::ChangeEntry {
            file_id: id,
            removed: false,
        });
    }

    let updated = wait_until(T_REMOTE_TO_LOCAL, || async {
        std::fs::read(fx.local_dir.join("doc.txt"))
            .map(|got| got == new_payload)
            .unwrap_or(false)
    })
    .await;
    assert!(
        updated,
        "local file should reflect Drive edit within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// remote delete propagates locally
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_2_remote_delete_propagates_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let content = b"existed once";
    let remote_id = mock.insert_file(Some(&root_id), "doomed.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("doomed.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "doomed.txt",
            remote_id: &remote_id,
            content,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Delete on Drive directly — simulates "trashed in web UI".
    {
        let mut st = mock.state.lock().unwrap();
        st.files.remove(&remote_id);
        st.change_log.push(common::ChangeEntry {
            file_id: remote_id.clone(),
            removed: true,
        });
    }

    let gone = wait_until(T_REMOTE_TO_LOCAL, || async {
        !fx.local_dir.join("doomed.txt").exists()
    })
    .await;
    assert!(
        gone,
        "local file should be removed within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// network drop, events queue, queue drains on recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_5_drop_and_recover_drains_queue() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Inject ~30 s worth of failures (5 requests with the dispatcher's 1 → 16 s
    // exponential backoff sums to ~31 s). The spec's drop-and-recover target is
    // 30 s; larger budgets would hit the dispatcher's MAX_ATTEMPTS guard and
    // abandon the op for an hour, defeating the test's intent.
    mock.fail_next_n(5);

    std::fs::write(fx.local_dir.join("queued-a.txt"), b"A").unwrap();
    std::fs::write(fx.local_dir.join("queued-b.txt"), b"B").unwrap();

    // While failures last we must see no file uploaded yet (or at most the bytes
    // that snuck through before the budget kicked in — we don't assert on this).

    // Wait a bit, then check that the queue has accumulated. We don't have a
    // clean way to inspect `pending_operation` from the test side without
    // re-implementing the rusqlite path, but the assertion below covers
    // convergence, which implicitly proves the queue drained.

    // After the budget exhausts, ops should retry and succeed. We allow ample
    // time because exponential backoff (1 → 16 s with jitter) means even a
    // moderate failure run takes a while.
    let drained = wait_until(T_RECOVERY, || async {
        let st = mock.state.lock().unwrap();
        let names: Vec<String> = st
            .descendants(&root_id)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        names.iter().any(|n| n == "queued-a.txt") && names.iter().any(|n| n == "queued-b.txt")
    })
    .await;
    assert!(
        drained,
        "both queued files should converge once the mock is healthy; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// empty directory create propagates to Drive (folders as persistent items)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_6_local_empty_dir_create_propagates() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // An *empty* directory — no file inside to drag it along.
    std::fs::create_dir(fx.local_dir.join("newdir")).unwrap();

    let created = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.files.values().any(|f| {
            f.is_folder() && f.name == "newdir" && f.parent_id.as_deref() == Some(root_id.as_str())
        })
    })
    .await;
    assert!(
        created,
        "empty dir should appear on Drive within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// empty directory created on Drive propagates to a local mkdir
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_6_remote_empty_dir_create_propagates_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    mock.insert_folder(Some(&root_id), "remotedir");

    let appeared = wait_until(T_REMOTE_TO_LOCAL, || async {
        fx.local_dir.join("remotedir").is_dir()
    })
    .await;
    assert!(
        appeared,
        "remote empty dir should land locally within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// deleting a synced (empty) directory locally trashes the Drive folder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_6_local_dir_delete_propagates() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Create the dir and let it converge to Drive (so it gets a persisted
    // sync_item with a remote_id to anchor the delete to).
    std::fs::create_dir(fx.local_dir.join("doomeddir")).unwrap();
    let created = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.files
            .values()
            .any(|f| f.is_folder() && f.name == "doomeddir")
    })
    .await;
    assert!(
        created,
        "precondition: dir reached Drive; alive? {:?}",
        daemon.poll_alive()
    );

    // Now remove it locally — the delete must propagate to Drive.
    std::fs::remove_dir(fx.local_dir.join("doomeddir")).unwrap();
    let gone = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        !st.files
            .values()
            .any(|f| f.is_folder() && f.name == "doomeddir")
    })
    .await;
    assert!(
        gone,
        "remote folder should be trashed within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// a folder trashed on Drive is removed locally
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_6_remote_dir_delete_propagates_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Create a remote folder; let the daemon mirror it locally.
    let dir_id = mock.insert_folder(Some(&root_id), "remotedir");
    let appeared = wait_until(T_REMOTE_TO_LOCAL, || async {
        fx.local_dir.join("remotedir").is_dir()
    })
    .await;
    assert!(
        appeared,
        "precondition: remote dir mirrored locally; alive? {:?}",
        daemon.poll_alive()
    );

    // Trash it on Drive — the local directory must be removed.
    {
        let mut st = mock.state.lock().unwrap();
        st.files.remove(&dir_id);
        st.change_log.push(common::ChangeEntry {
            file_id: dir_id.clone(),
            removed: true,
        });
    }

    let gone = wait_until(T_REMOTE_TO_LOCAL, || async {
        !fx.local_dir.join("remotedir").exists()
    })
    .await;
    assert!(
        gone,
        "local dir should be removed within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// trashing a NON-EMPTY folder on Drive removes the whole subtree locally
// (exercises the dispatcher's recursive remove_dir_all on DeleteLocal/dir)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_6_remote_nonempty_dir_delete_removes_subtree_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // A folder with a file inside, both created on Drive and mirrored locally.
    let dir_id = mock.insert_folder(Some(&root_id), "box");
    mock.insert_file(Some(&dir_id), "inside.txt", b"contents");

    let mirrored = wait_until(T_REMOTE_TO_LOCAL, || async {
        fx.local_dir.join("box/inside.txt").is_file()
    })
    .await;
    assert!(
        mirrored,
        "precondition: box/inside.txt mirrored locally; alive? {:?}",
        daemon.poll_alive()
    );

    // Trash only the FOLDER on Drive (not the child) — the local side must remove
    // the whole subtree, child included, via remove_dir_all.
    {
        let mut st = mock.state.lock().unwrap();
        st.files.remove(&dir_id);
        st.change_log.push(common::ChangeEntry {
            file_id: dir_id.clone(),
            removed: true,
        });
    }

    let gone = wait_until(T_REMOTE_TO_LOCAL, || async {
        !fx.local_dir.join("box").exists()
    })
    .await;
    assert!(
        gone,
        "the non-empty local dir (and its child) should be removed within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Seeding helpers — applied via sync rusqlite using the production schema.
// ---------------------------------------------------------------------------

struct SyncedItem<'a> {
    relative_path: &'a str,
    remote_id: &'a str,
    content: &'a [u8],
}

/// Seed account + mapping + drive_change_cursor (so the daemon skips initial-sync)
/// + sync_item rows for any files that should already be "synced".
fn seed_synced_state(fx: &FsFixture, mock: &DriveMock, root_id: &str, items: &[SyncedItem<'_>]) {
    seed_synced_state_with_dirs(fx, mock, root_id, &[], items);
}

/// Same as [`seed_synced_state`] but also registers known subdirectory mappings so
/// the reconciler's "remote folder id by relative path" cache starts populated.
fn seed_synced_state_with_dirs(
    fx: &FsFixture,
    _mock: &DriveMock,
    root_id: &str,
    _known_dirs: &[(&str, &str)],
    items: &[SyncedItem<'_>],
) {
    let path = fx.state_db_path();
    let conn = rusqlite::Connection::open(&path).expect("open state.db");
    // Bootstrap + apply migrations via the production constants.
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
        "INSERT OR REPLACE INTO account (id, email, created_at, linked_at) \
         VALUES (1, 'cs@example.com', 0, 0)",
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
    // Seeding the cursor with the placeholder "0" tells the daemon "initial sync
    // is already done"; the change poller will fetch newer changes from there.
    conn.execute(
        "INSERT OR REPLACE INTO drive_change_cursor \
            (mapping_id, page_token, updated_at) VALUES (1, '0', 0)",
        [],
    )
    .unwrap();

    for item in items {
        let (size, md5) = (item.content.len() as i64, common::hex_md5(item.content));
        conn.execute(
            "INSERT INTO sync_item (mapping_id, relative_path, kind, remote_id, size, md5, \
                                    local_inode, last_synced_at, state) \
             VALUES (1, ?1, 'file', ?2, ?3, ?4, NULL, 0, 'synced')",
            rusqlite::params![item.relative_path, item.remote_id, size, md5],
        )
        .unwrap();
    }

    // `known_dirs` is accepted today to keep the test surface stable; the daemon
    // discovers subfolder IDs at runtime via its own walk + cache (see
    // `reconcile::ensure_remote_folder`), so we don't need to seed them here. The
    // unused-binding helper below silences the warning without making the helper
    // signature less convenient to call from tests.
    let _ = (_known_dirs, fx.local_dir.as_path() as &Path);
}
