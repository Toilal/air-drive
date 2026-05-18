//! Shared test harness for the integration suite.
//!
//! Two pieces:
//!
//! - [`DriveMock`] — a [`wiremock`]-backed Drive API server. It serves enough of the
//!   `v3` surface to drive the reconciler end-to-end against a fake remote: `about.user`,
//!   `files.list` / `files.get` (+ `?alt=media`), the multipart `files.create` upload
//!   endpoint, `files.update`, `files.delete`, and the two `changes.*` endpoints. The
//!   mock keeps an in-memory `DriveState` that doubles as the assertion surface: tests
//!   pre-populate it to drive Drive→local scenarios and inspect it after the fact to
//!   verify local→Drive uploads.
//!
//! - [`FsFixture`] — a tempdir with a `config_dir` (where the binary writes
//!   `config.toml`, `state.db`, `tokens.json`, `lock`) and a `local_dir` (the watched
//!   sync folder). It also builds an [`assert_cmd::Command`] pointed at the mock via
//!   the test-only env vars listed below.
//!
//! ## Test-only env-var contract
//!
//! The binary honours these overrides exclusively from integration tests. They are
//! **not** part of the stable CLI contract (`contracts/cli.md`). Their names start with
//! `AIR_DRIVE_TEST_` precisely so they are conspicuous and easy to grep for.
//!
//! - `AIR_DRIVE_DRIVE_BASE_URL` — base URL for the Drive REST API (default
//!   `https://www.googleapis.com/drive/v3`). The mock points this at its own URI.
//! - `AIR_DRIVE_DRIVE_UPLOAD_BASE_URL` — base URL for the multipart upload endpoint
//!   (default `https://www.googleapis.com/upload/drive/v3`).
//! - `AIR_DRIVE_TEST_BEARER_TOKEN` — bypass the OAuth dance entirely. When set, the
//!   binary skips `yup-oauth2` and uses this static bearer for every API call.
//! - `AIR_DRIVE_TEST_ENGINE=http` — replace the rclone subprocess engine with an
//!   in-process HTTP engine that talks straight to the Drive API. Lets integration
//!   tests run without installing rclone.

#![allow(dead_code)]
// Helpers are picked up à-la-carte by individual test files.
// Integration tests are allowed to panic on setup failures — there is no recovery path
// inside a test; surfacing the issue loudly is the point. clippy.toml already opts test
// functions in, but the harness's non-#[test] helpers need an explicit allow.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use md5::{Digest, Md5};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path as wm_path, path_regex, query_param};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

// ---------------------------------------------------------------------------
// DriveMock
// ---------------------------------------------------------------------------

/// One stored Drive file or folder.
#[derive(Debug, Clone)]
pub struct DriveFile {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub mime_type: String,
    pub content: Vec<u8>,
    pub md5: String,
}

impl DriveFile {
    pub fn is_folder(&self) -> bool {
        self.mime_type == "application/vnd.google-apps.folder"
    }
}

/// Mutable state shared between the wiremock responders and the test code.
#[derive(Debug)]
pub struct DriveState {
    pub files: HashMap<String, DriveFile>,
    pub user_email: String,
    next_id: u64,
    next_token: u64,
    /// Stable sequence of "changes" — each remote mutation appends a fileId here so
    /// `changes.list` can replay them. The cursor (page token) indexes into this list.
    pub change_log: Vec<ChangeEntry>,
}

#[derive(Debug, Clone)]
pub struct ChangeEntry {
    pub file_id: String,
    pub removed: bool,
}

impl DriveState {
    fn new(user_email: impl Into<String>) -> Self {
        Self {
            files: HashMap::new(),
            user_email: user_email.into(),
            next_id: 1,
            next_token: 1,
            change_log: Vec::new(),
        }
    }

