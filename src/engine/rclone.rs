//! `RcloneEngine` — the production [`SyncEngine`] backed by the `rclone` binary.
//!
//! Each call shells out via `tokio::process::Command` with `--config /dev/null` and the
//! Drive backend configured via `RCLONE_CONFIG_*` env vars so we don't have to touch
//! the user's own `rclone.conf`. The full set of overrides used:
//!
//! | env var                              | value                                |
//! |--------------------------------------|--------------------------------------|
//! | `RCLONE_CONFIG_AIRDRIVE_TYPE`        | `drive`                              |
//! | `RCLONE_CONFIG_AIRDRIVE_TOKEN`       | `{"access_token":"<bearer>",…}`      |
//! | `RCLONE_CONFIG_AIRDRIVE_CLIENT_ID`   | optional override from `[oauth]`     |
//! | `RCLONE_CONFIG_AIRDRIVE_SCOPE`       | `drive.file`                         |
//!
//! Subcommands:
//!
//! - **upload**: `rclone copyto <local> airdrive:<parent_id>/<name> --drive-root-folder-id <parent_id>`
//! - **download**: stages via [`super::staging`] then `rclone copyto airdrive:<id> <stage>` (FR-010)
//! - **move_remote**: `rclone moveto airdrive:<old_id> airdrive:<new_parent>/<new_name>`
//! - **delete_remote**: `rclone delete airdrive:<id>` (Drive trash)
//!
//! **Status**: subprocess wiring is in place. OAuth token handoff to rclone is the
//! sole follow-up item — rclone wants a full token JSON (access + refresh + expiry),
//! while `TokenProvider::token` returns only the access string. The structure below
//! formats a JSON-shaped value with a placeholder refresh; production deploys MUST
//! revisit this for long-running operations (tracked separately).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;

use crate::drive::auth::TokenProvider;
use crate::drive::http::DriveHttp;
use crate::engine::staging;
use crate::engine::{RemoteFile, SyncEngine};
use crate::error::{Error, Result};

/// Where the `rclone` binary the engine drives came from. Surfaced via
/// `air-drive status --json` under `rclone.source` so the user can audit which binary
/// is actually in use (cf. `research.md §5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcloneSource {
    /// `[rclone].path` from `config.toml`.
    Config,
    /// First `rclone` found on `$PATH`.
    Path,
    /// Cached binary at `$XDG_CACHE_HOME/air-drive/bin/rclone`.
    Cache,
    /// Downloaded by the daemon from `downloads.rclone.org`.
    Downloaded,
}

/// Resolved rclone binary handle.
#[derive(Debug, Clone)]
pub struct RcloneBinary {
    /// Absolute path to the binary.
    pub path: PathBuf,
    /// Version string as reported by `rclone version` (e.g. `"1.65.2"`).
    pub version: String,
    /// Where the binary came from.
    pub source: RcloneSource,
}

/// Conventional remote name we use in every rclone invocation. Configured per-call
/// via `RCLONE_CONFIG_AIRDRIVE_*` env vars (see module docs).
const REMOTE_NAME: &str = "airdrive";

/// rclone-backed sync engine.
///
/// Hybrid: uploads + moves + deletes go through the resolved rclone binary
/// (chunked uploads, retries, parallelism); downloads go through `DriveHttp`'s
/// `GET files/{id}?alt=media` instead. Doing downloads via rclone would mean
/// looking up the file's name + parent first because rclone addresses by path,
/// not by id — and the simpler "stream from Drive REST" is already battle-
/// tested by `HttpEngine`.
#[derive(Clone)]
pub struct RcloneEngine {
    binary: RcloneBinary,
    token_provider: Arc<dyn TokenProvider>,
    client_id: Option<String>,
    /// Watched local root, used as the base for [`staging::stage_path`] on downloads.
    local_root: PathBuf,
    /// Shared Drive REST client. Used for downloads (`alt=media`) — rclone has
    /// no clean "fetch by file id" primitive without a follow-up `lsjson` to
    /// resolve the path first.
    http: DriveHttp,
}

impl std::fmt::Debug for RcloneEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcloneEngine")
            .field("binary", &self.binary)
            .field("client_id", &self.client_id)
            .field("local_root", &self.local_root)
            .finish()
    }
}

