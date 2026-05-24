//! Conflict scenarios.
//!
//! While the daemon was offline, both sides of a previously-synced file
//! diverge. On restart the daemon must detect the divergence and apply the
//! Q2 clarification rule (remote keeps canonical name; local content is
//! preserved under a `.conflict-<ts>.<ext>` sibling). The conflict shows
//! up in `status --json`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{ChangeEntry, DaemonProcess, DriveMock, FsFixture, fs_fixture, hex_md5, wait_until};

const T_CONVERGE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn us3_2_offline_double_edit_creates_conflict_pair() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    // Seed the "previously synced" state: same content on both sides.
    let original = b"v0 synced bytes";
    let remote_id = mock.insert_file(Some(&root_id), "doc.txt", original);

    let fx = fs_fixture();
    fx.populate_local(&[("doc.txt", original)]);
    fx.write_default_config();
    fx.write_token_file();
    seed_synced(&fx, &root_id, &remote_id, original);

    // Now simulate "daemon was offline; both sides edited independently".
    let remote_v2 = b"remote-edit B";
    {
        let mut st = mock.state.lock().unwrap();
        let f = st.files.get_mut(&remote_id).unwrap();
        f.content = remote_v2.to_vec();
        f.md5 = hex_md5(remote_v2);
        st.change_log.push(ChangeEntry {
            file_id: remote_id.clone(),
            removed: false,
        });
    }
    let local_v2 = b"local-edit C";
    std::fs::write(fx.local_dir.join("doc.txt"), local_v2).unwrap();

    // Start the daemon — the poller picks up the remote change, apply_remote
    // detects the local md5 doesn't match the last-synced fingerprint, opens
    // the conflict, downloads the remote bytes into `doc.txt`. The watcher
    // simultaneously notices `doc.conflict-*.txt` and enqueues an Upload.
    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Wait for the conflict pair to materialize: canonical doc.txt with the
    // remote bytes, plus a sibling .conflict-* file with the local bytes.
    let converged = wait_until(T_CONVERGE, || async {
        let canonical = std::fs::read(fx.local_dir.join("doc.txt")).ok();
        let sibling = find_conflict_sibling(&fx);
        match (canonical, sibling) {
            (Some(c), Some(p)) => {
                let s = std::fs::read(&p).ok();
                c == remote_v2 && s.as_deref() == Some(local_v2.as_slice())
            }
            _ => false,
        }
    })
    .await;
    assert!(
        converged,
        "conflict pair should be on disk within {T_CONVERGE:?}; alive? {:?}",
        daemon.poll_alive()
    );

    // status --json must list the conflict.
    daemon.shutdown().await;
    let mut cmd = common::air_drive_cmd(&fx, &mock);
    cmd.arg("status").arg("--json");
    let out = cmd.output().expect("spawn status");
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status output is JSON");
    let conflicts = body["conflicts"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        conflicts.len(),
        1,
        "exactly one conflict expected: {body:#}"
    );
    let c = &conflicts[0];
    assert_eq!(c["original_path"], "doc.txt");
    assert!(
        c["conflict_path"]
            .as_str()
            .unwrap_or("")
            .starts_with("doc.conflict-"),
        "conflict_path should be a .conflict-* sibling; got {:?}",
        c["conflict_path"]
    );
}

/// Walk the local dir looking for a single `doc.conflict-*.txt`.
fn find_conflict_sibling(fx: &FsFixture) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(&fx.local_dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file()
            && p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with("doc.conflict-") && n.ends_with(".txt"))
        {
            return Some(p);
        }
    }
    None
}

fn seed_synced(fx: &FsFixture, root_id: &str, remote_id: &str, content: &[u8]) {
    let path = fx.state_db_path();
    let conn = rusqlite::Connection::open(&path).expect("open state.db");
    conn.execute_batch(air_drive::state::schema::BOOTSTRAP)
        .unwrap();
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
         VALUES (1, 'a@x', 0, 0)",
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
        "INSERT OR REPLACE INTO drive_change_cursor \
            (mapping_id, page_token, updated_at) VALUES (1, '0', 0)",
        [],
    )
    .unwrap();
    let md5 = hex_md5(content);
    conn.execute(
        "INSERT INTO sync_item (mapping_id, relative_path, kind, remote_id, size, md5, \
                                local_inode, last_synced_at, state) \
         VALUES (1, 'doc.txt', 'file', ?1, ?2, ?3, NULL, 0, 'synced')",
        rusqlite::params![remote_id, content.len() as i64, md5],
    )
    .unwrap();
}