    fn mint_id(&mut self, kind: &str) -> String {
        let id = format!("drv-{kind}-{:04}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Insert a folder. Returns its Drive ID.
    pub fn insert_folder(&mut self, parent_id: Option<&str>, name: &str) -> String {
        let id = self.mint_id("dir");
        self.files.insert(
            id.clone(),
            DriveFile {
                id: id.clone(),
                parent_id: parent_id.map(str::to_owned),
                name: name.to_owned(),
                mime_type: "application/vnd.google-apps.folder".into(),
                content: Vec::new(),
                md5: String::new(),
            },
        );
        self.change_log.push(ChangeEntry {
            file_id: id.clone(),
            removed: false,
        });
        id
    }

    /// Insert a regular file with the given content. Returns its Drive ID. The md5 is
    /// computed eagerly so the mock can hand it back on `files.get`.
    pub fn insert_file(&mut self, parent_id: Option<&str>, name: &str, content: Vec<u8>) -> String {
        let md5 = hex_md5(&content);
        let id = self.mint_id("file");
        self.files.insert(
            id.clone(),
            DriveFile {
                id: id.clone(),
                parent_id: parent_id.map(str::to_owned),
                name: name.to_owned(),
                mime_type: guess_mime(name).into(),
                content,
                md5,
            },
        );
        self.change_log.push(ChangeEntry {
            file_id: id.clone(),
            removed: false,
        });
        id
    }

    /// Tracks an upload that arrived via `files.create`. Returns the new ID.
    pub fn record_upload(
        &mut self,
        parent_id: Option<String>,
        name: String,
        mime_type: String,
        content: Vec<u8>,
    ) -> String {
        let md5 = hex_md5(&content);
        let id = self.mint_id("file");
        self.files.insert(
            id.clone(),
            DriveFile {
                id: id.clone(),
                parent_id,
                name,
                mime_type,
                content,
                md5,
            },
        );
        self.change_log.push(ChangeEntry {
            file_id: id.clone(),
            removed: false,
        });
        id
    }

    /// Snapshot of files whose `parent_id` matches.
    pub fn children_of(&self, parent_id: &str) -> Vec<&DriveFile> {
        self.files
            .values()
            .filter(|f| f.parent_id.as_deref() == Some(parent_id))
            .collect()
    }

    /// Walk the descendant tree rooted at `root_id` (inclusive of leaves, exclusive of
    /// the root itself). Returns a sorted-by-name list of (path-from-root, file).
    pub fn descendants(&self, root_id: &str) -> Vec<(String, DriveFile)> {
        let mut out = Vec::new();
        self.collect_descendants(root_id, "", &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn collect_descendants(
        &self,
        parent_id: &str,
        prefix: &str,
        out: &mut Vec<(String, DriveFile)>,
    ) {
        for f in self.children_of(parent_id) {
            let p = if prefix.is_empty() {
                f.name.clone()
            } else {
                format!("{prefix}/{}", f.name)
            };
            if !f.is_folder() {
                out.push((p.clone(), f.clone()));
            } else {
                self.collect_descendants(&f.id, &p, out);
            }
        }
    }
}

/// Hex-encoded lowercase MD5 of `bytes` — the format Drive returns in `md5Checksum`.
pub fn hex_md5(bytes: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn guess_mime(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "txt" => "text/plain",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

/// wiremock-backed Drive REST API.
#[derive(Debug)]
pub struct DriveMock {
    pub server: MockServer,
    pub state: Arc<Mutex<DriveState>>,
    /// Decrement-and-fail counter for the 503 injection path (used by T049). Mounted
    /// at higher wiremock priority than the real responders so a positive value
    /// short-circuits every request to `HTTP 503` until it hits zero.
    pub fail_budget: Arc<AtomicUsize>,
}

impl DriveMock {
    /// Start the mock with a canned user email. Mounts every Drive endpoint the daemon
    /// needs for US1 (initial sync). Continuous-sync endpoints are mounted as well so
    /// the same mock can be reused by Phase 4 tests.
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(DriveState::new("alice@example.com")));
        let fail_budget = Arc::new(AtomicUsize::new(0));

        // Higher-priority gate that turns every request into a 503 as long as the
        // fail budget is positive. Mounted FIRST so wiremock picks it before the
        // real handlers when the matcher fires.
        Mock::given(FailMatcher(fail_budget.clone()))
            .respond_with(FailResponder(fail_budget.clone()))
            .with_priority(1)
            .mount(&server)
            .await;

        // GET /drive/v3/about?fields=user — caller asks for the linked user identity.
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/about"))
            .respond_with(AboutResponder(state.clone()))
            .mount(&server)
            .await;

        // GET /drive/v3/files (list / search).
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/files"))
            .respond_with(FilesListResponder(state.clone()))
            .mount(&server)
            .await;

        // GET /drive/v3/files/{id}?alt=media — content download.
        Mock::given(method("GET"))
            .and(path_regex(r"^/drive/v3/files/[^/]+$"))
            .and(query_param("alt", "media"))
            .respond_with(FileMediaResponder(state.clone()))
            .mount(&server)
            .await;

        // GET /drive/v3/files/{id} (metadata).
        Mock::given(method("GET"))
            .and(path_regex(r"^/drive/v3/files/[^/]+$"))
            .respond_with(FileMetadataResponder(state.clone()))
            .mount(&server)
            .await;

        // POST /upload/drive/v3/files?uploadType=multipart — multipart upload.
        Mock::given(method("POST"))
            .and(wm_path("/upload/drive/v3/files"))
            .respond_with(MultipartUploadResponder(state.clone()))
            .mount(&server)
            .await;

        // PATCH /upload/drive/v3/files/{id}?uploadType=media — in-place content
        // replacement (the daemon's `engine::HttpEngine::update` code path).
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/upload/drive/v3/files/[^/]+$"))
            .respond_with(MediaUpdateResponder(state.clone()))
            .mount(&server)
            .await;

        // POST /drive/v3/files — metadata-only resource creation (folders).
        Mock::given(method("POST"))
            .and(wm_path("/drive/v3/files"))
            .respond_with(FilesCreateResponder(state.clone()))
            .mount(&server)
            .await;

        // PATCH /drive/v3/files/{id} (rename / move metadata).
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/drive/v3/files/[^/]+$"))
            .respond_with(FilePatchResponder(state.clone()))
            .mount(&server)
            .await;

        // DELETE /drive/v3/files/{id} (trash).
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/drive/v3/files/[^/]+$"))
            .respond_with(FileDeleteResponder(state.clone()))
            .mount(&server)
            .await;

        // GET /drive/v3/changes/startPageToken.
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/changes/startPageToken"))
            .respond_with(StartPageTokenResponder(state.clone()))
            .mount(&server)
            .await;

        // GET /drive/v3/changes.
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/changes"))
            .respond_with(ChangesListResponder(state.clone()))
            .mount(&server)
            .await;

        // Naked POST /token — yup-oauth2 token endpoint. Returns a static refresh response
        // for any payload. Tests that need OAuth at all should also set
        // AIR_DRIVE_TEST_BEARER_TOKEN to bypass the full flow.
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "mock-access-token",
                "refresh_token": "mock-refresh-token",
                "expires_in": 3600,
                "token_type": "Bearer",
                "scope": "https://www.googleapis.com/auth/drive.file"
            })))
            .mount(&server)
            .await;

        // Any request that bears the test bearer is accepted; absence is a 401. Tests
        // set AIR_DRIVE_TEST_BEARER_TOKEN to inject it.
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer fake-test-token"))
            .respond_with(ResponseTemplate::new(404)) // never matches — auth is enforced per-route
            .mount(&server)
            .await;

        Self {
            server,
            state,
            fail_budget,
        }
    }

