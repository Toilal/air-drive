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

pub mod migrate;
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
    /// `[watch]` — local filesystem watcher tuning.
    pub watch: WatchConfig,
}

/// Optional OAuth client override (Q1 clarification — hybrid model).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OauthConfig {
    /// Override the project-owned `client_id` with the user's own Google Cloud client.
    /// `None` means "use the embedded default".
    pub client_id: Option<String>,
    /// Companion `client_secret` for the Desktop OAuth client. Google's token
    /// endpoint requires it even though the Desktop flow is otherwise PKCE-only —
    /// the value is distributed with the app and not actually secret. Leave
    /// `None` when `client_id` is also `None`; both come together.
    pub client_secret: Option<String>,
}

/// Folder mapping display info. The authoritative `remote_folder_id` lives in `state.db`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MappingConfig {
    /// Absolute path of the watched local folder, for display.
    pub local_path: Option<String>,
    /// Human-readable remote folder path, for display.
    pub remote_folder_name: Option<String>,
    /// When `true`, `air-drive map` creates any missing folder under a `path:`
    /// notation target on Drive (segment-by-segment from the deepest existing
    /// parent) without prompting. When `false` (default), the command prompts
    /// the user interactively before creating; on a non-interactive stdin or
    /// when the user declines, it errors with `MapRemoteUnresolvable`. Only
    /// applies to `path:` notation — bare IDs and URLs reference a specific
    /// resource that cannot be synthesised.
    pub auto_create_remote_root: bool,
}

/// Log record format for the tracing subscriber.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable single-line text (the default).
    #[default]
    Text,
    /// Structured JSON, one object per record — for log aggregators.
    Json,
}

/// ANSI colour policy for the stderr log layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogColor {
    /// Colour only when stderr is a terminal (the default).
    #[default]
    Auto,
    /// Always emit ANSI colour codes.
    Always,
    /// Never emit ANSI colour codes.
    Never,
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
    /// Persistent log level. Empty string means "unset" (fall back to the
    /// verbosity flag / `RUST_LOG` / the built-in `warn` default). A bare level
    /// (`"info"`, `"debug"`, …) applies to the `air_drive` target; a value
    /// containing `=` is treated as a full `RUST_LOG`-style filter directive
    /// (e.g. `"air_drive=debug,rclone=warn"`). `RUST_LOG` and the `-v` flags
    /// take precedence over this key.
    pub log_level: String,
    /// Log record format: `text` (default) or `json`.
    pub log_format: LogFormat,
    /// ANSI colour policy for the stderr layer: `auto` (default), `always`, or
    /// `never`. The file layer is always colour-free.
    pub log_color: LogColor,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            remote_poll_interval_seconds: 30,
            safety_net_interval_seconds: 300,
            log_file: String::new(),
            log_level: String::new(),
            log_format: LogFormat::default(),
            log_color: LogColor::default(),
        }
    }
}

/// Local filesystem watcher tuning. Currently only carries the
/// `ignore_patterns` list — glob patterns matched against the **file name**
/// (not the full path). Files whose name matches any pattern are never synced
/// (no upload, no rename propagation, no delete propagation).
///
/// Defaults cover the well-known editor/OS scratch files: vim swap, emacs
/// auto-save + backup + lock, gedit, LibreOffice locks, MS Office owner
/// files, JetBrains atomic-rename temps, and macOS/Windows OS metadata. Users
/// can override the whole list in `config.toml` to add their own patterns.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// Glob patterns matched against the file name. Default list seeded by
    /// [`WatchConfig::default`].
    pub ignore_patterns: Vec<String>,
    /// When `true`, `air-drive map` and `air-drive start` create
    /// `mapping.local_path` without prompting if it does not exist (including
    /// any intermediate parents). When `false` (default), the CLI prompts the
    /// user interactively before creating; on a non-interactive stdin (systemd
    /// unit, piped script) or when the user declines, it refuses to start with
    /// an actionable error.
    pub auto_create_root: bool,
    /// How symlinks under the watched root are handled. Defaults to
    /// [`SymlinkPolicy::Skip`] (the historical behaviour). See [`SymlinkPolicy`].
    pub symlinks: SymlinkPolicy,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            ignore_patterns: default_ignore_patterns()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            auto_create_root: false,
            symlinks: SymlinkPolicy::default(),
        }
    }
}

