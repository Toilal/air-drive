//! `RcloneEngine` — the production [`SyncEngine`] backed by the `rclone` binary.
//!
//! Each call shells out via `tokio::process::Command` with `--config /dev/null` and the
//! Drive backend configured via `RCLONE_CONFIG_*` env vars so we don't have to touch
//! the user's own `rclone.conf`. The full set of overrides used:
//!
//! | env var                              | value                                |
//! |--------------------------------------|--------------------------------------|
//! | `RCLONE_CONFIG_AIRDRIVE_TYPE`          | `drive`                              |
//! | `RCLONE_CONFIG_AIRDRIVE_TOKEN`         | `{"access_token":…,"refresh_token":…,"expiry":…}` |
//! | `RCLONE_CONFIG_AIRDRIVE_CLIENT_ID`     | optional override from `[oauth]`     |
//! | `RCLONE_CONFIG_AIRDRIVE_CLIENT_SECRET` | optional override from `[oauth]`     |
//! | `RCLONE_CONFIG_AIRDRIVE_SCOPE`         | `drive`                              |
//!
//! Subcommands:
//!
//! - **upload**: `rclone copyto <local> airdrive:<parent_id>/<name> --drive-root-folder-id <parent_id>`
//! - **download**: stages via [`super::staging`] then `rclone copyto airdrive:<id> <stage>`
//! - **move_remote**: Drive `files.update` via `DriveHttp` (rename + reparent) —
//!   a metadata-only op; rclone addresses Drive by path, not id, so `moveto`
//!   cannot locate a source given only its id.
//! - **delete_remote**: `rclone delete airdrive:<id>` (Drive trash)
//!
//! **Token handoff**: [`TokenProvider::rclone_token`] supplies the access token, its
//! real expiry, and — when available — the refresh token. With the refresh token plus
//! `client_id` + `client_secret` from `[oauth]`, rclone can refresh on its own during a
//! single long-running transfer instead of failing with `401` when the access token
//! expires mid-operation (issue #5). `--config /dev/null` is kept: rclone refreshes
//! in-memory and discards the write-back, since yup-oauth2 owns the canonical token.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::drive::auth::{RcloneToken, TokenProvider};
use crate::drive::http::DriveHttp;
use crate::engine::staging;
use crate::engine::{BulkDownload, BulkUpload, RemoteFile, SyncEngine};
use crate::error::{Error, Result};