    /// Make the mock answer the next `n` HTTP requests with a `503 Service
    /// Unavailable`. Subsequent requests fall through to the real handlers. Used by
    /// the T049 test to simulate a transient Drive outage.
    pub fn fail_next_n(&self, n: usize) {
        self.fail_budget.store(n, Ordering::Relaxed);
    }

    /// Drop any pending 503-injection budget. Useful for tests that flip the mock
    /// back to healthy on demand.
    pub fn lift_failures(&self) {
        self.fail_budget.store(0, Ordering::Relaxed);
    }

    /// Base URL the binary should hit for the REST API (everything but uploads).
    pub fn drive_base_url(&self) -> String {
        format!("{}/drive/v3", self.server.uri())
    }

    /// Base URL for multipart uploads (`upload.googleapis.com` in production).
    pub fn upload_base_url(&self) -> String {
        format!("{}/upload/drive/v3", self.server.uri())
    }

    /// Convenience: insert a folder at the mock root.
    pub fn insert_folder(&self, parent: Option<&str>, name: &str) -> String {
        self.state.lock().unwrap().insert_folder(parent, name)
    }

    /// Convenience: insert a file at the mock root.
    pub fn insert_file(&self, parent: Option<&str>, name: &str, content: &[u8]) -> String {
        self.state
            .lock()
            .unwrap()
            .insert_file(parent, name, content.to_vec())
    }

