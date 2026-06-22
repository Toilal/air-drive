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
//! - **folder rename/move** — a directory renamed locally moves on Drive, and a
//!   directory renamed on Drive moves locally, via a *continuously-running* daemon
//!   ([`common::DaemonProcess`]) rather than the one-shot initial-sync (#7).
//! - **trash + restore** — a file trashed on Drive is removed locally, then a
//!   restore re-links the tombstoned row and pulls the same file back without a
//!   duplicate (#8), exercised against the continuous daemon.
//! - **native Google Doc** — a Doc created on Drive is materialised locally as a
//!   `.gdoc` shortcut file (JSON pointer) and is never uploaded back (#3).
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

    // Wire account + mapping rows so `start` has its prerequisites.
    seed_account_and_mapping(&fx);

    // Run the daemon. This invokes the real rclone subprocess against the real
    // Drive backend, configured via the RCLONE_CONFIG_AIRDRIVE_* env vars the
    // production code path emits.
    let mut cmd = fx.air_drive_cmd();
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "`start` (upload) failed; stderr=\n{stderr}");

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
    // rclone here) so the subsequent `start` is purely a
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
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "`start` (download) failed; stderr=\n{stderr}");

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
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`start` (empty dir upload) failed; stderr=\n{stderr}"
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
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(
        code, 0,
        "`start` (empty dir download) failed; stderr=\n{stderr}"
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
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "`start` (nested upload) failed; stderr=\n{stderr}");

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
// E7 — a folder renamed locally moves on Drive (continuous daemon, #7)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e7_local_dir_rename_propagates_via_rclone() {
    use std::time::Duration;
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    fx.populate_local("docs/spec.txt", b"folder-rename e2e");
    seed_account_and_mapping(&fx);

    // Continuous daemon: initial-sync uploads docs/ + spec.txt, then it keeps
    // running so the subsequent local rename is handled as a live event.
    let mut daemon = common::DaemonProcess::spawn(&fx, &["--remote-poll-interval", "15"]).await;

    // Wait until `docs/spec.txt` has fully landed on Drive — not just the `docs`
    // folder. Renaming the local dir while the file upload is still in flight
    // would pull the source out from under rclone mid-copy ("object not found").
    let ready = common::wait_until(Duration::from_secs(90), || async {
        let docs = metadata::list_children(&fx.drive, &fx.run_folder_id)
            .await
            .ok()
            .and_then(|cs| cs.into_iter().find(|c| c.is_folder() && c.name == "docs"));
        match docs {
            Some(d) => metadata::list_children(&fx.drive, &d.id)
                .await
                .map(|cs| cs.iter().any(|c| c.name == "spec.txt"))
                .unwrap_or(false),
            None => false,
        }
    })
    .await;
    assert!(
        ready,
        "initial sync should upload docs/spec.txt to Drive; alive? {:?}",
        daemon.poll_alive()
    );
    let docs_id = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list run folder")
        .into_iter()
        .find(|c| c.is_folder() && c.name == "docs")
        .expect("docs folder")
        .id;

    // The continuous watcher only attaches inside `daemon::run`, which runs
    // *after* the initial sync that just uploaded spec.txt. Renaming in that gap
    // would race the inotify subscription and the event would be lost. The pause
    // control socket is created as the continuous loop comes up, so wait for it
    // (then settle briefly) to be sure the watcher is live before we rename.
    let control_sock = fx.config_dir.join("runtime").join("control.sock");
    let loop_up =
        common::wait_until(Duration::from_secs(30), || async { control_sock.exists() }).await;
    assert!(
        loop_up,
        "daemon continuous loop never came up; alive? {:?}",
        daemon.poll_alive()
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Rename the directory locally.
    std::fs::rename(fx.local_dir.join("docs"), fx.local_dir.join("documents")).unwrap();

    // The SAME Drive folder id must now be named `documents` — a move, not a
    // re-create (which would mint a new id).
    let renamed = common::wait_until(Duration::from_secs(90), || async {
        metadata::list_children(&fx.drive, &fx.run_folder_id)
            .await
            .map(|cs| {
                cs.iter()
                    .any(|c| c.is_folder() && c.name == "documents" && c.id == docs_id)
            })
            .unwrap_or(false)
    })
    .await;
    if !renamed {
        // Dump the real Drive state so a failure says *why*: documents absent =
        // the local rename never propagated; documents present with a different
        // id = it was re-created instead of moved.
        let children = metadata::list_children(&fx.drive, &fx.run_folder_id)
            .await
            .unwrap_or_default();
        let snapshot: Vec<String> = children
            .iter()
            .map(|c| format!("{} (id={}, folder={})", c.name, c.id, c.is_folder()))
            .collect();
        panic!(
            "local folder rename should move the same Drive folder (docs_id={docs_id}); \
             alive? {:?}; run-folder children now: {snapshot:?}",
            daemon.poll_alive()
        );
    }

    daemon.shutdown().await;
    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E8 — a folder renamed on Drive moves locally (continuous daemon, #7)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e8_remote_dir_rename_propagates_locally() {
    use std::time::Duration;
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    fx.populate_local("docs/spec.txt", b"remote-folder-rename e2e");
    seed_account_and_mapping(&fx);

    let mut daemon = common::DaemonProcess::spawn(&fx, &["--remote-poll-interval", "15"]).await;

    // Wait for initial sync to create `docs` on Drive, capture its id.
    let appeared = common::wait_until(Duration::from_secs(90), || async {
        metadata::list_children(&fx.drive, &fx.run_folder_id)
            .await
            .map(|cs| cs.iter().any(|c| c.is_folder() && c.name == "docs"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        appeared,
        "initial sync should create `docs` on Drive; alive? {:?}",
        daemon.poll_alive()
    );
    let docs_id = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list run folder")
        .into_iter()
        .find(|c| c.is_folder() && c.name == "docs")
        .expect("docs folder")
        .id;

    // Rename the folder on Drive (web-UI style) via the REST API.
    fx.drive
        .patch_json(
            &format!("files/{docs_id}"),
            &[],
            &serde_json::json!({ "name": "documents" }),
        )
        .await
        .expect("rename folder on Drive");

    // The change poller must propagate the rename to the local tree: the child
    // follows the folder, and the old path disappears.
    let renamed = common::wait_until(Duration::from_secs(120), || async {
        fx.local_dir.join("documents/spec.txt").is_file() && !fx.local_dir.join("docs").exists()
    })
    .await;
    assert!(
        renamed,
        "remote folder rename should move the local dir; alive? {:?}",
        daemon.poll_alive()
    );

    daemon.shutdown().await;
    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E9 — a file trashed then restored on Drive doesn't duplicate locally (#8)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e9_remote_trash_then_restore_does_not_duplicate() {
    use std::time::Duration;
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    let content = b"trash-restore e2e";
    fx.populate_local("keep.txt", content);
    seed_account_and_mapping(&fx);

    // Continuous daemon: initial-sync uploads keep.txt, then it stays alive so
    // the trash + restore that follow are handled as live remote changes.
    let mut daemon = common::DaemonProcess::spawn(&fx, &["--remote-poll-interval", "15"]).await;

    // Wait for the upload to land on Drive and capture the file id.
    let appeared = common::wait_until(Duration::from_secs(90), || async {
        metadata::list_children(&fx.drive, &fx.run_folder_id)
            .await
            .map(|cs| cs.iter().any(|c| c.name == "keep.txt"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        appeared,
        "initial sync should upload keep.txt; alive? {:?}",
        daemon.poll_alive()
    );
    let file_id = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list run folder")
        .into_iter()
        .find(|c| c.name == "keep.txt")
        .expect("keep.txt on Drive")
        .id;

    // Trash on Drive → the daemon should remove the local copy.
    fx.drive
        .patch_json(
            &format!("files/{file_id}"),
            &[],
            &serde_json::json!({ "trashed": true }),
        )
        .await
        .expect("trash keep.txt on Drive");
    let removed = common::wait_until(Duration::from_secs(120), || async {
        !fx.local_dir.join("keep.txt").exists()
    })
    .await;
    assert!(
        removed,
        "remote trash should delete the local file; alive? {:?}",
        daemon.poll_alive()
    );

    // Restore on Drive → the daemon must re-link the tombstoned row and pull the
    // SAME file back, not create a duplicate (#8).
    fx.drive
        .patch_json(
            &format!("files/{file_id}"),
            &[],
            &serde_json::json!({ "trashed": false }),
        )
        .await
        .expect("restore keep.txt on Drive");
    let restored = common::wait_until(Duration::from_secs(120), || async {
        fx.local_dir.join("keep.txt").is_file()
    })
    .await;
    assert!(
        restored,
        "remote restore should re-create the local file; alive? {:?}",
        daemon.poll_alive()
    );

    // Exactly one local file under the root (no `keep (1).txt` conflict copy),
    // with the original content intact.
    assert_eq!(
        std::fs::read(fx.local_dir.join("keep.txt")).unwrap(),
        content,
        "restored content mismatch"
    );
    let local_keep = std::fs::read_dir(&fx.local_dir)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("keep"))
        .count();
    assert_eq!(
        local_keep, 1,
        "restore must not leave a duplicate local copy"
    );

    // And exactly one non-trashed keep.txt on Drive, still the original id.
    let on_drive: Vec<_> = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list run folder")
        .into_iter()
        .filter(|c| c.name == "keep.txt")
        .collect();
    assert_eq!(
        on_drive.len(),
        1,
        "restore must not duplicate the file on Drive; saw {on_drive:?}"
    );
    assert_eq!(
        on_drive[0].id, file_id,
        "restored file should keep its original Drive id"
    );

    daemon.shutdown().await;
    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E10 — a native Google Doc is materialised as a local `.gdoc` shortcut (#3)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e10_native_google_doc_becomes_local_shortcut() {
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    // Create a native Google Doc on Drive (no downloadable bytes) under the run
    // folder, directly via files.create with the Docs mime type.
    let created = fx
        .drive
        .post_json(
            "files",
            &[("fields", "id,name,mimeType")],
            &serde_json::json!({
                "name": "Meeting Notes",
                "mimeType": "application/vnd.google-apps.document",
                "parents": [fx.run_folder_id],
            }),
        )
        .await
        .expect("create native Google Doc on Drive");
    let doc_id = created
        .get("id")
        .and_then(|v| v.as_str())
        .expect("created doc has an id")
        .to_owned();

    seed_account_and_mapping(&fx);

    // The local tree is empty — initial-sync must represent the native doc as a
    // `.gdoc` shortcut file rather than skip it silently (#3).
    let mut cmd = fx.air_drive_cmd();
    cmd.arg("start");
    let (code, _stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "`start` (native doc) failed; stderr=\n{stderr}");

    // A `Meeting Notes.gdoc` shortcut must exist locally, carrying the doc id and
    // a docs.google.com URL that targets it.
    let shortcut = fx.local_dir.join("Meeting Notes.gdoc");
    let raw = std::fs::read_to_string(&shortcut).unwrap_or_else(|e| {
        panic!(
            "expected `Meeting Notes.gdoc` shortcut at {}: {e}",
            shortcut.display()
        )
    });
    let json: serde_json::Value = serde_json::from_str(&raw).expect("shortcut is valid JSON");
    assert_eq!(
        json.get("doc_id").and_then(|v| v.as_str()),
        Some(doc_id.as_str()),
        "shortcut doc_id should match the Drive doc"
    );
    let url = json.get("url").and_then(|v| v.as_str()).unwrap_or_default();
    assert!(
        url.contains("docs.google.com/document/") && url.contains(&doc_id),
        "shortcut url should target the doc; got `{url}`"
    );

    // The shortcut is one-directional: no `.gdoc` file may be uploaded to Drive —
    // only the original native doc should sit under the run folder.
    let children = metadata::list_children(&fx.drive, &fx.run_folder_id)
        .await
        .expect("list run folder");
    assert!(
        !children.iter().any(|c| c.name.ends_with(".gdoc")),
        "shortcut file must not be uploaded to Drive; saw {:?}",
        children.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// E11 — a folder created on Drive with a file inside it syncs down to local,
// live (continuous daemon). Guards the remote change feed end-to-end, including
// the parent-chain path resolution that the root-alias bug broke.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires real Drive credentials — run via `cargo test -- --ignored`"]
async fn e11_remote_new_folder_with_file_syncs_locally() {
    use std::time::Duration;
    skip_unless_configured!(cfg);
    let fx = common::E2eFixture::new(cfg).await;

    // A sentinel local file forces the initial sync to run and the change-cursor
    // baseline to be persisted; once it lands on Drive we know the poller is live
    // and any subsequent remote change will be after the baseline token.
    fx.populate_local("ready.txt", b"sentinel");
    seed_account_and_mapping(&fx);
    let mut daemon = common::DaemonProcess::spawn(&fx, &["--remote-poll-interval", "15"]).await;

    let baseline = common::wait_until(Duration::from_secs(90), || async {
        metadata::list_children(&fx.drive, &fx.run_folder_id)
            .await
            .map(|cs| cs.iter().any(|c| c.name == "ready.txt"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        baseline,
        "initial sync should upload the sentinel; alive? {:?}",
        daemon.poll_alive()
    );

    // Web-UI style: create a brand-new folder on Drive and drop a file inside it.
    let content = b"remote nested payload";
    let newdir_id = metadata::create_folder(&fx.drive, &fx.run_folder_id, "newdir")
        .await
        .expect("create newdir on Drive")
        .id;
    fx.drive
        .upload_multipart(
            &serde_json::json!({
                "name": "note.txt",
                "parents": [newdir_id],
                "mimeType": "text/plain",
            }),
            "text/plain",
            content,
        )
        .await
        .expect("upload note.txt into newdir on Drive");

    // The change feed must materialise the folder AND the nested file locally.
    let synced = common::wait_until(Duration::from_secs(120), || async {
        fx.local_dir.join("newdir/note.txt").is_file()
    })
    .await;
    assert!(
        synced,
        "remote new folder + file should sync locally; alive? {:?}",
        daemon.poll_alive()
    );
    assert_eq!(
        std::fs::read(fx.local_dir.join("newdir/note.txt")).unwrap(),
        content,
        "downloaded content mismatch for newdir/note.txt"
    );

    daemon.shutdown().await;
    fx.cleanup().await;
}

// NOTE: the symmetric local→remote case (e12 — a folder created locally with a
// file inside, propagated up by the live daemon) is covered at the mocked level
// by `us2_11_local_new_dir_with_nested_file_propagates`. It is NOT reinstated as
// an e2e yet: the `Created(dir)` rescan fixes the deterministic case, but the
// real-rclone/Drive live path still fails (file not uploaded within the window)
// for an undiagnosed reason — tracked in #21 (needs daemon-log capture in the
// e2e harness to investigate).

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
