//! `air-drive status [--json]` integration tests.
//!
//! Today we cover only schema validation. Counts mid-sync and the last-error
//! surface require a control socket the daemon doesn't expose yet; a follow-up
//! batch lands it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::process::Command;

use jsonschema::Validator;
use serde_json::Value;

use common::{DriveMock, FsFixture, air_drive_cmd, fs_fixture};

/// Run a `Command`, return `(exit_code, stdout, stderr)`.
fn run(mut cmd: Command) -> (i32, String, String) {
    let out = cmd.output().expect("spawn air-drive");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn load_schema() -> Validator {
    let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/integration/fixtures/status.schema.json");
    let raw = std::fs::read_to_string(&schema_path).unwrap_or_else(|e| {
        panic!("read {}: {e}", schema_path.display());
    });
    let schema_json: Value = serde_json::from_str(&raw).expect("schema is valid JSON");
    Validator::new(&schema_json).expect("schema compiles")
}

/// `air-drive status --json` emits a document that validates against
/// the contract schema, both before a daemon has linked and after a full
/// sync state is in place.
#[tokio::test]
async fn us3_1_status_json_validates_against_schema() {
    let validator = load_schema();
    let mock = DriveMock::start().await;
    let fx = fs_fixture();
    fx.write_default_config();
    fx.write_token_file();

    // 1. Fresh state — no account, no mapping, no daemon.
    let body = run_status_json(&fx, &mock);
    assert_validation(&validator, &body);
    assert_eq!(body["account"], Value::Null);
    assert_eq!(body["mapping"], Value::Null);
    assert_eq!(body["pid"], Value::Null);

    // 2. Seed account + mapping + a synced item + a conflict row so the
    // populated fields exercise their schema branches too.
    seed_post_link_state(&fx, &mock);
    let body = run_status_json(&fx, &mock);
    assert_validation(&validator, &body);
    assert!(body["account"].is_object(), "account populated: {body:?}");
    assert!(body["mapping"].is_object(), "mapping populated: {body:?}");
    assert_eq!(
        body["conflicts"].as_array().map(Vec::len).unwrap_or(0),
        1,
        "one conflict expected: {body:?}"
    );
}

fn run_status_json(fx: &FsFixture, mock: &DriveMock) -> Value {
    let mut cmd = air_drive_cmd(fx, mock);
    cmd.arg("status").arg("--json");
    let (code, stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "status --json exit: stderr={stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("status --json output is not valid JSON: {e}\n---stdout---\n{stdout}");
    })
}

fn assert_validation(validator: &Validator, body: &Value) {
    let errors: Vec<String> = validator
        .iter_errors(body)
        .map(|e| format!("{} at {}", e, e.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "status JSON failed schema validation:\n  {}\n---body---\n{body:#}",
        errors.join("\n  ")
    );
}

fn seed_post_link_state(fx: &FsFixture, _mock: &DriveMock) {
    let path = fx.state_db_path();
    let conn = rusqlite::Connection::open(&path).expect("open state.db");
    // Reuse the production schema constants so this stays in sync with the
    // migrations the daemon would apply.
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
         VALUES (1, 'alice@example.com', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO folder_mapping \
            (id, account_id, local_path, remote_folder_id, remote_folder_name, created_at) \
         VALUES (1, 1, ?1, 'rid-root', 'Sync', 0)",
        rusqlite::params![fx.local_dir.to_string_lossy()],
    )
    .unwrap();
    // A sync_item is needed before a conflict_record can reference it via
    // the foreign key constraint.
    conn.execute(
        "INSERT INTO sync_item (mapping_id, relative_path, kind, remote_id, size, md5, \
                                local_inode, last_synced_at, state) \
         VALUES (1, 'doc.txt', 'file', 'rid-doc', 11, 'abc', NULL, 0, 'conflict')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conflict_record (sync_item_id, original_relative_path, \
                                      conflict_relative_path, detected_at) \
         VALUES (1, 'doc.txt', 'doc.conflict-20260518T065900Z.txt', 1747560000)",
        [],
    )
    .unwrap();
}
