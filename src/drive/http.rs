//! Drive REST HTTP client.
//!
//! [`DriveHttp`] wraps a [`reqwest::Client`] and centralises:
//!
//! - **Base URLs** — defaults to the Google endpoints; overridable per process via
//!   `AIR_DRIVE_DRIVE_BASE_URL` and `AIR_DRIVE_DRIVE_UPLOAD_BASE_URL` (integration
//!   tests use this to redirect the daemon at a wiremock server).
//! - **Auth** — every request gets `Authorization: Bearer <tok>` from an
//!   `Arc<dyn TokenProvider>`. Refresh and OAuth dance live in [`super::auth`].
//! - **Retries** — transient HTTP responses (`429`, `503`) and connection-level errors
//!   are retried with exponential backoff (1 s → 16 s) plus ±20 % jitter, up to
//!   [`MAX_ATTEMPTS`] tries. Non-transient errors (4xx other than `429`, 5xx other
//!   than `503`) fail immediately so the caller can decide.
//!
//! The client is `Clone` (everything inside is `Arc`-shared) — pass it down by value.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::Value;

use crate::drive::auth::TokenProvider;
use crate::error::{Error, Result};

/// Maximum number of attempts (initial + retries) for a single request.
pub const MAX_ATTEMPTS: u32 = 6;

/// Initial backoff delay; doubles after each retry.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Hard cap on a single backoff delay before jitter.
const MAX_BACKOFF: Duration = Duration::from_secs(16);

/// Default REST base URL when `AIR_DRIVE_DRIVE_BASE_URL` is unset.
pub const DEFAULT_DRIVE_BASE: &str = "https://www.googleapis.com/drive/v3";

/// Default multipart upload base URL when `AIR_DRIVE_DRIVE_UPLOAD_BASE_URL` is unset.
pub const DEFAULT_UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";

/// Drive REST client. Cheap to `Clone` (internal state lives behind an [`Arc`]).
#[derive(Clone)]
pub struct DriveHttp {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    base_url: String,
    upload_base_url: String,
    token_provider: Arc<dyn TokenProvider>,
}

impl DriveHttp {
    /// Build a new client with default URLs (or env-var overrides) and the given token
    /// provider.
    pub fn new(token_provider: Arc<dyn TokenProvider>) -> Result<Self> {
        let base_url = std::env::var("AIR_DRIVE_DRIVE_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_DRIVE_BASE.to_owned());
        let upload_base_url = std::env::var("AIR_DRIVE_DRIVE_UPLOAD_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_UPLOAD_BASE.to_owned());
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Drive(format!("reqwest client init: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base_url,
                upload_base_url,
                token_provider,
            }),
        })
    }

