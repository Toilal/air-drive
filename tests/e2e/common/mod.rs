//! Harness for the real-Drive end-to-end suite (`tests/e2e/rclone_drive.rs`).
//!
//! Each test:
//!
//! 1. Reads its credentials from env vars (see [`E2eConfig::from_env`]). Missing or
//!    empty values short-circuit the test with [`skip_unless_configured!`] so a
//!    machine without secrets running `cargo test -- --ignored` doesn't fail.
//! 2. Builds an [`E2eFixture`] that owns a tempdir + a freshly-created Drive
//!    sub-folder under the configured parent. The sub-folder is named
//!    `air-drive-e2e-<run_id>` so concurrent runs (CI matrix, parallel branches) can
//!    coexist without colliding.
//! 3. Invokes the production `air-drive` binary against the real Drive API and the
//!    real `rclone` binary on `$PATH`. The harness does not bypass anything — it
//!    exercises exactly the production code path.
//! 4. Best-effort cleanup at the end of the test via [`E2eFixture::cleanup`]. As a
//!    safety net, [`E2eFixture::sweep_stale`] runs first and trashes any
//!    `air-drive-e2e-*` folder older than 24 h (cleans up after crashes / cancelled
//!    runs).
//!
//! ## Env-var contract
//!
//! | env var                              | purpose                                      |
//! |--------------------------------------|----------------------------------------------|
//! | `AIR_DRIVE_E2E_TOKENS`               | Contents of `tokens.json` (refresh token).   |
//! | `AIR_DRIVE_E2E_CLIENT_ID`            | GCP OAuth Desktop client id (PKCE).          |
//! | `AIR_DRIVE_E2E_PARENT_FOLDER_ID`     | Drive folder ID under which tests scratch.   |
//!
//! Acquisition flow lives in `tests/e2e/README.md`.

#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

use air_drive::config::OauthConfig;
use air_drive::drive::auth::{TOKENS_FILE, build_provider};
use air_drive::drive::http::DriveHttp;
use air_drive::drive::metadata;

/// Three required env vars. Use [`from_env`] to read them; absence of any one means
/// "skip the test".
pub struct E2eConfig {
    pub tokens_json: String,
    pub client_id: String,
    pub client_secret: String,
    pub parent_folder_id: String,
}

impl E2eConfig {
    /// Read the env-var quartet. Returns `None` if any of them is missing or empty
    /// so the caller can no-op gracefully.
    ///
    /// Before reading, the function attempts to load `<cwd>/.env` via
    /// [`dotenvy::dotenv`] so the local workflow is `setup_e2e` → file written
    /// → `cargo test -- --ignored`, with no `set -a; source .env` step in
    /// between. CI sets the env vars directly; `dotenv` returns `Err(NotFound)`
    /// there and we silently fall back to `std::env::var`.
    pub fn from_env() -> Option<Self> {
        // `dotenv()` only sets variables that aren't already in the env, so CI
        // (which exports them via the workflow YAML) wins over any stray local
        // `.env` someone forgot to delete.
        let _ = dotenvy::dotenv();

        let tokens_json = std::env::var("AIR_DRIVE_E2E_TOKENS").ok()?;
        let client_id = std::env::var("AIR_DRIVE_E2E_CLIENT_ID").ok()?;
        let client_secret = std::env::var("AIR_DRIVE_E2E_CLIENT_SECRET").ok()?;
        let parent_folder_id = std::env::var("AIR_DRIVE_E2E_PARENT_FOLDER_ID").ok()?;
        if tokens_json.is_empty()
            || client_id.is_empty()
            || client_secret.is_empty()
            || parent_folder_id.is_empty()
        {
            return None;
        }
        Some(Self {
            tokens_json,
            client_id,
            client_secret,
            parent_folder_id,
        })
    }
}