    /// Count of `files.create` (multipart upload) calls observed so far.
    pub async fn upload_count(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                r.method == wiremock::http::Method::POST && r.url.path() == "/upload/drive/v3/files"
            })
            .count()
    }

    /// Set the canned user email returned by `about.user`.
    pub fn set_user_email(&self, email: impl Into<String>) {
        self.state.lock().unwrap().user_email = email.into();
    }
}

// --- responders --------------------------------------------------------------

/// wiremock matcher that fires while the [`DriveMock`]'s fail budget is positive.
/// Paired with [`FailResponder`] at priority 1 so it overrides the real handlers.
struct FailMatcher(Arc<AtomicUsize>);
impl Match for FailMatcher {
    fn matches(&self, _req: &Request) -> bool {
        self.0.load(Ordering::Relaxed) > 0
    }
}

/// Companion responder for [`FailMatcher`]. Decrements the budget and returns 503.
struct FailResponder(Arc<AtomicUsize>);
impl Respond for FailResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        // Saturating decrement so a race that drops below zero rolls back to zero
        // on the next read.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
        ResponseTemplate::new(503)
    }
}

struct AboutResponder(Arc<Mutex<DriveState>>);
impl Respond for AboutResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let st = self.0.lock().unwrap();
        ResponseTemplate::new(200).set_body_json(json!({
            "user": {
                "emailAddress": st.user_email,
                "displayName": st.user_email.split('@').next().unwrap_or("user"),
            }
        }))
    }
}

struct FilesListResponder(Arc<Mutex<DriveState>>);
impl Respond for FilesListResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        // Honour `q="'PARENT_ID' in parents and trashed = false"` if present.
        let q = req
            .url
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let st = self.0.lock().unwrap();
        let parent = parse_parent_clause(&q);
        let files: Vec<Value> = match parent {
            Some(pid) => st
                .children_of(&pid)
                .into_iter()
                .map(file_to_value)
                .collect(),
            None => st.files.values().map(file_to_value).collect(),
        };
        ResponseTemplate::new(200).set_body_json(json!({ "files": files }))
    }
}

struct FileMetadataResponder(Arc<Mutex<DriveState>>);
impl Respond for FileMetadataResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let id = last_path_segment(req.url.path());
        let st = self.0.lock().unwrap();
        match st.files.get(&id) {
            Some(f) => ResponseTemplate::new(200).set_body_json(file_to_value(f)),
            None => ResponseTemplate::new(404).set_body_json(json!({
                "error": { "code": 404, "message": "File not found" }
            })),
        }
    }
}

struct FileMediaResponder(Arc<Mutex<DriveState>>);
impl Respond for FileMediaResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let id = last_path_segment(req.url.path());
        let st = self.0.lock().unwrap();
        match st.files.get(&id) {
            Some(f) => ResponseTemplate::new(200)
                .insert_header("content-type", f.mime_type.as_str())
                .set_body_bytes(f.content.clone()),
            None => ResponseTemplate::new(404),
        }
    }
}

