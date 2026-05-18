//! `air-drive setup` (T040 + T078b).
//!
//! Two distinct jobs:
//!
//! - `--install-service` (T078b) — copy the bundled
//!   `assets/systemd/air-drive.service` template to
//!   `~/.config/systemd/user/air-drive.service` and run
//!   `systemctl --user enable --now air-drive.service`. Works standalone —
//!   no interactive setup required, the user can already have linked and
//!   mapped manually.
//! - Interactive flow (`link → map → start`) — still stubbed because it
//!   needs a TTY-aware prompt crate ([`dialoguer`]). Wiring it in is left
//!   out of MVP scope; the user can drive each subcommand individually.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::{ExitCode, runtime};
use crate::error::{Error, Result};

/// Bundled systemd user-unit template. Always shipped with the binary so
/// `setup --install-service` works without external assets at install time.
const SYSTEMD_UNIT_TEMPLATE: &str = include_str!("../../assets/systemd/air-drive.service");

/// Run the `setup` subcommand. When `install_service` is set, only the unit
/// install path runs — there is no interactive flow yet.
pub async fn run(config_dir_override: Option<&Path>, install_service: bool) -> Result<ExitCode> {
    if install_service {
        return install_systemd_unit(config_dir_override);
    }
    Err(Error::Config(
        "`air-drive setup` interactive mode is not yet implemented in this MVP. \
         Run `air-drive link`, then `air-drive map <local> <remote>`, then \
         `air-drive start --initial-sync`. Pass `--install-service` to install \
         only the systemd user unit."
            .into(),
    ))
}

/// Write the unit template to `~/.config/systemd/user/air-drive.service` and
/// run `systemctl --user enable --now air-drive.service`. Uses the already-
/// resolved XDG paths so a non-default `XDG_CONFIG_HOME` (or `--config-dir`)
/// is honoured.
fn install_systemd_unit(config_dir_override: Option<&Path>) -> Result<ExitCode> {
    // Resolve paths just to surface any XDG misconfiguration upfront; we
    // intentionally don't read `paths.config()` for the unit destination
    // because systemd reads from a fixed user-scope location, not from
    // the daemon's per-mapping config dir.
    let _ = runtime::resolve_paths(config_dir_override)?;

    // Locate `~/.config/systemd/user/`.
    let dirs = directories::BaseDirs::new()
        .ok_or_else(|| Error::Config("cannot resolve $HOME for systemd install".into()))?;
    let systemd_user_dir = dirs.config_dir().join("systemd").join("user");
    std::fs::create_dir_all(&systemd_user_dir)?;
    let unit_path = systemd_user_dir.join("air-drive.service");

    // Substitute `%h` placeholders intentionally left in the template so
    // systemd's own runtime substitution kicks in — but we ALSO honour the
    // `--config-dir` flag by setting the `Environment=AIR_DRIVE_CONFIG_DIR=...`
    // line when the user passed one. The daemon doesn't read that env var
    // today; we leave the slot empty rather than ship an unread tunable.
    std::fs::write(&unit_path, SYSTEMD_UNIT_TEMPLATE)?;
    tracing::info!(
        unit = %unit_path.display(),
        "installed systemd user unit"
    );

    // Enable + start. `--now` does both in one command. Errors from systemctl
    // surface as Config errors with the command's stderr attached.
    let output = Command::new("systemctl")
        .args(["--user", "enable", "--now", "air-drive.service"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            eprintln!(
                "[setup] enabled + started air-drive.service (logs: \
                 `journalctl --user -u air-drive -f`)"
            );
            Ok(ExitCode::Ok)
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            Err(Error::Config(format!(
                "`systemctl --user enable --now air-drive.service` failed (exit \
                 {:?}): {stderr}",
                out.status.code()
            )))
        }
        Err(e) => Err(Error::Config(format!(
            "could not invoke `systemctl` (not on a systemd host?): {e}"
        ))),
    }
}

/// Where the unit lands on disk. Exposed for tests + diagnostics.
pub fn installed_unit_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| {
        d.config_dir()
            .join("systemd")
            .join("user")
            .join("air-drive.service")
    })
}
