//! End-to-end test-suite bootstrap.
//!
//! Drives the parts of the e2e onboarding that **can** be automated:
//!
//! 1. Run the OAuth dance against Google (opens the browser, the user approves)
//!    and persist a `tokens.json` to a config dir of the caller's choice.
//! 2. Create the parent Drive folder (`air-drive-e2e-parent`) the test suite
//!    needs as its scratch root.
//! 3. Push the three required secrets to GitHub Actions via
//!    `gh secret set --body`.
//!
//! What this script CAN'T do (manual prerequisites — see `tests/e2e/README.md`
//! §2 for the click-by-click flow):
//!
//! - Create the GCP project.
//! - Enable the Drive API on it.
//! - Create the OAuth **Desktop**-type client. `gcloud alpha iap oauth-clients`
//!   only handles IAP / web clients; there is no `gcloud`-equivalent for the
//!   Desktop client type. You set this up once in the Cloud Console, copy the
//!   `client_id`, and pass it to this script.
//!
//! ## Usage
//!
//! ```sh
//! cargo run --example setup_e2e -- \
//!     --client-id 1234567890-abc.apps.googleusercontent.com \
//!     --config-dir /tmp/air-drive-e2e-setup
//! ```
//!
//! Add `--dry-run` to print the three secret values without pushing.
//!
//! ## Re-runs
//!
//! The script is idempotent: a second run reuses the cached refresh token in
//! `<config-dir>/tokens.json` (no browser dance needed) and skips folder
//! creation if a name match already lives under My Drive. Pass
//! `--force-new-token` to wipe the token cache; `--parent-folder-name` to
//! override the default.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;

use air_drive::config::OauthConfig;
use air_drive::drive::auth::{TOKENS_FILE, build_provider};
use air_drive::drive::http::DriveHttp;
use air_drive::drive::metadata;

#[derive(Parser, Debug)]
#[command(
    name = "setup-e2e",
    about = "Bootstrap the e2e test suite: OAuth dance + parent folder + GitHub secrets"
)]
struct Args {
    /// GCP OAuth Desktop `client_id`. Get it from the Cloud Console after creating
    /// an OAuth client of type "Desktop app".
    #[arg(long)]
    client_id: String,

    /// Companion `client_secret` for the OAuth Desktop client (Google requires it
    /// at the token endpoint even though PKCE handles the auth proof). Copy from
    /// the Cloud Console OAuth client details panel.
    #[arg(long)]
    client_secret: String,

    /// Where to write the config + tokens.json. The directory is created if
    /// missing. After a successful run this also holds your refresh token, so
    /// pick a path you're comfortable keeping around (or delete it once the
    /// GitHub secret is set).
    #[arg(long)]
    config_dir: PathBuf,

    /// Name of the Drive folder to create as the test parent. Default is fine
    /// unless you want to share the same Google account across multiple repos.
    #[arg(long, default_value = "air-drive-e2e-parent")]
    parent_folder_name: String,

    /// GitHub repo to push the secrets to, in `owner/name` form. Defaults to
    /// whatever `gh repo view` reports for the current directory.
    #[arg(long)]
    repo: Option<String>,

    /// Where to write a `.env` file with the four resolved values, in
    /// `KEY='VALUE'`-per-line form. Always written (even in `--dry-run`) so
    /// the local e2e suite can pick them up via `set -a; source .env; set +a`
    /// or via direnv. Chmod-ed to `0600` because it carries the refresh token
    /// and the OAuth client_secret. Pass `--env-file ''` to skip.
    #[arg(long, default_value = ".env")]
    env_file: PathBuf,

    /// Print the four resolved values to stdout instead of running
    /// `gh secret set`. The OAuth dance + folder creation + `.env` write still
    /// run; only the final push to GitHub is skipped.
    #[arg(long)]
    dry_run: bool,

    /// Delete any existing `tokens.json` before starting so the OAuth dance
    /// fires fresh. Useful when the cached refresh token has expired.
    #[arg(long)]
    force_new_token: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();
    let args = Args::parse();

    if !args.dry_run {
        ensure_gh_authenticated()?;
    }

    std::fs::create_dir_all(&args.config_dir)?;
    let tokens_path = args.config_dir.join(TOKENS_FILE);
    if args.force_new_token && tokens_path.exists() {
        std::fs::remove_file(&tokens_path)?;
        eprintln!("[setup] dropped cached {}", tokens_path.display());
    }

    // 1. OAuth dance. `build_provider` writes tokens.json on first token fetch.
    eprintln!("[setup] starting OAuth flow (a browser tab will open if no cached token)");
    let oauth = OauthConfig {
        client_id: Some(args.client_id.clone()),
        client_secret: Some(args.client_secret.clone()),
    };
    let provider = build_provider(&oauth, &args.config_dir, None).await?;
    // Force a token fetch so the persistence callback runs.
    let _access = provider.token().await?;
    // yup-oauth2 flushes tokens.json slightly *after* `token()` returns, so a read
    // here races the write and can see an empty/partial file. Poll until the file
    // is a valid token array carrying a refresh token before trusting it.
    let tokens_json = read_tokens_when_ready(&tokens_path).await?;
    eprintln!("[setup] tokens.json written ({} bytes)", tokens_json.len());

