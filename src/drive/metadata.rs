//! Drive v3 metadata helpers used by `link`, `map`, and the reconciler.
//!
//! The free functions in this module are thin, side-effect-free wrappers over
//! [`DriveHttp`]. They speak Drive's v3 JSON, decode the bits we care about into typed
//! structs, and return [`crate::error::Result`] so call sites compose with `?`.
//!
//! What lives here vs. elsewhere:
//!
//! - **Here**: `about.user` (capture email), `files.get`/`files.list`
//!   (resolve folders, build directory trees), and `resolve_path` — the contract
//!   on `map`'s `<remote-folder>` argument (Drive ID, `path:` notation, or
//!   `https://drive.google.com/...` URL).
//! - **Elsewhere**: HTTP plumbing → [`super::http`]; OAuth / bearers → [`super::auth`];
//!   `changes.list` polling → `drive::changes`.

use serde::Deserialize;
use serde_json::Value;

use crate::drive::http::DriveHttp;
use crate::error::{Error, Result};

/// MIME type Drive uses for folders.
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// True when a Drive `name` is safe to use as a **single** local path component.
///
/// Drive names are attacker-controlled (anyone who can drop a file into a synced
/// shared folder picks the name), and the reconciler joins them into the local
/// `relative_path` it then `local_root.join`s and writes to. A name of `..`,
/// `/etc/x`, or one embedding a separator would escape the mapped root — an
/// arbitrary-write primitive. We reject rather than sanitise: a name that can't
/// be represented as one safe component is skipped (and logged) instead of being
/// silently rewritten to something the user didn't choose.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

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
        Err(Error::DriveHttp { status: 404, .. }) => {
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
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    // Drive caps a page at 1000 items; a folder with more children spans several
    // pages. Follow `nextPageToken` to the end — a single un-paginated request
    // would silently truncate the child set and make the daemon miss files or
    // create duplicate folders.
    loop {
        let mut query: Vec<(&str, &str)> = vec![
            ("q", q.as_str()),
            (
                "fields",
                "nextPageToken,files(id,name,mimeType,size,md5Checksum)",
            ),
            ("pageSize", "1000"),
        ];
        if let Some(token) = &page_token {
            query.push(("pageToken", token.as_str()));
        }
        let body = http.get_json("files", &query).await?;
        drop(query); // end the borrow of `page_token` before reassigning it below
        let arr = body
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Drive("files.list response missing `files`".into()))?;
        for v in arr {
            out.push(meta_from_json(v)?);
        }
        page_token = body
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        if page_token.is_none() {
            return Ok(out);
        }
    }
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

/// Move a Drive file or folder to the trash by id (recoverable for ~30 days).
/// Used to propagate a deletion under the `trash` policy; the `permanent` policy
/// uses `files.delete` (via [`DriveHttp::delete`]) instead. Idempotent: trashing
/// an already-trashed item is a no-op on Drive's side.
pub async fn trash(http: &DriveHttp, id: &str) -> Result<()> {
    let body = serde_json::json!({ "trashed": true });
    http.patch_json(&format!("files/{id}"), &[("fields", "id")], &body)
        .await?;
    Ok(())
}

/// Resolve the `<remote-folder>` argument of `air-drive map`:
///
/// - URL like `https://drive.google.com/drive/folders/<id>` → ID extracted
///   then verified via `files.get`. The URL references a specific existing
///   resource — `auto_create` is irrelevant here.
/// - Anything else (with or without the optional `path:` prefix) is parsed as
///   a path under My Drive root and walked segment by segment. `auto_create`
///   controls whether missing segments are created on the fly. A bare folder
///   name is equivalent to `path:<name>`.
///
/// The resolved file MUST be a folder, otherwise an [`Error::Mapping`] is returned.
pub async fn resolve_path(http: &DriveHttp, spec: &str, auto_create: bool) -> Result<String> {
    let trimmed = spec.trim();

    // Drive URL — identifies a specific existing resource.
    if let Some(id) = extract_id_from_url(trimmed) {
        let meta = get_file(http, &id).await?;
        ensure_folder(&meta)?;
        return Ok(meta.id);
    }

    // Everything else is a path. Strip the optional `path:` prefix.
    let path = trimmed.strip_prefix("path:").unwrap_or(trimmed);
    resolve_path_notation(http, path, auto_create).await
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

async fn resolve_path_notation(http: &DriveHttp, path: &str, auto_create: bool) -> Result<String> {
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
        let found = children
            .into_iter()
            .find(|c| c.is_folder() && c.name == *seg);
        current = match found {
            Some(c) => c.id,
            None if auto_create => {
                let created = create_folder(http, &current, seg).await?;
                tracing::info!(
                    parent = %current,
                    name = %seg,
                    new_id = %created.id,
                    "created missing remote folder"
                );
                created.id
            }
            None => {
                return Err(Error::Mapping(format!(
                    "no subfolder `{seg}` under `{current}` (enable \
                     `mapping.auto_create_remote_root` to create it automatically)"
                )));
            }
        };
    }
    Ok(current)
}

/// `true` when `s` is parseable as a Drive URL (the input would be handled by
/// the URL branch of [`resolve_path`]). Used by callers that need to
/// distinguish a recoverable path-style spec from a URL referring to a
/// specific, non-recreatable resource.
pub fn is_drive_url(s: &str) -> bool {
    extract_id_from_url(s.trim()).is_some()
}

/// Pull the file/folder ID out of a `https://drive.google.com/...` URL. Returns `None`
/// for non-URL inputs so the caller falls back to "treat as path".
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

    #[tokio::test]
    async fn list_children_follows_next_page_token() {
        use crate::drive::auth::StaticToken;
        use std::sync::Arc;
        use wiremock::matchers::{method, query_param_is_missing};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // First page: a file + nextPageToken (no pageToken in the request yet).
        Mock::given(method("GET"))
            .and(query_param_is_missing("pageToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nextPageToken": "p2",
                "files": [{ "id": "f1", "name": "a.txt", "mimeType": "text/plain" }],
            })))
            .mount(&server)
            .await;
        // Second page (pageToken=p2): another file, no nextPageToken → stop.
        Mock::given(method("GET"))
            .and(wiremock::matchers::query_param("pageToken", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "files": [{ "id": "f2", "name": "b.txt", "mimeType": "text/plain" }],
            })))
            .mount(&server)
            .await;

        let http = DriveHttp::with_bases(
            Arc::new(StaticToken::new("t")),
            format!("{}/drive/v3", server.uri()),
            format!("{}/upload/drive/v3", server.uri()),
        )
        .unwrap();

        let children = list_children(&http, "parent").await.unwrap();
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt"], "both pages must be returned");
    }

    #[test]
    fn is_safe_name_accepts_plain_names_and_rejects_traversal() {
        for ok in ["spec.txt", "My Folder", "réport-2026", ".hidden", "a.b.c"] {
            assert!(is_safe_name(ok), "{ok:?} should be safe");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "../etc",
            "/etc/passwd",
            "a\\b",
            "x\0y",
        ] {
            assert!(!is_safe_name(bad), "{bad:?} should be rejected");
        }
    }

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
