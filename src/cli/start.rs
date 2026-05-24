//! `air-drive start`.
//!
//! Acquires the single-instance lock, loads account + mapping, optionally runs
//! the initial reconciliation, then enters the continuous-sync daemon loop
//! ([`crate::daemon::run`]). The loop returns cleanly on SIGTERM / SIGINT
//! (`daemon::wait_for_shutdown_signal`).

use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::cli::interactive;
use crate::cli::{ExitCode, runtime};
use crate::config::Config;
use crate::daemon::{DaemonContext, lock::Lock};
use crate::drive::http::DriveHttp;
use crate::drive::metadata;
use crate::error::{Error, Result};
use crate::reconcile;
use crate::state::cursor;
use crate::state::mapping::FolderMapping;
use crate::state::{Db, accounts, mapping};

/// Run the `start` subcommand. Honours the `--initial-sync` flag and the
/// `--remote-poll-interval` override.
pub async fn run(
    config_dir_override: Option<&Path>,
    cfg: &Config,
    initial_sync: bool,
    remote_poll_interval: Option<u64>,
    no_download_rclone: bool,
) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;

    // 1. Acquire the single-instance lock. On contention, exit 6.
    let _lock = match Lock::acquire(paths.config()) {
        Ok(l) => l,
        Err(Error::Lock { pid }) => {
            tracing::error!(holder_pid = ?pid, "another air-drive daemon is already running");
            return Ok(ExitCode::LockHeld);
        }
        Err(e) => return Err(e),
    };

    // 2. Load account + mapping. Both must exist to proceed.
    let db = runtime::open_state(&paths).await?;
    let Some(_account) = accounts::get_single(db.connection()).await? else {
        return Err(Error::Mapping(
            "no linked account — run `air-drive link` first".into(),
        ));
    };
    let Some(mapping_row) = mapping::get_single(db.connection()).await? else {
        return Err(Error::Mapping(
            "no folder mapping — run `air-drive map` first".into(),
        ));
    };

    // 3. Initial-sync gate: if the cursor has never been set and --initial-sync
    //    isn't passed, refuse.
    let cursor_exists = cursor::get(db.connection(), mapping_row.id)
        .await?
        .is_some();
    if !cursor_exists && !initial_sync {
        return Err(Error::Config(
            "first-time start requires --initial-sync".into(),
        ));
    }

    // 4. Build the engine + HTTP client.
    let token = runtime::build_token_provider(cfg, &paths).await?;
    let http = runtime::build_drive_http(token.clone())?;
    let local_root = std::path::PathBuf::from(&mapping_row.local_path);
    // Pre-flight: the watcher will inotify-subscribe to `local_root` shortly,
    // and the engine + reconciler will write into it. A missing folder used to
    // surface as a raw `notify watch(...): No such file or directory` error;
    // honour `watch.auto_create_root` instead.
    ensure_local_root(&local_root, cfg.watch.auto_create_root)?;
    // Remote-side counterpart: probe that `remote_folder_id` still resolves to
    // a live (non-trashed) folder. If it was trashed between two daemon runs
    // and the user originally pointed `map` at a `path:` notation target, we
    // can recreate it (gated by `mapping.auto_create_remote_root` or an
    // interactive confirmation) and persist the refreshed Drive ID.
    let remote_root_id = ensure_remote_root(&db, &http, cfg, &mapping_row).await?;
    let engine = runtime::build_engine(
        cfg,
        &paths,
        &http,
        token,
        local_root.clone(),
        no_download_rclone,
    )
    .await?;

    // Housekeeping: drop any leftover partials from a previous crash.
    crate::engine::staging::cleanup_orphans(&local_root)?;

    // 5. First-time initial sync.
    if initial_sync && !cursor_exists {
        reconcile::initial(
            &http,
            engine.clone(),
            &db,
            mapping_row.id,
            &local_root,
            &remote_root_id,
        )
        .await?;
    }

    // Test-only escape hatch: integration tests that exercise only the initial-sync
    // path (`tests/integration/initial_sync.rs`) invoke `start --initial-sync` and
    // wait for the binary to exit via `Command::output()`. They don't want the
    // continuous loop to kick in. The env var is documented in
    // `tests/integration/common/mod.rs`.
    if std::env::var("AIR_DRIVE_TEST_EXIT_AFTER_INITIAL_SYNC").as_deref() == Ok("1") {
        return Ok(ExitCode::Ok);
    }

    // 6. Continuous loop. The cursor must exist by now; if it doesn't,
    //    something is badly wrong — surface as an error rather than silently
    //    skipping the loop.
    if cursor::get(db.connection(), mapping_row.id)
        .await?
        .is_none()
    {
        return Err(Error::Mapping(
            "drive_change_cursor missing after initial-sync — refusing to enter loop".into(),
        ));
    }

    let poll_interval = remote_poll_interval
        .unwrap_or(cfg.daemon.remote_poll_interval_seconds)
        .clamp(10, 60);

    let ctx = DaemonContext {
        db,
        engine,
        http,
        mapping_id: mapping_row.id,
        local_root,
        remote_root_id,
        remote_poll_interval: Duration::from_secs(poll_interval),
        runtime_dir: paths.runtime().to_path_buf(),
        watch_ignore_patterns: cfg.watch.ignore_patterns.clone(),
    };

    let cancel = CancellationToken::new();
    crate::daemon::run(ctx, cancel).await?;

    Ok(ExitCode::Ok)
}