    // 2. Parent folder. Try to find an existing one with the same name first to
    // keep the script idempotent.
    let drive = DriveHttp::new(provider)?;
    let folder_id = match find_existing_root_folder(&drive, &args.parent_folder_name).await? {
        Some(id) => {
            eprintln!(
                "[setup] reusing existing folder `{}` (id: {id})",
                args.parent_folder_name
            );
            id
        }
        None => {
            let created = metadata::create_folder(&drive, "root", &args.parent_folder_name).await?;
            eprintln!(
                "[setup] created folder `{}` (id: {})",
                args.parent_folder_name, created.id
            );
            created.id
        }
    };

    // 3. Local `.env` first so the values reach the developer's shell even if
    //    the subsequent GitHub push fails (e.g. PAT lacks `Secrets: write`).
    if !args.env_file.as_os_str().is_empty() {
        write_env_file(
            &args.env_file,
            &args.client_id,
            &args.client_secret,
            &folder_id,
            &tokens_json,
        )?;
        eprintln!(
            "[setup] wrote .env at {} (chmod 0600)",
            args.env_file.display()
        );
    }

    // 4. Push or print.
    if args.dry_run {
        println!("─── dry-run summary ─────────────────────────────────");
        println!("AIR_DRIVE_E2E_CLIENT_ID         = {}", args.client_id);
        println!(
            "AIR_DRIVE_E2E_CLIENT_SECRET     = (omitted, {} chars)",
            args.client_secret.len()
        );
        println!("AIR_DRIVE_E2E_PARENT_FOLDER_ID  = {folder_id}");
        println!(
            "AIR_DRIVE_E2E_TOKENS            = (omitted, {} bytes — read from {})",
            tokens_json.len(),
            tokens_path.display()
        );
        return Ok(());
    }

    push_secret(
        "AIR_DRIVE_E2E_CLIENT_ID",
        &args.client_id,
        args.repo.as_deref(),
    )?;
    push_secret(
        "AIR_DRIVE_E2E_CLIENT_SECRET",
        &args.client_secret,
        args.repo.as_deref(),
    )?;
    push_secret(
        "AIR_DRIVE_E2E_PARENT_FOLDER_ID",
        &folder_id,
        args.repo.as_deref(),
    )?;
    push_secret("AIR_DRIVE_E2E_TOKENS", &tokens_json, args.repo.as_deref())?;

    eprintln!();
    eprintln!("[setup] ✓ all 4 secrets pushed");
    eprintln!("[setup] trigger the workflow with: gh workflow run e2e");
    Ok(())
}

/// Read `tokens.json` once yup-oauth2 has actually flushed it. `token()` can
/// return before the on-disk write completes, so an immediate read races the
/// writer and may see an empty or partial file (which then gets pushed as an
/// empty `AIR_DRIVE_E2E_TOKENS` secret). Poll for up to ~5 s until the file
/// parses as a token array carrying a refresh token.
async fn read_tokens_when_ready(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if !s.trim().is_empty() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                    let has_refresh = v.as_array().is_some_and(|a| {
                        a.iter().any(|e| {
                            e.pointer("/token/refresh_token")
                                .and_then(serde_json::Value::as_str)
                                .is_some()
                        })
                    });
                    if has_refresh {
                        return Ok(s);
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(format!(
        "tokens.json at {} never became a valid token file (no refresh_token after 5s)",
        path.display()
    )
    .into())
}

/// Refuse to start when `gh auth status` exits non-zero — saves the user from
/// running the OAuth dance only to fail at the final secret push.
fn ensure_gh_authenticated() -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("gh").arg("auth").arg("status").status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "`gh auth status` exited with {s}; run `gh auth login` first \
             (or pass --dry-run to skip secret push)"
        )
        .into()),
        Err(e) => Err(format!(
            "`gh` CLI not available ({e}); install from https://cli.github.com/ \
             or pass --dry-run to skip secret push"
        )
        .into()),
    }
}

/// Look for an existing folder named `name` directly under My Drive root. Returns
/// the first match's id. Lets the script be re-run without piling up parent
/// folders.
async fn find_existing_root_folder(
    drive: &DriveHttp,
    name: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let children = metadata::list_children(drive, "root").await?;
    Ok(children
        .into_iter()
        .find(|c| c.is_folder() && c.name == name)
        .map(|c| c.id))
}

/// Write the four resolved values to `path` in `.env` form. Values are wrapped
/// in single quotes — JSON's escape rules don't produce literal single quotes
/// inside tokens.json so the format is unambiguous. Existing files are
/// overwritten; the file is then chmod-ed to `0600` because it carries the
/// refresh token and the client_secret.
fn write_env_file(
    path: &Path,
    client_id: &str,
    client_secret: &str,
    parent_folder_id: &str,
    tokens_json: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let body = format!(
        "# Generated by `cargo run --example setup_e2e` — do not commit.\n\
         # Load with `set -a; source .env; set +a` or via direnv.\n\
         AIR_DRIVE_E2E_CLIENT_ID='{client_id}'\n\
         AIR_DRIVE_E2E_CLIENT_SECRET='{client_secret}'\n\
         AIR_DRIVE_E2E_PARENT_FOLDER_ID='{parent_folder_id}'\n\
         AIR_DRIVE_E2E_TOKENS='{tokens_json}'\n"
    );
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn push_secret(
    name: &str,
    value: &str,
    repo: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::new("gh");
    cmd.arg("secret").arg("set").arg(name);
    if let Some(r) = repo {
        cmd.arg("--repo").arg(r);
    }
    // `--body -` reads from stdin. We pass the body inline via `--body <value>`
    // which is fine for our payload sizes (a few KB at most for tokens.json).
    cmd.arg("--body").arg(value);
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("`gh secret set {name}` failed (exit {status})").into());
    }
    eprintln!("[setup] pushed {name}");
    Ok(())
}
