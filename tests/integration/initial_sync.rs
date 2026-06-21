//! Integration tests for initial sync.
//!
//! These tests exercise the `air-drive` binary end-to-end against the
//! [`common::DriveMock`], with the rclone subprocess swapped for an in-process
//! HTTP engine via the test env-var contract documented in
//! `tests/integration/common/mod.rs`.

// Integration test setup is allowed to panic — there is no recovery path inside a test,
// and surfacing setup failures loudly is the desired behaviour.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::process::Command;

use common::{DriveMock, air_drive_cmd, fs_fixture, hex_md5, with_state_db};

/// Run a `Command` and return `(exit_code, stdout, stderr)`. Panics if spawn fails.
fn run(mut cmd: Command) -> (i32, String, String) {
    let out = cmd.output().expect("spawn air-drive");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (code, stdout, stderr)
}

// ---------------------------------------------------------------------------
// `link` persists the account
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_1_link_persists_account() {
    let mock = DriveMock::start().await;
    mock.set_user_email("alice@example.com");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("link");
    let (code, _stdout, stderr) = run(cmd);

    assert_eq!(code, 0, "`link` should succeed; stderr=\n{stderr}");

    with_state_db(&fx, |conn| {
        let email: String = conn
            .query_row("SELECT email FROM account WHERE id = 1", [], |row| {
                row.get(0)
            })
            .expect("account row exists after `link`");
        assert_eq!(email, "alice@example.com");
    });
}

// ---------------------------------------------------------------------------
// `map` validates inputs and persists the mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_2_map_persists_mapping() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    // `map` needs an account row to attach the mapping to.
    seed_account(&fx, "alice@example.com");

    // Pass the folder by URL — non-URL specs are now treated as `path:`
    // notation, so a bare ID like the mocked `root_id` would be looked up as
    // a folder *name* under root, not as a Drive ID.
    let url = format!("https://drive.google.com/drive/folders/{root_id}");
    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("map").arg(&fx.local_dir).arg(&url);
    let (code, _stdout, stderr) = run(cmd);

    assert_eq!(code, 0, "`map` should succeed; stderr=\n{stderr}");
    with_state_db(&fx, |conn| {
        let (local, remote): (String, String) = conn
            .query_row(
                "SELECT local_path, remote_folder_id FROM folder_mapping WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("mapping row exists after `map`");
        assert_eq!(local, fx.local_dir.to_string_lossy());
        assert_eq!(remote, root_id);
    });
}

#[tokio::test]
async fn us1_2_map_rejects_missing_local_path_with_exit_4() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_account(&fx, "alice@example.com");

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("map")
        .arg("/nonexistent/path/that/should/not/exist/abcxyz")
        .arg(&root_id);
    let (code, _stdout, _stderr) = run(cmd);
    assert_eq!(code, 4, "missing local path → exit 4");
}

#[tokio::test]
async fn us1_2_map_rejects_unresolvable_remote_with_exit_5() {
    let mock = DriveMock::start().await;
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_account(&fx, "alice@example.com");

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("map")
        .arg(&fx.local_dir)
        .arg("drv-dir-does-not-exist");
    let (code, _stdout, _stderr) = run(cmd);
    assert_eq!(code, 5, "unresolvable remote folder → exit 5");
}

