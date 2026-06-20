//! OAuth / token acquisition for the Drive API.
//!
//! Two implementations of [`TokenProvider`] live in this module:
//!
//! - [`StaticToken`] — fixed access token used by integration tests via
//!   `AIR_DRIVE_TEST_BEARER_TOKEN`. The factory short-circuits the OAuth dance whenever
//!   that env var is non-empty.
//! - [`YupOAuthProvider`] — wraps a [`yup_oauth2`] `InstalledFlowAuthenticator` configured
//!   with `HTTPRedirect` and the Drive scopes. Google's Desktop OAuth requires both
//!   a `client_id` and a `client_secret` at the token endpoint, distributed together
//!   with the app — the "secret" is a public identifier in this flow (cf. rclone,
//!   gcloud, Insync), the real auth proof is PKCE. Both are passed via
//!   `Config.oauth.client_id` + `Config.oauth.client_secret`.
//!
//! Token storage lives at `<config_dir>/tokens.json`. The file MUST be `0600` — the
//! factory refuses to start otherwise.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use time::OffsetDateTime;
use yup_oauth2::{ApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod};

use crate::config::OauthConfig;
use crate::error::{Error, Result};

/// Project-owned Google OAuth client ID used when `[oauth].client_id` is unset.
///
/// This is the public side of an OAuth client — distributed in every binary by design,
/// no secret involved. PKCE proves the redirect originated from the same process so a
/// distributed client_id is safe (RFC 7636). Users who prefer their own GCP project
/// override this via `Config.oauth.client_id`.
///
/// **MVP placeholder**: replace before the first public release.
pub const EMBEDDED_CLIENT_ID: &str = "REPLACE_BEFORE_RELEASE.apps.googleusercontent.com";

/// Drive scopes requested by the daemon. The full `drive` scope (read/write on
/// every file the user owns or has access to) is the only way to see content the
/// daemon did not itself create: `drive.file` restricts visibility to files
/// opened or written through the app, which makes syncing an already-populated
/// Drive folder impossible. Google flags `drive` as a sensitive scope — apps in
/// `Testing` mode keep refresh tokens for 7 days, and going to `Production` puts
/// the OAuth client through a verification review. That trade-off is accepted at
/// constitution level (see `CLAUDE.md`).
pub const DRIVE_SCOPES: &[&str] = &["https://www.googleapis.com/auth/drive"];

/// File name of the OAuth token cache inside the config directory.
pub const TOKENS_FILE: &str = "tokens.json";

/// Full set of credentials rclone needs to **self-refresh** the access token
/// during a single long-running operation (one that outlives the ~1 h token
/// lifetime). The daemon's own HTTP client only needs the access token; rclone,
/// as a separate process, needs the refresh token + client credentials too.
#[derive(Debug, Clone)]
pub struct RcloneToken {
    /// Current access token (no `Bearer ` prefix).
    pub access_token: String,
    /// Refresh token, when one is available. `None` means rclone cannot
    /// self-refresh — only the access token's remaining lifetime is usable.
    pub refresh_token: Option<String>,
    /// Access-token expiry as an RFC 3339 timestamp, when known. `None` lets the
    /// caller fall back to a far-future placeholder (the legacy behaviour).
    pub expiry_rfc3339: Option<String>,
}

/// Async provider of a fresh OAuth bearer token. Implementations MUST handle refresh
/// internally so callers always receive a valid (non-expired) token.
#[async_trait::async_trait]
pub trait TokenProvider: Send + Sync {
    /// Return a usable access token. The returned string SHOULD NOT include the
    /// `Bearer ` prefix — that's added by the HTTP layer.
    async fn token(&self) -> Result<String>;

    /// Return the credential bundle rclone needs to self-refresh. The default
    /// returns just the access token (no refresh token, no expiry), which keeps
    /// the legacy single-token behaviour for providers — like [`StaticToken`] —
    /// that have nothing more to offer.
    async fn rclone_token(&self) -> Result<RcloneToken> {
        Ok(RcloneToken {
            access_token: self.token().await?,
            refresh_token: None,
            expiry_rfc3339: None,
        })
    }
}

/// Constant-string token provider. Used in tests via `AIR_DRIVE_TEST_BEARER_TOKEN`
/// to bypass the OAuth dance entirely.
pub struct StaticToken(String);

