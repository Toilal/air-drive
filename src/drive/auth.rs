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

/// Drive scopes requested by the daemon. `drive.file` covers everything we create or
/// open; `drive.metadata.readonly` lets `about.user` and `files.list` work for the
/// initial discovery.
pub const DRIVE_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/drive.file",
    "https://www.googleapis.com/auth/drive.metadata.readonly",
];

/// File name of the OAuth token cache inside the config directory.
pub const TOKENS_FILE: &str = "tokens.json";

/// Async provider of a fresh OAuth bearer token. Implementations MUST handle refresh
/// internally so callers always receive a valid (non-expired) token.
#[async_trait::async_trait]
pub trait TokenProvider: Send + Sync {
    /// Return a usable access token. The returned string SHOULD NOT include the
    /// `Bearer ` prefix — that's added by the HTTP layer.
    async fn token(&self) -> Result<String>;
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

/// Boxed async fetcher returning a fresh access token on each call. Used to hide the
/// concrete `Authenticator<C>` generic from callers — we don't want hyper-util/
/// hyper-rustls types leaking through our public surface.
type Fetcher = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync>;

/// yup-oauth2-backed provider. The token fetch is wrapped in a `Box<dyn Fn ...>` so
/// the connector type (`HttpsConnector<HttpConnector>` from hyper-util) stays internal.
pub struct YupOAuthProvider {
    fetcher: Fetcher,
}

#[async_trait::async_trait]
impl TokenProvider for YupOAuthProvider {
    async fn token(&self) -> Result<String> {
        (self.fetcher)().await
    }
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
            Ok(access)
        })
    });
    Ok(Arc::new(YupOAuthProvider { fetcher }))
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
