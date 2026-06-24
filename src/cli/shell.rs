//! `air-drive shell` — desktop shell integration.
//!
//! Installs (and removes) the file-manager extension that paints a per-file
//! sync-status emblem. Today this targets **GNOME Files (Nautilus)** on Linux,
//! the default file manager on Ubuntu/GNOME; other desktops degrade to a clear
//! "not yet supported" message rather than a half-install.
//!
//! `install`:
//!   1. detects the platform + file manager,
//!   2. ensures the `python3-nautilus` bridge is present (installing it via the
//!      host package manager when possible, or printing the exact command),
//!   3. deploys the bundled extension to
//!      `~/.local/share/nautilus-python/extensions/air-drive-overlay.py`.
//!
//! `uninstall` removes the extension file (idempotent) and leaves the shared
//! system package in place. `status` reports what's detected without changing
//! anything.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Subcommand;

use crate::cli::{ExitCode, runtime};
use crate::error::{Error, Result};

/// The bundled Nautilus extension, shipped with the binary so `shell install`
/// needs no external assets at install time.
const NAUTILUS_EXTENSION: &str = include_str!("../../assets/nautilus/air-drive-overlay.py");

/// File name the extension is deployed under (and removed by `uninstall`).
const EXTENSION_FILENAME: &str = "air-drive-overlay.py";

/// MIME package + desktop handler that make a native-Doc shortcut open in the
/// browser on double-click (instead of a text editor). Bundled with the binary.
const MIME_PACKAGE: &str = include_str!("../../assets/mime/air-drive-shortcuts.xml");
const DESKTOP_ENTRY: &str =
    include_str!("../../assets/applications/air-drive-open-shortcut.desktop");
const MIME_TYPE: &str = "application/vnd.air-drive.shortcut";
const MIME_FILENAME: &str = "air-drive-shortcuts.xml";
const DESKTOP_FILENAME: &str = "air-drive-open-shortcut.desktop";

/// `air-drive shell <action>`.
#[derive(Debug, Subcommand)]
pub enum ShellAction {
    /// Install the file-manager status-emblem extension and its dependency.
    Install {
        /// Deploy the extension only; do not try to install the system
        /// dependency (`python3-nautilus`). Use when you manage packages
        /// yourself.
        #[arg(long)]
        skip_deps: bool,
    },
    /// Remove the file-manager extension. Leaves the system package installed.
    Uninstall,
    /// Report what shell integration detects (platform, file manager,
    /// dependency, extension, daemon socket) without changing anything.
    Status,
}

/// Dispatch a `shell` subcommand.
pub async fn run(config_dir_override: Option<&Path>, action: ShellAction) -> Result<ExitCode> {
    // Surface any XDG misconfiguration upfront, mirroring the other subcommands.
    let _ = runtime::resolve_paths(config_dir_override)?;
    match action {
        ShellAction::Install { skip_deps } => install(skip_deps),
        ShellAction::Uninstall => uninstall(),
        ShellAction::Status => status(),
    }
}

/// Supported (and detected-but-unsupported) host package managers, used to turn
/// "the dependency is missing" into an actionable install command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkgManager {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

impl PkgManager {
    /// The privileged install invocation as `(program, args)`. `sudo` is used so
    /// the command can run from the user's shell and prompt on a TTY.
    fn install_argv(self) -> (&'static str, Vec<&'static str>) {
        match self {
            // Debian/Ubuntu ship the bridge and its GIR typelib separately.
            PkgManager::Apt => (
                "sudo",
                vec![
                    "apt-get",
                    "install",
                    "-y",
                    "python3-nautilus",
                    "gir1.2-nautilus-4.1",
                ],
            ),
            PkgManager::Dnf => ("sudo", vec!["dnf", "install", "-y", "nautilus-python"]),
            PkgManager::Pacman => (
                "sudo",
                vec!["pacman", "-S", "--noconfirm", "python-nautilus"],
            ),
            PkgManager::Zypper => ("sudo", vec!["zypper", "install", "-y", "python3-nautilus"]),
        }
    }

    /// The command as a copy-pasteable string for the user.
    fn install_hint(self) -> String {
        let (prog, args) = self.install_argv();
        format!("{prog} {}", args.join(" "))
    }
}

