//! Refresh token revoked.
//!
//! Google revokes the daemon's token (mocked here as a permanent 401 from
//! the Drive API). The daemon must:
//!
//! - stay alive (no panic / no exit),
//! - flip its `state_meta.blocked_kind` to `auth`,
//! - so that `air-drive status --json` surfaces `state="blocked"` and
//!   `last_error.kind="auth"` with a re-link hint embedded in the message.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::process::Command;
use std::time::Duration;

use common::{DaemonProcess, DriveMock, FsFixture, air_drive_cmd, fs_fixture, wait_until};

#[tokio::test]
async fn us3_4_refresh_revoked_blocks_with_kind_auth() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_minimal(&fx, &root_id);

    // Flip the mock into "always 401" mode BEFORE the daemon starts. The
    // poller's first `changes.list` will trip, set blocked, and the
    // dispatcher loop will sit on the blocked flag.
    mock.start_auth_failures();

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // `air-drive status --json` reads state_meta off the disk DB — wait for
    // the daemon to persist the blocked flag (one poll cycle ≤ ~12 s).
    let saw_blocked = wait_until(Duration::from_secs(40), || async {
        let body = read_status_json(&fx, &mock);
        body["state"] == "blocked"
            && body["last_error"].get("kind").and_then(|v| v.as_str()) == Some("auth")
    })
    .await;
    assert!(
        saw_blocked,
        "daemon should flip to blocked/auth within 40 s; alive? {:?}",
        daemon.poll_alive()
    );

    // Daemon stays alive — verify it before tearing down.
    assert!(
        daemon.poll_alive().is_none(),
        "daemon exited after auth failure (should stay running)"
    );

    daemon.shutdown().await;
}

fn read_status_json(fx: &FsFixture, mock: &DriveMock) -> serde_json::Value {
    let mut cmd = air_drive_cmd(fx, mock);
    cmd.arg("status").arg("--json");
    let out = cmd.output().expect("spawn status");
    if !out.status.success() {
        panic!(
            "status --json failed (exit {:?}); stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    serde_json::from_slice(&out.stdout).expect("status output is JSON")
}

fn seed_minimal(fx: &FsFixture, root_id: &str) {
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
}

// Keep the type-import path consistent (silences any future unused warning if
// the assertion path changes).
#[allow(dead_code)]
fn _unused(_: Command) {}