/// Where the `rclone` binary the engine drives came from. Surfaced via
/// `air-drive status --json` under `rclone.source` so the user can audit which binary
/// is actually in use.
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
/// Every transfer goes through the rclone binary so we get its chunked /
/// parallel / resumable machinery for both directions. For downloads we have
/// to do a one-shot `files.get` against DriveHttp first because rclone's
/// Drive backend addresses by path, not by id, and we want to fetch by id;
/// the lookup gives us the file's name + parent id, which we then plug into
/// `rclone copyto airdrive:<name> <staging> --drive-root-folder-id <parent>`.
#[derive(Clone)]
pub struct RcloneEngine {
    binary: RcloneBinary,
    token_provider: Arc<dyn TokenProvider>,
    client_id: Option<String>,
    /// OAuth `client_secret` from `[oauth]`. Passed to rclone alongside `client_id`
    /// so it can refresh the access token itself during a long transfer. `None`
    /// (embedded client, no override) means rclone cannot self-refresh — see #180.
    client_secret: Option<String>,
    /// Watched local root, used as the base for [`staging::stage_path`] on downloads.
    local_root: PathBuf,
    /// Shared Drive REST client. Used for the metadata lookup that precedes
    /// every `download` (rclone Drive addresses by path; we have the id).
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
    /// - `client_secret` — companion `client_secret` from `Config.oauth.client_secret`,
    ///   passed to rclone so it can self-refresh the token during long transfers.
    /// - `local_root` — watched local folder; used as the base for staged downloads.
    pub fn new(
        binary: RcloneBinary,
        token_provider: Arc<dyn TokenProvider>,
        client_id: Option<String>,
        client_secret: Option<String>,
        local_root: PathBuf,
        http: DriveHttp,
    ) -> Self {
        Self {
            binary,
            token_provider,
            client_id,
            client_secret,
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
        let creds = self.token_provider.rclone_token().await?;
        // Without client credentials rclone can't refresh even when it has a refresh
        // token — it would fall back to its own built-in client, which doesn't match
        // our token. This is the embedded-client case (see #180); flag it for the long
        // transfers it affects without spamming every invocation at a higher level.
        if creds.refresh_token.is_some() && self.client_secret.is_none() {
            tracing::debug!(
                "rclone has a refresh token but no client_secret; it cannot self-refresh \
                 (set [oauth].client_id + client_secret) — long transfers may hit 401"
            );
        }
        let token_json = format_token_json(&creds);
        let mut cmd = Command::new(&self.binary.path);
        cmd.env("RCLONE_CONFIG_AIRDRIVE_TYPE", "drive")
            .env("RCLONE_CONFIG_AIRDRIVE_TOKEN", token_json)
            .env("RCLONE_CONFIG_AIRDRIVE_SCOPE", "drive");
        if let Some(cid) = &self.client_id {
            cmd.env("RCLONE_CONFIG_AIRDRIVE_CLIENT_ID", cid);
        }
        if let Some(cs) = &self.client_secret {
            cmd.env("RCLONE_CONFIG_AIRDRIVE_CLIENT_SECRET", cs);
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

    /// Spawn `cmd` for a long-running bulk transfer and **stream** its stderr to
    /// the `rclone` tracing target line by line (rclone writes `--stats` /
    /// `-v` progress to stderr), so the operator sees live progress at `info`
    /// instead of a silent multi-minute pause. The last few stderr lines are
    /// retained and folded into [`Error::Rclone`] on a non-zero exit.
    async fn run_streaming(&self, mut cmd: Command) -> Result<()> {
        let mut child = cmd.spawn().map_err(|e| Error::Rclone {
            stderr: format!("spawn rclone: {e}"),
        })?;
        // Keep a small tail of stderr so a failure carries actionable context
        // even though we've already streamed the lines out to the log.
        let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        if let Some(stderr) = child.stderr.take() {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(line) = lines.next_line().await.map_err(Error::Io)? {
                tracing::info!(target: "rclone", "{line}");
                tail.push_back(line);
                if tail.len() > 20 {
                    tail.pop_front();
                }
            }
        }
        let status = child.wait().await.map_err(|e| Error::Rclone {
            stderr: format!("wait rclone: {e}"),
        })?;
        if !status.success() {
            return Err(Error::Rclone {
                stderr: tail.into_iter().collect::<Vec<_>>().join("\n"),
            });
        }
        Ok(())
    }

    /// Write a newline-delimited `--files-from` list under the local staging dir
    /// and return its path. The caller is responsible for discarding it.
    fn write_files_from(local_root: &Path, rel_paths: &[&str]) -> Result<PathBuf> {
        let path = staging::stage_path(local_root, "bulk-files-from.txt")?;
        let body = rel_paths.join("\n");
        std::fs::write(&path, body)?;
        Ok(path)
    }

    /// Append the bootstrap-bulk flags shared by both directions: the
    /// `--files-from` set, bounded parallelism, and streamed one-line progress
    /// stats. The `copy` subcommand and the src/dst operands are added by the
    /// caller *before* this — rclone parses the first positional as the
    /// subcommand, so `copy` MUST come first. Empty directories are propagated by
    /// the reconciler's directory pass, not here, so `--create-empty-src-dirs`
    /// is intentionally omitted (it would risk duplicate Drive folders).
    fn add_bulk_flags(cmd: &mut Command, files_from: &Path) {
        cmd.arg("--files-from")
            .arg(files_from)
            // Follow symlinks so a `[watch].symlinks = follow` tree uploads link
            // targets' bytes during the initial bulk copy. Harmless when the
            // `--files-from` set contains no links (the `skip` default).
            .arg("--copy-links")
            .arg("--transfers")
            .arg("8")
            .arg("--checkers")
            .arg("16")
            .arg("--stats")
            .arg("1s")
            .arg("--stats-one-line")
            .arg("-v");
    }
}

/// Placeholder expiry used when the provider can't supply a real one (e.g.
/// [`crate::drive::auth::StaticToken`] in tests). Far enough out that rclone treats
/// the token as fresh — the legacy behaviour.
const FAR_FUTURE_EXPIRY: &str = "2099-01-01T00:00:00Z";

/// Encode the credentials in the JSON shape rclone's Drive backend expects:
/// `{"access_token": ..., "token_type": "Bearer", "expiry": ..., "refresh_token": ...}`.
///
/// The `expiry` is the token's **real** RFC 3339 expiry when known, so rclone refreshes
/// itself exactly when needed during a long transfer; it falls back to a far-future
/// placeholder otherwise. `refresh_token` is included only when available — with it (and
/// `client_id` + `client_secret`) rclone can self-refresh; without it, only the access
/// token's remaining lifetime is usable. Built with `serde_json` so string escaping is
/// handled correctly.
fn format_token_json(creds: &RcloneToken) -> String {
    let expiry = creds
        .expiry_rfc3339
        .clone()
        .unwrap_or_else(|| FAR_FUTURE_EXPIRY.to_owned());
    let mut map = serde_json::Map::new();
    map.insert(
        "access_token".to_owned(),
        serde_json::Value::String(creds.access_token.clone()),
    );
    map.insert(
        "token_type".to_owned(),
        serde_json::Value::String("Bearer".to_owned()),
    );
    map.insert("expiry".to_owned(), serde_json::Value::String(expiry));
    if let Some(rt) = &creds.refresh_token {
        map.insert(
            "refresh_token".to_owned(),
            serde_json::Value::String(rt.clone()),
        );
    }
    serde_json::Value::Object(map).to_string()
}

#[async_trait::async_trait]
impl SyncEngine for RcloneEngine {
    async fn upload(&self, local: &Path, remote_parent_id: &str, name: &str) -> Result<RemoteFile> {
        let mut cmd = self.base_command().await?;
        cmd.arg("copyto")
            .arg(local)
            // Follow symlinks so a `[watch].symlinks = follow` source uploads the
            // target's bytes. A no-op for regular files, and the watcher / walker
            // only ever hand us paths it already decided to sync, so this never
            // pulls in a link we meant to skip.
            .arg("--copy-links")
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

    async fn update(&self, remote_id: &str, local: &Path) -> Result<RemoteFile> {
        // Same dance as `download`: look up name + parent via a cheap
        // files.get call, then let rclone address the file by path (the only
        // form its Drive backend accepts) and overwrite in place via copyto.
        let meta =
            crate::drive::metadata::get_file_raw(&self.http, remote_id, "id,name,parents").await?;
        let name = meta
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Drive(format!("file {remote_id} has no `name`")))?
            .to_owned();
        let parent_id = meta
            .get("parents")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.as_str())
            .ok_or_else(|| Error::Drive(format!("file {remote_id} has no `parents`")))?
            .to_owned();

        let mut cmd = self.base_command().await?;
        cmd.arg("copyto")
            .arg(local)
            // Follow symlinks (see `upload`) so a followed symlink's edits propagate.
            .arg("--copy-links")
            .arg(format!("{REMOTE_NAME}:{name}"))
            .arg("--drive-root-folder-id")
            .arg(&parent_id);
        self.run(cmd).await?;

        // Re-fetch size + md5 from rclone so the caller can refresh the cached
        // fingerprint without an extra Drive API round-trip.
        let mut probe = self.base_command().await?;
        probe
            .arg("lsjson")
            .arg("--hash")
            .arg(format!("{REMOTE_NAME}:{name}"))
            .arg("--drive-root-folder-id")
            .arg(&parent_id);
        let raw = self.run(probe).await?;
        parse_lsjson_single(&raw)
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

        // Look up name + parent so we can address the file the way rclone wants
        // it (`<remote>:<path>` with `--drive-root-folder-id <folder>`). One
        // cheap GET against `files.get?fields=name,parents` — much cheaper than
        // streaming the bytes through this process when the file is large
        // (rclone's chunked + parallel download takes over for the actual
        // transfer).
        let meta =
            match crate::drive::metadata::get_file_raw(&self.http, remote_id, "id,name,parents")
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    staging::discard(&staging_path)?;
                    return Err(e);
                }
            };
        let name = meta
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Drive(format!("file {remote_id} has no `name`")))?
            .to_owned();
        let parent_id = meta
            .get("parents")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.as_str())
            .ok_or_else(|| Error::Drive(format!("file {remote_id} has no `parents`")))?
            .to_owned();

        let mut cmd = self.base_command().await?;
        cmd.arg("copyto")
            .arg(format!("{REMOTE_NAME}:{name}"))
            .arg(&staging_path)
            .arg("--drive-root-folder-id")
            .arg(&parent_id);
        if let Err(e) = self.run(cmd).await {
            staging::discard(&staging_path)?;
            return Err(e);
        }
        staging::promote(&staging_path, local)?;
        Ok(())
    }

    async fn move_remote(
        &self,
        remote_id: &str,
        new_parent_id: &str,
        new_name: &str,
    ) -> Result<()> {
        // A rename/move is a pure metadata change, so drive it through the Drive
        // API (`files.update`) rather than `rclone moveto`: rclone addresses Drive
        // by path, not id, so given only the object's id it cannot locate the
        // source and fails with "directory not found". This mirrors the HTTP
        // engine and the create/remove-dir paths, which also use `DriveHttp` for
        // metadata-only operations.
        let current =
            crate::drive::metadata::get_file_raw(&self.http, remote_id, "id,parents").await?;
        let old_parents: Vec<String> = current
            .get("parents")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        let mut query: Vec<(&str, &str)> = Vec::new();
        let add = new_parent_id.to_owned();
        let remove = old_parents.join(",");
        let needs_move = !old_parents.iter().any(|p| p == new_parent_id);
        if needs_move {
            query.push(("addParents", &add));
            query.push(("removeParents", &remove));
        }
        let body = serde_json::json!({ "name": new_name });
        self.http
            .patch_json(&format!("files/{remote_id}"), &query, &body)
            .await?;
        Ok(())
    }

    async fn delete_remote(&self, remote_id: &str) -> Result<()> {
        let mut cmd = self.base_command().await?;
        cmd.arg("delete").arg(format!("{REMOTE_NAME}:{remote_id}"));
        self.run(cmd).await?;
        Ok(())
    }

    async fn create_dir_remote(&self, remote_parent_id: &str, name: &str) -> Result<RemoteFile> {
        // Folder create is a metadata op, not a byte transfer — handle it via
        // DriveHttp like the other metadata lookups this engine already does
        // (download/update both call `files.get`), rather than shelling out to
        // `rclone mkdir` (which addresses by path and wouldn't return the id).
        let meta =
            crate::drive::metadata::create_folder(&self.http, remote_parent_id, name).await?;
        Ok(RemoteFile {
            id: meta.id,
            mime: meta.mime_type,
            size: 0,
            md5: None,
        })
    }

    async fn remove_dir_remote(&self, remote_id: &str) -> Result<()> {
        // `rclone delete` removes files, not a directory itself; a Drive
        // `files.delete` by id trashes the (expected-empty) folder directly.
        self.http.delete(&format!("files/{remote_id}")).await
    }

    async fn bulk_download(
        &self,
        items: &[BulkDownload],
        remote_root_id: &str,
        local_root: &Path,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        // rclone addresses Drive by path, so it needs only the relative paths;
        // the Drive ids carried by `items` are for the id-addressed HTTP engine.
        let rel_paths: Vec<&str> = items.iter().map(|i| i.rel_path.as_str()).collect();
        let files_from = Self::write_files_from(local_root, &rel_paths)?;
        let mut cmd = self.base_command().await?;
        // `copy` MUST be the first positional (rclone reads it as the
        // subcommand), then source → destination.
        cmd.arg("copy")
            .arg(format!("{REMOTE_NAME}:"))
            .arg(local_root)
            .arg("--drive-root-folder-id")
            .arg(remote_root_id)
            // Native Google Docs have no downloadable bytes; the reconciler
            // handles them as shortcuts and leaves them out of `items`. Skip
            // them defensively so a stray entry can't trigger an export.
            .arg("--drive-skip-gdocs");
        Self::add_bulk_flags(&mut cmd, &files_from);
        let result = self.run_streaming(cmd).await;
        let _ = staging::discard(&files_from);
        result
    }

    async fn bulk_upload(
        &self,
        items: &[BulkUpload],
        local_root: &Path,
        remote_root_id: &str,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        // rclone recreates the remote folder tree itself from the relative
        // paths, so the per-file `remote_parent_id` is unused here (it serves
        // the id-addressed HTTP engine).
        let rel_paths: Vec<&str> = items.iter().map(|i| i.rel_path.as_str()).collect();
        let files_from = Self::write_files_from(local_root, &rel_paths)?;
        let mut cmd = self.base_command().await?;
        // `copy` MUST be the first positional (rclone reads it as the
        // subcommand), then source → destination.
        cmd.arg("copy")
            .arg(local_root)
            .arg(format!("{REMOTE_NAME}:"))
            .arg("--drive-root-folder-id")
            .arg(remote_root_id);
        Self::add_bulk_flags(&mut cmd, &files_from);
        let result = self.run_streaming(cmd).await;
        let _ = staging::discard(&files_from);
        result
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
            None,
            PathBuf::from("/tmp/root"),
            http,
        );
        assert_eq!(engine.binary().version, "1.65.0");
        assert_eq!(engine.binary().source, RcloneSource::Path);
    }

    fn creds(access: &str, refresh: Option<&str>, expiry: Option<&str>) -> RcloneToken {
        RcloneToken {
            access_token: access.to_owned(),
            refresh_token: refresh.map(str::to_owned),
            expiry_rfc3339: expiry.map(str::to_owned),
        }
    }

    #[test]
    fn format_token_json_escapes_quotes_and_backslashes() {
        let s = format_token_json(&creds(r#"abc"\xyz"#, None, None));
        assert!(s.contains(r#""access_token":"abc\"\\xyz""#));
        assert!(s.contains(r#""token_type":"Bearer""#));
    }

    #[test]
    fn format_token_json_without_expiry_uses_far_future_and_omits_refresh() {
        let s = format_token_json(&creds("tok", None, None));
        assert!(s.contains("2099-01-01"));
        assert!(!s.contains("refresh_token"));
    }

    #[test]
    fn format_token_json_includes_real_expiry_and_refresh_when_present() {
        let s = format_token_json(&creds("tok", Some("rt-123"), Some("2030-06-01T12:00:00Z")));
        assert!(s.contains(r#""expiry":"2030-06-01T12:00:00Z""#));
        assert!(s.contains(r#""refresh_token":"rt-123""#));
        assert!(!s.contains("2099-01-01"));
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

    /// Token provider that returns a full credential bundle (refresh token + real
    /// expiry), to exercise the rclone self-refresh handoff in `base_command`.
    struct FullToken;

    #[async_trait::async_trait]
    impl TokenProvider for FullToken {
        async fn token(&self) -> Result<String> {
            Ok("acc".to_owned())
        }
        async fn rclone_token(&self) -> Result<RcloneToken> {
            Ok(RcloneToken {
                access_token: "acc".to_owned(),
                refresh_token: Some("rt-xyz".to_owned()),
                expiry_rfc3339: Some("2030-01-01T00:00:00Z".to_owned()),
            })
        }
    }

    /// Collect a command's environment into a map of UTF-8 key/value pairs.
    fn env_map(cmd: &Command) -> std::collections::HashMap<String, String> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v?.to_str()?.to_owned())))
            .collect()
    }

    fn test_http() -> DriveHttp {
        DriveHttp::with_bases(
            Arc::new(StaticToken::new("x")),
            "http://x",
            "http://x/upload",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn base_command_injects_client_secret_and_full_token() {
        let engine = RcloneEngine::new(
            dummy_binary(),
            Arc::new(FullToken),
            Some("cid".to_owned()),
            Some("csecret".to_owned()),
            PathBuf::from("/tmp/root"),
            test_http(),
        );
        let cmd = engine.base_command().await.unwrap();
        let env = env_map(&cmd);
        assert_eq!(
            env.get("RCLONE_CONFIG_AIRDRIVE_CLIENT_ID")
                .map(String::as_str),
            Some("cid")
        );
        assert_eq!(
            env.get("RCLONE_CONFIG_AIRDRIVE_CLIENT_SECRET")
                .map(String::as_str),
            Some("csecret")
        );
        let token = env.get("RCLONE_CONFIG_AIRDRIVE_TOKEN").unwrap();
        assert!(token.contains(r#""refresh_token":"rt-xyz""#), "{token}");
        assert!(
            token.contains(r#""expiry":"2030-01-01T00:00:00Z""#),
            "{token}"
        );
    }

    #[tokio::test]
    async fn create_and_remove_dir_remote_go_through_http_not_rclone() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // RcloneEngine's directory methods are metadata ops that use `self.http`
        // (DriveHttp), bypassing the rclone subprocess — so they're testable
        // against a mock HTTP server without rclone present. The dummy binary
        // path below is never executed.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "folder-123",
                "name": "newdir",
                "mimeType": "application/vnd.google-apps.folder",
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/files/folder-123"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let token = Arc::new(StaticToken::new("t"));
        let http = DriveHttp::with_bases(token.clone(), server.uri(), server.uri()).unwrap();
        let engine = RcloneEngine::new(
            dummy_binary(),
            token,
            None,
            None,
            PathBuf::from("/tmp/root"),
            http,
        );

        let rf = engine
            .create_dir_remote("parent-id", "newdir")
            .await
            .unwrap();
        assert_eq!(rf.id, "folder-123");
        assert_eq!(rf.mime, "application/vnd.google-apps.folder");

        // Must not error — exercises remove_dir_remote's files.delete path.
        engine.remove_dir_remote("folder-123").await.unwrap();
    }

    #[tokio::test]
    async fn base_command_omits_client_secret_when_absent() {
        let engine = RcloneEngine::new(
            dummy_binary(),
            Arc::new(StaticToken::new("tok")),
            None,
            None,
            PathBuf::from("/tmp/root"),
            test_http(),
        );
        let cmd = engine.base_command().await.unwrap();
        let env = env_map(&cmd);
        assert!(!env.contains_key("RCLONE_CONFIG_AIRDRIVE_CLIENT_SECRET"));
        assert!(!env.contains_key("RCLONE_CONFIG_AIRDRIVE_CLIENT_ID"));
        // StaticToken has no refresh token and no real expiry → far-future placeholder.
        let token = env.get("RCLONE_CONFIG_AIRDRIVE_TOKEN").unwrap();
        assert!(token.contains("2099-01-01"), "{token}");
        assert!(!token.contains("refresh_token"), "{token}");
    }
}