/// Ensure the watched folder exists before the daemon attaches the inotify
/// watcher or the engine writes into it.
///
/// - If `path` is an existing directory: succeed.
/// - If `path` exists but is not a directory: actionable error.
/// - If `path` is missing and `auto_create` is `true`: create silently.
/// - If `path` is missing and `auto_create` is `false`:
///   - on an interactive stdin (TTY): prompt the user; create on confirmation.
///   - otherwise (systemd, piped script): refuse with an actionable error
///     pointing the user at the `watch.auto_create_root` toggle. Conservative
///     by design so a daemon restart cannot silently materialise a new tree.
fn ensure_local_root(path: &Path, auto_create: bool) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(Error::Mapping(format!(
            "watched folder `{}` exists but is not a directory; \
             remove the file or pick a different `local_path` via `air-drive map`",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let allow_create = if auto_create {
                true
            } else {
                interactive::confirm(&format!(
                    "watched folder `{}` does not exist — create it?",
                    path.display()
                ))?
            };
            if !allow_create {
                return Err(Error::Mapping(format!(
                    "watched folder `{}` does not exist. \
                     Create it manually, set `watch.auto_create_root = true` in config.toml, \
                     or re-run interactively to confirm.",
                    path.display()
                )));
            }
            std::fs::create_dir_all(path).map_err(|io| {
                Error::Mapping(format!(
                    "watched folder `{}` does not exist and could not be created: {io}",
                    path.display()
                ))
            })?;
            tracing::info!(path = %path.display(), "created watched folder");
            Ok(())
        }
        Err(e) => Err(Error::Mapping(format!(
            "cannot inspect watched folder `{}`: {e}",
            path.display()
        ))),
    }
}

