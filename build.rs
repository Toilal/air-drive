//! Embed a git-aware version string into the binary at build time.
//!
//! Sets `AIR_DRIVE_VERSION` for the crate. The runtime helper
//! [`air_drive::VERSION`](`air_drive::VERSION`) reads it via `option_env!`
//! and falls back to `CARGO_PKG_VERSION` when the binary was built outside
//! a git checkout (release tarball, vendored sdist, etc.).
//!
//! Format, SemVer 2.0.0 compliant:
//!
//! - **on a clean tag** (HEAD = `v0.1.1`, working tree clean):
//!   `0.1.1` — the tag as-is, stripped of its `v` prefix.
//! - **off-tag, or dirty** (any deviation from a clean tag):
//!   `<next-patch>-dev.<count>+g<sha>[.dirty]`, e.g. `0.1.2-dev.12+gff7bba8`
//!   or `0.1.2-dev.12+gff7bba8.dirty`.
//!
//! Precedence chain (per SemVer): `0.1.1 < 0.1.2-dev.0 < 0.1.2`. Dev builds
//! sort strictly above the previous release and strictly below the next one,
//! which is the property `git describe`-style suffixes lack.

use std::process::Command;

fn main() {
    // Re-run when the commit, branch, or working tree changes. `.git/HEAD`
    // covers branch + commit, `.git/index` covers `git add` / dirty toggles.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let version = compose_version();
    println!("cargo:rustc-env=AIR_DRIVE_VERSION={version}");
}

fn compose_version() -> String {
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

    // Not inside a git checkout (tarball build, vendored deps, …): fall back
    // to the Cargo manifest version verbatim.
    let Some(head_sha) = git(&["rev-parse", "--short", "HEAD"]) else {
        return pkg_version;
    };

    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    // Fast path: HEAD sits exactly on a tag AND the working tree is clean →
    // emit the tag (sans `v` prefix). Any uncommitted change disqualifies the
    // build from masquerading as the tagged release.
    if !dirty && let Some(tag) = git(&["describe", "--tags", "--exact-match", "HEAD"]) {
        return tag.trim_start_matches('v').to_string();
    }

    // Off-tag (or dirty). Build a pre-release of the next patch so the result
    // sorts above the latest tag but below the next planned release.
    let latest_tag = git(&["describe", "--tags", "--abbrev=0"]);
    let base = latest_tag
        .as_deref()
        .map(|t| t.trim_start_matches('v').to_string())
        .unwrap_or_else(|| pkg_version.clone());
    let bumped = bump_patch(&base).unwrap_or(base);
    let count = latest_tag
        .as_deref()
        .and_then(|t| git(&["rev-list", &format!("{t}..HEAD"), "--count"]))
        .unwrap_or_else(|| "0".to_string());
    let dirty_suffix = if dirty { ".dirty" } else { "" };

    format!("{bumped}-dev.{count}+g{head_sha}{dirty_suffix}")
}

/// Parse `MAJOR.MINOR.PATCH...` and return `MAJOR.MINOR.(PATCH+1)`. Silently
/// ignores any pre-release / build metadata suffix on the input.
fn bump_patch(v: &str) -> Option<String> {
    // Slice off anything past `-` or `+` (pre-release / build metadata).
    let core_end = v.find(['-', '+']).unwrap_or(v.len());
    let core = &v[..core_end];
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    let patch: u64 = parts.next()?.parse().ok()?;
    Some(format!("{major}.{minor}.{}", patch + 1))
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