    /// Explicit constructor — used by tests that prefer not to mutate env vars.
    pub fn with_bases(
        token_provider: Arc<dyn TokenProvider>,
        base_url: impl Into<String>,
        upload_base_url: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Drive(format!("reqwest client init: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base_url: base_url.into(),
                upload_base_url: upload_base_url.into(),
                token_provider,
            }),
        })
    }

    /// REST base URL the client is currently using (e.g. for diagnostics).
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// Multipart-upload base URL the client is currently using.
    pub fn upload_base_url(&self) -> &str {
        &self.inner.upload_base_url
    }

    // -- public verbs ---------------------------------------------------------

    /// `GET <base>/<path>?<query>` returning the parsed JSON body.
    pub async fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let url = self.url(path);
        self.with_retry(|| async {
            let req = self.inner.client.get(&url).query(query);
            let req = self.with_bearer(req).await?;
            let resp = req.send().await.map_err(map_reqwest)?;
            json_or_err(resp).await
        })
        .await
    }

    /// `GET <base>/<path>?<query>` returning the raw body bytes (binary downloads).
    pub async fn get_bytes(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let url = self.url(path);
        self.with_retry(|| async {
            let req = self.inner.client.get(&url).query(query);
            let req = self.with_bearer(req).await?;
            let resp = req.send().await.map_err(map_reqwest)?;
            bytes_or_err(resp).await
        })
        .await
    }

    /// `POST <base>/<path>?<query>` with a JSON body returning the parsed JSON
    /// response. Used for resource creation (e.g. folder creation in
    /// [`crate::drive::metadata::create_folder`]) that does not involve a multipart
    /// upload — there is no content part, just metadata.
    pub async fn post_json(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: &Value,
    ) -> Result<Value> {
        let url = self.url(path);
        self.with_retry(|| async {
            let req = self.inner.client.post(&url).query(query).json(body);
            let req = self.with_bearer(req).await?;
            let resp = req.send().await.map_err(map_reqwest)?;
            json_or_err(resp).await
        })
        .await
    }

    /// `PATCH <base>/<path>?<query>` with a JSON body.
    pub async fn patch_json(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: &Value,
    ) -> Result<Value> {
        let url = self.url(path);
        self.with_retry(|| async {
            let req = self.inner.client.patch(&url).query(query).json(body);
            let req = self.with_bearer(req).await?;
            let resp = req.send().await.map_err(map_reqwest)?;
            json_or_err(resp).await
        })
        .await
    }

    /// `DELETE <base>/<path>`.
    pub async fn delete(&self, path: &str) -> Result<()> {
        let url = self.url(path);
        self.with_retry(|| async {
            let req = self.inner.client.delete(&url);
            let req = self.with_bearer(req).await?;
            let resp = req.send().await.map_err(map_reqwest)?;
            empty_or_err(resp).await
        })
        .await
    }

    /// `PATCH <upload_base>/files/<file_id>?uploadType=media` — overwrites the
    /// content of an existing remote file while keeping its Drive ID stable.
    /// Body is the raw bytes; `content_type` is sent verbatim. Returns the file
    /// resource Drive sends back.
    pub async fn patch_upload_media(
        &self,
        file_id: &str,
        content_type: &str,
        content: &[u8],
    ) -> Result<Value> {
        let url = format!(
            "{}/files/{file_id}?uploadType=media",
            self.inner.upload_base_url
        );
        let header_value = content_type.to_owned();
        let body = Arc::new(content.to_vec());
        self.with_retry(|| {
            let url = url.clone();
            let header_value = header_value.clone();
            let body = body.clone();
            async move {
                let req = self
                    .inner
                    .client
                    .patch(&url)
                    .header(
                        CONTENT_TYPE,
                        HeaderValue::from_str(&header_value)
                            .map_err(|e| Error::Drive(format!("invalid content-type: {e}")))?,
                    )
                    .body((*body).clone());
                let req = self.with_bearer(req).await?;
                let resp = req.send().await.map_err(map_reqwest)?;
                json_or_err(resp).await
            }
        })
        .await
    }

    /// `POST <upload_base>/files?uploadType=multipart` with a Drive-style
    /// multipart/related body: one JSON metadata part, one binary content part.
    /// Returns the parsed file resource (id, name, size, md5Checksum…).
    pub async fn upload_multipart(
        &self,
        metadata: &Value,
        content_type: &str,
        content: &[u8],
    ) -> Result<Value> {
        let url = format!("{}/files?uploadType=multipart", self.inner.upload_base_url);
        let boundary = format!("air-drive-{:016x}", rand::thread_rng().r#gen::<u64>());
        let body = build_related_body(&boundary, metadata, content_type, content)?;
        let header_value = format!("multipart/related; boundary={boundary}");
        let body = Arc::new(body); // share across retries
        self.with_retry(|| {
            let url = url.clone();
            let header_value = header_value.clone();
            let body = body.clone();
            async move {
                let req = self
                    .inner
                    .client
                    .post(&url)
                    .header(
                        CONTENT_TYPE,
                        HeaderValue::from_str(&header_value).map_err(|e| {
                            Error::Drive(format!("invalid multipart content-type: {e}"))
                        })?,
                    )
                    .body((*body).clone());
                let req = self.with_bearer(req).await?;
                let resp = req.send().await.map_err(map_reqwest)?;
                json_or_err(resp).await
            }
        })
        .await
    }

    // -- internals ------------------------------------------------------------

    fn url(&self, path: &str) -> String {
        let p = path.trim_start_matches('/');
        format!("{}/{p}", self.inner.base_url)
    }

    async fn with_bearer(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let token = self.inner.token_provider.token().await?;
        let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| Error::Oauth(format!("invalid bearer token: {e}")))?;
        // Defense in depth: keep the bearer out of any header dump / Debug.
        header.set_sensitive(true);
        Ok(req.header(AUTHORIZATION, header))
    }

    async fn with_retry<F, Fut, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut delay = INITIAL_BACKOFF;
        for attempt in 1..=MAX_ATTEMPTS {
            match f().await {
                Ok(v) => return Ok(v),
                Err(e) if is_transient(&e) && attempt < MAX_ATTEMPTS => {
                    let sleep_for = jittered(delay);
                    tracing::debug!(
                        attempt,
                        retry_in_ms = sleep_for.as_millis() as u64,
                        error = %e,
                        "drive request transient failure; retrying"
                    );
                    tokio::time::sleep(sleep_for).await;
                    delay = (delay * 2).min(MAX_BACKOFF);
                }
                Err(e) => return Err(e),
            }
        }
        // Unreachable: the loop above either returns Ok or hits the last-attempt branch.
        Err(Error::Drive("retry loop exhausted (unreachable)".into()))
    }
}

