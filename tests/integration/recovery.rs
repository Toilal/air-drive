//! Transient-failure recovery (roadmap 080 — surface recovered state).
//!
//! When Drive is briefly unreachable, the poller flips `state_meta` to a
//! recoverable `transient` block so `air-drive status` reports it. On the next
//! successful Drive call the block clears itself — the daemon must report
//! "healthy again", not stay stuck. This is the blocked → recovered transition
//! that distinguishes a hiccup from a terminal failure (which needs a re-link).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::process::Command;
use std::time::Duration;

use common::{DaemonProcess, DriveMock, FsFixture, air_drive_cmd, fs_fixture, wait_until};

#[tokio::test]
async fn us3_5_transient_failure_blocks_then_recovers() {
    let mock = DriveMock::start().await;
    let root_id = mock.insert_folder(None, "Sync");
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();
    seed_minimal(&fx, &root_id);

    let mut daemon = DaemonProcess::spawn(&fx, &mock, &["--remote-poll-interval", "10"]).await;

    // Let the daemon finish startup and complete one *successful* poll before we
    // inject failures — the change cursor advancing past the seeded "0" proves
    // it. Arming earlier would also make the startup's own Drive probe burn the
    // HTTP retry back-off, which isn't what this test is about.
    let polled = wait_until(Duration::from_secs(40), || async {
        read_cursor(&fx) != "0"
    })
    .await;
    assert!(
        polled,
        "daemon should complete a first successful poll; alive? {:?}",
        daemon.poll_alive()
    );

    // 1. Make every Drive request fail with 503 (surviving the HTTP retry
    //    budget). The next poll's changes.list trips and sets a `transient`
    //    block. Budget covers one poll interval (~10 s) plus the HTTP layer's
    //    full retry back-off (~1+2+4+8+16 s) before the error surfaces.
    mock.fail_next_n(10_000);
    let saw_transient = wait_until(Duration::from_secs(70), || async {
        let body = read_status_json(&fx, &mock);
        body["state"] == "blocked"
            && body["last_error"].get("kind").and_then(|v| v.as_str()) == Some("transient")
    })
    .await;
    assert!(
        saw_transient,
        "a sustained transient failure should surface as blocked/transient; alive? {:?}",
        daemon.poll_alive()
    );

    // 2. Drive recovers. The next successful poll must clear the transient block
    //    — status reports healthy again rather than staying stuck.
    mock.fail_next_n(0);
    let recovered = wait_until(Duration::from_secs(70), || async {
        let body = read_status_json(&fx, &mock);
        body["state"] != "blocked" && body["last_error"].is_null()
    })
    .await;
    assert!(
        recovered,
        "the daemon should clear the transient block after a successful poll; alive? {:?}",
        daemon.poll_alive()
    );

    daemon.shutdown().await;
}

/// The mapping's change cursor as stored in `state.db` (a fresh read each call,
/// since `air-drive status` and the daemon run in separate processes).
fn read_cursor(fx: &FsFixture) -> String {
    let conn = rusqlite::Connection::open(fx.state_db_path()).expect("open state.db");
    conn.query_row(
        "SELECT page_token FROM drive_change_cursor WHERE mapping_id = 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "0".to_string())
}

fn read_status_json(fx: &FsFixture, mock: &DriveMock) -> serde_json::Value {
    let mut cmd = air_drive_cmd(fx, mock);
    cmd.arg("status").arg("--json");
    let out = cmd.output().expect("spawn status");
    assert!(
        out.status.success(),
        "status --json failed (exit {:?}); stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
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

// Keep the import path consistent (silences any future unused warning).
#[allow(dead_code)]
fn _unused(_: Command) {}
