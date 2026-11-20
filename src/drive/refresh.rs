//! Refresh-only OAuth token provider for the daemon.
//!
//! [`RefreshOnlyProvider`] reads the tokens `air-drive link` persisted to
//! `tokens.json` and renews the access token against Google's token endpoint with
//! the stored refresh token. It has **no interactive fallback**: unlike
//! `yup-oauth2`'s authenticator, which starts the full browser consent flow
//! whenever a refresh fails for any reason, a failed refresh here is returned to
//! the caller, classified so the daemon can react:
//!
//! - connection failure, timeout, `429` or `5xx` → [`Error::Network`] (transient,
//!   retried on the next tick);
//! - any other rejection (`invalid_grant`, `invalid_client`, …) or a missing
//!   refresh token → [`Error::Oauth`] (the user must run `air-drive link`).
//!
//! `tokens.json` is re-read whenever the cached access token is stale, so tokens
//! written by a later `air-drive link` are picked up without restarting the daemon.
//! The file keeps the `yup-oauth2` on-disk format (a JSON array of
//! `{"scopes": [...], "token": TokenInfo}`) so both providers share it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use yup_oauth2::storage::TokenInfo;

use crate::drive::auth::{DRIVE_SCOPES, RcloneToken, TokenProvider};
use crate::error::{Error, Result};

/// Upper bound on a single token-endpoint round trip. The provider holds its lock
/// across the refresh, so this also bounds how long concurrent callers can wait.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

const RELINK_HINT: &str = "run `air-drive link` to re-authorize";

/// One entry of `tokens.json`, in the shape `yup-oauth2` reads and writes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    scopes: Vec<String>,
    token: TokenInfo,
}

/// Token provider that only ever refreshes; see the module docs.
pub struct RefreshOnlyProvider {
    client: reqwest::Client,
    client_id: String,
    client_secret: String,
    token_uri: String,
    tokens_path: PathBuf,
    cached: Mutex<Option<TokenInfo>>,
}

