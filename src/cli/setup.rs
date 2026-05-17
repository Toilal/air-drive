//! `air-drive setup` (T040).
//!
//! The interactive wrapper that drives `link` → prompt for local/remote folders →
//! `map` → `start --initial-sync`. The prompts require a TTY-savvy crate
//! ([`dialoguer`] in particular); wiring that in is intentionally deferred so this
//! MVP delivery doesn't drag in a heavy dep just to satisfy a single subcommand.
//!
//! Today the subcommand stubs out with a clear error so the user knows to invoke the
//! sub-commands individually. The integration suite never exercises `setup` — it
//! drives the daemon through `link`, `map`, and `start` directly.

use std::path::Path;

use crate::cli::ExitCode;
use crate::error::{Error, Result};

/// Run the `setup` subcommand. Currently a stub — returns an error pointing the user
/// at the underlying sub-commands.
pub async fn run(_config_dir_override: Option<&Path>, _install_service: bool) -> Result<ExitCode> {
    Err(Error::Config(
        "`air-drive setup` interactive mode is not yet implemented in this MVP. \
         Run `air-drive link`, then `air-drive map <local> <remote>`, then \
         `air-drive start --initial-sync`."
            .into(),
    ))
}
