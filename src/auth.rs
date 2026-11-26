// src/auth.rs

//! Authentication: a static bearer token or OAuth2 client credentials
//!
//! [`Auth::Bearer`] sends a fixed token. [`Auth::ClientCredentials`] fetches a token from the
//! server's token endpoint (`grant_type=client_credentials`), caches it, renews it once 90% of
//! its lifetime (`expires_in`) has passed, and fetches a new one when the server answers 401.
//! Concurrent requests share one cached token and wait for a single fetch.
//!
//! Secrets (the bearer token, the client secret and fetched access tokens) are redacted from
//! `Debug` output and never logged or put in error messages.
//!
//! # Examples
//!
//! ```
//! # use reso_client::{Auth, ClientConfig, ClientCredentials, CredentialTransport};
//! let credentials = ClientCredentials::new(
//!     "https://auth.mls.example/oauth/token",
//!     "my-client-id",
//!     "my-client-secret",
//! )
//! .with_scope("api")
//! .with_transport(CredentialTransport::BasicHeader);
//!
//! let config = ClientConfig::with_auth("https://api.mls.example/OData", credentials);
//! assert!(matches!(config.auth, Auth::ClientCredentials(_)));
//! ```

use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::error::{ResoError, Result};

/// Share of a token's lifetime after which it is renewed (RESO's TypeScript client uses 90%).
const RENEW_AT_FRACTION: f64 = 0.9;

/// How the client id and secret are sent to the token endpoint
///
/// RFC 6749 recommends HTTP Basic; many RESO servers also accept form fields. The default is
/// [`Body`](Self::Body).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CredentialTransport {
    /// `client_id` and `client_secret` as form fields in the POST body (default)
    #[default]
    Body,
    /// HTTP Basic `Authorization` header with the client id and secret
    BasicHeader,
    /// `client_id` and `client_secret` in the token URL's query string
    Query,
}

impl CredentialTransport {
    /// Parse `body`, `header` (or `basic`) or `query`, ignoring case
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::CredentialTransport;
    /// assert_eq!(CredentialTransport::parse("Header"), Some(CredentialTransport::BasicHeader));
    /// assert_eq!(CredentialTransport::parse("cookie"), None);
    /// ```
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "body" => Some(Self::Body),
            "header" | "basic" => Some(Self::BasicHeader),
            "query" => Some(Self::Query),
            _ => None,
        }
    }
}

/// OAuth2 client credentials settings
///
/// # Examples
///
/// ```
/// # use reso_client::ClientCredentials;
/// let credentials = ClientCredentials::new(
///     "https://auth.mls.example/oauth/token",
///     "my-client-id",
///     "my-client-secret",
/// )
/// .with_scope("api");
///
/// // The secret never appears in Debug output
/// assert!(!format!("{:?}", credentials).contains("my-client-secret"));
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct ClientCredentials {
    /// Token endpoint URL
    pub token_url: String,
    /// OAuth2 client id
    pub client_id: String,
    /// OAuth2 client secret
    pub client_secret: String,
    /// Optional `scope` sent with the token request
    pub scope: Option<String>,
    /// How the id and secret are sent
    pub transport: CredentialTransport,
}

impl std::fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("scope", &self.scope)
            .field("transport", &self.transport)
            .finish()
    }
}

impl ClientCredentials {
    /// Create client credentials settings with no scope and the [`Body`](CredentialTransport::Body)
    /// transport
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scope: None,
            transport: CredentialTransport::default(),
        }
    }

    /// Set the `scope` sent with the token request
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Set how the id and secret are sent
    pub fn with_transport(mut self, transport: CredentialTransport) -> Self {
        self.transport = transport;
        self
    }
}

/// How the client authenticates to the server
///
/// # Examples
///
/// ```
/// # use reso_client::{Auth, ClientCredentials};
/// let bearer = Auth::Bearer("static-token".into());
/// let oauth: Auth = ClientCredentials::new("https://auth.example/token", "id", "secret").into();
///
/// assert!(!format!("{:?}", bearer).contains("static-token"));
/// assert!(matches!(oauth, Auth::ClientCredentials(_)));
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub enum Auth {
    /// A fixed bearer token, sent as is; a 401 is returned to the caller
    Bearer(String),
    /// OAuth2 client credentials: the token is fetched, cached and renewed
    ClientCredentials(ClientCredentials),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Bearer(_) => f.debug_tuple("Bearer").field(&"<redacted>").finish(),
            Auth::ClientCredentials(c) => f.debug_tuple("ClientCredentials").field(c).finish(),
        }
    }
}

impl From<ClientCredentials> for Auth {
    fn from(credentials: ClientCredentials) -> Self {
        Auth::ClientCredentials(credentials)
    }
}

impl From<String> for Auth {
    fn from(token: String) -> Self {
        Auth::Bearer(token)
    }
}

impl From<&str> for Auth {
    fn from(token: &str) -> Self {
        Auth::Bearer(token.to_string())
    }
}

/// A fetched access token and when to renew it (`None`: no `expires_in`, keep until a 401)
struct CachedToken {
    access_token: String,
    renew_at: Option<Instant>,
}

impl CachedToken {
    fn is_fresh(&self) -> bool {
        self.renew_at.is_none_or(|at| Instant::now() < at)
    }
}