struct MultipartUploadResponder(Arc<Mutex<DriveState>>);
impl Respond for MultipartUploadResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        // Drive multipart uploads use `multipart/related; boundary=...` with two parts:
        // (1) application/json metadata, (2) the binary content. We do a minimal parse
        // here — enough for assertion-grade fidelity, not a full RFC 2387 implementation.
        let content_type = req
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let parsed = parse_multipart_related(content_type, &req.body);
        let mut st = self.0.lock().unwrap();
        let (metadata, content) = parsed.unwrap_or_else(|| (json!({}), Vec::new()));
        let name = metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_owned();
        let mime = metadata
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        let parent = metadata
            .get("parents")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = st.record_upload(parent, name, mime, content);
        let f = st.files.get(&id).cloned().expect("just inserted");
        ResponseTemplate::new(200).set_body_json(file_to_value(&f))
    }
}

/// Handler for `POST /drive/v3/files` — metadata-only file/folder creation. The
/// daemon uses this exclusively for folder creation during the reconciler's
/// `ensure_remote_folder` walk; regular files go through the multipart endpoint.
struct FilesCreateResponder(Arc<Mutex<DriveState>>);
impl Respond for FilesCreateResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_owned();
        let mime = body
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        let parent = body
            .get("parents")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut st = self.0.lock().unwrap();
        let id = if mime == "application/vnd.google-apps.folder" {
            st.insert_folder(parent.as_deref(), &name)
        } else {
            st.record_upload(parent, name, mime, Vec::new())
        };
        let f = st.files.get(&id).cloned().expect("just inserted");
        ResponseTemplate::new(200).set_body_json(file_to_value(&f))
    }
}

/// Handler for `PATCH /upload/drive/v3/files/{id}?uploadType=media` — replaces a
/// file's content (and updates its md5/size) in place. The Drive ID is preserved.
struct MediaUpdateResponder(Arc<Mutex<DriveState>>);
impl Respond for MediaUpdateResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let id = last_path_segment(req.url.path());
        let mut st = self.0.lock().unwrap();
        let Some(f) = st.files.get_mut(&id) else {
            return ResponseTemplate::new(404);
        };
        f.content = req.body.clone();
        f.md5 = hex_md5(&f.content);
        let clone = f.clone();
        st.change_log.push(ChangeEntry {
            file_id: id,
            removed: false,
        });
        ResponseTemplate::new(200).set_body_json(file_to_value(&clone))
    }
}

struct FilePatchResponder(Arc<Mutex<DriveState>>);
impl Respond for FilePatchResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let id = last_path_segment(req.url.path());
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let mut st = self.0.lock().unwrap();
        let Some(f) = st.files.get_mut(&id) else {
            return ResponseTemplate::new(404);
        };
        if let Some(new_name) = body.get("name").and_then(Value::as_str) {
            f.name = new_name.to_owned();
        }
        // `addParents` / `removeParents` come as query params, comma-separated.
        if let Some((_, parents)) = req.url.query_pairs().find(|(k, _)| k == "addParents") {
            if let Some(p) = parents.split(',').next() {
                f.parent_id = Some(p.to_owned());
            }
        }
        let clone = f.clone();
        st.change_log.push(ChangeEntry {
            file_id: id,
            removed: false,
        });
        ResponseTemplate::new(200).set_body_json(file_to_value(&clone))
    }
}

struct FileDeleteResponder(Arc<Mutex<DriveState>>);
impl Respond for FileDeleteResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let id = last_path_segment(req.url.path());
        let mut st = self.0.lock().unwrap();
        if st.files.remove(&id).is_some() {
            st.change_log.push(ChangeEntry {
                file_id: id,
                removed: true,
            });
            ResponseTemplate::new(204)
        } else {
            ResponseTemplate::new(404)
        }
    }
}

