//! Resolution step 4: fetch a verified `rclone` binary into the cache.
//!
//! rclone publishes per-version release archives under `downloads.rclone.org`, plus a
//! `SHA256SUMS` manifest in the same directory. We:
//!
//! 1. Map the host target triple to rclone's `os`/`arch` tokens.
//! 2. Fetch `v<VER>/SHA256SUMS` over HTTPS and read the expected hash for our archive.
//! 3. Fetch `v<VER>/rclone-v<VER>-<os>-<arch>.zip` and **reject** it unless its SHA-256
//!    matches the manifest line.
//! 4. Extract the single `rclone` binary from the archive and write it atomically into
//!    `<cache>/bin/rclone` (a temp file in the same dir, then `rename`), `chmod 0755`.
//!
//! The version is **pinned** (not `current`) so the download is reproducible and the
//! SHA-256 check is deterministic; bump [`RCLONE_VERSION`] deliberately. GPG signature
//! verification of `SHA256SUMS` is a possible future hardening — out of scope here,
//! where the contract is HTTPS + per-archive SHA-256.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::engine::rclone_path::CACHE_BIN_SUBPATH;
use crate::error::{Error, Result};

/// Pinned rclone release to download. Bump deliberately (the SHA-256 check keys off the
/// matching `SHA256SUMS` manifest, so a floating "current" would not be reproducible).
pub const RCLONE_VERSION: &str = "1.74.3";

/// Base of the official rclone download server.
const DOWNLOADS_BASE: &str = "https://downloads.rclone.org";

/// Fetch, verify, and cache the pinned rclone binary. Returns the path to the cached
/// binary (`<cache_dir>/bin/rclone`).
pub async fn download_to_cache(cache_dir: &Path) -> Result<PathBuf> {
    let (os, arch) = target_tokens()?;
    let archive = archive_file_name(os, arch);
    let base = format!("{DOWNLOADS_BASE}/v{RCLONE_VERSION}");

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| rclone_err(format!("HTTP client init: {e}")))?;

    // 1. Expected SHA-256 from the version's manifest.
    let sums = http_text(&client, &format!("{base}/SHA256SUMS")).await?;
    let expected = sha256_for(&sums, &archive).ok_or_else(|| {
        rclone_err(format!(
            "SHA256SUMS for rclone v{RCLONE_VERSION} has no entry for {archive}"
        ))
    })?;

    // 2. Download the archive and verify it before trusting a single byte of content.
    let bytes = http_bytes(&client, &format!("{base}/{archive}")).await?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(rclone_err(format!(
            "SHA-256 mismatch for {archive}: expected {expected}, got {actual}. \
             Refusing to cache a corrupt or tampered download."
        )));
    }

    // 3. Extract the binary and install it atomically.
    let binary_name = binary_name(os);
    let bin = extract_binary(&bytes, binary_name)?;
    let dest = cache_dir.join(CACHE_BIN_SUBPATH);
    install_binary(&dest, &bin)?;
    Ok(dest)
}

/// rclone's `(os, arch)` tokens for the host, or an error on an unsupported target.
fn target_tokens() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "osx",
        "windows" => "windows",
        other => {
            return Err(rclone_err(format!(
                "no rclone auto-download mapping for OS {other:?}; \
                 install rclone manually or set [rclone].path"
            )));
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        other => {
            return Err(rclone_err(format!(
                "no rclone auto-download mapping for architecture {other:?}; \
                 install rclone manually or set [rclone].path"
            )));
        }
    };
    Ok((os, arch))
}

/// `rclone-v1.74.3-linux-amd64` — shared by the archive name and the in-zip directory.
fn archive_stem(os: &str, arch: &str) -> String {
    format!("rclone-v{RCLONE_VERSION}-{os}-{arch}")
}

/// `rclone-v1.74.3-linux-amd64.zip`.
fn archive_file_name(os: &str, arch: &str) -> String {
    format!("{}.zip", archive_stem(os, arch))
}

