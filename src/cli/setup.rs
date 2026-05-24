//! `air-drive setup`.
//!
//! Three distinct jobs, all reachable from the same `setup` subcommand:
//!
//! - `--install-service` — copy the bundled
//!   `assets/systemd/air-drive.service` template to
//!   `~/.config/systemd/user/air-drive.service` and run
//!   `systemctl --user enable --now air-drive.service`. Works standalone —
//!   no interactive setup required, the user can already have linked and
//!   mapped manually.
//! - `--uninstall-service` — symmetric reverse: stop and disable the unit,
//!   remove the file, refresh the systemd cache. Idempotent on a clean host;
//!   degrades gracefully when `systemctl` is unavailable.
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
/// install path runs. When `uninstall_service` is set, the symmetric removal
/// path runs. The two flags are mutually exclusive (enforced by clap). When
/// neither is set, the interactive flow is stubbed and returns an error.
pub async fn run(
    config_dir_override: Option<&Path>,
    install_service: bool,
    uninstall_service: bool,
) -> Result<ExitCode> {
    if install_service {
        return install_systemd_unit(config_dir_override);
    }
    if uninstall_service {
        return uninstall_systemd_unit(config_dir_override);
    }
    Err(Error::Config(
        "`air-drive setup` interactive mode is not yet implemented in this MVP. \
         Run `air-drive link`, then `air-drive map <local> <remote>`, then \
         `air-drive start --initial-sync`. Pass `--install-service` to install \
         only the systemd user unit, or `--uninstall-service` to remove it."
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

/// Reverse [`install_systemd_unit`]: stop and disable the unit, remove the
/// file, refresh the systemd user-scope cache. Idempotent — succeeds when the
/// unit is already gone, the file is already absent, or `systemctl` is
/// unavailable on the host.
///
/// Honours the same XDG path resolution as the install path, so the
/// install/uninstall pair always operates on the same file. Never touches the
/// daemon's config, state, tokens, account, mapping, or local watched folder.
fn uninstall_systemd_unit(config_dir_override: Option<&Path>) -> Result<ExitCode> {
    // Resolve paths just to surface any XDG misconfiguration upfront — mirrors
    // the install path. The result isn't consumed.
    let _ = runtime::resolve_paths(config_dir_override)?;

    let Some(unit_path) = installed_unit_path() else {
        return Err(Error::Config(
            "cannot resolve $HOME for systemd uninstall".into(),
        ));
    };

    let mut systemctl_skipped = false;

    // Step 1 — stop + disable the unit. Tolerated outcomes:
    //   - success: unit was loaded; now stopped and disabled.
    //   - `systemctl` missing on PATH (NotFound): flip the skipped flag and
    //     continue to the file removal — we may still have a stray file to
    //     clean up on a non-systemd host.
    //   - non-zero exit reporting "unit not loaded" / "could not be found":
    //     the unit was never enabled; treat as success and continue.
    //   - other non-zero exit: surface as Error::Config — a real systemd error
    //     the user should know about.
    let disable = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "air-drive.service"])
        .output();
    match disable {
        Ok(out) if out.status.success() => {
            tracing::info!("stopped and disabled air-drive.service");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if is_unit_not_loaded(&stderr) {
                tracing::info!("air-drive.service was not loaded; nothing to disable");
            } else {
                return Err(Error::Config(format!(
                    "`systemctl --user disable --now air-drive.service` failed (exit \
                     {:?}): {stderr}",
                    out.status.code()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "`systemctl` not found on PATH; skipping systemd interactions \
                 (the unit file will still be removed if present)"
            );
            systemctl_skipped = true;
        }
        Err(e) => {
            return Err(Error::Config(format!(
                "could not invoke `systemctl` (uninstall, disable step): {e}"
            )));
        }
    }

    // Step 2 — remove the unit file. Tolerated outcomes:
    //   - success: file existed and is now gone.
    //   - NotFound: file already absent — idempotent no-op.
    //   - other io::Error: surface (extremely unlikely under user-owned XDG).
    let file_removed = match std::fs::remove_file(&unit_path) {
        Ok(()) => {
            tracing::info!(unit = %unit_path.display(), "removed unit file");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                unit = %unit_path.display(),
                "no unit file to remove"
            );
            false
        }
        Err(e) => return Err(e.into()),
    };

    // Step 3 — refresh the systemd cache so `list-unit-files` immediately
    // reflects the absence. Skipped if `systemctl` was unavailable in step 1.
    if !systemctl_skipped {
        let reload = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        match reload {
            Ok(out) if out.status.success() => {
                tracing::info!("refreshed systemd user-scope cache");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                tracing::warn!(
                    "`systemctl --user daemon-reload` failed (exit {:?}): {stderr}",
                    out.status.code()
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // systemctl vanished between steps 1 and 3 — extremely unlikely.
                tracing::warn!("`systemctl` not found on PATH; skipping daemon-reload");
            }
            Err(e) => {
                tracing::warn!("could not invoke `systemctl daemon-reload`: {e}");
            }
        }
    }

    // Single-line confirmation summarising what changed.
    let summary = match (file_removed, systemctl_skipped) {
        (true, false) => format!(
            "[setup] removed air-drive.service ({}) and refreshed systemd cache",
            unit_path.display()
        ),
        (true, true) => format!(
            "[setup] removed air-drive.service ({}); systemctl unavailable, \
             cache refresh skipped",
            unit_path.display()
        ),
        (false, false) => "[setup] nothing to remove — air-drive.service was not installed".into(),
        (false, true) => "[setup] nothing to remove and systemctl unavailable — no-op".into(),
    };
    eprintln!("{summary}");
    Ok(ExitCode::Ok)
}

/// Recognise the systemctl stderr that means "this unit isn't loaded on the
/// host". The exact wording varies across systemd versions; the substrings
/// covered here have been stable since systemd 247.
///
/// Kept narrow on purpose: a generic "no such file or directory" is NOT
/// matched, because that phrase also shows up in unrelated failures (e.g. a
/// shebang's interpreter missing on `$PATH`) and would mask real errors.
fn is_unit_not_loaded(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("could not be found") || s.contains("not loaded") || s.contains("does not exist")
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
