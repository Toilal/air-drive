//! `air-drive init` — interactive Google Cloud OAuth bootstrap.
//!
//! Walks the user through the four GCP Console steps that **must** be done by
//! hand (Google does not expose a public API for creating Desktop OAuth
//! clients — see `examples/setup_e2e.rs` for the rationale). Each step opens
//! the right pre-filled Console URL in the browser, prompts for the value the
//! user needs to copy back, and writes the result to `~/.config/air-drive/config.toml`.
//!
//! The command is opt-in idempotent: it refuses to overwrite an existing
//! `[oauth].client_id` unless `--force` is set.

use std::io::{BufRead, Write};
use std::path::Path;

use crate::cli::{ExitCode, runtime};
use crate::config::Config;
use crate::error::{Error, Result};

/// `air-drive init` entry point.
pub async fn run(
    config_dir_override: Option<&Path>,
    force: bool,
    link_after: bool,
) -> Result<ExitCode> {
    let paths = runtime::resolve_paths(config_dir_override)?;
    let config_path = paths.config().join("config.toml");

    let existing = Config::load(&config_path).ok();
    if let Some(cfg) = &existing
        && cfg.oauth.client_id.is_some()
        && !force
    {
        eprintln!(
            "config.toml already has [oauth].client_id set ({}).\n\
             Re-run with --force to overwrite.",
            cfg.oauth.client_id.as_deref().unwrap_or("?"),
        );
        return Ok(ExitCode::GenericError);
    }

    std::fs::create_dir_all(paths.config())?;

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    writeln!(stdout, "air-drive init — Google Cloud OAuth bootstrap")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "This walks through 4 manual steps in the GCP Console. Google does not"
    )?;
    writeln!(
        stdout,
        "expose an API for creating Desktop OAuth clients, so a few clicks are"
    )?;
    writeln!(stdout, "unavoidable. Each step opens a pre-filled URL.")?;
    writeln!(stdout)?;

    // Step 1 — create the project.
    writeln!(stdout, "[1/5] Create a Google Cloud project")?;
    open_or_print(&mut stdout, "https://console.cloud.google.com/projectcreate")?;
    let project_id = prompt(&mut stdin, &mut stdout, "      Project ID once created: ")?;
    validate_project_id(&project_id)?;

    // Step 2 — enable Drive API.
    writeln!(stdout)?;
    writeln!(stdout, "[2/5] Enable the Google Drive API")?;
    let url = format!(
        "https://console.cloud.google.com/apis/library/drive.googleapis.com?project={project_id}"
    );
    open_or_print(&mut stdout, &url)?;
    let _ = prompt(
        &mut stdin,
        &mut stdout,
        "      Click 'Enable', then press Enter: ",
    )?;

    // Step 3a — consent screen "Branding" (Auth Platform setup wizard).
    writeln!(stdout)?;
    writeln!(stdout, "[3/5] Configure the Auth Platform (consent + audience)")?;
    let url =
        format!("https://console.cloud.google.com/auth/overview/create?project={project_id}");
    open_or_print(&mut stdout, &url)?;
    writeln!(stdout, "      a. Branding (the form on this page):")?;
    writeln!(stdout, "         - App name: air-drive (or anything)")?;
    writeln!(stdout, "         - User support email: your Google email")?;
    writeln!(stdout, "         - Audience: External")?;
    writeln!(stdout, "         - Contact info: your Google email")?;
    writeln!(stdout, "         - Click CREATE.")?;
    writeln!(stdout)?;
    writeln!(stdout, "      b. Then open the Audience tab (left menu) and")?;
    writeln!(
        stdout,
        "         add your Google account email under 'Test users'."
    )?;
    let url =
        format!("https://console.cloud.google.com/auth/audience?project={project_id}");
    open_or_print(&mut stdout, &url)?;
    let _ = prompt(&mut stdin, &mut stdout, "      Press Enter when done: ")?;

    // Step 4 — OAuth Desktop client (new Auth Platform location).
    writeln!(stdout)?;
    writeln!(stdout, "[4/5] Create the OAuth client credentials")?;
    let url = format!(
        "https://console.cloud.google.com/auth/clients/create?project={project_id}"
    );
    open_or_print(&mut stdout, &url)?;
    writeln!(stdout, "      - Application type: Desktop app")?;
    writeln!(stdout, "      - Name: air-drive (or anything)")?;
    writeln!(stdout, "      - Click CREATE.")?;
    writeln!(stdout)?;
    let client_id = prompt(&mut stdin, &mut stdout, "      Paste the Client ID: ")?;
    validate_client_id(&client_id)?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "      The new console no longer shows the secret in the modal —"
    )?;
    writeln!(
        stdout,
        "      click the client name (or open the URL below) to reveal it."
    )?;
    let url = format!(
        "https://console.cloud.google.com/auth/clients/{client_id}?project={project_id}"
    );
    open_or_print(&mut stdout, &url)?;
    let client_secret = prompt(&mut stdin, &mut stdout, "      Paste the Client secret: ")?;
    validate_client_secret(&client_secret)?;

    // Step 5 — write config.toml. We deliberately persist the defaulted
    // `[watch].ignore_patterns` list too: TOML round-trip with `Default`
    // would otherwise drop it, leaving the user unaware of what's filtered.
    // Once on disk the user can edit / extend the patterns directly.
    writeln!(stdout)?;
    writeln!(stdout, "[5/5] Writing {}", config_path.display())?;
    let mut cfg = existing.unwrap_or_default();
    cfg.oauth.client_id = Some(client_id);
    cfg.oauth.client_secret = Some(client_secret);
    // Ensure the watch section is materialised (Default already populates it,
    // but a previously-loaded config may have had it stripped).
    if cfg.watch.ignore_patterns.is_empty() {
        cfg.watch = Default::default();
    }
    cfg.save(&config_path)?;
    writeln!(stdout, "      ✓ done")?;
    writeln!(stdout)?;

    if link_after {
        writeln!(stdout, "Running `air-drive link`...")?;
        drop(stdout);
        drop(stdin);
        return crate::cli::link::run(config_dir_override, &cfg, None).await;
    }

    writeln!(stdout, "Next: run `air-drive link` to authorize the account.")?;
    Ok(ExitCode::Ok)
}

