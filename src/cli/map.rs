//! `air-drive map`.
//!
//! Canonicalises the local path (creating it if missing), resolves the remote folder
//! argument (Drive ID / URL / `path:` notation) to a Drive ID, and writes the
//! singleton `folder_mapping` row.
//!
//! Exit codes:
//!
//! - `0` — success.
//! - `4` — local path doesn't exist *and* couldn't be created, or exists but is a file.
//! - `5` — remote folder cannot be resolved.

use std::path::{Path, PathBuf};

use crate::cli::interactive;
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

    // 1. Local path: canonicalise; create only when authorised (config flag
    //    or interactive prompt). On non-TTY stdin we refuse silently to keep
    //    daemon invocations conservative.
    let canonical = match canonicalise_local(&local_path, cfg.watch.auto_create_root) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(path = %local_path.display(), error = %e, "invalid local path");
            return Ok(ExitCode::MapLocalInvalid);
        }
    };
    // 2. Resolve the remote folder spec. A `Mapping` error means "no such
    //    folder" — when the spec uses `path:` notation and the user (via
    //    config or an interactive prompt) authorises it, retry with auto-create
    //    enabled. Any other error path is non-recoverable here → exit 5.
    let cfg_auto = cfg.mapping.auto_create_remote_root;
    let is_path_notation = remote_folder.trim_start().starts_with("path:");
    let remote_id = match metadata::resolve_path(&http, &remote_folder, cfg_auto).await {
        Ok(id) => id,
        Err(Error::Mapping(msg)) if !cfg_auto && is_path_notation => {
            let create = interactive::confirm(&format!(
                "remote folder for `{remote_folder}` does not exist on Drive — create it?"
            ))?;
            if !create {
                tracing::error!(error = %msg, "cannot resolve remote folder");
                return Ok(ExitCode::MapRemoteUnresolvable);
            }
            metadata::resolve_path(&http, &remote_folder, true).await?
        }
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

/// Try to canonicalise a local path the user passed on the CLI.
///
/// When the path is missing, creation is gated:
/// - `auto_create == true`: create without prompting.
/// - `auto_create == false` (default):
///   - interactive stdin → ask the user; create on confirmation.
///   - non-interactive stdin → refuse conservatively with an actionable error.
fn canonicalise_local(local_path: &Path, auto_create: bool) -> Result<PathBuf> {
    let path = if local_path.exists() {
        local_path.canonicalize()?
    } else {
        let allow_create = if auto_create {
            true
        } else {
            interactive::confirm(&format!(
                "local folder `{}` does not exist — create it?",
                local_path.display()
            ))?
        };
        if !allow_create {
            return Err(Error::Mapping(format!(
                "local folder `{}` does not exist. Create it manually, or set \
                 `watch.auto_create_root = true` in config.toml, or re-run \
                 interactively to confirm.",
                local_path.display()
            )));
        }
        std::fs::create_dir_all(local_path)?;
        tracing::info!(path = %local_path.display(), "created local folder");
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
