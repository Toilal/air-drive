//! `air-drive resume` (T067, FR-015).
//!
//! Same wire protocol as `pause`, just a different verb. On the daemon side
//! `resume` flips the pause flag and the dispatcher's `wait_for_resume`
//! returns immediately — the in-flight backlog drains on its own without a
//! separate "force convergence" step.

use std::path::Path;

use crate::cli::ExitCode;
use crate::error::Result;

/// `air-drive resume` entry point.
pub async fn run(config_dir_override: Option<&Path>) -> Result<ExitCode> {
    super::pause::send("resume", config_dir_override).await
}