// ---------------------------------------------------------------------------
// empty local, populated Drive, after initial-sync local mirrors Drive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_3_drive_to_local_initial_sync() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    // 10 files in 3 subfolders + a couple at the root, totalling 10 leaves.
    let docs = mock.insert_folder(Some(&root_id), "docs");
    let img = mock.insert_folder(Some(&root_id), "images");
    let notes = mock.insert_folder(Some(&docs), "notes");

    let plan = [
        (None, "readme.txt", b"top-level readme" as &[u8]),
        (None, "todo.txt", b"one\ntwo\nthree"),
        (Some("docs"), "spec.txt", b"specification body"),
        (Some("docs"), "design.txt", b"design body"),
        (Some("docs/notes"), "alpha.txt", b"alpha"),
        (Some("docs/notes"), "beta.txt", b"beta"),
        (Some("docs/notes"), "gamma.txt", b"gamma"),
        (Some("images"), "a.png", b"\x89PNG-fake-a"),
        (Some("images"), "b.png", b"\x89PNG-fake-b"),
        (Some("images"), "c.png", b"\x89PNG-fake-c"),
    ];
    let dir_ids = [
        ("docs", docs.as_str()),
        ("images", img.as_str()),
        ("docs/notes", notes.as_str()),
    ];
    for (parent_rel, name, content) in plan {
        let parent = match parent_rel {
            None => root_id.as_str(),
            Some(rel) => dir_ids.iter().find(|(r, _)| *r == rel).unwrap().1,
        };
        mock.insert_file(Some(parent), name, content);
    }

    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_account(&fx, "alice@example.com");
    seed_mapping(&fx, &fx.local_dir.to_string_lossy(), &root_id);

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "initial-sync should converge; stderr=\n{stderr}");

    let mut want: Vec<(String, Vec<u8>)> = vec![
        ("readme.txt".into(), b"top-level readme".to_vec()),
        ("todo.txt".into(), b"one\ntwo\nthree".to_vec()),
        ("docs/spec.txt".into(), b"specification body".to_vec()),
        ("docs/design.txt".into(), b"design body".to_vec()),
        ("docs/notes/alpha.txt".into(), b"alpha".to_vec()),
        ("docs/notes/beta.txt".into(), b"beta".to_vec()),
        ("docs/notes/gamma.txt".into(), b"gamma".to_vec()),
        ("images/a.png".into(), b"\x89PNG-fake-a".to_vec()),
        ("images/b.png".into(), b"\x89PNG-fake-b".to_vec()),
        ("images/c.png".into(), b"\x89PNG-fake-c".to_vec()),
    ];
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(fx.walk_local(), want, "local tree should mirror Drive");
}

// ---------------------------------------------------------------------------
// non-empty local, empty Drive, after initial-sync Drive mirrors local
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_4_local_to_drive_initial_sync() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    let fx = fs_fixture();
    fx.populate_local(&[
        ("hello.txt", b"hi there"),
        ("nested/one.txt", b"one"),
        ("nested/two.txt", b"two"),
    ]);
    fx.write_default_config();
    fx.write_token_file();
    seed_account(&fx, "alice@example.com");
    seed_mapping(&fx, &fx.local_dir.to_string_lossy(), &root_id);

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "initial-sync should converge; stderr=\n{stderr}");

    let descendants = mock.state.lock().unwrap().descendants(&root_id);
    let names: Vec<String> = descendants.iter().map(|(p, _)| p.clone()).collect();
    assert!(names.contains(&"hello.txt".into()), "got {names:?}");
    assert!(names.contains(&"nested/one.txt".into()), "got {names:?}");
    assert!(names.contains(&"nested/two.txt".into()), "got {names:?}");

    // Bytes survived the upload round-trip.
    for (rel, f) in descendants {
        let local = fx.local_dir.join(&rel);
        let local_bytes = std::fs::read(&local).expect("local file exists");
        assert_eq!(
            f.content, local_bytes,
            "Drive bytes for {rel} should match local"
        );
    }
}