struct StartPageTokenResponder(Arc<Mutex<DriveState>>);
impl Respond for StartPageTokenResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let st = self.0.lock().unwrap();
        let token = st.change_log.len();
        ResponseTemplate::new(200).set_body_json(json!({ "startPageToken": token.to_string() }))
    }
}

struct ChangesListResponder(Arc<Mutex<DriveState>>);
impl Respond for ChangesListResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let from: usize = req
            .url
            .query_pairs()
            .find(|(k, _)| k == "pageToken")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let st = self.0.lock().unwrap();
        let entries: Vec<Value> = st
            .change_log
            .iter()
            .skip(from)
            .map(|c| {
                let file = st.files.get(&c.file_id).map(file_to_value);
                json!({
                    "fileId": c.file_id,
                    "removed": c.removed,
                    "file": file,
                })
            })
            .collect();
        let new_token = st.change_log.len();
        ResponseTemplate::new(200).set_body_json(json!({
            "changes": entries,
            "newStartPageToken": new_token.to_string(),
        }))
    }
}

// --- helpers -----------------------------------------------------------------

fn file_to_value(f: &DriveFile) -> Value {
    let mut v = json!({
        "id": f.id,
        "name": f.name,
        "mimeType": f.mime_type,
    });
    if let Some(p) = &f.parent_id {
        v["parents"] = json!([p]);
    }
    if !f.is_folder() {
        v["size"] = json!(f.content.len().to_string());
        v["md5Checksum"] = json!(f.md5);
    }
    v
}

fn last_path_segment(p: &str) -> String {
    p.rsplit('/').next().unwrap_or("").to_owned()
}

/// Parse a Drive-style query clause `"'PARENT_ID' in parents and trashed = false"` and
/// return `PARENT_ID` if it matches that shape.
fn parse_parent_clause(q: &str) -> Option<String> {
    let q = q.trim();
    let start = q.find('\'')?;
    let end = q[start + 1..].find('\'')? + start + 1;
    let id = &q[start + 1..end];
    let tail = &q[end + 1..];
    if tail.trim_start().starts_with("in parents") {
        Some(id.to_owned())
    } else {
        None
    }
}

/// Crude `multipart/related` splitter. Extracts the first JSON metadata part and the
/// first non-JSON part as raw bytes. Returns `None` on shape mismatch.
fn parse_multipart_related(content_type: &str, body: &[u8]) -> Option<(Value, Vec<u8>)> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|t| t.strip_prefix("boundary=").map(|b| b.trim_matches('"')))?;
    let sep = format!("--{boundary}");
    let sep_bytes = sep.as_bytes();
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut cursor = 0;
    while cursor < body.len() {
        let Some(start) = find_subslice(&body[cursor..], sep_bytes) else {
            break;
        };
        let abs_start = cursor + start + sep_bytes.len();
        let next = find_subslice(&body[abs_start..], sep_bytes).map(|i| abs_start + i);
        let end = next.unwrap_or(body.len());
        let chunk = &body[abs_start..end];
        // Trim leading CRLF and trailing `--\r\n` artefacts.
        let trimmed = chunk
            .strip_prefix(b"\r\n")
            .unwrap_or(chunk)
            .strip_suffix(b"\r\n")
            .unwrap_or(chunk);
        if !trimmed.is_empty() && trimmed != b"--" {
            parts.push(trimmed);
        }
        cursor = end;
        if next.is_none() {
            break;
        }
    }
    let mut metadata: Option<Value> = None;
    let mut content: Option<Vec<u8>> = None;
    for part in parts {
        // Split headers from body on the first \r\n\r\n.
        let split = find_subslice(part, b"\r\n\r\n")?;
        let headers = &part[..split];
        let payload = &part[split + 4..];
        let is_json = std::str::from_utf8(headers)
            .map(|s| s.to_ascii_lowercase().contains("application/json"))
            .unwrap_or(false);
        if is_json && metadata.is_none() {
            metadata = serde_json::from_slice(payload).ok();
        } else if !is_json && content.is_none() {
            content = Some(payload.to_vec());
        }
    }
    Some((metadata.unwrap_or(json!({})), content.unwrap_or_default()))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// FsFixture