/// True when `bin` is an executable on `$PATH`.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

/// First package manager found on `$PATH`, if any.
fn detect_pkg_manager() -> Option<PkgManager> {
    if on_path("apt-get") {
        Some(PkgManager::Apt)
    } else if on_path("dnf") {
        Some(PkgManager::Dnf)
    } else if on_path("pacman") {
        Some(PkgManager::Pacman)
    } else if on_path("zypper") {
        Some(PkgManager::Zypper)
    } else {
        None
    }
}

/// Is the `python3-nautilus` bridge importable?
///
/// Probes the **system** interpreter first (`/usr/bin/python3`): Nautilus loads
/// the bridge against the system Python that `libnautilus-python.so` is linked
/// to, not whatever `python3` a user's pyenv / conda / venv happens to put on
/// `$PATH` (which usually lacks `gi`). Falls back to `$PATH` `python3` for
/// non-standard layouts. Tries each Nautilus GIR version we support so the check
/// spans a range of GNOME releases.
fn nautilus_python_present() -> bool {
    for py in ["/usr/bin/python3", "python3"] {
        for ver in ["4.1", "4.0", "3.0"] {
            let probe = format!(
                "import gi; gi.require_version('Nautilus','{ver}'); from gi.repository import Nautilus"
            );
            let ok = Command::new(py)
                .arg("-c")
                .arg(&probe)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return true;
            }
        }
    }
    false
}

/// Absolute path the extension is deployed to.
fn extension_path() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new()
        .ok_or_else(|| Error::Config("cannot resolve $HOME for the extension path".into()))?;
    Ok(dirs
        .data_dir()
        .join("nautilus-python")
        .join("extensions")
        .join(EXTENSION_FILENAME))
}

/// Refuse early on platforms/desktops we don't integrate with yet, with an
/// actionable message instead of a partial install.
fn ensure_supported() -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Config(
            "shell integration currently targets Linux/GNOME (Nautilus). \
             macOS and Windows support is planned."
                .into(),
        ));
    }
    if !on_path("nautilus") {
        return Err(Error::Config(
            "GNOME Files (Nautilus) was not found on PATH. Shell integration \
             currently targets Nautilus; support for other file managers \
             (Dolphin, Nemo) is planned."
                .into(),
        ));
    }
    Ok(())
}

/// Ensure `python3-nautilus` is present, installing it via the host package
/// manager when possible. Returns `Ok(())` whether the dependency ends up
/// present or not — a missing dependency only means the deployed extension stays
/// dormant until the user installs it, which we make explicit. `skip_deps`
/// bypasses installation entirely.
fn ensure_dependency(skip_deps: bool) {
    if nautilus_python_present() {
        eprintln!("[shell] dependency python3-nautilus: present");
        return;
    }
    let hint = detect_pkg_manager().map(PkgManager::install_hint);
    if skip_deps {
        match hint {
            Some(cmd) => eprintln!(
                "[shell] python3-nautilus is missing (--skip-deps set); install it with: {cmd}"
            ),
            None => eprintln!(
                "[shell] python3-nautilus is missing (--skip-deps set); install your distro's \
                 nautilus-python package"
            ),
        }
        return;
    }
    let Some(pm) = detect_pkg_manager() else {
        eprintln!(
            "[shell] python3-nautilus is missing and no known package manager was found. \
             Install your distro's nautilus-python package, then re-run `air-drive shell install`."
        );
        return;
    };
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "[shell] python3-nautilus is missing. Not running on a terminal, so I won't invoke \
             sudo. Install it with: {}",
            pm.install_hint()
        );
        return;
    }
    let (prog, args) = pm.install_argv();
    eprintln!("[shell] installing python3-nautilus: {}", pm.install_hint());
    match Command::new(prog).args(&args).status() {
        Ok(s) if s.success() => eprintln!("[shell] dependency installed"),
        Ok(s) => eprintln!(
            "[shell] dependency install exited {:?}. Install it manually with: {} — the extension \
             was still deployed and will activate once the dependency is present.",
            s.code(),
            pm.install_hint()
        ),
        Err(e) => eprintln!(
            "[shell] could not run `{prog}` ({e}). Install python3-nautilus manually with: {} — \
             the extension was still deployed.",
            pm.install_hint()
        ),
    }
}

