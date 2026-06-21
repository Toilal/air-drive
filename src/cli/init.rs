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
    open_or_print(
        &mut stdout,
        "https://console.cloud.google.com/projectcreate",
    )?;
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

    // Step 3 — consent screen "Branding" + audience. The audience choice is
    // what decides whether the client hits the 7-day refresh-token cap:
    //   - Internal (Workspace orgs only): no cap, no verification, no
    //     test-user list — the cleanest path when the account allows it.
    //   - External (personal @gmail.com): sensitive-scope clients left in
    //     'Testing' cap refresh tokens at 7 days; publishing to 'Production'
    //     lifts the cap (an 'unverified app' warning remains, harmless for a
    //     personal client). See docs/user/oauth-setup.md.
    writeln!(stdout)?;
    writeln!(
        stdout,
        "[3/5] Configure the Auth Platform (consent + audience)"
    )?;
    writeln!(stdout)?;
    writeln!(stdout, "      Audience type:")?;
    writeln!(
        stdout,
        "      - Internal: Google Workspace orgs only (e.g. you@company.com)."
    )?;
    writeln!(
        stdout,
        "        No 7-day token cap, no verification, no test-user list."
    )?;
    writeln!(
        stdout,
        "      - External: personal @gmail.com accounts. Sensitive-scope"
    )?;
    writeln!(
        stdout,
        "        clients in 'Testing' re-prompt every 7 days; this wizard then"
    )?;
    writeln!(
        stdout,
        "        walks you through publishing to Production to remove the cap."
    )?;
    writeln!(stdout)?;
    let workspace = parse_yes(&prompt(
        &mut stdin,
        &mut stdout,
        "      Is this Drive account part of a Google Workspace org? [y/N]: ",
    )?);
    writeln!(stdout)?;
    let url = format!("https://console.cloud.google.com/auth/overview/create?project={project_id}");
    open_or_print(&mut stdout, &url)?;
    writeln!(stdout, "      a. Branding (the form on this page):")?;
    writeln!(stdout, "         - App name: air-drive (or anything)")?;
    writeln!(stdout, "         - User support email: your Google email")?;
    if workspace {
        writeln!(stdout, "         - Audience: Internal")?;
    } else {
        writeln!(stdout, "         - Audience: External")?;
    }
    writeln!(stdout, "         - Contact info: your Google email")?;
    writeln!(stdout, "         - Click CREATE.")?;
    if workspace {
        writeln!(stdout)?;
        writeln!(
            stdout,
            "      Internal audience: no test users and no publishing step are"
        )?;
        writeln!(
            stdout,
            "      needed — your org's accounts are authorized automatically."
        )?;
    } else {
        writeln!(stdout)?;
        writeln!(
            stdout,
            "      b. Open the Audience tab (left menu) and add your Google"
        )?;
        writeln!(stdout, "         account email under 'Test users'.")?;
        writeln!(stdout)?;
        writeln!(
            stdout,
            "      c. To avoid weekly re-consent (the 7-day cap on sensitive"
        )?;
        writeln!(
            stdout,
            "         scopes in 'Testing' mode), click 'PUBLISH APP' on the"
        )?;
        writeln!(
            stdout,
            "         same Audience tab and confirm. At consent time Google"
        )?;
        writeln!(
            stdout,
            "         shows an 'unverified app' warning — expected for a"
        )?;
        writeln!(
            stdout,
            "         personal client; click 'Advanced -> Go to air-drive"
        )?;
        writeln!(
            stdout,
            "         (unsafe)'. Verification is only needed to share the app."
        )?;
        let url = format!("https://console.cloud.google.com/auth/audience?project={project_id}");
        open_or_print(&mut stdout, &url)?;
    }
    let _ = prompt(&mut stdin, &mut stdout, "      Press Enter when done: ")?;

    // Step 4 — OAuth Desktop client (new Auth Platform location).
    writeln!(stdout)?;
    writeln!(stdout, "[4/5] Create the OAuth client credentials")?;
    let url = format!("https://console.cloud.google.com/auth/clients/create?project={project_id}");
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
    let url =
        format!("https://console.cloud.google.com/auth/clients/{client_id}?project={project_id}");
    open_or_print(&mut stdout, &url)?;
    let client_secret = prompt(&mut stdin, &mut stdout, "      Paste the Client secret: ")?;
    validate_client_secret(&client_secret)?;

    // Step 5 — write config.toml. We only persist what the user just typed
    // in (the [oauth] credentials). Every other section is left to the
    // migration step below, which fills the schema with default values *and*
    // a leading `# description` line per field — that's how the user
    // discovers options like `watch.auto_create_root` without reading the
    // changelog. Re-running with `--force` preserves any existing comments
    // and user overrides because we drive a format-preserving `toml_edit`
    // document instead of re-rendering through serde.
    writeln!(stdout)?;
    writeln!(stdout, "[5/5] Writing {}", config_path.display())?;
    write_oauth_seed(&config_path, &client_id, &client_secret)?;
    crate::config::migrate::migrate_on_disk(&config_path)?;
    let cfg = Config::load(&config_path)?;
    writeln!(stdout, "      ✓ done")?;
    writeln!(stdout)?;

    if link_after {
        writeln!(stdout, "Running `air-drive link`...")?;
        drop(stdout);
        drop(stdin);
        return crate::cli::link::run(config_dir_override, &cfg, None).await;
    }

    writeln!(
        stdout,
        "Next: run `air-drive link` to authorize the account."
    )?;
    Ok(ExitCode::Ok)
}

/// Write (or update) the `[oauth]` section of `config.toml`, preserving every
/// other section verbatim. If the file does not exist yet we create an empty
/// `toml_edit` document; if it does, we parse it and surgically set the two
/// keys. The rest of the schema is left to [`crate::config::migrate`] to
/// materialise with descriptive comments on the very next read.
fn write_oauth_seed(path: &Path, client_id: &str, client_secret: &str) -> Result<()> {
    use toml_edit::{DocumentMut, Item, Table, value};

    let mut doc: DocumentMut = match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .map_err(|e: toml_edit::TomlError| Error::Toml(e.to_string()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(e) => return Err(e.into()),
    };

    if !doc.contains_table("oauth") {
        let mut t = Table::new();
        t.set_implicit(false);
        doc.insert("oauth", Item::Table(t));
    }
    let oauth = doc
        .get_mut("oauth")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            Error::Config("config.toml has a non-table [oauth] entry; refusing to edit".into())
        })?;
    oauth["client_id"] = value(client_id);
    oauth["client_secret"] = value(client_secret);

    std::fs::write(path, doc.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
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

/// Parse a yes/no answer, defaulting to **No** on an empty line or anything
/// that isn't an explicit affirmative. Accepts `y` / `yes` (case-insensitive).
/// The conservative default matters: a wrong "yes" steers the user to an
/// Internal audience their account can't actually use, so the safe fallback is
/// the External path that works for everyone.
fn parse_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
        assert!(validate_client_id("123-abc.apps.googleusercontent.com").is_ok());
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

    #[test]
    fn parse_yes_accepts_affirmatives() {
        assert!(parse_yes("y"));
        assert!(parse_yes("Y"));
        assert!(parse_yes("yes"));
        assert!(parse_yes("  YES  "));
    }

    #[test]
    fn parse_yes_defaults_to_no() {
        assert!(!parse_yes(""));
        assert!(!parse_yes("n"));
        assert!(!parse_yes("no"));
        assert!(!parse_yes("nope"));
        assert!(!parse_yes("external"));
    }
}
