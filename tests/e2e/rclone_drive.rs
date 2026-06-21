//! End-to-end smoke against a real Google Drive account + the real `rclone` binary.
//!
//! Every test in this file is `#[ignore]`d so the default `cargo test` skips it. CI
//! and developers run them via:
//!
//! ```sh
//! cargo test --test rclone_drive -- --ignored
//! ```
//!
//! Each test also self-skips with a `[e2e]` message when the required env vars are
//! missing (see [`common::E2eConfig`]), so running the suite on a machine without
//! secrets is a no-op rather than a failure.
//!
//! The scenarios deliberately stay narrow — their purpose is to validate the
//! `RcloneEngine` ↔ rclone-subprocess ↔ Drive integration that the mocked suite
//! cannot exercise:
//!
//! - **link round-trip** — `air-drive link` against the real `about.user` endpoint.
//! - **initial-sync upload** — local fixture file → Drive (via rclone), verified by
//!   listing the run folder via the Drive REST API.
//! - **initial-sync download** — same file pulled into a fresh local dir on a second
//!   run, md5 round-trip checked against the seeded content.
//! - **empty-directory propagation** — an empty local dir is created on Drive, an
//!   empty Drive folder is materialised locally, and a nested file lands inside a
//!   directory created on the fly (folder support, #1).
//!
//! See `tests/e2e/README.md` for the setup procedure (GCP project, OAuth Desktop
//! credentials, token acquisition).

// Setup failures + assertion panics are the expected failure mode in tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::process::Command;

use air_drive::drive::metadata;