impl StaticToken {
    /// Build a provider that always returns `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

#[async_trait::async_trait]
impl TokenProvider for StaticToken {
    async fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

/// A freshly fetched access token plus its expiry, as returned by yup-oauth2.
struct AccessAndExpiry {
    access_token: String,
    expiry: Option<OffsetDateTime>,
}

/// Boxed async fetcher returning a fresh access token (+ expiry) on each call. Used to
/// hide the concrete `Authenticator<C>` generic from callers — we don't want hyper-util/
/// hyper-rustls types leaking through our public surface.
type Fetcher =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<AccessAndExpiry>> + Send>> + Send + Sync>;

/// yup-oauth2-backed provider. The token fetch is wrapped in a `Box<dyn Fn ...>` so
/// the connector type (`HttpsConnector<HttpConnector>` from hyper-util) stays internal.
pub struct YupOAuthProvider {
    fetcher: Fetcher,
    /// Path to `tokens.json`, read to recover the refresh token for rclone.
    tokens_path: PathBuf,
}

#[async_trait::async_trait]
impl TokenProvider for YupOAuthProvider {
    async fn token(&self) -> Result<String> {
        Ok((self.fetcher)().await?.access_token)
    }

    async fn rclone_token(&self) -> Result<RcloneToken> {
        let AccessAndExpiry {
            access_token,
            expiry,
        } = (self.fetcher)().await?;
        let expiry_rfc3339 = match expiry {
            Some(dt) => Some(
                dt.format(&time::format_description::well_known::Rfc3339)
                    .map_err(|e| Error::Oauth(format!("format token expiry: {e}")))?,
            ),
            None => None,
        };
        Ok(RcloneToken {
            access_token,
            refresh_token: read_refresh_token(&self.tokens_path)?,
            expiry_rfc3339,
        })
    }
}

/// Recover the refresh token yup-oauth2 persisted in `tokens.json`.
///
/// The file is a JSON array of `{"scopes": [...], "token": {"refresh_token": ...,
/// "access_token": ..., "expires_at": ..., "id_token": ...}}`. We parse it as an
/// untyped [`serde_json::Value`] (not the typed `TokenInfo`) so we don't couple to
/// `time`'s `expires_at` serialization. Prefer the entry scoped to Drive; otherwise
/// take the first entry that carries a refresh token.
///
/// Returns `Ok(None)` when the file is absent or holds no refresh token — the caller
/// treats that as "rclone cannot self-refresh", not an error.
fn read_refresh_token(tokens_path: &Path) -> Result<Option<String>> {
    let bytes = match std::fs::read(tokens_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Oauth(format!("parse tokens.json: {e}")))?;
    let Some(entries) = v.as_array() else {
        return Ok(None);
    };
    let drive_scope = DRIVE_SCOPES[0];
    let scoped_to_drive = |e: &&serde_json::Value| {
        e.get("scopes")
            .and_then(|s| s.as_array())
            .is_some_and(|arr| arr.iter().any(|s| s.as_str() == Some(drive_scope)))
    };
    let has_refresh = |e: &&serde_json::Value| {
        e.pointer("/token/refresh_token")
            .and_then(|r| r.as_str())
            .is_some()
    };
    let pick = entries
        .iter()
        .find(scoped_to_drive)
        .or_else(|| entries.iter().find(has_refresh));
    Ok(pick
        .and_then(|e| e.pointer("/token/refresh_token"))
        .and_then(|r| r.as_str())
        .map(str::to_owned))
}

/// Decide which provider to use and return it behind `Arc<dyn TokenProvider>`.
///
/// Resolution order:
///
/// 1. If `test_bearer_override` is `Some(non-empty)` → [`StaticToken`]. Used by the
///    integration test harness via `AIR_DRIVE_TEST_BEARER_TOKEN` (read once at CLI
///    startup so we never touch `std::env::set_var` from the daemon code path —
///    `set_var` is `unsafe` under Rust 2024 and the crate forbids `unsafe_code`).
/// 2. Otherwise build a [`YupOAuthProvider`] backed by `tokens.json` in `config_dir`.
///    The token file MUST be `0600` if it already exists; missing is OK (yup-oauth2
///    will create it on first use).
pub async fn build_provider(
    oauth: &OauthConfig,
    config_dir: &Path,
    test_bearer_override: Option<&str>,
) -> Result<Arc<dyn TokenProvider>> {
    if let Some(tok) = test_bearer_override {
        if !tok.is_empty() {
            return Ok(Arc::new(StaticToken::new(tok)));
        }
    }

    let tokens_path = config_dir.join(TOKENS_FILE);
    enforce_owner_only_if_exists(&tokens_path)?;

    let secret = build_application_secret(oauth);
    let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
        .flow_delegate(Box::new(BrowserOpeningDelegate))
        .persist_tokens_to_disk(&tokens_path)
        .build()
        .await
        .map_err(|e| Error::Oauth(e.to_string()))?;

    // Clone the Authenticator into the closure on every invocation. `Authenticator` is
    // `Clone` (it wraps an inner `Arc`), and `token()` takes `&self`, so we can call it
    // without recreating the OAuth handshake state.
    let auth = Arc::new(auth);
    let fetcher: Fetcher = Box::new(move || {
        let auth = Arc::clone(&auth);
        Box::pin(async move {
            let t = auth
                .token(DRIVE_SCOPES)
                .await
                .map_err(|e| Error::Oauth(e.to_string()))?;
            let access = t
                .token()
                .ok_or_else(|| Error::Oauth("token() returned no access token".into()))?
                .to_owned();
            Ok(AccessAndExpiry {
                access_token: access,
                expiry: t.expiration_time(),
            })
        })
    });
    Ok(Arc::new(YupOAuthProvider {
        fetcher,
        tokens_path,
    }))
}

/// `InstalledFlowDelegate` that opens the OAuth consent URL in the user's default
/// browser instead of just printing it to stdout. yup-oauth2's stock delegate
/// (`DefaultInstalledFlowDelegate`) only emits the URL — useful for headless setups
/// but a poor UX for the interactive `link` / setup-e2e flows.
///
/// On a headless host (`webbrowser::open` returns `Err`) we still print the URL so
/// the user can copy/paste from the terminal.
struct BrowserOpeningDelegate;

impl yup_oauth2::authenticator_delegate::InstalledFlowDelegate for BrowserOpeningDelegate {
    fn present_user_url<'a>(
        &'a self,
        url: &'a str,
        _need_code: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Always print first so the user has a copy/paste fallback if the auto-open
            // fails or sends them to the wrong browser profile.
            eprintln!("[auth] open this URL in your test-account browser:\n  {url}");
            match webbrowser::open(url) {
                Ok(()) => {
                    eprintln!("[auth] browser launched — complete the consent + grant flow there");
                }
                Err(e) => {
                    eprintln!("[auth] could not auto-open browser ({e}); use the URL above");
                }
            }
            Ok(String::new())
        })
    }
}

