//! Drive v3 metadata helpers used by `link`, `map`, and the reconciler.
//!
//! The free functions in this module are thin, side-effect-free wrappers over
//! [`DriveHttp`]. They speak Drive's v3 JSON, decode the bits we care about into typed
//! structs, and return [`crate::error::Result`] so call sites compose with `?`.
//!
//! What lives here vs. elsewhere:
//!
//! - **Here**: `about.user` (FR-001 — capture email), `files.get`/`files.list`
//!   (resolve folders, build directory trees), and `resolve_path` — the spec's
//!   contract on `map`'s `<remote-folder>` argument (Drive ID, `path:` notation, or
//!   `https://drive.google.com/...` URL).
//! - **Elsewhere**: HTTP plumbing → [`super::http`]; OAuth / bearers → [`super::auth`];
//!   `changes.list` polling → `drive::changes` (T052, Phase 4).

use serde::Deserialize;
use serde_json::Value;

use crate::drive::http::DriveHttp;
use crate::error::{Error, Result};

/// MIME type Drive uses for folders.
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// Result of [`about_user`] — the linked user's identity.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AboutUser {
    /// Primary email (`about.user.emailAddress`).
    pub email: String,
    /// Display name (`about.user.displayName`). Optional in the API; may be empty.
    pub display_name: String,
}

/// Drive file metadata, decoded down to the fields the daemon needs.
///
/// `size` and `md5` are populated whenever the request asked for them; callers that
/// only need id/name/mime can ignore the optionals. The reconciler uses the enriched
/// shape to avoid N+1 follow-up `files.get` calls during the initial walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveFileMeta {
    /// Drive file ID (`id`).
    pub id: String,
    /// Display name (`name`).
    pub name: String,
    /// MIME type (`mimeType`).
    pub mime_type: String,
    /// Size in bytes (`size`). `None` for folders and when the field wasn't requested.
    pub size: Option<i64>,
    /// Hex-lowercase MD5 from Drive (`md5Checksum`). `None` for folders, native Google
    /// Docs, and when the field wasn't requested.
    pub md5: Option<String>,
}

impl DriveFileMeta {
    /// `true` if this file is a Drive folder (mime = `application/vnd.google-apps.folder`).
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }
}

fn meta_from_json(v: &Value) -> Result<DriveFileMeta> {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Drive("file resource missing `id`".into()))?
        .to_owned();
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Drive("file resource missing `name`".into()))?
        .to_owned();
    let mime_type = v
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream")
        .to_owned();
    // Drive returns `size` as a string; tolerate both shapes.
    let size = v.get("size").and_then(|x| match x {
        Value::String(s) => s.parse::<i64>().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    });
    let md5 = v
        .get("md5Checksum")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(DriveFileMeta {
        id,
        name,
        mime_type,
        size,
        md5,
    })
}

/// `GET about?fields=user(emailAddress,displayName)`.
pub async fn about_user(http: &DriveHttp) -> Result<AboutUser> {
    let body = http
        .get_json("about", &[("fields", "user(emailAddress,displayName)")])
        .await?;
    let user = body
        .get("user")
        .ok_or_else(|| Error::Drive("about response missing `user`".into()))?;
    let email = user
        .get("emailAddress")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Drive("about.user missing emailAddress".into()))?
        .to_owned();
    let display_name = user
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok(AboutUser {
        email,
        display_name,
    })
}

/// `GET files/{id}?fields=id,name,mimeType`. Returns `Err(Error::Mapping)` when the file
/// doesn't exist (HTTP 404) so callers can map it to the `map` exit code `5`.
pub async fn get_file(http: &DriveHttp, id: &str) -> Result<DriveFileMeta> {
    let path = format!("files/{id}");
    match http
        .get_json(&path, &[("fields", "id,name,mimeType")])
        .await
    {
        Ok(v) => meta_from_json(&v),
        Err(Error::Drive(msg)) if msg.starts_with("HTTP 404") => {
            Err(Error::Mapping(format!("remote file `{id}` not found")))
        }
        Err(e) => Err(e),
    }
}

/// Raw `files.get` returning the full JSON resource. Used by callers that need fields
/// outside [`DriveFileMeta`] (e.g. `parents`, `size`, `md5Checksum`).
pub async fn get_file_raw(http: &DriveHttp, id: &str, fields: &str) -> Result<Value> {
    let path = format!("files/{id}");
    http.get_json(&path, &[("fields", fields)]).await
}

/// List immediate (non-trashed) children of `parent_id` with full fingerprint fields.
///
/// Requests `id,name,mimeType,size,md5Checksum` in a single `files.list` call so the
/// reconciler doesn't need a follow-up `get_file_raw` per leaf to build its fingerprint
/// set. Callers that only need id/name/mime simply ignore the extra `Option` fields.
pub async fn list_children(http: &DriveHttp, parent_id: &str) -> Result<Vec<DriveFileMeta>> {
    let q = format!("'{parent_id}' in parents and trashed = false");
    let body = http
        .get_json(
            "files",
            &[
                ("q", q.as_str()),
                ("fields", "files(id,name,mimeType,size,md5Checksum)"),
            ],
        )
        .await?;
    let arr = body
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Drive("files.list response missing `files`".into()))?;
    arr.iter().map(meta_from_json).collect()
}

/// Drive file root identifier. Drive treats the user's My Drive root as a special
/// placeholder `"root"` that you can use in queries and URLs.
pub const ROOT_PLACEHOLDER: &str = "root";

