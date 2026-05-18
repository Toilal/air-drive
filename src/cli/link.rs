//! `air-drive link` (T037, FR-001).
//!
//! Walks the user through the OAuth dance, captures the linked user's primary email
//! via Drive's `about.user`, and persists an `account` row. Idempotent — re-running
//! refreshes the `linked_at` timestamp.
//!
//! Exit codes (per `contracts/cli.md`):
//!
//! - `0` — success.
//! - `2` — OAuth error (refused, revoked, invalid `client_id`).
//! - `3` — network failure reaching Google.

use std::path::Path;

use crate::cli::{ExitCode, runtime};
use crate::config::Config;
use crate::drive::metadata;
use crate::error::Result;
use crate::state::accounts;
use crate::state::meta;
use crate::state::unix_now;

/// Run the `link` subcommand. `account_label`, when present, is logged so the user
/// gets feedback; the column to persist it lives in a future schema migration (multi-
/// account rollout). Returning `OauthError` / `NetworkError` is the caller's job
/// (see [`super::fallback_exit_code`]), which inspects [`crate::error::Error`].
pub async fn run(
    config_dir_override: Option<&Path>,
    cfg: &Config,
    account_label: Option<String>,
) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;
    let token = runtime::build_token_provider(cfg, &paths).await?;
    let http = runtime::build_drive_http(token)?;
    let about = metadata::about_user(&http).await?;
    let db = runtime::open_state(&paths).await?;
    accounts::upsert(db.connection(), &about.email, unix_now()).await?;
    // A successful link with fresh tokens clears any prior `blocked_kind =
    // auth` state on disk. The daemon (if running) will notice on its next
    // 30 s poll and resume work; restart isn't required.
    meta::clear_blocked(db.connection()).await?;
    if let Some(label) = account_label {
        tracing::warn!(
            label,
            email = %about.email,
            "`--account-label` is accepted but not yet persisted (account table has no label column — pending multi-account migration)"
        );
    }
    tracing::info!(email = %about.email, "account linked");
    Ok(ExitCode::Ok)
}
