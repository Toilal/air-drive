//! `air-drive pause` (T066, FR-015).
//!
//! Thin client over `daemon::pause::send_command`. Connects to the control
//! socket, sends `pause`, prints the daemon's reply. Exit `0` on ack, `7`
//! when no daemon is running (socket missing or refuses the connection).

use std::path::Path;

use crate::cli::{ExitCode, runtime};
use crate::daemon::pause::{send_command, socket_path};
use crate::error::{Error, Result};

/// `air-drive pause` entry point.
pub async fn run(config_dir_override: Option<&Path>) -> Result<ExitCode> {
    send("pause", config_dir_override).await
}

/// Shared implementation between `pause` and `resume`.
pub(super) async fn send(command: &str, config_dir_override: Option<&Path>) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;
    let sock = socket_path(paths.runtime());
    match send_command(&sock, command).await {
        Ok(reply) if reply == "ok" => Ok(ExitCode::Ok),
        Ok(other) => Err(Error::Config(format!("control socket replied: {other}"))),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            tracing::error!(
                socket = %sock.display(),
                "no daemon running on this config dir"
            );
            Ok(ExitCode::NoDaemonRunning)
        }
        Err(e) => Err(Error::Io(e)),
    }
}
