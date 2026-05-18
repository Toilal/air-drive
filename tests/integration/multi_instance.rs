//! `air-drive start` single-instance enforcement (T064, FR-017).
//!
//! Once a daemon is up against a given config dir, a second `start` against
//! the same config dir MUST exit with code 6 and surface the running PID in
//! its stderr.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::process::Command;
use std::time::Duration;

use common::{DaemonProcess, DriveMock, FsFixture, air_drive_cmd, fs_fixture};

fn run(mut cmd: Command) -> (i32, String, String) {
    let out = cmd.output().expect("spawn air-drive");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn second_start_exits_6_with_holder_pid() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_minimal_state(&fx, &root_id);

    // 1. First daemon — keeps running until shutdown.
    let daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;
    let holder_pid = daemon.pid();

    // Give the first daemon a moment to grab the lock cleanly. `DaemonProcess`
    // already sleeps 300 ms post-spawn, but a small extra pad on slow runners
    // can't hurt.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Second start against the same config dir — must refuse with exit 6.
    let mut cmd = air_drive_cmd(&fx, &mock);
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);

    assert_eq!(
        code, 6,
        "second start should exit 6 (lock held); stderr was:\n{stderr}"
    );
    let pid_str = holder_pid.to_string();
    assert!(
        stderr.contains(&pid_str),
        "stderr should name the running daemon pid {pid_str}; got:\n{stderr}"
    );

    daemon.shutdown().await;
}

fn seed_minimal_state(fx: &FsFixture, root_id: &str) {
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
    // Seed the cursor so the first daemon's start skips initial-sync and goes
    // straight to the loop. Otherwise the test races against the initial-walk.
    conn.execute(
        "INSERT OR REPLACE INTO drive_change_cursor \
            (mapping_id, page_token, updated_at) VALUES (1, '0', 0)",
        [],
    )
    .unwrap();
}