// ---------------------------------------------------------------------------

/// Disk-backed test fixture. Owns the `TempDir` so dropping the fixture wipes the
/// whole tree. Construct with [`fs_fixture`].
pub struct FsFixture {
    /// Root tempdir. Kept alive for the duration of the test.
    pub _tmp: TempDir,
    /// Path passed to `--config-dir`. Holds `config.toml`, `state.db`, `tokens.json`,
    /// and the runtime socket / lock files.
    pub config_dir: PathBuf,
    /// Path used as the watched local folder. Pre-created empty.
    pub local_dir: PathBuf,
}

impl FsFixture {
    /// Write a `config.toml` with the mock URLs wired in. The TOML schema mirrors
    /// `contracts/config.md`. Daemon defaults are kept.
    pub fn write_default_config(&self) {
        let toml = "\
[oauth]
# embedded client_id is used; OAuth itself is bypassed via AIR_DRIVE_TEST_BEARER_TOKEN

[mapping]

[daemon]

[rclone]
";
        std::fs::write(self.config_dir.join("config.toml"), toml).unwrap();
    }

    /// Write a no-op `tokens.json` with `0600` perms so the OAuth-permission preflight
    /// passes. Real auth still goes through the test bearer override.
    pub fn write_token_file(&self) {
        let path = self.config_dir.join("tokens.json");
        // The shape doesn't matter — the bearer override short-circuits actual use.
        std::fs::write(&path, b"{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    /// Recursively create a populated local file tree. Each entry is `(relative_path,
    /// content)`. Parent directories are created on the fly.
    pub fn populate_local(&self, entries: &[(&str, &[u8])]) {
        for (rel, bytes) in entries {
            let p = self.local_dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, bytes).unwrap();
        }
    }

    /// Read the contents of every file under `local_dir`, returning `(rel_path, bytes)`
    /// sorted by path so tests can compare deterministically.
    pub fn walk_local(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        walk_dir(&self.local_dir, &self.local_dir, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Path to the state DB the binary uses under `config_dir`.
    pub fn state_db_path(&self) -> PathBuf {
        self.config_dir.join("state.db")
    }
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_dir(root, &p, out);
        } else if p.is_file() {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&p).unwrap_or_default();
            out.push((rel, bytes));
        }
    }
}

/// Build a fresh [`FsFixture`].
pub fn fs_fixture() -> FsFixture {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let config_dir = tmp.path().join("config");
    let local_dir = tmp.path().join("local");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&local_dir).unwrap();
    FsFixture {
        _tmp: tmp,
        config_dir,
        local_dir,
    }
}

// ---------------------------------------------------------------------------
// Command builder
// ---------------------------------------------------------------------------

/// Build an [`assert_cmd`] command that runs the freshly-built `air-drive` binary with
/// all the test overrides wired in.
///
/// Wiring:
/// - `--config-dir <fx.config_dir>` so the binary uses our tempdir
/// - `--no-download-rclone` so failed rclone downloads can't hang CI
/// - `AIR_DRIVE_DRIVE_BASE_URL` + `AIR_DRIVE_DRIVE_UPLOAD_BASE_URL` point at the mock
/// - `AIR_DRIVE_TEST_BEARER_TOKEN=fake-test-token` bypasses OAuth
/// - `AIR_DRIVE_TEST_ENGINE=http` swaps rclone for the in-process HTTP engine
/// - `RUST_LOG=info` so test output captures useful diagnostics
pub fn air_drive_cmd(fx: &FsFixture, mock: &DriveMock) -> StdCommand {
    let mut cmd = StdCommand::cargo_bin("air-drive").expect("cargo-built binary");
    cmd.arg("--config-dir")
        .arg(&fx.config_dir)
        .arg("--no-download-rclone")
        .env("AIR_DRIVE_DRIVE_BASE_URL", mock.drive_base_url())
        .env("AIR_DRIVE_DRIVE_UPLOAD_BASE_URL", mock.upload_base_url())
        .env("AIR_DRIVE_TEST_BEARER_TOKEN", "fake-test-token")
        .env("AIR_DRIVE_TEST_ENGINE", "http")
        // Default to "exit cleanly after initial-sync" so simple
        // Command::output() tests don't hang on the continuous loop. Tests that
        // DO want the loop (Phase 4 / DaemonProcess) clear this env override.
        .env("AIR_DRIVE_TEST_EXIT_AFTER_INITIAL_SYNC", "1")
        .env("RUST_LOG", "info");
    cmd
}

