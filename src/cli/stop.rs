//! `air-drive stop`.
//!
//! Stops a running daemon by sending it `SIGTERM` — the same graceful-shutdown
//! path as Ctrl-C or `systemctl stop`, so in-flight operations wind down and the
//! control socket + lock are released cleanly. The target PID is read from the
//! single-instance lock file. Exit `0` once the signal is delivered, `7` when no
//! daemon is running on this config dir.

use std::path::Path;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::cli::{ExitCode, runtime};
use crate::daemon::lock::Lock;
use crate::error::{Error, Result};

/// `air-drive stop` entry point.
pub async fn run(config_dir_override: Option<&Path>) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;
    match Lock::holder_pid(paths.config()) {
        Some(pid) => {
            let raw = i32::try_from(pid)
                .map_err(|_| Error::Config(format!("implausible daemon pid {pid}")))?;
            kill(Pid::from_raw(raw), Signal::SIGTERM)
                .map_err(|e| Error::Config(format!("failed to signal daemon (pid {pid}): {e}")))?;
            tracing::info!(pid, "sent SIGTERM to the running daemon");
            println!("Stopping air-drive daemon (pid {pid})…");
            Ok(ExitCode::Ok)
        }
        None => {
            tracing::error!("no daemon running on this config dir");
            Ok(ExitCode::NoDaemonRunning)
        }
    }
}
