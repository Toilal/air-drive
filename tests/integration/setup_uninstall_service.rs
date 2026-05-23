//! Integration coverage for `air-drive setup --uninstall-service` (feature 002).
//!
//! Three user stories from `specs/002-uninstall-service-flag/spec.md`:
//!
//! - US1 (P1) — happy path: unit present + `systemctl` works → file removed,
//!   shim sees `disable --now` then `daemon-reload`, exit 0.
//! - US2 (P2) — idempotent: nothing to remove → exit 0 with a clear message.
//! - US3 (P3) — graceful: `systemctl` missing → warning, file still removed,
//!   exit 0.
//!
//! Plus the mutually-exclusive flag guard (FR-008).
//!
//! The tests fake `systemctl` via a small shell-script shim on `$PATH` and
//! redirect `$XDG_CONFIG_HOME` to a tempdir so the unit file location is
//! confined to the test fixture (the `directories` crate honours
//! `XDG_CONFIG_HOME` on Linux, which is the only platform this feature
//! supports).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

/// A tempdir containing an executable `systemctl` shim. The shim writes its
/// own argv (joined by spaces, one line per invocation) to a log file inside
/// the tempdir so tests can assert on the sequence of calls.
struct Shim {
    dir: TempDir,
    log_path: PathBuf,
}

impl Shim {
    /// Shim that always exits 0. Use for the US1 happy path.
    fn always_ok() -> Self {
        Self::build("", 0)
    }

    /// Shim that exits non-zero on `disable` with a stderr the production
    /// code's `is_unit_not_loaded` helper recognises, and exits 0 on
    /// `daemon-reload`. Use for the US2 not-loaded path.
    fn disable_unit_not_loaded() -> Self {
        let script = "if [ \"$2\" = \"disable\" ]; then \
                        echo 'Failed to disable unit: Unit file air-drive.service does not exist.' \
                        1>&2; \
                        exit 1; \
                      fi\n";
        Self::build(script, 0)
    }

    fn build(extra_branch: &str, default_exit: i32) -> Self {
        let dir = tempfile::tempdir().expect("create shim tempdir");
        let log_path = dir.path().join("calls.log");
        let shim_path = dir.path().join("systemctl");
        // The shebang points at an absolute `/bin/sh` so the shim does not
        // require any binary on `$PATH` — the test deliberately confines
        // `$PATH` to the shim directory itself (or, in US3, to a directory
        // that contains nothing at all). `printf` and `[ ... ]` are POSIX,
        // so plain `sh` is enough.
        let body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {log:?}\n\
             {extra_branch}\
             exit {default_exit}\n",
            log = log_path,
        );
        std::fs::write(&shim_path, body).expect("write shim script");
        set_executable(&shim_path);
        Self { dir, log_path }
    }

    fn path_dir(&self) -> &Path {
        self.dir.path()
    }

    fn calls(&self) -> Vec<String> {
        match std::fs::read_to_string(&self.log_path) {
            Ok(s) => s.lines().map(str::to_owned).collect(),
            Err(_) => Vec::new(),
        }
    }
}

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

/// Builds an `assert_cmd::Command` invoking the cargo-built binary with:
///
/// - `--config-dir <tempdir>/daemon-config` for the daemon-side paths
///   (irrelevant here but required by the dispatcher).
/// - `XDG_CONFIG_HOME=<tempdir>/xdg-config` so the unit-file destination
///   resolves into the fixture rather than the user's real `~/.config`.
/// - `HOME=<tempdir>` for completeness — `directories::BaseDirs::new()`
///   requires `HOME` and we want to be robust against test isolation tweaks.
/// - `PATH` set to whatever the caller wants (typically the shim dir, or an
///   empty dir for US3). Standard system binaries are NOT inherited.
struct Fixture {
    tmp: TempDir,
    xdg_config_home: PathBuf,
    daemon_config_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("create fixture tempdir");
        let xdg_config_home = tmp.path().join("xdg-config");
        let daemon_config_dir = tmp.path().join("daemon-config");
        std::fs::create_dir_all(&xdg_config_home).unwrap();
        std::fs::create_dir_all(&daemon_config_dir).unwrap();
        Self {
            tmp,
            xdg_config_home,
            daemon_config_dir,
        }
    }

    /// Absolute path where the binary will look for / write the unit file
    /// when `XDG_CONFIG_HOME` is set to `self.xdg_config_home`.
    fn unit_path(&self) -> PathBuf {
        self.xdg_config_home
            .join("systemd")
            .join("user")
            .join("air-drive.service")
    }

    fn place_unit_file(&self, contents: &str) {
        let path = self.unit_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
    }

    /// Build the command. `path_override` becomes the `$PATH` the binary sees.
    fn command(&self, path_override: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::cargo_bin("air-drive").expect("cargo-built binary");
        cmd.env_clear()
            .env("HOME", self.tmp.path())
            .env("XDG_CONFIG_HOME", &self.xdg_config_home)
            .env("PATH", path_override)
            .env("RUST_LOG", "info")
            .arg("--config-dir")
            .arg(&self.daemon_config_dir);
        cmd
    }
}

// ---------------------------------------------------------------------------
// US1 — happy path
// ---------------------------------------------------------------------------

