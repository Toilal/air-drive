//! `air-drive start` (T039, FR-014, FR-017).
//!
//! For the MVP this only implements the `--initial-sync` path: acquire the
//! single-instance lock, load the linked account + mapping, run the initial
//! reconciliation, then exit cleanly. The continuous-sync loop is wired in by Phase 4
//! (T053+).

use std::path::Path;

use crate::cli::{ExitCode, runtime};
use crate::config::Config;
use crate::daemon::lock::Lock;
use crate::error::{Error, Result};
use crate::reconcile;
use crate::state::cursor;
use crate::state::{accounts, mapping};

/// Run the `start` subcommand. Honours the `--initial-sync` flag and the
/// `--remote-poll-interval` override (the latter is plumbed through Config but no-op
/// in the MVP — Phase 4 wires it into the change poller).
pub async fn run(
    config_dir_override: Option<&Path>,
    cfg: &Config,
    initial_sync: bool,
    _remote_poll_interval: Option<u64>,
    no_download_rclone: bool,
) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;

    // 1. Acquire the single-instance lock (FR-017). On contention, exit 6.
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

    // 3. Initial-sync gate: if the cursor has never been set and --initial-sync isn't
    //    passed, refuse (per the spec, contracts/cli.md).
    let cursor_exists = cursor::get(db.connection(), mapping_row.id)
        .await?
        .is_some();
    if !cursor_exists && !initial_sync {
        return Err(Error::Config(
            "first-time start requires --initial-sync".into(),
        ));
    }

    // 4. Build the engine and run reconciliation.
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

    // FR-010 housekeeping: drop any leftover partials from a previous crash before
    // staging new downloads.
    crate::engine::staging::cleanup_orphans(&local_root)?;

    if initial_sync && !cursor_exists {
        reconcile::initial(
            &http,
            engine,
            &db,
            mapping_row.id,
            &local_root,
            &mapping_row.remote_folder_id,
        )
        .await?;
    }

    // Continuous-sync loop is Phase 4. For now `start` exits cleanly after
    // initial-sync converges.
    Ok(ExitCode::Ok)
}