/// Open a URL in the default browser; fall back to just printing it if the
/// system has no display or the platform refuses (headless CI, ssh session).
fn open_or_print<W: Write>(w: &mut W, url: &str) -> std::io::Result<()> {
    writeln!(w, "      → {url}")?;
    // Best effort — never fail the flow on a browser open error.
    let _ = webbrowser::open(url);
    Ok(())
}

fn prompt<R: BufRead, W: Write>(r: &mut R, w: &mut W, label: &str) -> Result<String> {
    write!(w, "{label}")?;
    w.flush()?;
    let mut buf = String::new();
    r.read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// GCP rule: 6-30 chars, lowercase letters/digits/hyphens, starts with a
/// letter, doesn't end with a hyphen. Source: Google Cloud documentation
/// "Creating and managing projects".
fn validate_project_id(id: &str) -> Result<()> {
    let ok = (6..=30).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.ends_with('-');
    if !ok {
        return Err(Error::Config(format!(
            "invalid GCP project ID '{id}' — must be 6-30 chars, lowercase \
             letters/digits/hyphens, start with a letter, not end with a hyphen"
        )));
    }
    Ok(())
}

fn validate_client_id(id: &str) -> Result<()> {
    if !id.ends_with(".apps.googleusercontent.com") {
        return Err(Error::Config(format!(
            "invalid OAuth client_id '{id}' — must end with \
             '.apps.googleusercontent.com'"
        )));
    }
    Ok(())
}

fn validate_client_secret(secret: &str) -> Result<()> {
    if !secret.starts_with("GOCSPX-") {
        return Err(Error::Config(format!(
            "invalid OAuth client_secret — Desktop client secrets start with \
             'GOCSPX-' (you pasted {} chars)",
            secret.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_id_validator_accepts_typical_id() {
        assert!(validate_project_id("air-drive-12345").is_ok());
    }

    #[test]
    fn project_id_validator_rejects_uppercase() {
        assert!(validate_project_id("Air-drive").is_err());
    }

    #[test]
    fn project_id_validator_rejects_trailing_hyphen() {
        assert!(validate_project_id("air-drive-").is_err());
    }

    #[test]
    fn project_id_validator_rejects_short() {
        assert!(validate_project_id("ad-12").is_err());
    }

    #[test]
    fn client_id_validator_accepts_typical() {
        assert!(
            validate_client_id("123-abc.apps.googleusercontent.com").is_ok()
        );
    }

    #[test]
    fn client_id_validator_rejects_wrong_suffix() {
        assert!(validate_client_id("123-abc.example.com").is_err());
    }

    #[test]
    fn client_secret_validator_accepts_typical() {
        assert!(validate_client_secret("GOCSPX-abcdef123").is_ok());
    }

    #[test]
    fn client_secret_validator_rejects_wrong_prefix() {
        assert!(validate_client_secret("abc-secret").is_err());
    }
}
