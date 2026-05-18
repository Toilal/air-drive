//! `air-drive map` (T038, FR-002).
//!
//! Canonicalises the local path (creating it if missing), resolves the remote folder
//! argument (Drive ID / URL / `path:` notation) to a Drive ID, and writes the
//! singleton `folder_mapping` row.
//!
//! Exit codes (per `contracts/cli.md`):
//!
//! - `0` — success.
//! - `4` — local path doesn't exist *and* couldn't be created, or exists but is a file.
//! - `5` — remote folder cannot be resolved.

use std::path::{Path, PathBuf};

use crate::cli::{ExitCode, runtime};
use crate::config::Config;
use crate::drive::metadata;
use crate::error::{Error, Result};
use crate::state::accounts::AccountId;
use crate::state::mapping;
use crate::state::unix_now;

/// Run the `map` subcommand.
pub async fn run(
    config_dir_override: Option<&Path>,
    cfg: &Config,
    local_path: PathBuf,
    remote_folder: String,
) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;
    let token = runtime::build_token_provider(cfg, &paths).await?;
    let http = runtime::build_drive_http(token)?;

    // 1. Local path: canonicalise, create if missing. If we can't, exit 4.
    let canonical = match canonicalise_local(&local_path) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(path = %local_path.display(), error = %e, "invalid local path");
            return Ok(ExitCode::MapLocalInvalid);
        }
    };
    // 2. Resolve the remote folder spec. A `Mapping` error means "no such folder" → 5.
    let remote_id = match metadata::resolve_path(&http, &remote_folder).await {
        Ok(id) => id,
        Err(Error::Mapping(msg)) => {
            tracing::error!(error = %msg, "cannot resolve remote folder");
            return Ok(ExitCode::MapRemoteUnresolvable);
        }
        Err(e) => return Err(e),
    };

    // 3. Persist. Mapping requires an existing account; if none, treat as user error
    //    and surface ExitCode::GenericError so the user knows to `link` first.
    let db = runtime::open_state(&paths).await?;
    let account = crate::state::accounts::get_single(db.connection()).await?;
    let Some(account) = account else {
        tracing::error!("no linked account — run `air-drive link` first");
        return Ok(ExitCode::GenericError);
    };
    let remote_name = metadata::get_file(&http, &remote_id)
        .await
        .ok()
        .map(|m| m.name);
    persist_mapping(
        &db,
        account.id,
        canonical.to_string_lossy().as_ref(),
        &remote_id,
        remote_name.as_deref(),
    )
    .await?;
    tracing::info!(
        local = %canonical.display(),
        remote = %remote_id,
        "mapping recorded"
    );
    Ok(ExitCode::Ok)
}

/// Try to canonicalise a local path the user passed on the CLI. If the path doesn't
/// exist yet, attempt to create it. Errors propagate up so the caller maps them to
/// the right exit code.
fn canonicalise_local(local_path: &Path) -> Result<PathBuf> {
    let path = if local_path.exists() {
        local_path.canonicalize()?
    } else {
        std::fs::create_dir_all(local_path)?;
        local_path.canonicalize()?
    };
    if !path.is_dir() {
        return Err(Error::Mapping(format!(
            "`{}` is not a directory",
            path.display()
        )));
    }
    Ok(path)
}

async fn persist_mapping(
    db: &crate::state::Db,
    account_id: AccountId,
    local_path: &str,
    remote_folder_id: &str,
    remote_folder_name: Option<&str>,
) -> Result<()> {
    mapping::upsert(
        db.connection(),
        account_id,
        local_path,
        remote_folder_id,
        remote_folder_name,
        unix_now(),
    )
    .await?;
    Ok(())
}
