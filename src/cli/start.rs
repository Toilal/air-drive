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
use crate::error::{Error, Result};
use crate::reconcile;
use crate::state::cursor;
use crate::state::{accounts, mapping};

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
            &mapping_row.remote_folder_id,
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
        remote_root_id: mapping_row.remote_folder_id.clone(),
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