/// Short-circuit the current test with a friendly print when the env trio isn't set.
/// Use at the top of every `#[tokio::test]` in `rclone_drive.rs`.
#[macro_export]
macro_rules! skip_unless_configured {
    ($cfg:ident) => {
        let Some($cfg) = $crate::common::E2eConfig::from_env() else {
            eprintln!(
                "[e2e] AIR_DRIVE_E2E_TOKENS / _CLIENT_ID / _CLIENT_SECRET / _PARENT_FOLDER_ID not set — skipping. \
                 See tests/e2e/README.md for setup."
            );
            return;
        };
    };
}

/// Disk-backed harness. Owns the tempdir + a freshly-minted Drive sub-folder.
pub struct E2eFixture {
    pub _tmp: TempDir,
    pub config_dir: PathBuf,
    pub local_dir: PathBuf,
    pub run_folder_id: String,
    pub run_folder_name: String,
    pub cfg: E2eConfig,
    pub drive: Arc<DriveHttp>,
}

impl E2eFixture {
    /// Build the fixture: tempdir, write config.toml + tokens.json with `0600`,
    /// build a real-Drive [`DriveHttp`] from the on-disk tokens, sweep stale
    /// `air-drive-e2e-*` folders under the parent, then create a fresh per-run
    /// sub-folder.
    pub async fn new(cfg: E2eConfig) -> Self {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let config_dir = tmp.path().join("config");
        let local_dir = tmp.path().join("local");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        // Write `config.toml` with the user's OAuth client_id override.
        let toml = format!(
            "[oauth]\nclient_id = \"{}\"\n\n[mapping]\n\n[daemon]\n\n[rclone]\n",
            cfg.client_id
        );
        std::fs::write(config_dir.join("config.toml"), toml).unwrap();

        // Write `tokens.json` at `0600`.
        let tokens_path = config_dir.join(TOKENS_FILE);
        std::fs::write(&tokens_path, &cfg.tokens_json).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tokens_path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&tokens_path, perms).unwrap();
        }

        // Build a real-Drive HTTP client from the tokens.
        let oauth = OauthConfig {
            client_id: Some(cfg.client_id.clone()),
            client_secret: Some(cfg.client_secret.clone()),
        };
        let provider = build_provider(&oauth, &config_dir, None)
            .await
            .expect("build_provider with real tokens");
        let drive = Arc::new(DriveHttp::new(provider).expect("DriveHttp::new"));

        // Sweep stale folders before claiming our own — keeps the test parent tidy.
        sweep_stale(&drive, &cfg.parent_folder_id).await;

        // Create the per-run sub-folder.
        let run_id = mint_run_id();
        let run_folder_name = format!("air-drive-e2e-{run_id}");
        let folder = metadata::create_folder(&drive, &cfg.parent_folder_id, &run_folder_name)
            .await
            .expect("create_folder for run");

        Self {
            _tmp: tmp,
            config_dir,
            local_dir,
            run_folder_id: folder.id,
            run_folder_name,
            cfg,
            drive,
        }
    }

    /// Best-effort cleanup of the per-run sub-folder. Call explicitly at the end of
    /// the test (Drop can't easily run async code without grabbing the current
    /// tokio runtime, which is racy across test binaries). Failures are logged but
    /// not propagated — the [`sweep_stale`] pass on the next run mops up.
    pub async fn cleanup(&self) {
        if let Err(e) = self
            .drive
            .delete(&format!("files/{}", self.run_folder_id))
            .await
        {
            eprintln!(
                "[e2e] cleanup of {} failed (will be swept on next run): {e}",
                self.run_folder_name
            );
        }
    }

    /// Path the binary uses to find `tokens.json`.
    pub fn tokens_path(&self) -> PathBuf {
        self.config_dir.join(TOKENS_FILE)
    }

    /// Build an [`assert_cmd`] command that runs the freshly-built `air-drive`
    /// binary against the real Drive + real rclone. The only env override is the
    /// `EXIT_AFTER_INITIAL_SYNC=1` knob — the e2e suite drives initial-sync via
    /// `Command::output()` and would hang on the continuous loop otherwise.
    pub fn air_drive_cmd(&self) -> StdCommand {
        let mut cmd = StdCommand::cargo_bin("air-drive").expect("cargo-built binary");
        cmd.arg("--config-dir")
            .arg(&self.config_dir)
            .env("RUST_LOG", "info")
            .env("AIR_DRIVE_TEST_EXIT_AFTER_INITIAL_SYNC", "1");
        cmd
    }

    /// Convenience: drop a file at `<local_dir>/<rel>` with `content`. Creates
    /// parent directories.
    pub fn populate_local(&self, rel: &str, content: &[u8]) {
        let p = self.local_dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }
}

