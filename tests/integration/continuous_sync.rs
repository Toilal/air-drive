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

use common::{DaemonProcess, DriveMock, FsFixture, fs_fixture, wait_until, with_state_db};

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
// A re-delivered remote modify (the change feed hands us the same change again
// after we already applied it, before the sync_item fingerprint was persisted)
// must NOT open a spurious conflict: on disk the file already equals the remote.
// Regression for the false-positive conflict the combined startup matrix flaked
// on.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us3_3_redelivered_remote_modify_does_not_conflict() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    // Remote AND local already hold the NEW bytes ("v2"), but the sync_item still
    // remembers the OLD fingerprint ("v1") — exactly the window between a
    // Download landing on disk and the dispatcher persisting the new md5.
    let new_bytes = b"v2 -- already applied on disk";
    let remote_id = mock.insert_file(Some(&root_id), "doc.txt", new_bytes);

    let fx = fs_fixture();
    fx.populate_local(&[("doc.txt", new_bytes)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "doc.txt",
            remote_id: &remote_id,
            content: b"v1 -- stale fingerprint",
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Re-deliver the doc.txt change, plus a sentinel create. Once the sentinel
    // lands locally we know the poller processed the whole batch (incl. doc.txt).
    let sentinel_id = mock.insert_file(Some(&root_id), "sentinel.txt", b"sentinel");
    {
        let mut st = mock.state.lock().unwrap();
        st.change_log.push(common::ChangeEntry {
            file_id: remote_id.clone(),
            removed: false,
        });
        st.change_log.push(common::ChangeEntry {
            file_id: sentinel_id,
            removed: false,
        });
    }

    let processed = wait_until(T_REMOTE_TO_LOCAL, || async {
        fx.local_dir.join("sentinel.txt").exists()
    })
    .await;
    assert!(
        processed,
        "the poller should process the batch (sentinel arrives); alive? {:?}",
        daemon.poll_alive()
    );

    // No conflict sibling was created for doc.txt, and its bytes are untouched.
    let conflict = std::fs::read_dir(&fx.local_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("doc.") && n.contains(".conflict-")
        });
    assert!(
        !conflict,
        "a re-delivered remote modify whose bytes already match local must not open a conflict"
    );
    assert_eq!(
        std::fs::read(fx.local_dir.join("doc.txt")).unwrap(),
        new_bytes,
        "doc.txt content should be untouched"
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
// renaming a folder locally moves it on Drive (no re-upload) and rewrites the
// descendant paths in the DB (folder rename/move, #7)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_7_local_dir_rename_propagates_without_reupload() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let docs_id = mock.insert_folder(Some(&root_id), "docs");
    let content = b"spec payload";
    let spec_id = mock.insert_file(Some(&docs_id), "spec.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("docs/spec.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state_with_dirs(
        &fx,
        &mock,
        &root_id,
        &[("docs", docs_id.as_str())],
        &[SyncedItem {
            relative_path: "docs/spec.txt",
            remote_id: &spec_id,
            content,
        }],
    );

    let uploads_before = mock.upload_count().await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Rename the directory locally — filesystems emit a single rename event on the
    // folder, none on its descendants.
    std::fs::rename(fx.local_dir.join("docs"), fx.local_dir.join("documents")).unwrap();

    let renamed = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.files
            .get(&docs_id)
            .map(|f| f.name == "documents")
            .unwrap_or(false)
    })
    .await;
    assert!(
        renamed,
        "folder rename should reach Drive within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );

    {
        let st = mock.state.lock().unwrap();
        // The child kept its id and its parent (the folder), i.e. it moved with the
        // folder rather than being re-created.
        let spec = st
            .files
            .get(&spec_id)
            .expect("spec.txt still exists on Drive");
        assert_eq!(spec.parent_id.as_deref(), Some(docs_id.as_str()));
    }
    // No re-upload: the rename is a metadata move, not a files.create.
    assert_eq!(
        mock.upload_count().await,
        uploads_before,
        "folder rename MUST NOT re-upload the child file"
    );
    daemon.shutdown().await;

    // Descendant paths were rewritten under the new prefix.
    with_state_db(&fx, |conn| {
        let dir_kind: String = conn
            .query_row(
                "SELECT kind FROM sync_item WHERE relative_path = 'documents'",
                [],
                |r| r.get(0),
            )
            .expect("renamed dir row 'documents' should exist");
        assert_eq!(dir_kind, "dir");
        let child_remote: Option<String> = conn
            .query_row(
                "SELECT remote_id FROM sync_item WHERE relative_path = 'documents/spec.txt'",
                [],
                |r| r.get(0),
            )
            .expect("descendant row should be rewritten to documents/spec.txt");
        assert_eq!(
            child_remote.as_deref(),
            Some(spec_id.as_str()),
            "descendant kept its remote_id (no re-upload)"
        );
        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_item WHERE relative_path = 'docs' \
                 OR relative_path LIKE 'docs/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old_count, 0,
            "no rows should remain under the old 'docs' prefix"
        );
    });
}

