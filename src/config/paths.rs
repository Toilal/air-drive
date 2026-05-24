//! XDG path resolution for `air-drive`.
//!
//! Three runtime directories are tracked:
//!
//! - **config** — persisted config + state DB + OAuth tokens (default
//!   `$XDG_CONFIG_HOME/air-drive`, i.e. `~/.config/air-drive`).
//! - **cache** — non-essential artefacts that can be safely deleted, primarily the cached
//!   `rclone` binary (default `$XDG_CACHE_HOME/air-drive`, i.e. `~/.cache/air-drive`).
//! - **runtime** — short-lived sockets and PID files (default `$XDG_RUNTIME_DIR/air-drive`;
//!   falls back to `<config_dir>/runtime` when no runtime dir is set — deterministic per
//!   user, no uid lookup required).
//!
//! The `--config-dir` CLI flag only overrides the **config** directory; the cache and
//! runtime directories stay on their XDG defaults so users who relocate config don't
//! accidentally split their rclone cache.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::error::{Error, Result};

const QUALIFIER: &str = "dev";
const ORGANISATION: &str = "air-drive";
const APPLICATION: &str = "air-drive";

/// Resolved on-disk locations for the running daemon.
#[derive(Debug, Clone)]
pub struct Paths {
    config: PathBuf,
    cache: PathBuf,
    runtime: PathBuf,
}

impl Paths {
    /// Resolve the three directories using XDG defaults.
    ///
    /// If `config_override` is `Some`, the **config** directory is forced to that path
    /// (the user has used `--config-dir`). Cache and runtime stay on XDG defaults.
    ///
    /// This does **not** create the directories on disk; call [`Paths::ensure_exist`]
    /// when the caller is ready to write into them.
    pub fn discover(config_override: Option<&Path>) -> Result<Self> {
        let dirs = ProjectDirs::from(QUALIFIER, ORGANISATION, APPLICATION).ok_or_else(|| {
            Error::Config("could not resolve XDG project directories (no $HOME?)".into())
        })?;

        // When the user explicitly overrides the config dir, force the runtime
        // and cache dirs to live under it too. This keeps test runs (which
        // pass distinct tempdirs via `--config-dir`) isolated — without it,
        // parallel test daemons would all bind the same control socket under
        // `$XDG_RUNTIME_DIR/air-drive/`.
        let (config, cache, runtime) = match config_override {
            Some(p) => {
                let p = p.to_path_buf();
                let cache = p.join("cache");
                let runtime = p.join("runtime");
                (p, cache, runtime)
            }
            None => {
                let cache = dirs.cache_dir().to_path_buf();
                let runtime = dirs
                    .runtime_dir()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| dirs.config_dir().join("runtime"));
                (dirs.config_dir().to_path_buf(), cache, runtime)
            }
        };

        Ok(Self {
            config,
            cache,
            runtime,
        })
    }

    /// Create every directory on disk with mode `0700`. Idempotent.
    ///
    /// `0700` keeps the parent directory readable only by the owning user — important
    /// for the config directory because it holds `tokens.json`.
    pub fn ensure_exist(&self) -> Result<()> {
        for dir in [&self.config, &self.cache, &self.runtime] {
            std::fs::create_dir_all(dir)?;
            set_owner_only(dir)?;
        }
        Ok(())
    }

    /// The config directory, holding `config.toml`, `tokens.json`, `state.db`, `lock`.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// The cache directory, holding the embedded `rclone` binary when downloaded.
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// The runtime directory, holding the control socket (`control.sock`).
    pub fn runtime(&self) -> &Path {
        &self.runtime
    }
}

/// Set mode `0700` on the given directory. No-op on non-Unix (the daemon targets Linux
/// only for this MVP — cf. constitution principle V — so the Unix path is the only one
/// exercised in practice).
#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_co_locates_cache_and_runtime_under_config() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::discover(Some(tmp.path())).unwrap();
        assert_eq!(paths.config(), tmp.path());
        // When the caller explicitly forces a config dir, the cache + runtime
        // co-locate beneath it (test isolation, scoped test fixtures, etc.).
        assert!(paths.cache().starts_with(tmp.path()));
        assert!(paths.runtime().starts_with(tmp.path()));
    }

    #[test]
    #[cfg(unix)]
    fn ensure_exist_is_idempotent_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: tmp.path().join("config"),
            cache: tmp.path().join("cache"),
            runtime: tmp.path().join("runtime"),
        };
        paths.ensure_exist().unwrap();
        paths.ensure_exist().unwrap();
        for dir in [paths.config(), paths.cache(), paths.runtime()] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
    }
}