/// Name of the binary inside the archive (`rclone.exe` on Windows).
fn binary_name(os: &str) -> &'static str {
    if os == "windows" {
        "rclone.exe"
    } else {
        "rclone"
    }
}

/// Find the lowercase hex SHA-256 for `file_name` in a `SHA256SUMS` body. Each line is
/// `<64-hex><whitespace><name>`; the name may be bare or prefixed with `*` (binary mode).
fn sha256_for(sums: &str, file_name: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim_start().trim_start_matches('*').trim();
        if name == file_name && is_hex_sha256(hash) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Lowercase hex of the SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A 64-char ASCII-hex string.
fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Pull the `rclone` binary out of the in-memory zip. The archive lays the binary out as
/// `rclone-v<VER>-<os>-<arch>/<binary_name>`; we match on the trailing path component so
/// we don't have to reconstruct the exact directory name.
fn extract_binary(zip_bytes: &[u8], binary_name: &str) -> Result<Vec<u8>> {
    let reader = Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| rclone_err(format!("rclone archive is not a valid zip: {e}")))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| rclone_err(format!("reading zip entry {i}: {e}")))?;
        if !entry.is_file() {
            continue;
        }
        let matches = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|leaf| leaf == binary_name);
        if matches {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .map_err(|e| rclone_err(format!("extracting {binary_name} from zip: {e}")))?;
            return Ok(buf);
        }
    }
    Err(rclone_err(format!(
        "rclone archive does not contain a {binary_name} entry"
    )))
}

/// Write `bytes` to `dest` atomically: a sibling temp file, fsync-free, then `rename`.
/// Creates the parent dir, and on Unix marks the binary `0755`.
fn install_binary(dest: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("download");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    set_executable(&tmp)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// GET a URL and return its body as text, mapping transport/status errors into
/// [`Error::Rclone`] (this is part of resolving the rclone binary).
async fn http_text(client: &reqwest::Client, url: &str) -> Result<String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| rclone_err(format!("GET {url}: {e}")))?
        .error_for_status()
        .map_err(|e| rclone_err(format!("GET {url}: {e}")))?
        .text()
        .await
        .map_err(|e| rclone_err(format!("reading body of {url}: {e}")))
}

/// GET a URL and return its body as bytes.
async fn http_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let bytes = client
        .get(url)
        .send()
        .await
        .map_err(|e| rclone_err(format!("GET {url}: {e}")))?
        .error_for_status()
        .map_err(|e| rclone_err(format!("GET {url}: {e}")))?
        .bytes()
        .await
        .map_err(|e| rclone_err(format!("reading body of {url}: {e}")))?;
    Ok(bytes.to_vec())
}