/// Generate a short, sort-friendly run id of the form `YYYYMMDDHHMMSS-XXXX`, where
/// the trailing four chars are pseudo-random.
fn mint_run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Take a slice of the nanosecond clock to derive entropy without a crypto rng.
    let entropy = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{now}-{:04x}", entropy & 0xffff)
}

/// Trash any `air-drive-e2e-*` folder under `parent_id` whose `createdTime` is more
/// than 24 hours old. Errors are logged but not propagated — we still want to run
/// the actual test even if the sweep flakes.
pub async fn sweep_stale(drive: &DriveHttp, parent_id: &str) {
    let cutoff_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(24 * 3600);

    let q = format!(
        "'{parent_id}' in parents and trashed = false \
         and mimeType = 'application/vnd.google-apps.folder' \
         and name contains 'air-drive-e2e-'"
    );
    let body = match drive
        .get_json(
            "files",
            &[("q", q.as_str()), ("fields", "files(id,name,createdTime)")],
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[e2e] sweep_stale: list failed: {e}");
            return;
        }
    };
    let Some(files) = body.get("files").and_then(|v| v.as_array()) else {
        return;
    };
    for f in files {
        let Some(id) = f.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let created_unix = f
            .get("createdTime")
            .and_then(|v| v.as_str())
            .and_then(parse_rfc3339_secs)
            .unwrap_or(u64::MAX);
        if created_unix > cutoff_unix {
            continue;
        }
        let path = format!("files/{id}");
        if let Err(e) = drive.delete(&path).await {
            eprintln!("[e2e] sweep_stale: delete {id} failed: {e}");
        }
    }
}

/// Best-effort parser for the RFC 3339 timestamps Drive emits (e.g.
/// `"2026-05-17T12:34:56.789Z"`). Returns seconds since Unix epoch on success.
fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    // Drive's createdTime is always UTC with a `Z` suffix; we parse the date/time
    // portion by hand to avoid pulling in `chrono` just for this.
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let mo: u64 = date_parts.next()?.parse().ok()?;
    let d: u64 = date_parts.next()?.parse().ok()?;
    let time_only = time.split('.').next()?;
    let mut t = time_only.split(':');
    let h: u64 = t.next()?.parse().ok()?;
    let mi: u64 = t.next()?.parse().ok()?;
    let se: u64 = t.next()?.parse().ok()?;
    // Days-from-civil algorithm (Howard Hinnant) — pure integer math, no leap-year
    // shenanigans. Returns days since 1970-01-01 for (y, mo, d).
    let mo_i = mo as i64;
    let y = if mo_i <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = if mo_i > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    if days < 0 {
        return None;
    }
    let secs = days as u64 * 86400 + h * 3600 + mi * 60 + se;
    Some(secs)
}

/// Spawn-and-control wrapper around a continuously-running `air-drive start`
/// against the real Drive, for e2e tests that need the daemon alive (continuous
/// sync — folder renames/moves). `kill_on_drop` ensures a panicking test never
/// leaks a runaway daemon. Mirrors the integration suite's `DaemonProcess`.
pub struct DaemonProcess {
    child: tokio::process::Child,
    pid: u32,
}

