//! Integration coverage for #11 — the pre-flight that checks `local_root`
//! exists before the daemon attaches its inotify watcher.
//!
//! Two paths to exercise:
//!
//! 1. `watch.auto_create_root = true` + missing folder → daemon creates it
//!    and proceeds.
//! 2. `watch.auto_create_root = false` (default) + missing folder → daemon
//!    refuses to start on a non-interactive stdin (the test harness pipes
//!    stdin, so `interactive::confirm` returns `false` conservatively) with
//!    an actionable error message that mentions the toggle and does **not**
//!    leak the raw `notify watch(...): No such file or directory (os error 2)`
//!    wording the pre-fix code surfaced.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::process::Command;

use common::{DriveMock, FsFixture, air_drive_cmd, fs_fixture};

fn run(mut cmd: Command) -> (i32, String, String) {
    let out = cmd.output().expect("spawn air-drive");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (code, stdout, stderr)
}

fn write_config_with_auto_create(fx: &FsFixture, auto_create: bool) {
    let toml = format!(
        "\
[oauth]

[mapping]

[daemon]

[rclone]

[watch]
auto_create_root = {auto_create}
"
    );
    std::fs::write(fx.config_dir.join("config.toml"), toml).unwrap();
}

fn open_or_create_state(fx: &FsFixture) -> rusqlite::Connection {
    let path = fx.state_db_path();
    let conn = rusqlite::Connection::open(&path).expect("open state.db");
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

fn seed_account_and_mapping(fx: &FsFixture, local_path: &str, remote_folder_id: &str) {
    let conn = open_or_create_state(fx);
    conn.execute(
        "INSERT OR REPLACE INTO account (id, email, created_at, linked_at) VALUES (1, ?1, 0, 0)",
        rusqlite::params!["alice@example.com"],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO folder_mapping
            (id, account_id, local_path, remote_folder_id, remote_folder_name, created_at)
         VALUES (1, 1, ?1, ?2, NULL, 0)",
        rusqlite::params![local_path, remote_folder_id],
    )
    .unwrap();
}

#[tokio::test]
async fn auto_create_root_true_creates_missing_local_root() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    write_config_with_auto_create(&fx, true);
    fx.write_token_file();

    // Point the mapping at a path that does NOT exist on disk.
    let target = fx.config_dir.join("watched-folder-not-yet-created");
    assert!(!target.exists(), "precondition: target must be absent");
    seed_account_and_mapping(&fx, &target.to_string_lossy(), &root_id);

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);

    assert_eq!(
        code, 0,
        "daemon should auto-create the folder and proceed; stderr=\n{stderr}"
    );
    assert!(
        target.is_dir(),
        "watched folder was not created at {}",
        target.display()
    );
}

#[tokio::test]
async fn auto_create_root_false_errors_actionably() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    write_config_with_auto_create(&fx, false);
    fx.write_token_file();

    let target = fx.config_dir.join("watched-folder-not-yet-created");
    assert!(!target.exists(), "precondition: target must be absent");
    seed_account_and_mapping(&fx, &target.to_string_lossy(), &root_id);

    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);

    assert_ne!(code, 0, "daemon should refuse to start; stderr=\n{stderr}");
    assert!(
        !target.exists(),
        "auto_create_root=false must NOT create the folder"
    );
    // Actionable message: mentions path + toggle, hides watcher wording.
    assert!(
        stderr.contains(&target.display().to_string()),
        "stderr should mention the watched folder path:\n{stderr}"
    );
    assert!(
        stderr.contains("watch.auto_create_root"),
        "stderr should point at the toggle:\n{stderr}"
    );
    assert!(
        !stderr.contains("notify watch"),
        "stderr leaked the raw watcher error:\n{stderr}"
    );
    assert!(
        !stderr.contains("os error 2"),
        "stderr leaked the raw os error:\n{stderr}"
    );
}