/// Build the [`ApplicationSecret`] from the daemon's [`OauthConfig`]. Endpoints can be
/// overridden by `AIR_DRIVE_OAUTH_TOKEN_URL` / `AIR_DRIVE_OAUTH_AUTH_URL` so the mock
/// in integration tests captures the dance (when a test ever drives the real flow —
/// most tests use [`StaticToken`] instead).
fn build_application_secret(oauth: &OauthConfig) -> ApplicationSecret {
    let client_id = oauth
        .client_id
        .clone()
        .unwrap_or_else(|| EMBEDDED_CLIENT_ID.to_owned());
    // Google's Desktop OAuth client REQUIRES a client_secret at the token endpoint
    // even though PKCE handles the actual proof — Google's spec is stricter than
    // the IETF Installed App profile. The "secret" is distributed with the app
    // (cf. rclone, gcloud, Insync) and serves as a client identifier, not real
    // confidentiality. When the user doesn't supply one we fall back to an empty
    // string so the obviously-misconfigured case fails with Google's own
    // "client_secret is missing" message rather than a silent local crash.
    let client_secret = oauth.client_secret.clone().unwrap_or_default();
    ApplicationSecret {
        client_id,
        client_secret,
        auth_uri: std::env::var("AIR_DRIVE_OAUTH_AUTH_URL")
            .unwrap_or_else(|_| "https://accounts.google.com/o/oauth2/auth".into()),
        token_uri: std::env::var("AIR_DRIVE_OAUTH_TOKEN_URL")
            .unwrap_or_else(|_| "https://oauth2.googleapis.com/token".into()),
        auth_provider_x509_cert_url: Some("https://www.googleapis.com/oauth2/v1/certs".into()),
        redirect_uris: vec!["http://127.0.0.1".into()],
        project_id: None,
        client_email: None,
        client_x509_cert_url: None,
    }
}

