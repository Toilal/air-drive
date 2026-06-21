//! In-process HTTP [`SyncEngine`] — the engine the integration suite drives.
//!
//! `HttpEngine` implements every method of [`super::SyncEngine`] by calling the Drive
//! REST API directly through [`crate::drive::http::DriveHttp`], skipping the rclone
//! subprocess entirely. The test harness selects this engine via
//! `AIR_DRIVE_TEST_ENGINE=http` so the integration tests can run without rclone (or
//! Google) being available.
//!
//! It is intentionally simple: a single `reqwest` client, no concurrency tuning, no
//! resumable uploads. Production traffic goes through [`super::rclone::RcloneEngine`]
//! which has all that machinery built in.

use std::path::Path;
use std::sync::Arc;

use serde_json::json;

use crate::drive::http::DriveHttp;
use crate::engine::staging;
use crate::engine::{RemoteFile, SyncEngine};
use crate::error::{Error, Result};

/// In-process HTTP engine.
#[derive(Clone)]
pub struct HttpEngine {
    http: Arc<DriveHttp>,
}

impl HttpEngine {
    /// Build a new engine over an existing [`DriveHttp`] client.
    pub fn new(http: DriveHttp) -> Self {
        Self {
            http: Arc::new(http),
        }
    }

    /// Convert a Drive `files` resource into a [`RemoteFile`].
    fn parse_remote_file(v: &serde_json::Value) -> Result<RemoteFile> {
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| Error::Drive("missing `id` in upload response".into()))?
            .to_owned();
        let mime = v
            .get("mimeType")
            .and_then(|x| x.as_str())
            .unwrap_or("application/octet-stream")
            .to_owned();
        // Drive returns size as a string; coerce.
        let size = match v.get("size") {
            Some(serde_json::Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        let md5 = v
            .get("md5Checksum")
            .and_then(|x| x.as_str())
            .map(str::to_owned);
        Ok(RemoteFile {
            id,
            mime,
            size,
            md5,
        })
    }

    fn content_type_for(name: &str) -> &'static str {
        match name.rsplit('.').next().unwrap_or("") {
            "txt" => "text/plain",
            "json" => "application/json",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            _ => "application/octet-stream",
        }
    }
}

#[async_trait::async_trait]
impl SyncEngine for HttpEngine {
    async fn upload(&self, local: &Path, remote_parent_id: &str, name: &str) -> Result<RemoteFile> {
        let content = tokio::fs::read(local).await?;
        let metadata = json!({
            "name": name,
            "parents": [remote_parent_id],
            "mimeType": Self::content_type_for(name),
        });
        let response = self
            .http
            .upload_multipart(&metadata, Self::content_type_for(name), &content)
            .await?;
        Self::parse_remote_file(&response)
    }

    async fn update(&self, remote_id: &str, local: &Path) -> Result<RemoteFile> {
        let content = tokio::fs::read(local).await?;
        let name = local.file_name().and_then(|s| s.to_str()).unwrap_or("blob");
        let response = self
            .http
            .patch_upload_media(remote_id, Self::content_type_for(name), &content)
            .await?;
        Self::parse_remote_file(&response)
    }

    async fn download(&self, remote_id: &str, local: &Path, local_root: &Path) -> Result<()> {
        // Every download — even nested files (`dir/sub/leaf.txt`) — stages
        // under the SAME `<local_root>/.air-drive-partial/` directory so the
        // start-up orphan-sweep finds all leftovers in one place.
        let op_id = format!(
            "{remote_id}-{}",
            local.file_name().and_then(|s| s.to_str()).unwrap_or("dest")
        );
        let staging_path = staging::stage_path(local_root, &op_id)?;

        let path = format!("files/{remote_id}");
        let body = match self.http.get_bytes(&path, &[("alt", "media")]).await {
            Ok(b) => b,
            Err(e) => {
                staging::discard(&staging_path)?;
                return Err(e);
            }
        };
        tokio::fs::write(&staging_path, &body).await?;
        staging::promote(&staging_path, local)?;
        Ok(())
    }

    async fn move_remote(
        &self,
        remote_id: &str,
        new_parent_id: &str,
        new_name: &str,
    ) -> Result<()> {
        // First fetch current parents so we can list them in `removeParents`.
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
        let body = json!({ "name": new_name });
        let path = format!("files/{remote_id}");
        self.http.patch_json(&path, &query, &body).await?;
        Ok(())
    }

    async fn delete_remote(&self, remote_id: &str) -> Result<()> {
        let path = format!("files/{remote_id}");
        self.http.delete(&path).await
    }

    async fn create_dir_remote(&self, remote_parent_id: &str, name: &str) -> Result<RemoteFile> {
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
        // A Drive `files.delete` by id trashes a folder just like a file.
        let path = format!("files/{remote_id}");
        self.http.delete(&path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::auth::StaticToken;

    #[test]
    fn content_type_for_picks_known_extensions() {
        assert_eq!(HttpEngine::content_type_for("a.txt"), "text/plain");
        assert_eq!(HttpEngine::content_type_for("a.json"), "application/json");
        assert_eq!(HttpEngine::content_type_for("a.png"), "image/png");
        assert_eq!(
            HttpEngine::content_type_for("a.bin"),
            "application/octet-stream"
        );
        assert_eq!(
            HttpEngine::content_type_for("noext"),
            "application/octet-stream"
        );
    }

    #[test]
    fn parse_remote_file_handles_string_size_and_md5() {
        let v = serde_json::json!({
            "id": "x",
            "mimeType": "text/plain",
            "size": "42",
            "md5Checksum": "abcd",
        });
        let f = HttpEngine::parse_remote_file(&v).unwrap();
        assert_eq!(f.id, "x");
        assert_eq!(f.size, 42);
        assert_eq!(f.md5.as_deref(), Some("abcd"));
    }

    #[test]
    fn parse_remote_file_defaults_when_size_missing() {
        let v = serde_json::json!({ "id": "x" });
        let f = HttpEngine::parse_remote_file(&v).unwrap();
        assert_eq!(f.size, 0);
        assert!(f.md5.is_none());
    }

    #[test]
    fn engine_can_be_constructed_with_static_token() {
        let provider = Arc::new(StaticToken::new("t"));
        let http = DriveHttp::with_bases(provider, "http://x", "http://x/upload").unwrap();
        let _engine = HttpEngine::new(http);
    }
}