/// Write the bundled extension to the nautilus-python extensions directory.
fn install(skip_deps: bool) -> Result<ExitCode> {
    ensure_supported()?;
    ensure_dependency(skip_deps);

    let dest = extension_path()?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, NAUTILUS_EXTENSION)?;
    tracing::info!(extension = %dest.display(), "deployed Nautilus overlay extension");
    eprintln!("[shell] installed extension: {}", dest.display());

    register_file_association();

    eprintln!(
        "[shell] fully restart the file manager to load it: `killall nautilus` (a plain \
         `nautilus -q` can leave a cached background instance that keeps the old emblems), or \
         log out and back in. Emblems show on synced files, and on the sync folder itself when \
         viewed from its parent."
    );
    Ok(ExitCode::Ok)
}

/// Remove the deployed extension. Idempotent; leaves the system package in place
/// (it is shared and may be used by other extensions).
fn uninstall() -> Result<ExitCode> {
    let dest = extension_path()?;
    match std::fs::remove_file(&dest) {
        Ok(()) => {
            tracing::info!(extension = %dest.display(), "removed Nautilus overlay extension");
            eprintln!("[shell] removed extension: {}", dest.display());
            eprintln!("[shell] fully restart the file manager to drop it: `killall nautilus`.");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("[shell] nothing to remove — extension was not installed");
        }
        Err(e) => return Err(e.into()),
    }
    unregister_file_association();
    eprintln!(
        "[shell] the python3-nautilus system package was left installed (it is shared). \
         Remove it via your package manager if you no longer want it."
    );
    Ok(ExitCode::Ok)
}

/// Where the MIME package and the desktop handler are deployed under
/// `~/.local/share`. Returns `(data_dir, mime_packages_dir, applications_dir)`.
fn association_dirs() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let dirs = directories::BaseDirs::new()
        .ok_or_else(|| Error::Config("cannot resolve $HOME for the file association".into()))?;
    let data = dirs.data_dir().to_path_buf();
    let mime = data.join("mime").join("packages");
    let apps = data.join("applications");
    Ok((data, mime, apps))
}

/// Register the MIME type + desktop handler so a native-Doc shortcut opens in
/// the browser on double-click. Best-effort: warns rather than failing the whole
/// install, since the status emblems work regardless.
fn register_file_association() {
    let (data, mime_dir, apps_dir) = match association_dirs() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[shell] skipping file association: {e}");
            return;
        }
    };
    // Resolve the absolute binary path for the desktop entry's Exec — the
    // session launcher's PATH may not include where `air-drive` lives.
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "air-drive".to_owned());
    let desktop = DESKTOP_ENTRY.replace("__AIRDRIVE_BIN__", &bin);

    if let Err(e) = std::fs::create_dir_all(&mime_dir)
        .and_then(|()| std::fs::write(mime_dir.join(MIME_FILENAME), MIME_PACKAGE))
    {
        eprintln!("[shell] could not install the MIME package: {e}");
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&apps_dir)
        .and_then(|()| std::fs::write(apps_dir.join(DESKTOP_FILENAME), desktop))
    {
        eprintln!("[shell] could not install the desktop handler: {e}");
        return;
    }

    // Refresh the freedesktop caches and make our handler the default. All
    // best-effort — a missing tool just means the user re-runs after installing
    // it, or sets "Open with" once by hand.
    run_quiet(Command::new("update-mime-database").arg(data.join("mime")));
    run_quiet(Command::new("update-desktop-database").arg(&apps_dir));
    run_quiet(
        Command::new("xdg-mime")
            .args(["default", DESKTOP_FILENAME])
            .arg(MIME_TYPE),
    );
    eprintln!("[shell] registered Google Doc shortcuts to open in the browser on double-click");
}

/// Reverse [`register_file_association`]: remove the MIME package + handler and
/// refresh the caches. Idempotent.
fn unregister_file_association() {
    let Ok((data, mime_dir, apps_dir)) = association_dirs() else {
        return;
    };
    let _ = std::fs::remove_file(mime_dir.join(MIME_FILENAME));
    let _ = std::fs::remove_file(apps_dir.join(DESKTOP_FILENAME));
    run_quiet(Command::new("update-mime-database").arg(data.join("mime")));
    run_quiet(Command::new("update-desktop-database").arg(&apps_dir));
}