/// Build an [`Error::Rclone`] from a message.
fn rclone_err(stderr: String) -> Error {
    Error::Rclone { stderr }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_tokens_resolves_current_host() {
        // On every platform this test suite runs on (Linux/macOS x86_64 or arm64), the
        // host must map to a known token pair rather than erroring.
        let res = target_tokens();
        if cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        )) {
            let (os, arch) = res.expect("supported host should map");
            assert!(["linux", "osx", "windows"].contains(&os));
            assert!(["amd64", "arm64", "386", "arm"].contains(&arch));
        }
    }

    #[test]
    fn archive_names_follow_rclone_layout() {
        assert_eq!(
            archive_file_name("linux", "amd64"),
            format!("rclone-v{RCLONE_VERSION}-linux-amd64.zip")
        );
        assert_eq!(binary_name("linux"), "rclone");
        assert_eq!(binary_name("windows"), "rclone.exe");
    }

    #[test]
    fn sha256_for_finds_matching_line() {
        let sums = "\
0000000000000000000000000000000000000000000000000000000000000000  rclone-v1.74.3-linux-arm64.zip
dbee7ccd7a5d617e4ed4cd4555c16669b511abfe8d31164f61be35ac9e999bd2  rclone-v1.74.3-linux-amd64.zip
";
        assert_eq!(
            sha256_for(sums, "rclone-v1.74.3-linux-amd64.zip").as_deref(),
            Some("dbee7ccd7a5d617e4ed4cd4555c16669b511abfe8d31164f61be35ac9e999bd2")
        );
        assert_eq!(sha256_for(sums, "rclone-v1.74.3-osx-amd64.zip"), None);
    }

    #[test]
    fn sha256_for_handles_binary_mode_star() {
        let sums = "abc0000000000000000000000000000000000000000000000000000000000def *rclone-v1.74.3-windows-amd64.zip";
        assert_eq!(
            sha256_for(sums, "rclone-v1.74.3-windows-amd64.zip").as_deref(),
            Some("abc0000000000000000000000000000000000000000000000000000000000def")
        );
    }

    #[test]
    fn sha256_for_rejects_non_hash_lines() {
        // Garbage where the hash should be must not match even if the name lines up.
        let sums = "not-a-hash  rclone-v1.74.3-linux-amd64.zip";
        assert_eq!(sha256_for(sums, "rclone-v1.74.3-linux-amd64.zip"), None);
    }

    #[test]
    fn sha256_hex_of_empty_is_known_constant() {
        // SHA-256 of the empty input is a well-known vector.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Build a minimal zip laying the binary out the way rclone does:
    /// `rclone-v.../<binary_name>` plus an unrelated sibling file.
    fn make_zip(binary_name: &str, payload: &[u8]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file("rclone-v1.74.3-linux-amd64/README.txt", opts)
                .unwrap();
            zw.write_all(b"not the binary").unwrap();
            zw.start_file(format!("rclone-v1.74.3-linux-amd64/{binary_name}"), opts)
                .unwrap();
            zw.write_all(payload).unwrap();
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extract_binary_picks_the_right_entry() {
        let payload = b"#!/bin/sh\necho rclone\n";
        let zip_bytes = make_zip("rclone", payload);
        let got = extract_binary(&zip_bytes, "rclone").expect("binary should extract");
        assert_eq!(got, payload);
    }

    #[test]
    fn extract_binary_errors_when_absent() {
        let zip_bytes = make_zip("rclone", b"x");
        let err = extract_binary(&zip_bytes, "rclone.exe").unwrap_err();
        match err {
            Error::Rclone { stderr } => assert!(stderr.contains("does not contain")),
            other => panic!("expected Rclone error, got {other:?}"),
        }
    }

    #[test]
    fn extract_binary_rejects_non_zip() {
        let err = extract_binary(b"definitely not a zip", "rclone").unwrap_err();
        assert!(matches!(err, Error::Rclone { .. }));
    }

    #[test]
    fn install_binary_writes_and_marks_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bin/rclone");
        install_binary(&dest, b"payload").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // No leftover temp file beside the destination.
        assert!(!dest.with_extension("download").exists());
    }

    /// Live end-to-end: actually fetch the pinned rclone from `downloads.rclone.org`,
    /// verify the SHA-256, extract, and confirm the cached binary reports a version.
    /// `#[ignore]`d so the default `cargo test` stays hermetic (no network). Run with:
    /// `cargo test --lib downloads_and_verifies_real_rclone -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the network (downloads.rclone.org)"]
    async fn downloads_and_verifies_real_rclone() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = download_to_cache(tmp.path())
            .await
            .expect("real rclone download + SHA-256 verify should succeed");
        assert!(dest.is_file(), "binary should be cached at {dest:?}");
        let version = crate::engine::rclone_path::probe_version(&dest)
            .await
            .expect("cached binary should report a version");
        assert_eq!(
            version, RCLONE_VERSION,
            "downloaded binary should be the pinned version"
        );
    }
}