// ---------------------------------------------------------------------------
// Response decoding
// ---------------------------------------------------------------------------

async fn json_or_err(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    if status.is_success() {
        let v = resp
            .json::<Value>()
            .await
            .map_err(|e| Error::Drive(format!("response is not valid JSON: {e}")))?;
        return Ok(v);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(http_error(status, body))
}

async fn bytes_or_err(resp: reqwest::Response) -> Result<Vec<u8>> {
    let status = resp.status();
    if status.is_success() {
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Drive(format!("body bytes: {e}")))?;
        return Ok(bytes.to_vec());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(http_error(status, body))
}

async fn empty_or_err(resp: reqwest::Response) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(http_error(status, body))
}

fn http_error(status: reqwest::StatusCode, body: String) -> Error {
    // Classify auth failures (401) separately from generic Drive errors so the
    // dispatcher / poller can flip the daemon to `blocked` instead of
    // retrying forever. 403 stays on the Drive side because Google uses it
    // for both permission denials and quota / not-allowed-by-policy, which
    // are NOT necessarily fatal auth failures.
    if status == reqwest::StatusCode::UNAUTHORIZED {
        Error::Oauth(format!("HTTP 401: {body}"))
    } else {
        Error::DriveHttp {
            status: status.as_u16(),
            body,
        }
    }
}

fn map_reqwest(e: reqwest::Error) -> Error {
    Error::Network(e.to_string())
}

/// Whether a previous request failure should be retried. Classification is
/// type-driven (numeric status / variant), not message string-matching:
///
/// - any connection-level [`Error::Network`] (the request never got a status);
/// - `429 Too Many Requests` and all `5xx` server errors (`500`/`502`/`503`/`504`);
/// - `403` only when Drive's body names a rate-limit reason (`rateLimitExceeded`
///   / `userRateLimitExceeded`) — a plain 403 (permission denied) is fatal.
fn is_transient(err: &Error) -> bool {
    match err {
        Error::Network(_) => true,
        Error::DriveHttp { status, body } => {
            *status == 429
                || (500..=504).contains(status)
                || (*status == 403
                    && (body.contains("rateLimitExceeded")
                        || body.contains("userRateLimitExceeded")))
        }
        _ => false,
    }
}

/// Sleep duration with ±20 % uniform jitter so a fleet of daemons doesn't synchronise
/// retries against the same backend.
fn jittered(base: Duration) -> Duration {
    let ms = base.as_millis() as i64;
    let jitter_range = ms / 5; // 20 %
    let mut rng = rand::thread_rng();
    let delta: i64 = rng.gen_range(-jitter_range..=jitter_range);
    let total = (ms + delta).max(0) as u64;
    Duration::from_millis(total)
}

