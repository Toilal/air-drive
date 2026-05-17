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

use std::path::PathBuf;
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

    /// Print the three resolved values to stdout instead of running
    /// `gh secret set`. The OAuth dance + folder creation still run; only the
    /// final push is skipped.
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
    };
    let provider = build_provider(&oauth, &args.config_dir, None).await?;
    // Force a token fetch so the persistence callback runs.
    let _access = provider.token().await?;
    let tokens_json = std::fs::read_to_string(&tokens_path)?;
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

    // 3. Push or print.
    if args.dry_run {
        println!("─── dry-run summary ─────────────────────────────────");
        println!("AIR_DRIVE_E2E_CLIENT_ID         = {}", args.client_id);
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
        "AIR_DRIVE_E2E_PARENT_FOLDER_ID",
        &folder_id,
        args.repo.as_deref(),
    )?;
    push_secret("AIR_DRIVE_E2E_TOKENS", &tokens_json, args.repo.as_deref())?;

    eprintln!();
    eprintln!("[setup] ✓ all 3 secrets pushed");
    eprintln!("[setup] trigger the workflow with: gh workflow run e2e");
    Ok(())
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