// ---------------------------------------------------------------------------
// renaming a folder on Drive renames it locally and rewrites descendant paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_7_remote_dir_rename_propagates_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let docs_id = mock.insert_folder(Some(&root_id), "docs");
    let content = b"spec payload";
    let spec_id = mock.insert_file(Some(&docs_id), "spec.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("docs/spec.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state_with_dirs(
        &fx,
        &mock,
        &root_id,
        &[("docs", docs_id.as_str())],
        &[SyncedItem {
            relative_path: "docs/spec.txt",
            remote_id: &spec_id,
            content,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Rename the folder on Drive (web-UI style): change its name + log the change.
    {
        let mut st = mock.state.lock().unwrap();
        st.files.get_mut(&docs_id).unwrap().name = "documents".to_string();
        st.change_log.push(common::ChangeEntry {
            file_id: docs_id.clone(),
            removed: false,
        });
    }

    let renamed = wait_until(T_REMOTE_TO_LOCAL, || async {
        fx.local_dir.join("documents/spec.txt").is_file() && !fx.local_dir.join("docs").exists()
    })
    .await;
    assert!(
        renamed,
        "folder rename should land locally within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;

    // Child kept its content, and the DB rows were rewritten under the new prefix.
    assert_eq!(
        std::fs::read(fx.local_dir.join("documents/spec.txt")).unwrap(),
        content
    );
    with_state_db(&fx, |conn| {
        let child_remote: Option<String> = conn
            .query_row(
                "SELECT remote_id FROM sync_item WHERE relative_path = 'documents/spec.txt'",
                [],
                |r| r.get(0),
            )
            .expect("descendant row rewritten to documents/spec.txt");
        assert_eq!(child_remote.as_deref(), Some(spec_id.as_str()));
        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_item WHERE relative_path = 'docs' \
                 OR relative_path LIKE 'docs/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old_count, 0,
            "no rows should remain under the old 'docs' prefix"
        );
    });
}

// ---------------------------------------------------------------------------
// A remote folder rename can surface ONLY as a descendant path-change (Drive
// delays or omits the folder's own change). The child's parent id is unchanged,
// so the daemon must recognise the *folder* was renamed and move the whole
// subtree — not move the child alone (which would strand the empty old dir and
// re-upload it as a duplicate folder). Regression for the e8/#19 flake.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_7_remote_folder_rename_seen_via_child_change_only() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let docs_id = mock.insert_folder(Some(&root_id), "docs");
    let content = b"spec payload";
    let spec_id = mock.insert_file(Some(&docs_id), "spec.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("docs/spec.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state_with_dirs(
        &fx,
        &mock,
        &root_id,
        &[("docs", docs_id.as_str())],
        &[SyncedItem {
            relative_path: "docs/spec.txt",
            remote_id: &spec_id,
            content,
        }],
    );

    let uploads_before = mock.upload_count().await;
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Rename the folder on Drive, then surface ONLY the child's change — drop the
    // setup change-log so the poller can't also see the folder's own change.
    {
        let mut st = mock.state.lock().unwrap();
        st.files.get_mut(&docs_id).unwrap().name = "documents".to_string();
        st.change_log.clear();
        st.change_log.push(common::ChangeEntry {
            file_id: spec_id.clone(),
            removed: false,
        });
    }

    let renamed = wait_until(T_REMOTE_TO_LOCAL, || async {
        fx.local_dir.join("documents/spec.txt").is_file() && !fx.local_dir.join("docs").exists()
    })
    .await;
    assert!(
        renamed,
        "a folder rename seen only via the child must move the whole dir and drop \
         the old path within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );

    let uploads_after = mock.upload_count().await;
    daemon.shutdown().await;
    // The subtree move must NOT re-upload anything — that would duplicate the
    // already-renamed folder on Drive.
    assert_eq!(
        uploads_after, uploads_before,
        "moving the subtree must not trigger an upload (no duplicate folder on Drive)"
    );
    assert_eq!(
        std::fs::read(fx.local_dir.join("documents/spec.txt")).unwrap(),
        content
    );
}

// ---------------------------------------------------------------------------
// trashing a file then restoring it from Drive's trash re-links to the same row
// (no duplicate sync_item) and brings the file back at its original path (#8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_8_trash_then_restore_no_duplicate() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let content = b"restore me";
    let remote_id = mock.insert_file(Some(&root_id), "doc.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("doc.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "doc.txt",
            remote_id: &remote_id,
            content,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Trash on Drive — a real trash keeps the file (reversible) but flips
    // `trashed`; the change surfaces with removed=false + file.trashed=true.
    {
        let mut st = mock.state.lock().unwrap();
        st.files.get_mut(&remote_id).unwrap().trashed = true;
        st.change_log.push(common::ChangeEntry {
            file_id: remote_id.clone(),
            removed: false,
        });
    }
    let gone = wait_until(T_REMOTE_TO_LOCAL, || async {
        !fx.local_dir.join("doc.txt").exists()
    })
    .await;
    assert!(
        gone,
        "trash should remove local doc.txt; alive? {:?}",
        daemon.poll_alive()
    );

    // Restore from trash: clear `trashed` + log the change (untrash).
    {
        let mut st = mock.state.lock().unwrap();
        st.files.get_mut(&remote_id).unwrap().trashed = false;
        st.change_log.push(common::ChangeEntry {
            file_id: remote_id.clone(),
            removed: false,
        });
    }
    let restored = wait_until(T_REMOTE_TO_LOCAL, || async {
        std::fs::read(fx.local_dir.join("doc.txt"))
            .map(|b| b == content)
            .unwrap_or(false)
    })
    .await;
    assert!(
        restored,
        "restore should bring doc.txt back; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;

    // Exactly one row for this Drive id, at the original path, tombstone cleared.
    with_state_db(&fx, |conn| {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_item WHERE remote_id = ?1",
                rusqlite::params![remote_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "restore must not duplicate the sync_item row");
        let (path, trashed): (String, Option<i64>) = conn
            .query_row(
                "SELECT relative_path, trashed_at FROM sync_item WHERE remote_id = ?1",
                rusqlite::params![remote_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path, "doc.txt");
        assert!(
            trashed.is_none(),
            "tombstone should be cleared after restore"
        );
    });
}

// ---------------------------------------------------------------------------
// a PERMANENT delete (removed=true, loss of access) removes the local file and
// drops the row entirely — no tombstone, since there is nothing to restore (#8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_9_permanent_delete_drops_row_without_tombstone() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let content = b"gone for good";
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

    // Permanent delete / loss of access: file gone, change carries removed=true.
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
        "permanent delete should remove local file; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;

    // No tombstone: the row is gone entirely (a permanent delete isn't restorable).
    with_state_db(&fx, |conn| {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_item WHERE remote_id = ?1",
                rusqlite::params![remote_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "permanent delete must drop the row, not tombstone it"
        );
    });
}

// ---------------------------------------------------------------------------
// Startup scan replays local changes made while the daemon was DOWN — a modify,
// a create, and a delete that inotify never saw (it wasn't running). On start
// the scan diffs the local tree against sync_item and feeds the differences
// through the same pipeline, so all three reach Drive.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_10_startup_scan_replays_offline_local_changes() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    // Two files already converged on both sides.
    let keep_id = mock.insert_file(Some(&root_id), "keep.txt", b"v0");
    let gone_id = mock.insert_file(Some(&root_id), "gone.txt", b"g0");

    let fx = fs_fixture();
    fx.populate_local(&[("keep.txt", b"v0" as &[u8]), ("gone.txt", b"g0")]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[
            SyncedItem {
                relative_path: "keep.txt",
                remote_id: &keep_id,
                content: b"v0",
            },
            SyncedItem {
                relative_path: "gone.txt",
                remote_id: &gone_id,
                content: b"g0",
            },
        ],
    );

    // Changes made WHILE THE DAEMON IS DOWN (no inotify): modify, create, delete.
    std::fs::write(fx.local_dir.join("keep.txt"), b"v1-edited-offline").unwrap();
    std::fs::write(fx.local_dir.join("fresh.txt"), b"new-offline").unwrap();
    std::fs::remove_file(fx.local_dir.join("gone.txt")).unwrap();

    // Start: the startup scan must replay all three to Drive.
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    let converged = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        let d = st.descendants(&root_id);
        let modified = d
            .iter()
            .any(|(p, f)| p == "keep.txt" && f.content == b"v1-edited-offline");
        let created = d
            .iter()
            .any(|(p, f)| p == "fresh.txt" && f.content == b"new-offline");
        let deleted = !st.files.contains_key(&gone_id);
        modified && created && deleted
    })
    .await;
    assert!(
        converged,
        "startup scan should replay offline modify+create+delete within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// A file created inside a BRAND-NEW local directory propagates, even though the
// recursive inotify watch on the new dir may not be registered before the file
// lands (its own event can be lost). The Created(dir) handler rescans the dir
// and enqueues what's already inside.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_11_local_new_dir_with_nested_file_propagates() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(&fx, &mock, &root_id, &[]);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Create the directory and the file inside it back-to-back, racing the
    // recursive-watch registration.
    std::fs::create_dir(fx.local_dir.join("newdir")).unwrap();
    std::fs::write(fx.local_dir.join("newdir/note.txt"), b"nested-while-live").unwrap();

    let converged = wait_until(T_LOCAL_TO_REMOTE, || async {
        let st = mock.state.lock().unwrap();
        st.descendants(&root_id)
            .iter()
            .any(|(p, f)| p == "newdir/note.txt" && f.content == b"nested-while-live")
    })
    .await;
    assert!(
        converged,
        "newdir/note.txt should reach Drive within {T_LOCAL_TO_REMOTE:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// A file renamed on Drive (name change, same content) is renamed locally — the
// row keeps its remote id and the bytes are moved, not re-downloaded. (Before
// this, a same-md5 remote change was swallowed as an echo and the rename lost.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us2_12_remote_file_rename_propagates_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let content = b"file-rename payload";
    let file_id = mock.insert_file(Some(&root_id), "old.txt", content);

    let fx = fs_fixture();
    fx.populate_local(&[("old.txt", content)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced_state(
        &fx,
        &mock,
        &root_id,
        &[SyncedItem {
            relative_path: "old.txt",
            remote_id: &file_id,
            content,
        }],
    );

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Rename the file on Drive (name change, same bytes) + log the change.
    {
        let mut st = mock.state.lock().unwrap();
        st.files.get_mut(&file_id).unwrap().name = "new.txt".to_string();
        st.change_log.push(common::ChangeEntry {
            file_id: file_id.clone(),
            removed: false,
        });
    }

    let renamed = wait_until(T_REMOTE_TO_LOCAL, || async {
        fx.local_dir.join("new.txt").is_file() && !fx.local_dir.join("old.txt").exists()
    })
    .await;
    assert!(
        renamed,
        "remote file rename should land locally within {T_REMOTE_TO_LOCAL:?}; alive? {:?}",
        daemon.poll_alive()
    );
    daemon.shutdown().await;

    // The bytes were moved (not re-downloaded), and the row kept its remote id
    // under the new path.
    assert_eq!(
        std::fs::read(fx.local_dir.join("new.txt")).unwrap(),
        content
    );
    with_state_db(&fx, |conn| {
        let remote: Option<String> = conn
            .query_row(
                "SELECT remote_id FROM sync_item WHERE relative_path = 'new.txt'",
                [],
                |r| r.get(0),
            )
            .expect("row rewritten to new.txt");
        assert_eq!(remote.as_deref(), Some(file_id.as_str()));
    });
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
    known_dirs: &[(&str, &str)],
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

    // Seed known subdirectories as persisted kind='dir' rows with their Drive id,
    // mirroring what the daemon records for folders (needed to anchor a folder
    // rename/move — issue #7).
    for (rel, remote_id) in known_dirs {
        conn.execute(
            "INSERT INTO sync_item (mapping_id, relative_path, kind, remote_id, size, md5, \
                                    local_inode, last_synced_at, state) \
             VALUES (1, ?1, 'dir', ?2, NULL, NULL, NULL, 0, 'synced')",
            rusqlite::params![rel, remote_id],
        )
        .unwrap();
    }
    let _ = fx.local_dir.as_path() as &Path;
}