/// Policy for symlinks discovered under the watched root.
///
/// - [`Skip`](SymlinkPolicy::Skip) (default) — symlinks are ignored entirely:
///   no upload, no rename/delete propagation. The pre-existing behaviour.
/// - [`Follow`](SymlinkPolicy::Follow) — a symlink is resolved to its target
///   and synced as a regular file or directory. Targets that resolve **outside**
///   the watched root are skipped (so a stray link can't exfiltrate unrelated
///   files), and directory-symlink cycles are detected and broken.
///
/// `preserve` (recording the link itself as a sidecar) is intentionally not
/// offered yet — Drive has no native symlink type, so the encoding is deferred
/// (issue #2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymlinkPolicy {
    /// Ignore symlinks entirely (default).
    #[default]
    Skip,
    /// Resolve symlinks to their target and sync the target's content.
    Follow,
}

/// Source-of-truth list of file-name globs the watcher ignores by default.
/// Kept as a `&[&str]` so it can be referenced from docs, init, and tests.
pub fn default_ignore_patterns() -> &'static [&'static str] {
    &[
        // vim: swap files + atomic-save sentinel.
        ".*.swp",
        ".*.swo",
        ".*.swx",
        ".*.swn",
        "4913",
        // emacs: auto-save, backup, lockfile.
        "#*#",
        "*~",
        ".#*",
        // gedit / nautilus.
        ".goutputstream-*",
        // LibreOffice owner-lock.
        ".~lock.*#",
        // MS Office owner-file.
        "~$*",
        // JetBrains atomic-rename temps.
        "*.___jb_tmp___",
        "*.___jb_old___",
        // OS metadata.
        ".DS_Store",
        "._*",
        "Thumbs.db",
        "desktop.ini",
    ]
}

/// Explicit `rclone` binary override.
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
    fn log_defaults_are_unset_text_auto() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.log_level, "");
        assert_eq!(cfg.log_format, LogFormat::Text);
        assert_eq!(cfg.log_color, LogColor::Auto);
    }

    #[test]
    fn log_keys_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        let mut cfg = Config::default();
        cfg.daemon.log_level = "air_drive=debug,rclone=warn".into();
        cfg.daemon.log_format = LogFormat::Json;
        cfg.daemon.log_color = LogColor::Never;

        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();

        assert_eq!(loaded.daemon.log_level, "air_drive=debug,rclone=warn");
        assert_eq!(loaded.daemon.log_format, LogFormat::Json);
        assert_eq!(loaded.daemon.log_color, LogColor::Never);
    }

    #[test]
    fn unknown_log_format_variant_rejected() {
        let toml = r#"
            [daemon]
            log_format = "yaml"
        "#;
        let res: std::result::Result<Config, _> = toml::from_str(toml);
        assert!(res.is_err());
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
    fn default_watch_block_has_seeded_patterns() {
        let cfg = Config::default();
        // Both the in-memory default list and the canonical defaults helper
        // must agree — the test catches accidental drift if one is edited.
        let expected: Vec<&str> = default_ignore_patterns().to_vec();
        let got: Vec<&str> = cfg
            .watch
            .ignore_patterns
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn save_default_writes_watch_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("[watch]"),
            "config.toml lacks [watch]: {body}"
        );
        assert!(
            body.contains("ignore_patterns"),
            "config.toml lacks ignore_patterns: {body}"
        );
        // Spot-check that a representative default made it in.
        assert!(
            body.contains(".DS_Store"),
            "config.toml missing .DS_Store default: {body}"
        );
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