impl RefreshOnlyProvider {
    /// Build a provider renewing tokens from `tokens_path` against `token_uri`.
    pub fn new(
        client_id: String,
        client_secret: String,
        token_uri: String,
        tokens_path: PathBuf,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REFRESH_TIMEOUT)
            .build()
            .map_err(|e| Error::Network(format!("build token HTTP client: {e}")))?;
        Ok(Self {
            client,
            client_id,
            client_secret,
            token_uri,
            tokens_path,
            cached: Mutex::new(None),
        })
    }

    async fn current(&self) -> Result<TokenInfo> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref().filter(|t| is_usable(t)) {
            return Ok(token.clone());
        }

        let mut entries = load_entries(&self.tokens_path)?;
        let index = select_drive_entry(&entries).ok_or_else(|| {
            Error::Oauth(format!(
                "no Drive token in {}; {RELINK_HINT}",
                self.tokens_path.display()
            ))
        })?;
        if is_usable(&entries[index].token) {
            let token = entries[index].token.clone();
            *cached = Some(token.clone());
            return Ok(token);
        }

        let refresh_token = entries[index]
            .token
            .refresh_token
            .clone()
            .ok_or_else(|| Error::Oauth(format!("no refresh token stored; {RELINK_HINT}")))?;
        let refreshed = self.refresh(&refresh_token).await?;
        entries[index].token = refreshed.clone();
        store_entries(&self.tokens_path, &entries)?;
        tracing::debug!("access token refreshed");
        *cached = Some(refreshed.clone());
        Ok(refreshed)
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenInfo> {
        let response = self
            .client
            .post(&self.token_uri)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| Error::Network(format!("token refresh: {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| Error::Network(format!("token refresh: {e}")))?;
        classify_refresh_response(status, &body, refresh_token)
    }
}

#[async_trait::async_trait]
impl TokenProvider for RefreshOnlyProvider {
    async fn token(&self) -> Result<String> {
        access_token_of(&self.current().await?)
    }

    async fn rclone_token(&self) -> Result<RcloneToken> {
        let token = self.current().await?;
        let expiry_rfc3339 = token
            .expires_at
            .map(|dt| {
                dt.format(&time::format_description::well_known::Rfc3339)
                    .map_err(|e| Error::Oauth(format!("format token expiry: {e}")))
            })
            .transpose()?;
        Ok(RcloneToken {
            access_token: access_token_of(&token)?,
            refresh_token: token.refresh_token,
            expiry_rfc3339,
        })
    }
}

fn access_token_of(token: &TokenInfo) -> Result<String> {
    token
        .access_token
        .clone()
        .ok_or_else(|| Error::Oauth(format!("stored token has no access token; {RELINK_HINT}")))
}

fn is_usable(token: &TokenInfo) -> bool {
    token.access_token.is_some() && !token.is_expired()
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct RefreshErrorBody {
    error: String,
    error_description: Option<String>,
}

fn classify_refresh_response(
    status: reqwest::StatusCode,
    body: &str,
    previous_refresh_token: &str,
) -> Result<TokenInfo> {
    if status.is_success() {
        let parsed: RefreshResponse = serde_json::from_str(body)
            .map_err(|e| Error::Oauth(format!("unexpected token refresh response: {e}")))?;
        return Ok(TokenInfo {
            access_token: Some(parsed.access_token),
            refresh_token: Some(
                parsed
                    .refresh_token
                    .unwrap_or_else(|| previous_refresh_token.to_owned()),
            ),
            expires_at: parsed
                .expires_in
                .map(|secs| OffsetDateTime::now_utc() + time::Duration::seconds(secs)),
            id_token: parsed.id_token,
        });
    }
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(Error::Network(format!("token refresh: HTTP {status}")));
    }
    let reason = match serde_json::from_str::<RefreshErrorBody>(body) {
        Ok(RefreshErrorBody {
            error,
            error_description: Some(description),
        }) => format!("{error}: {description}"),
        Ok(RefreshErrorBody { error, .. }) => error,
        Err(_) => format!("HTTP {status}"),
    };
    Err(Error::Oauth(format!(
        "refresh token rejected ({reason}); {RELINK_HINT}"
    )))
}

fn load_entries(path: &Path) -> Result<Vec<StoredToken>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Oauth(format!(
                "{} not found; {RELINK_HINT}",
                path.display()
            )));
        }
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Oauth(format!("parse {}: {e}", path.display())))
}

/// Prefer the entry scoped to Drive; otherwise the first one carrying a refresh
/// token (mirrors how `yup-oauth2` resolves a superset of the requested scopes).
fn select_drive_entry(entries: &[StoredToken]) -> Option<usize> {
    let drive_scope = DRIVE_SCOPES[0];
    entries
        .iter()
        .position(|e| e.scopes.iter().any(|s| s == drive_scope))
        .or_else(|| entries.iter().position(|e| e.token.refresh_token.is_some()))
}

