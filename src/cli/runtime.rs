//! Shared startup plumbing for every CLI subcommand.
//!
//! Centralises: config-dir resolution, opening the state DB, building the token
//! provider, the [`DriveHttp`] client, and selecting the [`SyncEngine`] flavour
//! (rclone for production, the in-process HTTP engine for the test harness).
//!
//! The engine selector reads `AIR_DRIVE_TEST_ENGINE` once on startup. Possible values:
//!
//! - `http` — use [`crate::engine::http::HttpEngine`] (integration tests).
//! - anything else / unset — use [`crate::engine::rclone::RcloneEngine`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::config::paths::Paths;
use crate::drive::auth::{TokenProvider, build_provider};
use crate::drive::http::DriveHttp;
use crate::engine::SyncEngine;
use crate::engine::http::HttpEngine;
use crate::engine::rclone::RcloneEngine;
use crate::engine::rclone_path;
use crate::error::Result;
use crate::state::Db;

/// Resolve the XDG-style paths for this run, honouring `--config-dir`. Also creates
/// the directories on disk with `0700` perms (idempotent).
pub fn resolve_paths(config_dir_override: Option<&Path>) -> Result<Paths> {
    let paths = Paths::discover(config_dir_override)?;
    paths.ensure_exist()?;
    Ok(paths)
}

/// Load `config.toml` from the resolved config dir. Missing file → defaults.
///
/// Runs the surgical schema migration first ([`crate::config::migrate`]):
/// any field present in the current `Config` schema but missing from the
/// on-disk file is inserted with its default value and a descriptive
/// comment, leaving the rest of the file (user comments, key order,
/// overrides) untouched. The strict `deny_unknown_fields` load happens
/// after migration so freshly-added fields are recognised on the very
/// same startup.
pub fn load_config(paths: &Paths) -> Result<Config> {
    let path = paths.config().join("config.toml");
    let report = crate::config::migrate::migrate_on_disk(&path)?;
    if report.changed() {
        for (section, key) in &report.inserted {
            tracing::info!(
                section = %section,
                key = %key,
                path = %path.display(),
                "config.toml: inserted missing field with default value"
            );
        }
    }
    Config::load(&path)
}

/// Open the SQLite state DB at `<config_dir>/state.db`, running pragmas + migrations.
pub async fn open_state(paths: &Paths) -> Result<Db> {
    Db::open(&paths.config().join("state.db")).await
}

/// Build the token provider, honouring `AIR_DRIVE_TEST_BEARER_TOKEN` for integration
/// tests. Read once here so the daemon code path never has to call
/// `std::env::set_var` (unsafe under Rust 2024).
pub async fn build_token_provider(cfg: &Config, paths: &Paths) -> Result<Arc<dyn TokenProvider>> {
    let override_tok = std::env::var("AIR_DRIVE_TEST_BEARER_TOKEN").ok();
    build_provider(&cfg.oauth, paths.config(), override_tok.as_deref()).await
}

/// Build the [`DriveHttp`] client with env-driven URL overrides (`AIR_DRIVE_DRIVE_BASE_URL`
/// and `AIR_DRIVE_DRIVE_UPLOAD_BASE_URL`).
pub fn build_drive_http(token: Arc<dyn TokenProvider>) -> Result<DriveHttp> {
    DriveHttp::new(token)
}

/// Build the [`SyncEngine`] selected by `AIR_DRIVE_TEST_ENGINE`. When the env var is
/// `http`, return an [`HttpEngine`] over the supplied `DriveHttp` client. Otherwise
/// build an [`RcloneEngine`] — `no_download_rclone` mirrors the global CLI flag.
pub async fn build_engine(
    cfg: &Config,
    paths: &Paths,
    http: &DriveHttp,
    token_provider: Arc<dyn TokenProvider>,
    local_root: PathBuf,
    no_download_rclone: bool,
) -> Result<Arc<dyn SyncEngine>> {
    if std::env::var("AIR_DRIVE_TEST_ENGINE").as_deref() == Ok("http") {
        return Ok(Arc::new(HttpEngine::new(http.clone())));
    }
    let binary = rclone_path::resolve(&cfg.rclone, paths.cache(), !no_download_rclone).await?;
    Ok(Arc::new(RcloneEngine::new(
        binary,
        token_provider,
        cfg.oauth.client_id.clone(),
        local_root,
        http.clone(),
    )))
}