/// Run a command for its side effect, ignoring success and warning on failure to
/// spawn. Used for the freedesktop cache-refresh tools.
fn run_quiet(cmd: &mut Command) {
    if let Err(e) = cmd.output() {
        tracing::debug!(error = %e, "shell integration helper not available");
    }
}

/// `air-drive open-shortcut <file>` — open the Google link a native-Doc shortcut
/// points to. Invoked by the desktop file association on double-click.
pub fn open_shortcut(path: &Path) -> Result<ExitCode> {
    let content = std::fs::read_to_string(path).map_err(Error::Io)?;
    let url = shortcut_target_url(&content)?;
    Command::new("xdg-open")
        .arg(&url)
        .spawn()
        .map_err(Error::Io)?;
    Ok(ExitCode::Ok)
}

/// Extract and validate the web URL from a shortcut file's JSON body. Only
/// http(s) URLs are accepted — a shortcut may come from a shared Drive folder,
/// so we never hand an arbitrary scheme (file://, etc.) to `xdg-open`.
fn shortcut_target_url(content: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| Error::Config(format!("shortcut is not valid JSON: {e}")))?;
    let url = value
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| Error::Config("shortcut file has no `url` field".into()))?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Error::Config(format!(
            "refusing to open non-http(s) shortcut URL: {url}"
        )));
    }
    Ok(url.to_owned())
}

/// Report detection without changing anything.
fn status() -> Result<ExitCode> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "(unset)".into());
    let nautilus = if on_path("nautilus") {
        "found"
    } else {
        "not found"
    };
    let dep = if nautilus_python_present() {
        "present".to_string()
    } else {
        match detect_pkg_manager() {
            Some(pm) => format!("MISSING (install: {})", pm.install_hint()),
            None => "MISSING (install your distro's nautilus-python package)".to_string(),
        }
    };
    let ext = extension_path()?;
    let ext_state = if ext.exists() {
        format!("installed ({})", ext.display())
    } else {
        "not installed".to_string()
    };

    println!("platform:        {}", std::env::consts::OS);
    println!("desktop:         {desktop}");
    println!("file manager:    nautilus {nautilus}");
    println!("python3-nautilus: {dep}");
    println!("extension:       {ext_state}");
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apt_install_hint_lists_both_packages() {
        let hint = PkgManager::Apt.install_hint();
        assert!(hint.contains("apt-get install"));
        assert!(hint.contains("python3-nautilus"));
        assert!(hint.contains("gir1.2-nautilus-4.1"));
    }

    #[test]
    fn install_argv_uses_sudo_for_every_pkg_manager() {
        for pm in [
            PkgManager::Apt,
            PkgManager::Dnf,
            PkgManager::Pacman,
            PkgManager::Zypper,
        ] {
            let (prog, args) = pm.install_argv();
            assert_eq!(prog, "sudo");
            assert!(!args.is_empty());
        }
    }

    #[test]
    fn extension_path_ends_with_nautilus_python_extension() {
        let p = extension_path().expect("path resolves");
        assert!(p.ends_with("nautilus-python/extensions/air-drive-overlay.py"));
    }

    #[test]
    fn shortcut_target_url_accepts_https_and_rejects_others() {
        let ok = r#"{"url":"https://docs.google.com/spreadsheets/d/ABC/edit","doc_id":"ABC"}"#;
        assert_eq!(
            shortcut_target_url(ok).unwrap(),
            "https://docs.google.com/spreadsheets/d/ABC/edit"
        );

        // Non-http(s) scheme is refused (a shared shortcut is untrusted input).
        let evil = r#"{"url":"file:///etc/passwd"}"#;
        assert!(shortcut_target_url(evil).is_err());

        // Missing url / not JSON.
        assert!(shortcut_target_url(r#"{"doc_id":"x"}"#).is_err());
        assert!(shortcut_target_url("not json").is_err());
    }

    #[test]
    fn bundled_desktop_entry_has_exec_placeholder() {
        // The Exec line is templated with the real binary path at install time.
        assert!(DESKTOP_ENTRY.contains("__AIRDRIVE_BIN__"));
        assert!(DESKTOP_ENTRY.contains(MIME_TYPE));
    }
}