/// Verify the remote root recorded in `folder_mapping` still resolves to a
/// live, non-trashed folder on Drive. If it doesn't and the original `<remote-
/// folder>` argument was `path:` notation, re-resolve the spec (creating
/// missing segments when authorised) and persist the refreshed Drive ID.
///
/// - If `remote_folder_id` resolves to a live folder: returns the stored id.
/// - If it 404s or comes back trashed AND `remote_folder_spec` uses `path:`:
///   gated by `mapping.auto_create_remote_root` (silent) or an interactive
///   prompt; on confirmation re-runs `resolve_path` with auto-create, updates
///   the DB, returns the new id.
/// - Otherwise (no spec / bare ID / URL spec / declined / non-interactive
///   stdin): actionable error pointing at `air-drive map` or the config flag.
async fn ensure_remote_root(
    db: &Db,
    http: &DriveHttp,
    cfg: &Config,
    mapping_row: &FolderMapping,
) -> Result<String> {
    let stored_id = mapping_row.remote_folder_id.as_str();
    match metadata::get_file_raw(http, stored_id, "id,mimeType,trashed").await {
        Ok(v) => {
            let trashed = v
                .get("trashed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !trashed {
                return Ok(stored_id.to_owned());
            }
            tracing::warn!(
                folder_id = %stored_id,
                "remote root is in the trash"
            );
        }
        Err(Error::Drive(msg)) if msg.starts_with("HTTP 404") => {
            tracing::warn!(
                folder_id = %stored_id,
                "remote root no longer exists on Drive"
            );
        }
        Err(e) => {
            // Transient errors (OAuth refresh failure, network, 5xx) are not
            // ours to resolve at pre-flight time — the daemon's continuous
            // loop handles them by flipping `state_meta.blocked_kind`. Trust
            // the stored ID and let the loop surface the real problem.
            tracing::warn!(
                folder_id = %stored_id,
                error = %e,
                "could not probe remote root at startup — deferring to daemon loop"
            );
            return Ok(stored_id.to_owned());
        }
    }

    // Below: the stored ID points at a missing or trashed folder. Try to
    // recover via the original `path:` spec, if we have one and it's path:.
    let Some(spec) = mapping_row.remote_folder_spec.as_deref() else {
        return Err(Error::Mapping(format!(
            "remote folder `{stored_id}` is missing or trashed and the original \
             mapping spec was not recorded. Run `air-drive map <local> <remote>` \
             to point the daemon at a fresh folder."
        )));
    };
    let trimmed_spec = spec.trim();
    if metadata::is_drive_url(trimmed_spec) {
        return Err(Error::Mapping(format!(
            "remote folder `{stored_id}` (URL spec `{trimmed_spec}`) is missing or trashed. \
             URLs reference a specific resource that cannot be recreated — run \
             `air-drive map <local> <name>` to point the daemon at a fresh folder."
        )));
    }

    let allow_create = if cfg.mapping.auto_create_remote_root {
        true
    } else {
        interactive::confirm(&format!(
            "remote folder for `{trimmed_spec}` is missing on Drive — recreate it?"
        ))?
    };
    if !allow_create {
        return Err(Error::Mapping(format!(
            "remote folder `{stored_id}` (spec `{trimmed_spec}`) is missing or trashed. \
             Re-run interactively to confirm, set `mapping.auto_create_remote_root = true` \
             in config.toml, or run `air-drive map` again."
        )));
    }

    let new_id = metadata::resolve_path(http, trimmed_spec, true).await?;
    mapping::update_remote_folder_id(db.connection(), &new_id).await?;
    tracing::info!(
        spec = %trimmed_spec,
        old_id = %stored_id,
        new_id = %new_id,
        "recreated remote root and refreshed mapping"
    );
    Ok(new_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_root_succeeds_when_dir_exists() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_local_root(tmp.path(), false).expect("existing dir should succeed");
        ensure_local_root(tmp.path(), true).expect("existing dir should succeed");
    }

    #[test]
    fn ensure_root_creates_when_missing_and_auto_create_true() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("nested/child");
        assert!(!target.exists());
        ensure_local_root(&target, true).expect("should create the folder");
        assert!(target.is_dir());
    }

    #[test]
    fn ensure_root_errors_actionably_when_missing_and_auto_create_false() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("missing");
        let err = ensure_local_root(&target, false).expect_err("should refuse");
        let msg = err.to_string();
        // Actionable: mentions the path and the config toggle, hides inotify wording.
        assert!(msg.contains(&target.display().to_string()), "msg: {msg}");
        assert!(msg.contains("watch.auto_create_root"), "msg: {msg}");
        assert!(
            !msg.contains("notify watch"),
            "raw watcher wording leaked: {msg}"
        );
        assert!(!msg.contains("os error"), "raw os error leaked: {msg}");
    }

    #[test]
    fn ensure_root_rejects_existing_non_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a-file-not-a-dir");
        std::fs::write(&file, "not a dir").unwrap();
        let err = ensure_local_root(&file, true).expect_err("file is not a dir");
        assert!(err.to_string().contains("not a directory"));
    }
}
