//! `air-drive` binary entry point.
//!
//! All real logic lives in the library crate (`air_drive`). This file parses the CLI,
//! initialises tracing, dispatches to the subcommand handler, and translates the
//! returned [`ExitCode`] into the process exit status the user observes.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode as StdExitCode;

use clap::Parser;

use air_drive::cli::{Cli, ExitCode, dispatch, fallback_exit_code};
use air_drive::config::paths::Paths;
use air_drive::config::{Config, DaemonConfig, RcloneConfig};
use air_drive::engine::rclone_path;
use air_drive::observability::{LogOptions, init_tracing};

fn main() -> StdExitCode {
    // Intercept `--version` / `-V` before clap so we can append the resolved
    // rclone version. clap's default `version` would just print the crate
    // version on its own.
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Ok(rt) = rt {
            rt.block_on(print_version());
        } else {
            println!("air-drive {}", air_drive::VERSION);
        }
        return StdExitCode::SUCCESS;
    }

    let cli = Cli::parse();

    // Resolve the logger from config.toml (best-effort — a broken config is
    // surfaced with a proper error later by `dispatch`, where it is loaded
    // strictly). The CLI `--log-file` flag overrides `[daemon].log_file`.
    let daemon_cfg = resolve_daemon_config(cli.config_dir.as_deref());
    let config_log_file =
        (!daemon_cfg.log_file.is_empty()).then(|| PathBuf::from(&daemon_cfg.log_file));
    let log_file = cli.log_file.clone().or(config_log_file);
    let log_opts = LogOptions {
        verbose: cli.verbose,
        log_file: log_file.as_deref(),
        level: &daemon_cfg.log_level,
        format: daemon_cfg.log_format,
        color: daemon_cfg.log_color,
    };
    if let Err(e) = init_tracing(&log_opts) {
        eprintln!("failed to initialise tracing: {e}");
        return StdExitCode::from(ExitCode::GenericError as u8);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build tokio runtime");
            return StdExitCode::from(ExitCode::GenericError as u8);
        }
    };
    let code = rt.block_on(async {
        match dispatch(cli).await {
            Ok(code) => code,
            Err(e) => {
                tracing::error!(error = %e, "command failed");
                eprintln!("Error: {e}");
                fallback_exit_code(&e)
            }
        }
    });
    StdExitCode::from(code as u8)
}

/// Load `[daemon]` from `config.toml` to drive logger setup before the runtime
/// starts. Best-effort: any failure (missing/broken config, undiscoverable
/// paths) falls back to defaults so logging still comes up; `dispatch` loads
/// the config strictly afterwards and reports real errors.
fn resolve_daemon_config(config_dir: Option<&Path>) -> DaemonConfig {
    Paths::discover(config_dir)
        .ok()
        .and_then(|paths| Config::load(&paths.config().join("config.toml")).ok())
        .map(|cfg| cfg.daemon)
        .unwrap_or_default()
}

/// Print the crate version + the resolved rclone binary's version when one
/// can be discovered. Uses the user's existing `Config.rclone` overrides if a
/// config file is on disk; otherwise falls back to the same `$PATH` /
/// `$XDG_CACHE_HOME/air-drive/bin/rclone` search the daemon does at startup
/// (with auto-download disabled — `--version` mustn't trigger a multi-MB
/// download as a side effect).
async fn print_version() {
    println!("air-drive {}", air_drive::VERSION);
    // Best-effort: read config if available, otherwise probe defaults.
    let (rclone_cfg, cache_dir) = match air_drive::config::paths::Paths::discover(None) {
        Ok(paths) => {
            let cfg = Config::load(&paths.config().join("config.toml")).ok();
            let rclone = cfg.map(|c| c.rclone).unwrap_or_default();
            (rclone, paths.cache().to_path_buf())
        }
        Err(_) => (
            RcloneConfig::default(),
            std::env::temp_dir().join("air-drive-cache"),
        ),
    };
    match rclone_path::resolve(&rclone_cfg, &cache_dir, false).await {
        Ok(bin) => {
            println!(
                "rclone {} ({} — {})",
                bin.version,
                source_label(bin.source),
                bin.path.display()
            );
        }
        Err(_) => {
            println!("rclone: not found (run `air-drive start` to auto-resolve)");
        }
    }
}

fn source_label(s: air_drive::engine::rclone::RcloneSource) -> &'static str {
    use air_drive::engine::rclone::RcloneSource;
    match s {
        RcloneSource::Config => "config",
        RcloneSource::Path => "PATH",
        RcloneSource::Cache => "cache",
        RcloneSource::Downloaded => "downloaded",
    }
}