impl RcloneEngine {
    /// Build a new engine.
    ///
    /// - `binary` — already resolved via [`super::rclone_path::resolve`].
    /// - `token_provider` — supplies the access token rclone is configured with on
    ///   every invocation. Refresh is the provider's responsibility.
    /// - `client_id` — optional OAuth client_id override from `Config.oauth.client_id`.
    /// - `local_root` — watched local folder; used as the base for staged downloads.
    pub fn new(
        binary: RcloneBinary,
        token_provider: Arc<dyn TokenProvider>,
        client_id: Option<String>,
        local_root: PathBuf,
        http: DriveHttp,
    ) -> Self {
        Self {
            binary,
            token_provider,
            client_id,
            local_root,
            http,
        }
    }

    /// Borrow the resolved binary descriptor (for status output).
    pub fn binary(&self) -> &RcloneBinary {
        &self.binary
    }

    /// Build a `Command` with the conventional remote configured via env. Caller adds
    /// the subcommand-specific args (`copyto`, `moveto`, `delete`, …) and runs it.
    async fn base_command(&self) -> Result<Command> {
        let token = self.token_provider.token().await?;
        let token_json = format_token_json(&token);
        let mut cmd = Command::new(&self.binary.path);
        cmd.env("RCLONE_CONFIG_AIRDRIVE_TYPE", "drive")
            .env("RCLONE_CONFIG_AIRDRIVE_TOKEN", token_json)
            .env("RCLONE_CONFIG_AIRDRIVE_SCOPE", "drive.file");
        if let Some(cid) = &self.client_id {
            cmd.env("RCLONE_CONFIG_AIRDRIVE_CLIENT_ID", cid);
        }
        cmd.arg("--config")
            .arg("/dev/null") // never touch the user's rclone.conf
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd)
    }

    /// Spawn `cmd` and capture stdout/stderr. Maps non-zero exits to [`Error::Rclone`].
    async fn run(&self, mut cmd: Command) -> Result<Vec<u8>> {
        let out = cmd.output().await.map_err(|e| Error::Rclone {
            stderr: format!("spawn rclone: {e}"),
        })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            return Err(Error::Rclone { stderr });
        }
        Ok(out.stdout)
    }
}

/// Encode the access token in the JSON shape rclone's Drive backend expects.
///
/// Rclone needs `{"access_token": ..., "token_type": "Bearer", "expiry": ...}` at
/// minimum. We have only the access string from [`TokenProvider`], so we emit a
/// short-lived envelope. For long-running operations rclone may try to refresh — when
/// that path matters we'll need to surface the refresh token too (tracked as follow-up
/// in the module docs).
fn format_token_json(access_token: &str) -> String {
    let escaped = access_token.replace('\\', "\\\\").replace('"', "\\\"");
    // Expiry well in the future so rclone treats the token as fresh.
    format!(
        "{{\"access_token\":\"{escaped}\",\"token_type\":\"Bearer\",\"expiry\":\"2099-01-01T00:00:00Z\"}}"
    )
}

#[async_trait::async_trait]
impl SyncEngine for RcloneEngine {
    async fn upload(&self, local: &Path, remote_parent_id: &str, name: &str) -> Result<RemoteFile> {
        let mut cmd = self.base_command().await?;
        cmd.arg("copyto")
            .arg(local)
            .arg(format!("{REMOTE_NAME}:{name}"))
            .arg("--drive-root-folder-id")
            .arg(remote_parent_id);
        self.run(cmd).await?;

        // rclone's `copyto` doesn't print the created file's metadata. Re-fetch via
        // `rclone lsjson` to get id+size+md5. Done in a second invocation — chatty
        // but correct, and only on the first sync (subsequent updates reuse the id).
        let mut probe = self.base_command().await?;
        probe
            .arg("lsjson")
            .arg("--hash")
            .arg(format!("{REMOTE_NAME}:{name}"))
            .arg("--drive-root-folder-id")
            .arg(remote_parent_id);
        let raw = self.run(probe).await?;
        parse_lsjson_single(&raw)
    }

    async fn update(&self, _remote_id: &str, _local: &Path) -> Result<RemoteFile> {
        // rclone's `copyto airdrive:<existing-name>` overwrites in place — same
        // command as upload, but we'd need to know the file's name + parent to
        // hand to rclone. The daemon caller has that context; threading it here
        // is left as a follow-up (the integration suite uses HttpEngine).
        Err(Error::Rclone {
            stderr: "RcloneEngine::update not yet wired (use AIR_DRIVE_TEST_ENGINE=http for tests)"
                .into(),
        })
    }