/// Refuse to start when the file exists and has world- or group-readable bits set.
/// Missing files are accepted — yup-oauth2 creates the file on first token write.
fn enforce_owner_only_if_exists(path: &PathBuf) -> Result<()> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(Error::InsecurePermissions {
                path: path.clone(),
                got: mode,
                want: 0o600,
            });
        }
    }
    let _ = meta; // silence non-unix warning
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_token_returns_constant() {
        let p = StaticToken::new("abc");
        assert_eq!(p.token().await.unwrap(), "abc");
        assert_eq!(p.token().await.unwrap(), "abc");
    }

    #[tokio::test]
    async fn static_token_rclone_token_has_no_refresh_or_expiry() {
        let p = StaticToken::new("abc");
        let rt = p.rclone_token().await.unwrap();
        assert_eq!(rt.access_token, "abc");
        assert!(rt.refresh_token.is_none());
        assert!(rt.expiry_rfc3339.is_none());
    }

    #[test]
    fn read_refresh_token_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let got = read_refresh_token(&tmp.path().join("nope.json")).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_refresh_token_prefers_drive_scoped_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(TOKENS_FILE);
        // Shape produced by yup-oauth2's on-disk JSONTokens: an array of
        // {scopes, token:{...}} entries. A non-Drive entry comes first to prove
        // the Drive-scoped one is selected.
        let body = format!(
            r#"[
              {{"scopes":["https://www.googleapis.com/auth/other"],
                "token":{{"access_token":"a0","refresh_token":"other-rt","expires_at":"2099-01-01T00:00:00Z","id_token":null}}}},
              {{"scopes":["{drive}"],
                "token":{{"access_token":"a1","refresh_token":"drive-rt","expires_at":"2099-01-01T00:00:00Z","id_token":null}}}}
            ]"#,
            drive = DRIVE_SCOPES[0]
        );
        std::fs::write(&path, body).unwrap();
        let got = read_refresh_token(&path).unwrap();
        assert_eq!(got.as_deref(), Some("drive-rt"));
    }

    #[test]
    fn read_refresh_token_falls_back_to_first_with_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(TOKENS_FILE);
        // No Drive-scoped entry; the first entry carrying a refresh token wins.
        let body = r#"[
          {"scopes":["https://www.googleapis.com/auth/other"],
            "token":{"access_token":"a0","refresh_token":"only-rt","expires_at":"2099-01-01T00:00:00Z","id_token":null}}
        ]"#;
        std::fs::write(&path, body).unwrap();
        let got = read_refresh_token(&path).unwrap();
        assert_eq!(got.as_deref(), Some("only-rt"));
    }

    #[tokio::test]
    async fn factory_honours_test_bearer_override() {
        let tmp = tempfile::tempdir().unwrap();
        let p = build_provider(&OauthConfig::default(), tmp.path(), Some("fixed-tok"))
            .await
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        assert_eq!(p.token().await.unwrap(), "fixed-tok");
    }

    #[tokio::test]
    async fn empty_test_bearer_falls_through_to_oauth() {
        // An empty override string MUST behave like "no override". We can't easily run
        // the real OAuth path here; instead we look at the only side-effect that would
        // happen before contacting Google: the tokens-perm check. If we hit the perm
        // error, that proves the override was ignored.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(TOKENS_FILE);
            std::fs::write(&path, b"{}").unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();
            match build_provider(&OauthConfig::default(), tmp.path(), Some("")).await {
                Err(Error::InsecurePermissions { .. }) => {}
                Err(other) => panic!("empty override should hit perms check, got {other:?}"),
                Ok(_) => panic!("empty override should not return a provider"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn factory_refuses_world_readable_tokens_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(TOKENS_FILE);
        std::fs::write(&path, b"{}").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644); // too loose
        std::fs::set_permissions(&path, perms).unwrap();
        let err = match build_provider(&OauthConfig::default(), tmp.path(), None).await {
            Ok(_) => panic!("0644 tokens.json must be rejected"),
            Err(e) => e,
        };
        match err {
            Error::InsecurePermissions { got, want, .. } => {
                assert_eq!(got, 0o644);
                assert_eq!(want, 0o600);
            }
            other => panic!("expected InsecurePermissions, got {other:?}"),
        }
    }
}