/// Run a `Command`, return `(exit_code, stdout, stderr)`.
fn run(mut cmd: Command) -> (i32, String, String) {
    let out = cmd.output().expect("spawn air-drive");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// ---------------------------------------------------------------------------
// E1 — `link` succeeds against the real Drive `about.user`
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e1_link_reaches_real_drive() {
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    let mut cmd = fx.air_drive_cmd();
    cmd.arg("link");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`link` against real Drive failed; stderr=\n{stderr}"
    );

    // The account row should now exist with a real email.
    let conn = rusqlite::Connection::open(fx.config_dir.join("state.db")).expect("open state.db");
    let email: String = conn
        .query_row("SELECT email FROM account WHERE id = 1", [], |r| r.get(0))
        .expect("account row exists after `link`");
    assert!(
        email.contains('@'),
        "expected an email-shaped string, got `{email}`"
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E2 — local→Drive initial sync via the rclone subprocess
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e2_initial_sync_uploads_via_rclone() {
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    // Seed local-only content under the watched root.
    let content = b"hello from air-drive e2e";
    fx.populate_local("greeting.txt", content);

    // Wire account + mapping rows so `start --initial-sync` has its prerequisites.
    seed_account_and_mapping(&fx);

    // Run the daemon. This invokes the real rclone subprocess against the real
    // Drive backend, configured via the RCLONE_CONFIG_AIRDRIVE_* env vars the
    // production code path emits.
    let mut cmd = fx.air_drive_cmd();
    cmd.arg("start").arg("--initial-sync");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`start --initial-sync` (upload) failed; stderr=\n{stderr}"
    );

    // Verify the file made it through rclone → Drive by listing the run folder
    // and matching by name + md5.
    let children = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list_children of run folder");
    let uploaded = children
        .iter()
        .find(|c| c.name == "greeting.txt")
        .unwrap_or_else(|| {
            panic!(
                "greeting.txt not found on Drive under {}; saw {:?}",
                fx.run_folder_name,
                children.iter().map(|c| &c.name).collect::<Vec<_>>()
            )
        });
    let expected_md5 = hex_md5(content);
    assert_eq!(
        uploaded.md5.as_deref(),
        Some(expected_md5.as_str()),
        "md5 round-trip mismatch"
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E3 — Drive→local initial sync via the rclone subprocess
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e3_initial_sync_downloads_via_rclone() {
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    // Stash a file directly on Drive (via the metadata + upload helpers, no
    // rclone here) so the subsequent `start --initial-sync` is purely a
    // download flow.
    let content = b"remote-seeded payload";
    let metadata_doc = serde_json::json!({
        "name": "seeded.txt",
        "parents": [fx.run_folder_id],
        "mimeType": "text/plain",
    });
    fx.drive
        .upload_multipart(&metadata_doc, "text/plain", content)
        .await
        .expect("seed remote file via DriveHttp");

    // Wire account + mapping rows.
    seed_account_and_mapping(&fx);

    // The local tree is empty — initial-sync will download.
    let mut cmd = fx.air_drive_cmd();
    cmd.arg("start").arg("--initial-sync");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`start --initial-sync` (download) failed; stderr=\n{stderr}"
    );

    let local_path = fx.local_dir.join("seeded.txt");
    let got = std::fs::read(&local_path).unwrap_or_else(|e| {
        panic!(
            "expected `seeded.txt` to be downloaded into {}: {e}",
            local_path.display()
        )
    });
    assert_eq!(got, content, "downloaded bytes mismatch");

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E4 — an empty local directory is created on Drive (folder support, #1)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e4_initial_sync_creates_empty_local_dir_on_drive() {
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    // An empty directory — no file to drag it along.
    std::fs::create_dir(fx.local_dir.join("emptydir")).expect("create local emptydir");
    seed_account_and_mapping(&fx);

    let mut cmd = fx.air_drive_cmd();
    cmd.arg("start").arg("--initial-sync");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`start --initial-sync` (empty dir upload) failed; stderr=\n{stderr}"
    );

    let children = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list_children of run folder");
    let dir = children.iter().find(|c| c.name == "emptydir");
    assert!(
        dir.is_some_and(metadata::DriveFileMeta::is_folder),
        "emptydir should exist as a folder on Drive; saw {:?}",
        children.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E5 — an empty Drive folder is materialised locally (folder support, #1)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e5_initial_sync_creates_empty_drive_dir_locally() {
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    // Seed an empty folder on Drive (no children) under the run folder.
    metadata::create_folder(&fx.drive, &fx.run_folder_id, "remoteempty")
        .await
        .expect("seed remote empty folder");
    seed_account_and_mapping(&fx);

    let mut cmd = fx.air_drive_cmd();
    cmd.arg("start").arg("--initial-sync");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`start --initial-sync` (empty dir download) failed; stderr=\n{stderr}"
    );

    assert!(
        fx.local_dir.join("remoteempty").is_dir(),
        "remoteempty should have been created locally"
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E6 — a nested file lands inside a directory created on the fly: exercises the
// dir-create (REST) + file-upload (rclone) interplay end-to-end
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e6_initial_sync_uploads_nested_file_into_created_dir() {
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    let content = b"nested e2e payload";
    fx.populate_local("docs/spec.txt", content);
    seed_account_and_mapping(&fx);

    let mut cmd = fx.air_drive_cmd();
    cmd.arg("start").arg("--initial-sync");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`start --initial-sync` (nested upload) failed; stderr=\n{stderr}"
    );

    // The `docs` folder must exist under the run folder…
    let top = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list_children of run folder");
    let docs = top
        .iter()
        .find(|c| c.name == "docs" && c.is_folder())
        .unwrap_or_else(|| {
            panic!(
                "docs folder missing under run folder; saw {:?}",
                top.iter().map(|c| &c.name).collect::<Vec<_>>()
            )
        });

    // …and `spec.txt` must live inside it with the right md5 (uploaded via rclone).
    let inside = metadata::list_children(&fx.drive, &docs.id)
        .await
        .expect("list_children of docs");
    let spec = inside
        .iter()
        .find(|c| c.name == "spec.txt")
        .unwrap_or_else(|| {
            panic!(
                "spec.txt missing inside docs; saw {:?}",
                inside.iter().map(|c| &c.name).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        spec.md5.as_deref(),
        Some(hex_md5(content).as_str()),
        "md5 round-trip mismatch for docs/spec.txt"
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Open the daemon's state.db (creates it + the v1 schema if missing) and seed an
/// account + folder_mapping row pointing at `fx`'s tempdir and the per-run Drive
/// folder. Using the production migrations keeps tests in lock-step with what the
/// daemon expects on disk.
fn seed_account_and_mapping(fx: &common::E2eFixture) {
    let path = fx.config_dir.join("state.db");
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
    conn.execute(
        "INSERT OR REPLACE INTO account (id, email, created_at, linked_at) \
         VALUES (1, 'e2e@example.com', 0, 0)",
        [],
    )
    .expect("seed account");
    conn.execute(
        "INSERT OR REPLACE INTO folder_mapping \
            (id, account_id, local_path, remote_folder_id, remote_folder_name, created_at) \
         VALUES (1, 1, ?1, ?2, NULL, 0)",
        rusqlite::params![fx.local_dir.to_string_lossy(), fx.run_folder_id.as_str(),],
    )
    .expect("seed mapping");
}

/// Hex-encoded lowercase MD5 — mirrors what Drive returns in `md5Checksum`.
fn hex_md5(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(bytes);
    hex::encode(h.finalize())
}