impl DaemonProcess {
    /// Spawn `air-drive --config-dir <fx> start <extra_args...>` staying in the
    /// continuous loop (`EXIT_AFTER_INITIAL_SYNC=0`, overriding the one-shot
    /// default `air_drive_cmd` sets). Returns once the child is up; callers poll
    /// the side effect they care about (a file/folder on Drive, a local path) via
    /// [`wait_until`].
    pub async fn spawn(fx: &E2eFixture, extra_args: &[&str]) -> Self {
        let mut cmd: tokio::process::Command = fx.air_drive_cmd().into();
        cmd.env("AIR_DRIVE_TEST_EXIT_AFTER_INITIAL_SYNC", "0");
        cmd.arg("start");
        // The harness always starts from a fresh config dir (empty change cursor),
        // and the daemon refuses a first-time continuous start without an explicit
        // initial pass ("first-time start requires --initial-sync"). Every
        // continuous-daemon scenario seeds local/remote state it then expects the
        // daemon to reconcile, so the initial sync is exactly what they want.
        cmd.arg("--initial-sync");
        for a in extra_args {
            cmd.arg(a);
        }
        cmd.kill_on_drop(true);
        let child = cmd.spawn().expect("spawn air-drive start");
        let pid = child.id().expect("child pid");
        // Let the process clear its bootstrap (Db::open + Lock::acquire).
        tokio::time::sleep(Duration::from_millis(300)).await;
        Self { child, pid }
    }

    /// Best-effort liveness check. `None` while running; `Some(status)` once exited.
    pub fn poll_alive(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// `SIGTERM`, wait up to 15 s for a clean drain, fall back to `SIGKILL`.
    pub async fn shutdown(mut self) -> std::process::ExitStatus {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let _ = kill(Pid::from_raw(self.pid as i32), Signal::SIGTERM);
        match tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => panic!("daemon wait failed: {e}"),
            Err(_) => {
                let _ = self.child.start_kill();
                self.child.wait().await.expect("wait after SIGKILL")
            }
        }
    }
}

/// Poll `cond` every 500 ms until it returns `true` or `timeout` expires. Returns
/// whether the condition was met. The poll cadence is deliberately gentle — these
/// conditions hit the real Drive REST API, which is rate-limited.
pub async fn wait_until<F, Fut>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod selftest {
    use super::*;

    #[test]
    fn parse_rfc3339_secs_known_epoch() {
        // 1970-01-01T00:00:00Z → 0
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_rfc3339_secs_one_full_day_advances_86400() {
        // Algorithm-independent invariant: a 24-h gap on adjacent dates must equal
        // exactly 86 400 seconds.
        let a = parse_rfc3339_secs("2026-05-17T12:34:56Z").unwrap();
        let b = parse_rfc3339_secs("2026-05-18T12:34:56Z").unwrap();
        assert_eq!(b - a, 86_400);

        // Sub-second precision is silently truncated to the second.
        assert_eq!(
            parse_rfc3339_secs("2026-05-17T12:34:56.789Z"),
            parse_rfc3339_secs("2026-05-17T12:34:56Z")
        );
    }

    #[test]
    fn parse_rfc3339_secs_leap_day_advances_correctly() {
        // 2024 is a leap year; Feb 29 → Mar 1 is +86 400 s.
        let leap = parse_rfc3339_secs("2024-02-29T00:00:00Z").unwrap();
        let after = parse_rfc3339_secs("2024-03-01T00:00:00Z").unwrap();
        assert_eq!(after - leap, 86_400);
        // 2025 (non-leap) — Feb 28 → Mar 1 also exactly one day.
        let non_leap_feb = parse_rfc3339_secs("2025-02-28T00:00:00Z").unwrap();
        let non_leap_mar = parse_rfc3339_secs("2025-03-01T00:00:00Z").unwrap();
        assert_eq!(non_leap_mar - non_leap_feb, 86_400);
    }

    #[test]
    fn parse_rfc3339_secs_rejects_garbage() {
        assert_eq!(parse_rfc3339_secs("not a date"), None);
        assert_eq!(parse_rfc3339_secs("2026/05/17 12:34"), None);
    }

    #[test]
    fn mint_run_id_is_sortable_and_unique_ish() {
        let a = mint_run_id();
        let b = mint_run_id();
        // Both follow the `<digits>-<hex4>` shape.
        assert!(a.split('-').count() == 2);
        assert!(b.split('-').count() == 2);
        // Run ids minted moments apart compare by their leading timestamp; we don't
        // assert strict inequality (clock resolution + the same-second case), but the
        // length is bounded and the format is consistent.
        assert!(a.len() <= 30);
    }
}