/// Create a Drive folder named `name` under `parent_id`. Used by the reconciler when
/// uploading a local file whose parent folder doesn't yet exist on the remote side.
/// Returns the new folder's metadata.
pub async fn create_folder(http: &DriveHttp, parent_id: &str, name: &str) -> Result<DriveFileMeta> {
    let body = serde_json::json!({
        "name": name,
        "mimeType": FOLDER_MIME,
        "parents": [parent_id],
    });
    let v = http
        .post_json("files", &[("fields", "id,name,mimeType")], &body)
        .await?;
    meta_from_json(&v)
}

/// Resolve the `<remote-folder>` argument of `air-drive map` (FR-002, `contracts/cli.md`):
///
/// - bare ID like `0AIQ...` or `1aBcDef-` → returned as-is after a `files.get` check
/// - URL like `https://drive.google.com/drive/folders/<id>` → ID extracted then verified
/// - `path:My Drive/Sync` notation → walked segment by segment from `My Drive` root
///
/// The resolved file MUST be a folder, otherwise an [`Error::Mapping`] is returned.
pub async fn resolve_path(http: &DriveHttp, spec: &str) -> Result<String> {
    let trimmed = spec.trim();

    // `path:` notation.
    if let Some(p) = trimmed.strip_prefix("path:") {
        return resolve_path_notation(http, p).await;
    }

    // Drive URL.
    if let Some(id) = extract_id_from_url(trimmed) {
        let meta = get_file(http, &id).await?;
        ensure_folder(&meta)?;
        return Ok(meta.id);
    }

    // Bare ID.
    let meta = get_file(http, trimmed).await?;
    ensure_folder(&meta)?;
    Ok(meta.id)
}

fn ensure_folder(meta: &DriveFileMeta) -> Result<()> {
    if !meta.is_folder() {
        return Err(Error::Mapping(format!(
            "`{}` resolves to a file, not a folder (mime: {})",
            meta.name, meta.mime_type
        )));
    }
    Ok(())
}

async fn resolve_path_notation(http: &DriveHttp, path: &str) -> Result<String> {
    // Strip leading/trailing slashes and split into segments. Empty path resolves to the
    // user's My Drive root.
    let segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Ok(ROOT_PLACEHOLDER.to_owned());
    }

    // `My Drive` is a synonym for the root — accept it as the leading segment so users
    // can paste exactly what the Drive UI shows them ("My Drive / Sync / Photos").
    let (start, rest) = if segments[0].eq_ignore_ascii_case("My Drive") {
        (ROOT_PLACEHOLDER.to_owned(), &segments[1..])
    } else {
        (ROOT_PLACEHOLDER.to_owned(), &segments[..])
    };

    let mut current = start;
    for seg in rest {
        let children = list_children(http, &current).await?;
        let next = children
            .into_iter()
            .find(|c| c.is_folder() && c.name == *seg)
            .ok_or_else(|| Error::Mapping(format!("no subfolder `{seg}` under `{current}`")))?;
        current = next.id;
    }
    Ok(current)
}

/// Pull the file/folder ID out of a `https://drive.google.com/...` URL. Returns `None`
/// for non-URL inputs so the caller falls back to "treat as bare ID".
fn extract_id_from_url(s: &str) -> Option<String> {
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    // Drive folder URL: .../folders/<id>[?...]
    if let Some(rest) = s.split("/folders/").nth(1) {
        return Some(strip_query(rest));
    }
    // Drive file URL: .../file/d/<id>/...
    if let Some(rest) = s.split("/file/d/").nth(1) {
        let id = rest.split('/').next().unwrap_or("");
        return Some(strip_query(id));
    }
    // `?id=<id>` query-string form (older share URLs).
    if let Some((_, q)) = s.split_once('?') {
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("id=") {
                return Some(strip_query(v));
            }
        }
    }
    None
}

fn strip_query(s: &str) -> String {
    s.split(['?', '#', '/']).next().unwrap_or("").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_id_handles_folders_url() {
        assert_eq!(
            extract_id_from_url("https://drive.google.com/drive/folders/ABCxyz123"),
            Some("ABCxyz123".into())
        );
        assert_eq!(
            extract_id_from_url("https://drive.google.com/drive/folders/ABCxyz123?usp=share"),
            Some("ABCxyz123".into())
        );
    }

    #[test]
    fn extract_id_handles_file_url() {
        assert_eq!(
            extract_id_from_url("https://drive.google.com/file/d/FILEID/view?usp=share"),
            Some("FILEID".into())
        );
    }

    #[test]
    fn extract_id_handles_query_param_url() {
        assert_eq!(
            extract_id_from_url("https://drive.google.com/open?id=Q123&authuser=0"),
            Some("Q123".into())
        );
    }

    #[test]
    fn extract_id_returns_none_for_non_url() {
        assert_eq!(extract_id_from_url("0AIQqU"), None);
        assert_eq!(extract_id_from_url("path:My Drive/Sync"), None);
    }

    #[test]
    fn meta_from_json_decodes_minimum_fields() {
        let v = serde_json::json!({
            "id": "id1",
            "name": "foo",
            "mimeType": FOLDER_MIME,
        });
        let m = meta_from_json(&v).unwrap();
        assert_eq!(m.id, "id1");
        assert_eq!(m.name, "foo");
        assert!(m.is_folder());
    }

    #[test]
    fn meta_from_json_defaults_mime_when_missing() {
        let v = serde_json::json!({ "id": "id1", "name": "foo" });
        let m = meta_from_json(&v).unwrap();
        assert_eq!(m.mime_type, "application/octet-stream");
        assert!(!m.is_folder());
    }

    #[test]
    fn meta_from_json_rejects_missing_id_or_name() {
        let v = serde_json::json!({ "name": "foo" });
        assert!(meta_from_json(&v).is_err());
        let v = serde_json::json!({ "id": "x" });
        assert!(meta_from_json(&v).is_err());
    }
}
