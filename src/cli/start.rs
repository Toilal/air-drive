//! `air-drive start`.
//!
//! Acquires the single-instance lock, loads account + mapping, optionally runs
//! the initial reconciliation, then enters the continuous-sync daemon loop
//! ([`crate::daemon::run`]). The loop returns cleanly on SIGTERM / SIGINT
//! (`daemon::wait_for_shutdown_signal`).

use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

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