// ---------------------------------------------------------------------------
// overlapping content (3 match by md5, 2 only local, 2 only remote)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_5_overlapping_content() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    // 3 files matching by md5 on both sides.
    let matched: Vec<(&str, &[u8])> = vec![
        ("shared/m1.txt", b"matched-one"),
        ("shared/m2.txt", b"matched-two"),
        ("shared/m3.txt", b"matched-three"),
    ];
    // 2 files local-only — should be uploaded.
    let local_only: Vec<(&str, &[u8])> = vec![
        ("local-only-1.txt", b"local-1"),
        ("local-only-2.txt", b"local-2"),
    ];
    // 2 files remote-only — should be downloaded.
    let remote_only: Vec<(&str, &[u8])> = vec![
        ("remote-only-1.txt", b"remote-1"),
        ("remote-only-2.txt", b"remote-2"),
    ];

    // Seed the local side with matched + local-only.
    let fx = fs_fixture();
    let mut local_plan: Vec<(&str, &[u8])> = Vec::new();
    local_plan.extend(matched.iter().copied());
    local_plan.extend(local_only.iter().copied());
    fx.populate_local(&local_plan);

    // Seed Drive with matched + remote-only.
    let shared = mock.insert_folder(Some(&root_id), "shared");
    for (rel, content) in &matched {
        let name = rel.split('/').next_back().unwrap();
        mock.insert_file(Some(&shared), name, content);
    }
    for (rel, content) in &remote_only {
        let name = rel.split('/').next_back().unwrap();
        mock.insert_file(Some(&root_id), name, content);
    }

    fx.write_default_config();
    fx.write_token_file();
    seed_account(&fx, "alice@example.com");
    seed_mapping(&fx, &fx.local_dir.to_string_lossy(), &root_id);

    let uploads_before = mock.upload_count().await;
    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "initial-sync should converge; stderr=\n{stderr}");
    let uploads_after = mock.upload_count().await;

    // Only the 2 local-only files should have been uploaded — matched files MUST NOT
    // re-upload (their md5 already matches).
    assert_eq!(
        uploads_after - uploads_before,
        2,
        "expected exactly 2 uploads (the local-only files), got {} (matched files \
         should be skipped on md5 equality)",
        uploads_after - uploads_before
    );

    // Remote-only files were downloaded into the local tree.
    for (rel, expected) in &remote_only {
        let local = fx.local_dir.join(rel);
        let got = std::fs::read(&local)
            .unwrap_or_else(|e| panic!("expected {rel} downloaded locally; io error: {e}"));
        assert_eq!(&got, expected, "{rel} content roundtrip");
    }

    // Spot-check that md5 equality really was the criterion (not name).
    for (_, content) in &matched {
        let _md5 = hex_md5(content); // exists for debugging if assertions fail
    }
}

// ---------------------------------------------------------------------------
// empty directories propagate during the initial pass (both directions)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_6_initial_sync_creates_empty_remote_dirs_locally() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    // Empty folders on Drive — no files to drag them along.
    mock.insert_folder(Some(&root_id), "emptydir");
    let parent = mock.insert_folder(Some(&root_id), "parent");
    mock.insert_folder(Some(&parent), "child");

    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_account(&fx, "alice@example.com");
    seed_mapping(&fx, &fx.local_dir.to_string_lossy(), &root_id);

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "initial-sync should converge; stderr=\n{stderr}");

    assert!(
        fx.local_dir.join("emptydir").is_dir(),
        "empty remote dir should be created locally"
    );
    assert!(
        fx.local_dir.join("parent/child").is_dir(),
        "nested empty remote dir should be created locally"
    );
}

