//! On-disk configuration: `config.toml` and XDG path resolution.
//!
//! The full schema is documented in `specs/001-minimal-sync-daemon/contracts/config.md`.
//! Sections:
//!
//! - `[oauth]` — optional override of the embedded OAuth `client_id`.
//! - `[mapping]` — display info for the folder mapping (the canonical
//!   `remote_folder_id` lives in `state.db`, not here).
//! - `[daemon]` — runtime tuning.
//! - `[rclone]` — explicit override of the `rclone` binary path.
//!
//! Every section is optional; a missing file is equivalent to `Config::default()`.

pub mod paths;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Top-level configuration document loaded from `config.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `[oauth]` — OAuth client override.
    pub oauth: OauthConfig,
    /// `[mapping]` — folder mapping display info.
    pub mapping: MappingConfig,
    /// `[daemon]` — runtime tuning.
    pub daemon: DaemonConfig,
    /// `[rclone]` — explicit `rclone` binary override.
    pub rclone: RcloneConfig,
}

/// Optional OAuth client override (Q1 clarification — hybrid model).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OauthConfig {
    /// Override the project-owned `client_id` with the user's own Google Cloud client.
    /// `None` means "use the embedded default".
    pub client_id: Option<String>,
}

/// Folder mapping display info. The authoritative `remote_folder_id` lives in `state.db`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MappingConfig {
    /// Absolute path of the watched local folder, for display.
    pub local_path: Option<String>,
    /// Human-readable remote folder path, for display.
    pub remote_folder_name: Option<String>,
}

/// Daemon runtime tuning.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Interval at which the daemon polls Drive `changes.list`, in seconds. Clamped
    /// to `[10, 60]` by the daemon at startup.
    pub remote_poll_interval_seconds: u64,
    /// Interval of the safety-net reconciliation cycle, in seconds. Constitution
    /// principle II requires this to stay ≥ 5 min.
    pub safety_net_interval_seconds: u64,
    /// Optional log file path; empty string disables file logging (stderr only).
    pub log_file: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            remote_poll_interval_seconds: 30,
            safety_net_interval_seconds: 300,
            log_file: String::new(),
        }
    }
}

/// Explicit `rclone` binary override (cf. `research.md §5`, step 1).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RcloneConfig {
    /// Absolute path to a user-provided `rclone` binary. When set, the daemon uses this
    /// instead of probing `$PATH` / cache / downloading.
    pub path: Option<String>,
    /// Minimum acceptable rclone version (informational; the binary check uses a
    /// compiled-in constant).
    pub min_version: Option<String>,
}

impl Config {
    /// Load `Config` from a TOML file. Returns [`Config::default()`] if the file is
    /// absent. Returns an error on parse failure or unknown keys.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Serialise `Config` to a TOML file with mode `0644`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_default() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load(&tmp.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.daemon.remote_poll_interval_seconds, 30);
        assert_eq!(cfg.daemon.safety_net_interval_seconds, 300);
        assert!(cfg.oauth.client_id.is_none());
    }

    #[test]
    fn round_trip_preserves_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        let mut cfg = Config::default();
        cfg.oauth.client_id = Some("custom.apps.googleusercontent.com".into());
        cfg.daemon.remote_poll_interval_seconds = 45;
        cfg.daemon.log_file = "/tmp/air-drive.log".into();
        cfg.rclone.path = Some("/usr/local/bin/rclone".into());

        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();

        assert_eq!(
            loaded.oauth.client_id.as_deref(),
            Some("custom.apps.googleusercontent.com")
        );
        assert_eq!(loaded.daemon.remote_poll_interval_seconds, 45);
        assert_eq!(loaded.daemon.log_file, "/tmp/air-drive.log");
        assert_eq!(loaded.rclone.path.as_deref(), Some("/usr/local/bin/rclone"));
    }

    #[test]
    fn unknown_keys_rejected() {
        let toml = r#"
            [daemon]
            unknown_field = 42
        "#;
        let res: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(res.is_err());
    }
}