/// Hands out access tokens for requests, fetching and caching them for client credentials
pub(crate) struct TokenProvider {
    auth: Auth,
    cache: Mutex<Option<CachedToken>>,
}

impl TokenProvider {
    pub(crate) fn new(auth: Auth) -> Self {
        Self {
            auth,
            cache: Mutex::new(None),
        }
    }

    /// The token to send now; fetches one when none is cached or the cached one is due for renewal
    pub(crate) async fn token(&self, http: &Client) -> Result<String> {
        let credentials = match &self.auth {
            Auth::Bearer(token) => return Ok(token.clone()),
            Auth::ClientCredentials(c) => c,
        };
        // Holding the lock across the fetch makes concurrent callers wait for one fetch.
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref().filter(|c| c.is_fresh()) {
            return Ok(cached.access_token.clone());
        }
        let fetched = fetch_token(http, credentials).await?;
        let token = fetched.access_token.clone();
        *cache = Some(fetched);
        Ok(token)
    }

    /// After the server rejected `rejected` with a 401, the token to retry with
    ///
    /// Returns `None` for a static bearer token (nothing to renew). When another request has
    /// already replaced the rejected token, that newer token is returned without a fetch.
    pub(crate) async fn renew_after_401(
        &self,
        http: &Client,
        rejected: &str,
    ) -> Result<Option<String>> {
        let credentials = match &self.auth {
            Auth::Bearer(_) => return Ok(None),
            Auth::ClientCredentials(c) => c,
        };
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            if cached.access_token != rejected && cached.is_fresh() {
                return Ok(Some(cached.access_token.clone()));
            }
        }
        info!("Server answered 401; fetching a new access token");
        let fetched = fetch_token(http, credentials).await?;
        let token = fetched.access_token.clone();
        *cache = Some(fetched);
        Ok(Some(token))
    }
}

/// POST a client credentials grant to the token endpoint
async fn fetch_token(http: &Client, credentials: &ClientCredentials) -> Result<CachedToken> {
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
    if let Some(scope) = credentials.scope.as_deref() {
        form.push(("scope", scope));
    }
    let id_and_secret = [
        ("client_id", credentials.client_id.as_str()),
        ("client_secret", credentials.client_secret.as_str()),
    ];

    let mut request = http
        .post(&credentials.token_url)
        .header("Accept", "application/json");
    match credentials.transport {
        CredentialTransport::Body => form.extend(id_and_secret),
        CredentialTransport::BasicHeader => {
            request = request.basic_auth(&credentials.client_id, Some(&credentials.client_secret));
        }
        CredentialTransport::Query => request = request.query(&id_and_secret),
    }

    debug!("Requesting an access token from {}", credentials.token_url);
    let requested_at = Instant::now();
    // `without_url` keeps a query-string secret out of the error message.
    let response = request
        .form(&form)
        .send()
        .await
        .map_err(|e| ResoError::TokenRequest(format!("request failed: {}", e.without_url())))?;

    let status = response.status();
    let body = response.text().await.map_err(|e| {
        ResoError::TokenRequest(format!("reading response failed: {}", e.without_url()))
    })?;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();

    if !status.is_success() {
        // Only the standard OAuth2 error fields are reported; the body itself is not, in case it
        // echoes the request.
        let mut message = format!("token endpoint returned {}", status.as_u16());
        if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
            message.push_str(&format!(": {}", error));
            if let Some(description) = json.get("error_description").and_then(|v| v.as_str()) {
                message.push_str(&format!(" ({})", description));
            }
        }
        return Err(ResoError::TokenRequest(message));
    }

    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ResoError::TokenRequest("token response has no access_token".to_string()))?
        .to_string();

    // `expires_in` is a number per RFC 6749; some servers send it as a string.
    let expires_in = json.get("expires_in").and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    });
    let renew_at = expires_in
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(|secs| requested_at + Duration::from_secs_f64(secs * RENEW_AT_FRACTION));
    debug!(
        "Access token received, expires_in: {:?} seconds",
        expires_in
    );

    Ok(CachedToken {
        access_token,
        renew_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_parse() {
        assert_eq!(
            CredentialTransport::parse("body"),
            Some(CredentialTransport::Body)
        );
        assert_eq!(
            CredentialTransport::parse(" BASIC "),
            Some(CredentialTransport::BasicHeader)
        );
        assert_eq!(
            CredentialTransport::parse("query"),
            Some(CredentialTransport::Query)
        );
        assert_eq!(CredentialTransport::parse(""), None);
    }

    #[test]
    fn debug_redacts_secrets() {
        let creds = ClientCredentials::new("https://auth.example/token", "the-id", "the-secret");
        let auth: Auth = creds.into();
        let text = format!("{:?}", auth);
        assert!(text.contains("the-id"));
        assert!(!text.contains("the-secret"));

        let bearer = Auth::from("the-token");
        assert!(!format!("{:?}", bearer).contains("the-token"));
    }

    #[test]
    fn cached_token_freshness() {
        let no_expiry = CachedToken {
            access_token: "a".into(),
            renew_at: None,
        };
        assert!(no_expiry.is_fresh());
        let past = CachedToken {
            access_token: "a".into(),
            renew_at: Some(Instant::now() - Duration::from_secs(1)),
        };
        assert!(!past.is_fresh());
    }
}