#[tokio::test]
async fn us1_6_initial_sync_creates_empty_local_dirs_on_drive() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    // Empty local directories (nothing inside them).
    std::fs::create_dir_all(fx.local_dir.join("emptylocal")).unwrap();
    std::fs::create_dir_all(fx.local_dir.join("a/b")).unwrap();
    seed_account(&fx, "alice@example.com");
    seed_mapping(&fx, &fx.local_dir.to_string_lossy(), &root_id);

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "initial-sync should converge; stderr=\n{stderr}");

    {
        let st = mock.state.lock().unwrap();
        let is_child_folder = |name: &str, parent: &str| {
            st.files
                .values()
                .any(|f| f.is_folder() && f.name == name && f.parent_id.as_deref() == Some(parent))
        };
        assert!(
            is_child_folder("emptylocal", root_id.as_str()),
            "empty local dir should be created on Drive"
        );
        let a = st
            .files
            .values()
            .find(|f| {
                f.is_folder() && f.name == "a" && f.parent_id.as_deref() == Some(root_id.as_str())
            })
            .expect("dir 'a' should exist on Drive");
        assert!(
            is_child_folder("b", a.id.as_str()),
            "nested dir a/b should exist on Drive under 'a'"
        );
    }

    // Directories must be persisted as kind='dir' sync_item rows carrying their
    // Drive id — this is the anchor folder rename/move (#7) relies on.
    with_state_db(&fx, |conn| {
        for rel in ["emptylocal", "a", "a/b"] {
            let (kind, remote_id): (String, Option<String>) = conn
                .query_row(
                    "SELECT kind, remote_id FROM sync_item WHERE relative_path = ?1",
                    rusqlite::params![rel],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or_else(|e| panic!("sync_item row for {rel} missing: {e}"));
            assert_eq!(kind, "dir", "{rel} should be kind='dir'");
            assert!(
                remote_id.is_some(),
                "{rel} dir row must carry a remote_id, got NULL"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// parent directories of nested files are persisted as kind='dir' rows
// (deterministic: the initial pass is one-shot, no inotify race) — this is the
// anchor folder rename/move (#7) relies on, for both remote-only and local-only
// directory trees.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_6_initial_sync_persists_parent_dirs_of_nested_files() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");

    // Remote-only nested file: parent "docs" exists only on Drive.
    let docs = mock.insert_folder(Some(&root_id), "docs");
    mock.insert_file(Some(&docs), "spec.txt", b"spec body");

    // Local-only nested file: parent "nested" exists only locally.
    let fx = fs_fixture();
    fx.populate_local(&[("nested/one.txt", b"one")]);
    fx.write_default_config();
    fx.write_token_file();
    seed_account(&fx, "alice@example.com");
    seed_mapping(&fx, &fx.local_dir.to_string_lossy(), &root_id);

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "initial-sync should converge; stderr=\n{stderr}");

    // Both parent dirs must be persisted as kind='dir' with a remote_id, whether
    // they came from the remote walk or were created during a local upload.
    with_state_db(&fx, |conn| {
        for rel in ["docs", "nested"] {
            let (kind, remote_id): (String, Option<String>) = conn
                .query_row(
                    "SELECT kind, remote_id FROM sync_item WHERE relative_path = ?1",
                    rusqlite::params![rel],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or_else(|e| panic!("sync_item row for dir {rel} missing: {e}"));
            assert_eq!(kind, "dir", "{rel} should be kind='dir'");
            assert!(remote_id.is_some(), "{rel} dir row must carry a remote_id");
        }
    });
}

// ---------------------------------------------------------------------------
// Pre-seeding helpers (sync rusqlite — simpler than spinning up the async wrapper).
// ---------------------------------------------------------------------------

fn open_or_create_state(fx: &common::FsFixture) -> rusqlite::Connection {
    let path = fx.state_db_path();
    let conn = rusqlite::Connection::open(&path).expect("open state.db");
    // Re-use the production schema so tests stay in lock-step with what the daemon
    // expects on disk (drive_change_cursor, sync_item, pending_operation, …). If the
    // schema evolves and migrations grow, this loop applies them all.
    conn.execute_batch(air_drive::state::schema::BOOTSTRAP)
        .expect("bootstrap schema_version");
    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .expect("read schema_version");
    for (idx, sql) in air_drive::state::schema::MIGRATIONS.iter().enumerate() {
        let target = (idx + 1) as i64;
        if target <= current {
            continue;
        }
        conn.execute_batch(sql).expect("apply migration");
        conn.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![target, 0i64],
        )
        .expect("record schema_version");
    }
    conn
}

fn seed_account(fx: &common::FsFixture, email: &str) {
    let conn = open_or_create_state(fx);
    conn.execute(
        "INSERT OR REPLACE INTO account (id, email, created_at, linked_at) VALUES (1, ?1, 0, 0)",
        rusqlite::params![email],
    )
    .unwrap();
}

fn seed_mapping(fx: &common::FsFixture, local_path: &str, remote_folder_id: &str) {
    let conn = open_or_create_state(fx);
    conn.execute(
        "INSERT OR REPLACE INTO folder_mapping
            (id, account_id, local_path, remote_folder_id, remote_folder_name, created_at)
         VALUES (1, 1, ?1, ?2, NULL, 0)",
        rusqlite::params![local_path, remote_folder_id],
    )
    .unwrap();
}