#[test]
fn us1_happy_path_removes_unit_and_runs_systemctl() {
    let fx = Fixture::new();
    let shim = Shim::always_ok();
    fx.place_unit_file("[Unit]\nDescription=stub\n");

    let mut cmd = fx.command(shim.path_dir());
    cmd.args(["setup", "--uninstall-service"]);
    let out = cmd.output().expect("spawn air-drive");

    assert!(
        out.status.success(),
        "expected exit 0, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !fx.unit_path().exists(),
        "unit file should have been removed",
    );
    let calls = shim.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("--user disable --now air-drive.service")),
        "shim should have observed `disable --now`; calls = {calls:?}",
    );
    assert!(
        calls.iter().any(|c| c.contains("--user daemon-reload")),
        "shim should have observed `daemon-reload`; calls = {calls:?}",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("removed air-drive.service"),
        "stderr should report removal; got:\n{stderr}",
    );
}

#[test]
fn us1_mutually_exclusive_flags_rejected_by_clap() {
    let fx = Fixture::new();
    let shim = Shim::always_ok();
    fx.place_unit_file("[Unit]\nDescription=stub\n");

    let mut cmd = fx.command(shim.path_dir());
    cmd.args(["setup", "--install-service", "--uninstall-service"]);
    let out = cmd.output().expect("spawn air-drive");

    assert!(
        !out.status.success(),
        "expected non-zero exit, got success\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflicts with"),
        "stderr should describe the conflict; got:\n{stderr}",
    );
    // No side effects: the shim should not have been called, the file should
    // still be there.
    assert!(
        shim.calls().is_empty(),
        "shim should not have been invoked; calls = {:?}",
        shim.calls(),
    );
    assert!(
        fx.unit_path().exists(),
        "unit file should not have been touched",
    );
}

// ---------------------------------------------------------------------------
// US2 — idempotency
// ---------------------------------------------------------------------------

#[test]
fn us2_no_op_on_clean_host() {
    let fx = Fixture::new();
    // Use a shim that returns "unit not loaded" on `disable` to mirror what
    // a real systemd would say when the unit was never enabled.
    let shim = Shim::disable_unit_not_loaded();
    // Intentionally no unit file present.

    let start = Instant::now();
    let mut cmd = fx.command(shim.path_dir());
    cmd.args(["setup", "--uninstall-service"]);
    let out = cmd.output().expect("spawn air-drive");
    let elapsed = start.elapsed();

    assert!(
        out.status.success(),
        "expected exit 0 on clean-host no-op, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    // SC-003: no-op completes well under 1 s.
    assert!(
        elapsed.as_secs() < 5,
        "no-op uninstall took {elapsed:?}; expected well under 5 s",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nothing to remove"),
        "stderr should mention that nothing was removed; got:\n{stderr}",
    );
}

#[test]
fn us2_second_invocation_is_also_a_no_op() {
    let fx = Fixture::new();
    let shim = Shim::always_ok();
    fx.place_unit_file("[Unit]\nDescription=stub\n");

    // First call — real uninstall.
    let mut cmd = fx.command(shim.path_dir());
    cmd.args(["setup", "--uninstall-service"]);
    let first = cmd.output().expect("first spawn air-drive");
    assert!(
        first.status.success(),
        "first uninstall should succeed\nstderr:\n{}",
        String::from_utf8_lossy(&first.stderr),
    );

    // Second call — different shim that now reports "unit not loaded" because
    // the unit is genuinely gone. Use a fresh shim so the call log is clean.
    let shim2 = Shim::disable_unit_not_loaded();
    let mut cmd2 = fx.command(shim2.path_dir());
    cmd2.args(["setup", "--uninstall-service"]);
    let second = cmd2.output().expect("second spawn air-drive");

    assert!(
        second.status.success(),
        "second (no-op) uninstall should succeed\nstderr:\n{}",
        String::from_utf8_lossy(&second.stderr),
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("nothing to remove"),
        "second invocation should report no-op; got:\n{stderr}",
    );
}

// ---------------------------------------------------------------------------
// US3 — graceful fallback
// ---------------------------------------------------------------------------

#[test]
fn us3_missing_systemctl_still_removes_file() {
    let fx = Fixture::new();
    fx.place_unit_file("[Unit]\nDescription=stub\n");

    // Empty PATH dir — no `systemctl` reachable.
    let empty_path_dir = tempfile::tempdir().expect("create empty path dir");

    let mut cmd = fx.command(empty_path_dir.path());
    cmd.args(["setup", "--uninstall-service"]);
    let out = cmd.output().expect("spawn air-drive");

    assert!(
        out.status.success(),
        "expected exit 0 when systemctl is missing, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !fx.unit_path().exists(),
        "unit file should still be removed even without systemctl",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("systemctl") && stderr.to_lowercase().contains("not found"),
        "stderr should warn about missing systemctl; got:\n{stderr}",
    );
}

#[test]
fn us3_missing_systemctl_and_no_file_is_still_ok() {
    let fx = Fixture::new();
    // No unit file present.
    let empty_path_dir = tempfile::tempdir().expect("create empty path dir");

    let mut cmd = fx.command(empty_path_dir.path());
    cmd.args(["setup", "--uninstall-service"]);
    let out = cmd.output().expect("spawn air-drive");

    assert!(
        out.status.success(),
        "expected exit 0, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("systemctl") && stderr.to_lowercase().contains("not found"),
        "stderr should warn about missing systemctl; got:\n{stderr}",
    );
}