// ---------------------------------------------------------------------------
// Multipart/related body construction
// ---------------------------------------------------------------------------

/// Build the two-part body Drive expects for `uploadType=multipart`: a JSON metadata
/// part followed by the binary content part, separated by `--<boundary>`.
fn build_related_body(
    boundary: &str,
    metadata: &Value,
    content_type: &str,
    content: &[u8],
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(content.len() + 256);
    let meta_bytes = serde_json::to_vec(metadata)
        .map_err(|e| Error::Drive(format!("metadata serialisation: {e}")))?;
    write_all(&mut out, format!("--{boundary}\r\n").as_bytes());
    write_all(
        &mut out,
        b"Content-Type: application/json; charset=UTF-8\r\n\r\n",
    );
    write_all(&mut out, &meta_bytes);
    write_all(&mut out, b"\r\n");
    write_all(&mut out, format!("--{boundary}\r\n").as_bytes());
    write_all(
        &mut out,
        format!("Content-Type: {content_type}\r\n\r\n").as_bytes(),
    );
    write_all(&mut out, content);
    write_all(&mut out, b"\r\n");
    write_all(&mut out, format!("--{boundary}--\r\n").as_bytes());
    Ok(out)
}

fn write_all(buf: &mut Vec<u8>, src: &[u8]) {
    buf.extend_from_slice(src);
}

// ---------------------------------------------------------------------------
// Headers helper exposed to other modules (e.g. drive::metadata for custom calls)
// ---------------------------------------------------------------------------

/// Return a static map of headers the daemon attaches to every request. Currently
/// empty; reserved for future tracing / User-Agent additions without churning every
/// caller.
pub fn default_headers() -> HashMap<&'static str, &'static str> {
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::auth::StaticToken;

    #[test]
    fn url_concatenates_base_and_path() {
        let provider = Arc::new(StaticToken::new("t"));
        let http = DriveHttp::with_bases(provider, "http://x/v3", "http://x/upload").unwrap();
        assert_eq!(http.url("files"), "http://x/v3/files");
        assert_eq!(http.url("/files/abc"), "http://x/v3/files/abc");
    }

    #[test]
    fn is_transient_classifies_by_status_not_string() {
        let http = |status: u16, body: &str| Error::DriveHttp {
            status,
            body: body.into(),
        };
        // Retry-eligible: 429, all 5xx, network, and 403-with-rate-limit reason.
        assert!(is_transient(&http(429, "")));
        assert!(is_transient(&http(500, "")));
        assert!(is_transient(&http(502, "")));
        assert!(is_transient(&http(503, "")));
        assert!(is_transient(&http(504, "")));
        assert!(is_transient(&Error::Network("connection refused".into())));
        assert!(is_transient(&http(
            403,
            r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#
        )));
        // Fatal: 404, a plain 403 (permission denied), and non-HTTP errors.
        assert!(!is_transient(&http(404, "not found")));
        assert!(!is_transient(&http(403, "permission denied")));
        assert!(!is_transient(&Error::Oauth("revoked".into())));
        assert!(!is_transient(&Error::Drive("missing field".into())));
    }

    #[test]
    fn build_related_body_round_trips_metadata_and_content() {
        let metadata = serde_json::json!({ "name": "hi.txt", "parents": ["p1"] });
        let body = build_related_body("BND", &metadata, "text/plain", b"hello").unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("--BND\r\n"));
        assert!(s.contains("Content-Type: application/json"));
        assert!(s.contains("\"name\":\"hi.txt\""));
        assert!(s.contains("Content-Type: text/plain"));
        assert!(s.contains("hello"));
        assert!(s.ends_with("--BND--\r\n"));
    }

    #[test]
    fn jittered_stays_within_plus_minus_20_percent() {
        let base = Duration::from_millis(1000);
        for _ in 0..200 {
            let j = jittered(base);
            assert!(j >= Duration::from_millis(800));
            assert!(j <= Duration::from_millis(1200));
        }
    }
}