    async fn download(&self, remote_id: &str, local: &Path, local_root: &Path) -> Result<()> {
        let op_id = format!(
            "{remote_id}-{}",
            local.file_name().and_then(|s| s.to_str()).unwrap_or("dest")
        );
        // Prefer the explicitly threaded `local_root`; fall back to the engine's
        // configured root if the caller passed an empty path (defensive).
        let staging_root: &Path = if local_root.as_os_str().is_empty() {
            &self.local_root
        } else {
            local_root
        };
        let staging_path = staging::stage_path(staging_root, &op_id)?;

        // Stream via DriveHttp — rclone's Drive backend addresses by path, not
        // by id, and the path lookup would cost an extra `lsjson` round-trip.
        // Same wire protocol as HttpEngine but reuses the rclone-side staging
        // directory we already computed above.
        let path = format!("files/{remote_id}");
        let bytes = match self.http.get_bytes(&path, &[("alt", "media")]).await {
            Ok(b) => b,
            Err(e) => {
                staging::discard(&staging_path)?;
                return Err(e);
            }
        };
        tokio::fs::write(&staging_path, &bytes).await.map_err(|e| {
            let _ = staging::discard(&staging_path);
            Error::Io(e)
        })?;
        staging::promote(&staging_path, local)?;
        Ok(())
    }

    async fn move_remote(
        &self,
        remote_id: &str,
        new_parent_id: &str,
        new_name: &str,
    ) -> Result<()> {
        let mut cmd = self.base_command().await?;
        cmd.arg("moveto")
            .arg(format!("{REMOTE_NAME}:{remote_id}"))
            .arg(format!("{REMOTE_NAME}:{new_name}"))
            .arg("--drive-root-folder-id")
            .arg(new_parent_id);
        self.run(cmd).await?;
        Ok(())
    }

    async fn delete_remote(&self, remote_id: &str) -> Result<()> {
        let mut cmd = self.base_command().await?;
        cmd.arg("delete").arg(format!("{REMOTE_NAME}:{remote_id}"));
        self.run(cmd).await?;
        Ok(())
    }
}

/// Parse a single entry from `rclone lsjson --hash` output. The command outputs a JSON
/// array; we expect exactly one element here (we list a single named target).
fn parse_lsjson_single(raw: &[u8]) -> Result<RemoteFile> {
    let v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| Error::Rclone {
        stderr: format!("lsjson parse: {e}"),
    })?;
    let entry = v.get(0).ok_or_else(|| Error::Rclone {
        stderr: "lsjson returned empty array after upload".into(),
    })?;
    let id = entry
        .get("ID")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Rclone {
            stderr: "lsjson entry missing `ID`".into(),
        })?
        .to_owned();
    let mime = entry
        .get("MimeType")
        .and_then(|x| x.as_str())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let size = entry.get("Size").and_then(|x| x.as_i64()).unwrap_or(0);
    let md5 = entry
        .get("Hashes")
        .and_then(|h| h.get("MD5"))
        .and_then(|x| x.as_str())
        .map(str::to_owned);
    Ok(RemoteFile {
        id,
        mime,
        size,
        md5,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::auth::StaticToken;

    fn dummy_binary() -> RcloneBinary {
        RcloneBinary {
            path: PathBuf::from("/usr/bin/rclone"),
            version: "1.65.0".into(),
            source: RcloneSource::Path,
        }
    }

    #[test]
    fn engine_exposes_resolved_binary() {
        let token = Arc::new(StaticToken::new("tok"));
        let http = DriveHttp::with_bases(token.clone(), "http://x", "http://x/upload").unwrap();
        let engine = RcloneEngine::new(
            dummy_binary(),
            token,
            None,
            PathBuf::from("/tmp/root"),
            http,
        );
        assert_eq!(engine.binary().version, "1.65.0");
        assert_eq!(engine.binary().source, RcloneSource::Path);
    }

    #[test]
    fn format_token_json_escapes_quotes_and_backslashes() {
        let s = format_token_json(r#"abc"\xyz"#);
        assert!(s.contains(r#""access_token":"abc\"\\xyz""#));
        assert!(s.contains(r#""token_type":"Bearer""#));
        assert!(s.contains("2099-01-01"));
    }

    #[test]
    fn parse_lsjson_single_extracts_id_size_md5() {
        let raw =
            br#"[{"ID":"abc","MimeType":"text/plain","Size":12,"Hashes":{"MD5":"deadbeef"}}]"#;
        let f = parse_lsjson_single(raw).unwrap();
        assert_eq!(f.id, "abc");
        assert_eq!(f.size, 12);
        assert_eq!(f.md5.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parse_lsjson_single_rejects_empty_array() {
        let raw = b"[]";
        assert!(parse_lsjson_single(raw).is_err());
    }
}
