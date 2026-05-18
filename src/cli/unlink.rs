//! `air-drive unlink` (T040b, FR-018, FR-019).
//!
//! Removes the linked account row, the folder-mapping row, and the OAuth tokens file.
//! The local watched folder *contents* are intentionally left alone (FR-019).
//!
//! Exit codes (per `contracts/cli.md`):
//!
//! - `0` — success.
//! - `8` — refused because a daemon is currently running against this config.

use std::path::Path;

use crate::cli::{ExitCode, runtime};
use crate::daemon::lock::Lock;
use crate::drive::auth::TOKENS_FILE;
use crate::error::{Error, Result};

/// Run the `unlink` subcommand. `--yes` skips the interactive confirmation.
pub async fn run(config_dir_override: Option<&Path>, _yes: bool) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;

    // Refuse if a daemon is running (FR-018). We try to acquire the lock; if it
    // succeeds, drop it immediately and proceed. If it fails with Lock, exit 8.
    match Lock::acquire(paths.config()) {
        Ok(_held) => { /* we hold it, drop at end of scope */ }
        Err(Error::Lock { pid }) => {
            tracing::error!(holder_pid = ?pid, "cannot unlink while daemon is running");
            return Ok(ExitCode::UnlinkWhileRunning);
        }
        Err(e) => return Err(e),
    }

    // Clear DB rows. CASCADE on `account.id = 1` drops every dependent row
    // (mapping, items, ops, conflicts, cursor) — cf. state::tests::
    // delete_account_cascades_to_dependent_rows.
    let db = runtime::open_state(&paths).await?;
    db.connection()
        .call(|c| {
            c.execute("DELETE FROM account WHERE id = 1", [])?;
            Ok(())
        })
        .await
        .map_err(Into::<Error>::into)?;

    // Delete tokens.json if present.
    let tokens_path = paths.config().join(TOKENS_FILE);
    match std::fs::remove_file(&tokens_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    tracing::info!("account, mapping and tokens cleared (local files untouched)");
    Ok(ExitCode::Ok)
}