/// Open the binary's state DB read-only and run a closure. Synchronous on purpose —
/// tests don't need the async wrapper.
pub fn with_state_db<F, R>(fx: &FsFixture, f: F) -> R
where
    F: FnOnce(&rusqlite::Connection) -> R,
{
    let conn = rusqlite::Connection::open(fx.state_db_path()).expect("open state.db");
    f(&conn)
}

// ---------------------------------------------------------------------------
// Long-running daemon process (used by Phase 4 / US2 tests)
// ---------------------------------------------------------------------------

/// Spawn-and-control wrapper around the `air-drive start` subprocess, for tests that
/// need the daemon staying alive (US2 — continuous sync). `kill_on_drop` ensures a
/// missed [`DaemonProcess::shutdown`] call still terminates the child, so a panicking
/// test never leaks a runaway daemon.
pub struct DaemonProcess {
    child: tokio::process::Child,
    pid: u32,
}

impl DaemonProcess {
    /// Spawn `air-drive --config-dir <fx> ... start <extra_args...>`. Returns once
    /// the child is up. Doesn't perform a true readiness probe — callers that need
    /// "the daemon is now listening" should poll the side-effect they care about
    /// (file present on Drive, etc.) via [`wait_until`].
    pub async fn spawn(fx: &FsFixture, mock: &DriveMock, extra_args: &[&str]) -> Self {
        let mut cmd: tokio::process::Command = air_drive_cmd(fx, mock).into();
        // The daemon must enter the continuous loop for Phase 4 tests, so we
        // override the "exit after initial-sync" default that `air_drive_cmd`
        // sets for the simpler initial_sync suite.
        cmd.env("AIR_DRIVE_TEST_EXIT_AFTER_INITIAL_SYNC", "0");
        cmd.arg("start");
        for a in extra_args {
            cmd.arg(a);
        }
        cmd.kill_on_drop(true);
        let child = cmd.spawn().expect("spawn air-drive start");
        let pid = child.id().expect("child pid");
        // Give the process a beat to clear its bootstrap (Db::open + Lock::acquire).
        // 300 ms is empirically enough on a dev laptop; CI tweaks via env var.
        tokio::time::sleep(Duration::from_millis(300)).await;
        Self { child, pid }
    }

    /// Best-effort check that the child is still running. Returns `None` if it has
    /// exited (with its status); `Some(())` otherwise.
    pub fn poll_alive(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// OS-level process id of the spawned daemon. Used by tests that assert
    /// the lock-contention error message names the running PID (T064).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Send `SIGTERM` and wait up to 10 s for the daemon to drain. Falls back to
    /// `SIGKILL` if the daemon doesn't exit in time. Returns the final exit status.
    pub async fn shutdown(mut self) -> std::process::ExitStatus {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let _ = kill(Pid::from_raw(self.pid as i32), Signal::SIGTERM);
        match tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => panic!("daemon wait failed: {e}"),
            Err(_) => {
                let _ = self.child.start_kill();
                self.child.wait().await.expect("wait after SIGKILL")
            }
        }
    }
}

/// Poll `cond` every 100 ms until it returns `true` or `timeout` expires. Returns
/// whether the condition was met. Tests use this to wait for a sync to converge
/// without sleeping for the worst case.
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