/// Write through a sibling temp file + rename so a crash mid-write never leaves a
/// truncated `tokens.json` (which would force a full re-link).
fn store_entries(path: &Path, entries: &[StoredToken]) -> Result<()> {
    use std::io::Write;

    let json =
        serde_json::to_vec(entries).map_err(|e| Error::Oauth(format!("serialize tokens: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn token(access: &str, refresh: Option<&str>, expires_in_secs: i64) -> TokenInfo {
        TokenInfo {
            access_token: Some(access.into()),
            refresh_token: refresh.map(str::to_owned),
            expires_at: Some(OffsetDateTime::now_utc() + time::Duration::seconds(expires_in_secs)),
            id_token: None,
        }
    }

    fn write_tokens(dir: &Path, tok: TokenInfo) -> PathBuf {
        let path = dir.join("tokens.json");
        let entries = vec![StoredToken {
            scopes: vec![DRIVE_SCOPES[0].to_owned()],
            token: tok,
        }];
        store_entries(&path, &entries).unwrap();
        path
    }

    fn provider(token_uri: String, tokens_path: PathBuf) -> RefreshOnlyProvider {
        RefreshOnlyProvider::new("cid".into(), "csecret".into(), token_uri, tokens_path).unwrap()
    }

    #[tokio::test]
    async fn unexpired_token_is_served_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_tokens(tmp.path(), token("live", Some("rt"), 3600));
        let p = provider("http://127.0.0.1:1/token".into(), path);
        assert_eq!(p.token().await.unwrap(), "live");
    }

    #[tokio::test]
    async fn expired_token_is_refreshed_and_persisted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fresh",
                "expires_in": 3599,
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_tokens(tmp.path(), token("stale", Some("rt"), -10));
        let p = provider(format!("{}/token", server.uri()), path.clone());

        assert_eq!(p.token().await.unwrap(), "fresh");
        assert_eq!(p.token().await.unwrap(), "fresh");

        let stored = load_entries(&path).unwrap();
        assert_eq!(stored[0].token.access_token.as_deref(), Some("fresh"));
        assert_eq!(stored[0].token.refresh_token.as_deref(), Some("rt"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn unreachable_token_endpoint_is_transient() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_tokens(tmp.path(), token("stale", Some("rt"), -10));
        let p = provider("http://127.0.0.1:1/token".into(), path);
        match p.token().await {
            Err(Error::Network(_)) => {}
            other => panic!("expected Network, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn token_endpoint_5xx_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_tokens(tmp.path(), token("stale", Some("rt"), -10));
        let p = provider(format!("{}/token", server.uri()), path);
        match p.token().await {
            Err(Error::Network(_)) => {}
            other => panic!("expected Network, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_grant_requires_relink() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Token has been expired or revoked.",
            })))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_tokens(tmp.path(), token("stale", Some("rt"), -10));
        let p = provider(format!("{}/token", server.uri()), path);
        match p.token().await {
            Err(Error::Oauth(msg)) => {
                assert!(msg.contains("invalid_grant"), "{msg}");
                assert!(msg.contains("air-drive link"), "{msg}");
            }
            other => panic!("expected Oauth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_tokens_file_requires_relink() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(
            "http://127.0.0.1:1/token".into(),
            tmp.path().join("tokens.json"),
        );
        assert!(matches!(p.token().await, Err(Error::Oauth(_))));
    }

    #[tokio::test]
    async fn relinked_tokens_are_picked_up_without_restart() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
            })))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_tokens(tmp.path(), token("stale", Some("dead"), -10));
        let p = provider(format!("{}/token", server.uri()), path);
        assert!(matches!(p.token().await, Err(Error::Oauth(_))));

        write_tokens(tmp.path(), token("relinked", Some("new-rt"), 3600));
        assert_eq!(p.token().await.unwrap(), "relinked");
    }

    #[tokio::test]
    async fn rclone_token_carries_refresh_token_and_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_tokens(tmp.path(), token("live", Some("rt"), 3600));
        let p = provider("http://127.0.0.1:1/token".into(), path);
        let rt = p.rclone_token().await.unwrap();
        assert_eq!(rt.access_token, "live");
        assert_eq!(rt.refresh_token.as_deref(), Some("rt"));
        assert!(rt.expiry_rfc3339.is_some());
    }

    #[test]
    fn reads_yup_oauth2_on_disk_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tokens.json");
        let body = format!(
            r#"[{{"scopes":["{}"],"token":{{"access_token":"a","refresh_token":"r","expires_at":[2026,322,16,32,47,905994934,0,0,0],"id_token":null}}}}]"#,
            DRIVE_SCOPES[0]
        );
        std::fs::write(&path, body).unwrap();
        let entries = load_entries(&path).unwrap();
        assert_eq!(select_drive_entry(&entries), Some(0));
        assert_eq!(entries[0].token.refresh_token.as_deref(), Some("r"));
        assert!(entries[0].token.expires_at.is_some());
    }
}
