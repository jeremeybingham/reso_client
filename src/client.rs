// src/client.rs

//! Client configuration and connection management

use std::time::Duration;

use reqwest::Client;
use tracing::debug;

use crate::auth::{Auth, ClientCredentials, CredentialTransport, TokenProvider};
use crate::error::{ResoError, Result};
use crate::retry::{
    is_retryable_status, parse_retry_after, ClientStats, Counters, Limiter, RateLimit, RetryPolicy,
};

/// Configuration for RESO client
///
/// Holds all configuration needed to connect to a RESO Web API server,
/// including the base URL, authentication ([`Auth`]: a static bearer token or
/// OAuth2 client credentials), optional dataset ID, and HTTP timeout settings.
///
/// # Examples
///
/// ```
/// # use reso_client::ClientConfig;
/// # use std::time::Duration;
/// // Create basic configuration
/// let config = ClientConfig::new(
///     "https://api.mls.com/odata",
///     "your-token"
/// );
///
/// // With dataset ID
/// let config = ClientConfig::new(
///     "https://api.mls.com/odata",
///     "your-token"
/// )
/// .with_dataset_id("actris_ref");
///
/// // With custom timeout
/// let config = ClientConfig::new(
///     "https://api.mls.com/odata",
///     "your-token"
/// )
/// .with_timeout(Duration::from_secs(60));
///
/// // With OAuth2 client credentials
/// # use reso_client::ClientCredentials;
/// let config = ClientConfig::with_auth(
///     "https://api.mls.com/odata",
///     ClientCredentials::new("https://auth.mls.com/token", "client-id", "client-secret"),
/// );
/// ```
#[derive(Clone)]
pub struct ClientConfig {
    /// Base URL of the RESO Web API server
    pub base_url: String,

    /// How requests are authenticated
    pub auth: Auth,

    /// Optional dataset ID (inserted between base_url and resource). Bridge Interactive-specific: it is not part of the
    /// RESO Web API standard, so leave it unset for other servers.
    pub dataset_id: Option<String>,

    /// HTTP timeout duration
    pub timeout: Duration,

    /// Retry and backoff for failed requests (default: never retry)
    pub retry: RetryPolicy,

    /// Client-side request and byte ceilings (default: none)
    pub rate_limit: RateLimit,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("dataset_id", &self.dataset_id)
            .field("timeout", &self.timeout)
            .field("retry", &self.retry)
            .field("rate_limit", &self.rate_limit)
            .finish()
    }
}

impl ClientConfig {
    /// Create configuration from environment variables
    ///
    /// Expects:
    /// - `RESO_BASE_URL` - Base URL of the RESO server (e.g., `https://api.mls.com/api/v2/OData`)
    /// - Either OAuth2 client credentials:
    ///   - `RESO_CLIENT_ID`, `RESO_CLIENT_SECRET`, `RESO_TOKEN_URI` - client id, secret and
    ///     token endpoint
    ///   - `RESO_SCOPE` (optional) - scope sent with the token request
    ///   - `RESO_CREDENTIAL_TRANSPORT` (optional) - `body` (default), `header` or `query`
    /// - or `RESO_TOKEN` - a static bearer token (used when `RESO_CLIENT_ID` is not set)
    /// - `RESO_DATASET_ID` (optional) - Dataset ID inserted in URL path
    /// - `RESO_TIMEOUT` (optional) - Timeout in seconds (default: 30)
    /// - `RESO_MAX_RETRIES` (optional) - Retries for 429/502/503/504 and network errors (default: 0)
    /// - `RESO_RATE_PER_SECOND`, `RESO_RATE_PER_HOUR`, `RESO_BYTES_PER_DAY` (optional) - client-side
    ///   [`RateLimit`] (default: none)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::ClientConfig;
    /// // Reads RESO_BASE_URL, RESO_TOKEN, and optional variables from environment
    /// let config = ClientConfig::from_env()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("RESO_BASE_URL")
            .map_err(|_| ResoError::Config("RESO_BASE_URL not set".into()))?;

        let auth = auth_from_env()?;

        let dataset_id = std::env::var("RESO_DATASET_ID").ok();

        let timeout_secs = std::env::var("RESO_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        let number = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
        let mut rate_limit = RateLimit::default();
        if let Some(n) = number("RESO_RATE_PER_SECOND") {
            rate_limit = rate_limit.per_second(n.min(u32::MAX as u64) as u32);
        }
        if let Some(n) = number("RESO_RATE_PER_HOUR") {
            rate_limit = rate_limit.per_hour(n.min(u32::MAX as u64) as u32);
        }
        if let Some(n) = number("RESO_BYTES_PER_DAY") {
            rate_limit = rate_limit.bytes_per_day(n);
        }

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            dataset_id,
            timeout: Duration::from_secs(timeout_secs),
            retry: RetryPolicy::new(
                number("RESO_MAX_RETRIES").unwrap_or(0).min(u32::MAX as u64) as u32
            ),
            rate_limit,
        })
    }

    /// Create configuration with a static bearer token
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ClientConfig;
    /// let config = ClientConfig::new(
    ///     "https://api.mls.com/odata",
    ///     "your-bearer-token"
    /// );
    /// ```
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_auth(base_url, Auth::Bearer(token.into()))
    }

    /// Create configuration with any [`Auth`], such as OAuth2 [`ClientCredentials`]
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::{ClientConfig, ClientCredentials};
    /// let config = ClientConfig::with_auth(
    ///     "https://api.mls.com/odata",
    ///     ClientCredentials::new("https://auth.mls.com/token", "client-id", "client-secret")
    ///         .with_scope("api"),
    /// );
    /// ```
    pub fn with_auth(base_url: impl Into<String>, auth: impl Into<Auth>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: auth.into(),
            dataset_id: None,
            timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            rate_limit: RateLimit::default(),
        }
    }

    /// Set dataset ID
    ///
    /// **Bridge Interactive-specific.** Bridge servers put a dataset identifier in the URL path; the RESO Web API
    /// standard does not, so leave this unset for other servers.
    /// When set, URLs will be formatted as: `{base_url}/{dataset_id}/{resource}`
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ClientConfig;
    /// let config = ClientConfig::new("https://api.mls.com/odata", "token")
    ///     .with_dataset_id("actris_ref");
    /// ```
    pub fn with_dataset_id(mut self, dataset_id: impl Into<String>) -> Self {
        self.dataset_id = Some(dataset_id.into());
        self
    }

    /// Set custom timeout
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ClientConfig;
    /// # use std::time::Duration;
    /// let config = ClientConfig::new("https://api.mls.com/odata", "token")
    ///     .with_timeout(Duration::from_secs(60));
    /// ```
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Retry failed requests
    ///
    /// Network errors and 429/502/503/504 answers are retried with exponential backoff and
    /// jitter, honouring `Retry-After`; see [`RetryPolicy`]. Other 4xx answers are never retried.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::{ClientConfig, RetryPolicy};
    /// let config = ClientConfig::new("https://api.mls.com/odata", "token")
    ///     .with_retry(RetryPolicy::new(5));
    /// ```
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Limit how fast this client sends requests and receives bytes; see [`RateLimit`]
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::{ClientConfig, RateLimit};
    /// let config = ClientConfig::new("https://api.mls.com/odata", "token")
    ///     .with_rate_limit(RateLimit::default().per_second(2).per_hour(5000));
    /// ```
    pub fn with_rate_limit(mut self, rate_limit: RateLimit) -> Self {
        self.rate_limit = rate_limit;
        self
    }
}

/// Read the auth settings: client credentials when `RESO_CLIENT_ID` is set, else `RESO_TOKEN`
fn auth_from_env() -> Result<Auth> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let Some(client_id) = var("RESO_CLIENT_ID") else {
        return var("RESO_TOKEN").map(Auth::Bearer).ok_or_else(|| {
            ResoError::Config(
                "RESO_TOKEN not set (or set RESO_CLIENT_ID, RESO_CLIENT_SECRET and RESO_TOKEN_URI)"
                    .into(),
            )
        });
    };
    let required = |name: &str| {
        var(name).ok_or_else(|| {
            ResoError::Config(format!("{} not set (needed with RESO_CLIENT_ID)", name))
        })
    };
    let mut credentials = ClientCredentials::new(
        required("RESO_TOKEN_URI")?,
        client_id,
        required("RESO_CLIENT_SECRET")?,
    );
    if let Some(scope) = var("RESO_SCOPE") {
        credentials = credentials.with_scope(scope);
    }
    if let Some(transport) = var("RESO_CREDENTIAL_TRANSPORT") {
        let transport = CredentialTransport::parse(&transport).ok_or_else(|| {
            ResoError::Config(format!(
                "RESO_CREDENTIAL_TRANSPORT must be body, header or query, not '{}'",
                transport
            ))
        })?;
        credentials = credentials.with_transport(transport);
    }
    Ok(Auth::ClientCredentials(credentials))
}

/// RESO Web API client
///
/// Requests are authenticated with the configured [`Auth`]. With OAuth2 client credentials the
/// client fetches a token on first use, reuses it until 90% of its lifetime has passed, and on a
/// 401 fetches a new token and retries the request once. Share one client (for example in an
/// `Arc`) so concurrent requests share its token.
pub struct ResoClient {
    config: ClientConfig,
    http_client: Client,
    tokens: TokenProvider,
    limiter: Limiter,
    counters: Counters,
}

impl ResoClient {
    /// Create a new client from environment variables
    ///
    /// # Environment Variables
    ///
    /// - `RESO_BASE_URL` - Base URL of the RESO server (required)
    ///   Example: `https://api.mls.com/api/v2/OData`
    /// - `RESO_TOKEN` - OAuth bearer token (required)
    /// - `RESO_DATASET_ID` - Dataset ID for URL path (optional)
    /// - `RESO_TIMEOUT` - Timeout in seconds (optional, default: 30)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::ResoClient;
    /// let client = ResoClient::from_env()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_env() -> Result<Self> {
        let config = ClientConfig::from_env()?;
        Self::with_config(config)
    }

    /// Create a new client with manual configuration
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, ClientConfig};
    /// let config = ClientConfig::new(
    ///     "https://api.mls.com/reso/odata",
    ///     "your-token"
    /// );
    /// let client = ResoClient::with_config(config)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn with_config(config: ClientConfig) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| ResoError::Config(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self {
            tokens: TokenProvider::new(config.auth.clone()),
            limiter: Limiter::new(config.rate_limit.clone()),
            counters: Counters::default(),
            config,
            http_client,
        })
    }

    /// Get the base URL
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, ClientConfig};
    /// let config = ClientConfig::new("https://api.mls.com/odata", "token");
    /// let client = ResoClient::with_config(config)?;
    /// assert_eq!(client.base_url(), "https://api.mls.com/odata");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// Counters of requests sent, retries made and response bytes received so far
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, ClientConfig};
    /// let client = ResoClient::with_config(ClientConfig::new("https://api.mls.com/odata", "token"))?;
    /// let stats = client.stats();
    /// println!("{} requests, {} retries, {} bytes", stats.requests, stats.retries, stats.bytes);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn stats(&self) -> ClientStats {
        self.counters.snapshot()
    }

    /// The token provider, shared with a [`Downloader`](crate::Downloader)
    pub(crate) fn tokens(&self) -> &TokenProvider {
        &self.tokens
    }

    /// The HTTP client used for API and token requests
    pub(crate) fn http(&self) -> &Client {
        &self.http_client
    }

    /// Build full URL with optional dataset_id
    ///
    /// Some RESO servers require a dataset ID in the URL path between the base URL
    /// and the resource/query path (e.g., `https://api.mls.com/odata/{dataset_id}/Property`).
    /// This method handles both cases transparently.
    pub(crate) fn build_url(&self, path: &str) -> String {
        match &self.config.dataset_id {
            Some(dataset_id) => format!("{}/{}/{}", self.config.base_url, dataset_id, path),
            None => format!("{}/{}", self.config.base_url, path),
        }
    }

    /// Send an authenticated GET request, retrying per the [`RetryPolicy`], and handle errors
    ///
    /// 1. Waits for the rate limit, then sends the GET with the Authorization header
    /// 2. On a 401 with client credentials, renews the token and repeats once
    /// 3. On a network error or 429/502/503/504, waits (`Retry-After` or backoff) and repeats up
    ///    to `max_retries` times; after that the last error is returned
    /// 4. Converts error responses to the matching ResoError variant
    async fn send_authenticated_request(
        &self,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::Response> {
        let policy = &self.config.retry;
        let mut retries = 0u32;
        loop {
            let outcome = self.attempt(url, accept).await;

            let retryable = match &outcome {
                Err(ResoError::Network(_)) => true,
                Err(_) => false,
                Ok(response) => is_retryable_status(response.status().as_u16()),
            };
            if retryable && retries < policy.max_retries {
                let retry_after = match &outcome {
                    Ok(response) => response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| parse_retry_after(v, std::time::SystemTime::now())),
                    Err(_) => None,
                };
                if let Some(delay) = policy.delay_for(retries, retry_after) {
                    debug!(
                        "Retry {} of {} in {:?} ({})",
                        retries + 1,
                        policy.max_retries,
                        delay,
                        match &outcome {
                            Ok(response) => response.status().to_string(),
                            Err(_) => "network error".to_string(),
                        }
                    );
                    self.counters.retry();
                    drop(outcome);
                    tokio::time::sleep(delay).await;
                    retries += 1;
                    continue;
                }
            }

            let response = outcome?;
            let status = response.status();

            // Check for error responses and extract the body for detailed error information
            if !status.is_success() {
                let body = self.read_bytes(response).await.unwrap_or_default();
                // from_status() parses OData error format if present and maps to appropriate error variant
                return Err(ResoError::from_status(
                    status.as_u16(),
                    &String::from_utf8_lossy(&body),
                ));
            }

            return Ok(response);
        }
    }

    /// One attempt: token, rate limit, request, and the one-time renewal after a 401
    async fn attempt(&self, url: &str, accept: &str) -> Result<reqwest::Response> {
        let token = self.tokens.token(&self.http_client).await?;
        let mut response = self.send_get(url, accept, &token).await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(renewed) = self
                .tokens
                .renew_after_401(&self.http_client, &token)
                .await?
            {
                debug!("Retrying once with a new access token");
                response = self.send_get(url, accept, &renewed).await?;
            }
        }
        Ok(response)
    }

    /// Send one GET with the given bearer token, after waiting for the rate limit
    async fn send_get(&self, url: &str, accept: &str, token: &str) -> Result<reqwest::Response> {
        self.limiter.acquire().await;
        self.counters.request();
        self.http_client
            .get(url)
            .bearer_auth(token)
            .header("Accept", accept)
            .send()
            .await
            .map_err(|e| ResoError::Network(e.to_string()))
    }

    /// Read a response body, counting its bytes
    async fn read_bytes(&self, response: reqwest::Response) -> Result<Vec<u8>> {
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ResoError::Parse(format!("Failed to read response: {}", e)))?;
        self.counters.bytes(bytes.len() as u64);
        self.limiter.record_bytes(bytes.len() as u64).await;
        Ok(bytes.to_vec())
    }

    /// Parse JSON response from a successful request
    async fn parse_json_response(&self, response: reqwest::Response) -> Result<serde_json::Value> {
        let bytes = self.read_bytes(response).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| ResoError::Parse(format!("Failed to parse JSON: {}", e)))
    }

    /// Parse text response from a successful request
    async fn parse_text_response(&self, response: reqwest::Response) -> Result<String> {
        let bytes = self.read_bytes(response).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Execute a query and return raw JSON
    ///
    /// Executes a standard OData query and returns the full JSON response.
    /// The response follows the OData format with records in a `value` array.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, QueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let query = QueryBuilder::new("Property")
    ///     .filter("City eq 'Austin'")
    ///     .select(&["ListingKey", "City", "ListPrice"])
    ///     .top(10)
    ///     .build()?;
    ///
    /// let results = client.execute(&query).await?;
    ///
    /// // Access records from OData response
    /// if let Some(records) = results["value"].as_array() {
    ///     for record in records {
    ///         println!("{}", record["ListingKey"]);
    ///     }
    /// }
    ///
    /// // Access count if requested with with_count()
    /// if let Some(count) = results["@odata.count"].as_u64() {
    ///     println!("Total: {}", count);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute(&self, query: &crate::queries::Query) -> Result<serde_json::Value> {
        use tracing::{debug, info};

        let url = self.build_url(&query.to_odata_string());
        info!("Executing query: {}", url);

        let response = self
            .send_authenticated_request(&url, "application/json")
            .await?;
        let json = self.parse_json_response(response).await?;

        debug!(
            "Query result: {} records",
            json.get("value")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0)
        );

        Ok(json)
    }

    /// Execute a direct key access query and return a single record
    ///
    /// Direct key access queries (e.g., `Property('12345')`) return a single object
    /// instead of an array wrapped in `{"value": [...]}`. This method is optimized
    /// for such queries.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, QueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// // Fetch a single property by key
    /// let query = QueryBuilder::by_key("Property", "12345")
    ///     .select(&["ListingKey", "City", "ListPrice"])
    ///     .build()?;
    ///
    /// let record = client.execute_by_key(&query).await?;
    ///
    /// // With expand
    /// let query = QueryBuilder::by_key("Property", "12345")
    ///     .expand(&["ListOffice", "ListAgent"])
    ///     .build()?;
    ///
    /// let record = client.execute_by_key(&query).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_by_key(&self, query: &crate::queries::Query) -> Result<serde_json::Value> {
        use tracing::info;

        let url = self.build_url(&query.to_odata_string());
        info!("Executing key access query: {}", url);

        let response = self
            .send_authenticated_request(&url, "application/json")
            .await?;
        self.parse_json_response(response).await
    }

    /// Execute a count-only query and return the count as an integer
    ///
    /// Sends `$count=true&$top=0` (plus the query's `$filter`) on the normal resource path and
    /// reads `@odata.count` from the JSON body, so no records are fetched. This works on
    /// servers that have no `/Resource/$count` route, such as the RESO reference server. The
    /// server must support `$count=true`; if it does not return `@odata.count`, a
    /// [`ResoError::Parse`] is returned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, QueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let query = QueryBuilder::new("Property")
    ///     .filter("City eq 'Austin'")
    ///     .count()
    ///     .build()?;
    ///
    /// let count = client.execute_count(&query).await?;
    /// println!("Total properties in Austin: {}", count);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_count(&self, query: &crate::queries::Query) -> Result<u64> {
        use tracing::info;

        let url = self.build_url(&query.to_odata_string());
        info!("Executing count query: {}", url);

        let response = self
            .send_authenticated_request(&url, "application/json")
            .await?;
        let body = self.parse_json_response(response).await?;

        let count = body
            .get("@odata.count")
            .and_then(|c| {
                c.as_u64()
                    .or_else(|| c.as_str().and_then(|t| t.trim().parse().ok()))
            })
            .ok_or_else(|| {
                ResoError::Parse(
                    "Failed to parse count: no numeric @odata.count in response".into(),
                )
            })?;

        info!("Count result: {}", count);

        Ok(count)
    }

    /// Fetch $metadata XML
    ///
    /// Retrieves the OData metadata document which describes the schema,
    /// entity types, properties, and relationships available in the API.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::ResoClient;
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let metadata = client.fetch_metadata().await?;
    ///
    /// // Parse or save the XML metadata
    /// println!("Metadata length: {} bytes", metadata.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch_metadata(&self) -> Result<String> {
        use tracing::info;

        let url = self.build_url("$metadata");
        info!("Fetching metadata from: {}", url);

        let response = self
            .send_authenticated_request(&url, "application/xml")
            .await?;
        self.parse_text_response(response).await
    }

    /// Fetch `$metadata` and parse it into a [`Metadata`](crate::Metadata) model (entity sets, keys, properties,
    /// navigation properties, enum types).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::ResoClient;
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let model = client.fetch_metadata_model().await?;
    /// for name in model.resource_names() {
    ///     println!("{}", name);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch_metadata_model(&self) -> Result<crate::metadata::Metadata> {
        crate::metadata::Metadata::parse(&self.fetch_metadata().await?)
    }

    /// Execute a replication query
    ///
    /// The replication endpoint is designed for bulk data transfer and supports
    /// up to 2000 records per request. The response includes a `next` link in
    /// the headers for pagination through large datasets.
    ///
    /// # Important Notes
    ///
    /// - Replication functionality requires MLS authorization
    /// - Results are ordered oldest to newest by default
    /// - Use `$select` to reduce payload size and improve performance
    /// - For datasets >10,000 records, replication is required
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, ReplicationQueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let query = ReplicationQueryBuilder::new("Property")
    ///     .filter("StandardStatus eq 'Active'")
    ///     .select(&["ListingKey", "City", "ListPrice"])
    ///     .top(2000)
    ///     .build()?;
    ///
    /// let response = client.execute_replication(&query).await?;
    ///
    /// println!("Retrieved {} records", response.record_count);
    ///
    /// // Continue with next link if more records available
    /// if let Some(next_link) = response.next_link {
    ///     let next_response = client.execute_next_link(&next_link).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_replication(
        &self,
        query: &crate::queries::ReplicationQuery,
    ) -> Result<crate::replication::ReplicationResponse> {
        use tracing::{debug, info};

        let url = self.build_url(&query.to_odata_string());
        info!("Executing replication query: {}", url);

        let page = self.get_page(&url).await?;
        debug!(
            "Retrieved {} records, next link: {:?}",
            page.records.len(),
            page.next_link
        );

        Ok(crate::replication::ReplicationResponse::new(
            page.records,
            page.next_link,
        ))
    }

    /// Execute a next link from a previous response
    ///
    /// Takes the URL from a previous response's next link and fetches the next batch of
    /// records. The next link of the new batch is read the same way as
    /// [`execute_page`](Self::execute_page) reads it: the body's `@odata.nextLink`, else a
    /// `next` header, else a `Link` header with `rel="next"`, resolved to an absolute URL.
    ///
    /// For standard OData paging, [`execute_page`](Self::execute_page) and
    /// [`next_page`](Self::next_page) are simpler.
    ///
    /// # Errors
    ///
    /// Returns [`ResoError::Parse`] when `next_link` is not on the configured server (another
    /// scheme, host or port), because the request would carry the bearer token there.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, ReplicationQueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let query = ReplicationQueryBuilder::new("Property")
    ///     .top(2000)
    ///     .build()?;
    ///
    /// let mut response = client.execute_replication(&query).await?;
    /// let mut total_records = response.record_count;
    ///
    /// // Continue fetching while next link is available
    /// while let Some(next_link) = response.next_link {
    ///     response = client.execute_next_link(&next_link).await?;
    ///     total_records += response.record_count;
    /// }
    ///
    /// println!("Total records fetched: {}", total_records);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_next_link(
        &self,
        next_link: &str,
    ) -> Result<crate::replication::ReplicationResponse> {
        use tracing::{debug, info};

        info!("Executing next link: {}", next_link);

        let page = self.fetch_page(next_link).await?;
        debug!(
            "Retrieved {} records, next link: {:?}",
            page.records.len(),
            page.next_link
        );

        Ok(crate::replication::ReplicationResponse::new(
            page.records,
            page.next_link,
        ))
    }

    /// Execute a query and return its first page
    ///
    /// The server decides the page size. When it has more records it offers a next link,
    /// which [`Page::next_link`](crate::Page::next_link) holds as an absolute URL whichever
    /// form the server used (body `@odata.nextLink`, `next` header or `Link` header). Follow it
    /// with [`next_page`](Self::next_page).
    ///
    /// `$top` caps the total number of records, not the page size: leave it out to walk a
    /// whole result set.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, QueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let query = QueryBuilder::new("Property")
    ///     .filter("StandardStatus eq 'Active'")
    ///     .select(&["ListingKey", "ListPrice"])
    ///     .build()?;
    ///
    /// let mut page = client.execute_page(&query).await?;
    /// loop {
    ///     for record in &page.records {
    ///         println!("{}", record["ListingKey"]);
    ///     }
    ///     match client.next_page(&page).await? {
    ///         Some(next) => page = next,
    ///         None => break,
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_page(&self, query: &crate::queries::Query) -> Result<crate::Page> {
        use tracing::info;

        let url = self.build_url(&query.to_odata_string());
        info!("Executing query (page): {}", url);
        self.get_page(&url).await
    }

    /// Fetch the page after `page`, or `None` when `page` is the last one
    ///
    /// # Errors
    ///
    /// Returns [`ResoError::Parse`] when the next link is not on the configured server
    /// (another scheme, host or port), or when the server returns the same URL as the next
    /// link of the page it came from, which would loop forever.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, QueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let first = client.execute_page(&QueryBuilder::new("Member").build()?).await?;
    /// if let Some(second) = client.next_page(&first).await? {
    ///     println!("{} more records", second.records.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn next_page(&self, page: &crate::Page) -> Result<Option<crate::Page>> {
        let Some(next_link) = page.next_link.as_deref() else {
            return Ok(None);
        };
        if next_link == page.url {
            return Err(ResoError::Parse(format!(
                "Server returned the page's own URL as its next link: {}",
                next_link
            )));
        }
        self.fetch_page(next_link).await.map(Some)
    }

    /// Fetch one page from an absolute URL, such as a saved next link
    ///
    /// Use it to resume paging from a next link stored earlier. The URL must be on the
    /// configured server.
    ///
    /// # Errors
    ///
    /// Returns [`ResoError::Parse`] when `url` is not on the configured server (another
    /// scheme, host or port), because the request would carry the bearer token there.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::ResoClient;
    /// # async fn example(client: &ResoClient, saved_link: &str) -> Result<(), Box<dyn std::error::Error>> {
    /// let page = client.fetch_page(saved_link).await?;
    /// println!("{} records, more: {}", page.records.len(), page.has_more());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch_page(&self, url: &str) -> Result<crate::Page> {
        crate::paging::check_same_origin(&self.config.base_url, url)?;
        self.get_page(url).await
    }

    /// GET a collection URL and build a [`Page`](crate::Page) from the response
    async fn get_page(&self, url: &str) -> Result<crate::Page> {
        let response = self
            .send_authenticated_request(url, "application/json")
            .await?;
        // Headers must be copied before the body is read, since reading consumes the response.
        let headers = response.headers().clone();
        let json = self.parse_json_response(response).await?;
        Ok(crate::Page::from_response(json, &headers, url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Helper to clean up env vars after tests
    fn cleanup_env_vars() {
        std::env::remove_var("RESO_BASE_URL");
        std::env::remove_var("RESO_TOKEN");
        std::env::remove_var("RESO_DATASET_ID");
        std::env::remove_var("RESO_TIMEOUT");
        for name in [
            "RESO_CLIENT_ID",
            "RESO_CLIENT_SECRET",
            "RESO_TOKEN_URI",
            "RESO_SCOPE",
            "RESO_CREDENTIAL_TRANSPORT",
            "RESO_RATE_PER_SECOND",
            "RESO_RATE_PER_HOUR",
            "RESO_BYTES_PER_DAY",
            "RESO_MAX_RETRIES",
        ] {
            std::env::remove_var(name);
        }
    }

    fn bearer(config: &ClientConfig) -> &str {
        match &config.auth {
            Auth::Bearer(token) => token,
            Auth::ClientCredentials(_) => panic!("expected a bearer token"),
        }
    }

    #[test]
    fn test_config_new_basic() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token");

        assert_eq!(config.base_url, "https://api.example.com/odata");
        assert_eq!(bearer(&config), "test-token");
        assert_eq!(config.dataset_id, None);
        assert_eq!(config.timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_config_new_trailing_slash_removed() {
        let config = ClientConfig::new("https://api.example.com/odata/", "test-token");

        assert_eq!(config.base_url, "https://api.example.com/odata");
        assert!(!config.base_url.ends_with('/'));
    }

    #[test]
    fn test_config_new_multiple_trailing_slashes() {
        let config = ClientConfig::new("https://api.example.com/odata///", "test-token");

        // Only the trailing slashes are removed
        assert_eq!(config.base_url, "https://api.example.com/odata");
    }

    #[test]
    fn test_config_with_dataset_id() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token")
            .with_dataset_id("actris_ref");

        assert_eq!(config.dataset_id, Some("actris_ref".to_string()));
    }

    #[test]
    fn test_config_with_timeout() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token")
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_config_builder_chaining() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token")
            .with_dataset_id("my_dataset")
            .with_timeout(Duration::from_secs(120));

        assert_eq!(config.dataset_id, Some("my_dataset".to_string()));
        assert_eq!(config.timeout, Duration::from_secs(120));
    }

    #[test]
    #[serial]
    fn test_config_from_env_success() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_TOKEN", "my-test-token");

        let config = ClientConfig::from_env().unwrap();

        assert_eq!(config.base_url, "https://api.example.com/odata");
        assert_eq!(bearer(&config), "my-test-token");
        assert_eq!(config.dataset_id, None);
        assert_eq!(config.timeout, Duration::from_secs(30));

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_reads_rate_limits_and_retries() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata/");
        std::env::set_var("RESO_TOKEN", "my-test-token");
        std::env::set_var("RESO_RATE_PER_SECOND", "2");
        std::env::set_var("RESO_RATE_PER_HOUR", "99999999999");
        std::env::set_var("RESO_BYTES_PER_DAY", "1000000");
        std::env::set_var("RESO_MAX_RETRIES", "3");

        let config = ClientConfig::from_env().unwrap();

        assert_eq!(config.base_url, "https://api.example.com/odata");
        assert_eq!(config.rate_limit.per_second, Some(2));
        // Values above u32::MAX are capped rather than rejected
        assert_eq!(config.rate_limit.per_hour, Some(u32::MAX));
        assert_eq!(config.rate_limit.bytes_per_day, Some(1_000_000));
        assert_eq!(config.retry.max_retries, 3);

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_with_dataset_id() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_TOKEN", "my-test-token");
        std::env::set_var("RESO_DATASET_ID", "actris_ref");

        let config = ClientConfig::from_env().unwrap();

        assert_eq!(config.dataset_id, Some("actris_ref".to_string()));

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_with_custom_timeout() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_TOKEN", "my-test-token");
        std::env::set_var("RESO_TIMEOUT", "120");

        let config = ClientConfig::from_env().unwrap();

        assert_eq!(config.timeout, Duration::from_secs(120));

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_invalid_timeout_uses_default() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_TOKEN", "my-test-token");
        std::env::set_var("RESO_TIMEOUT", "invalid");

        let config = ClientConfig::from_env().unwrap();

        // Invalid timeout should fall back to default (30)
        assert_eq!(config.timeout, Duration::from_secs(30));

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_missing_base_url() {
        cleanup_env_vars();
        std::env::set_var("RESO_TOKEN", "my-test-token");

        let result = ClientConfig::from_env();

        assert!(result.is_err());
        match result {
            Err(ResoError::Config(msg)) => {
                assert!(msg.contains("RESO_BASE_URL"));
            }
            _ => panic!("Expected Config error"),
        }

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_missing_token() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");

        let result = ClientConfig::from_env();

        assert!(result.is_err());
        match result {
            Err(ResoError::Config(msg)) => {
                assert!(msg.contains("RESO_TOKEN"));
            }
            _ => panic!("Expected Config error"),
        }

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_trailing_slash_removed() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata/");
        std::env::set_var("RESO_TOKEN", "my-test-token");

        let config = ClientConfig::from_env().unwrap();

        assert_eq!(config.base_url, "https://api.example.com/odata");
        assert!(!config.base_url.ends_with('/'));

        cleanup_env_vars();
    }

    #[test]
    fn test_config_debug_redacts_token() {
        let config = ClientConfig::new("https://api.example.com/odata", "super-secret-token");

        let debug_str = format!("{:?}", config);

        assert!(debug_str.contains("ClientConfig"));
        assert!(debug_str.contains("https://api.example.com/odata"));
        assert!(debug_str.contains("<redacted>"));
        assert!(!debug_str.contains("super-secret-token"));
    }

    #[test]
    fn test_config_clone() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token")
            .with_dataset_id("test_dataset")
            .with_timeout(Duration::from_secs(45));

        let cloned = config.clone();

        assert_eq!(cloned.base_url, config.base_url);
        assert_eq!(bearer(&cloned), bearer(&config));
        assert_eq!(cloned.dataset_id, config.dataset_id);
        assert_eq!(cloned.timeout, config.timeout);
    }

    #[test]
    fn test_client_base_url() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token");
        let client = ResoClient::with_config(config).unwrap();

        assert_eq!(client.base_url(), "https://api.example.com/odata");
    }

    #[test]
    fn test_client_build_url_without_dataset_id() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token");
        let client = ResoClient::with_config(config).unwrap();

        let url = client.build_url("Property");
        assert_eq!(url, "https://api.example.com/odata/Property");

        let url_with_query = client.build_url("Property?$top=10");
        assert_eq!(
            url_with_query,
            "https://api.example.com/odata/Property?$top=10"
        );
    }

    #[test]
    fn test_client_build_url_with_dataset_id() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token")
            .with_dataset_id("actris_ref");
        let client = ResoClient::with_config(config).unwrap();

        let url = client.build_url("Property");
        assert_eq!(url, "https://api.example.com/odata/actris_ref/Property");

        let url_with_query = client.build_url("Property?$top=10");
        assert_eq!(
            url_with_query,
            "https://api.example.com/odata/actris_ref/Property?$top=10"
        );
    }

    #[test]
    fn test_client_build_url_complex_path() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token");
        let client = ResoClient::with_config(config).unwrap();

        let url = client.build_url("Property/$count");
        assert_eq!(url, "https://api.example.com/odata/Property/$count");

        let url_metadata = client.build_url("$metadata");
        assert_eq!(url_metadata, "https://api.example.com/odata/$metadata");
    }

    #[test]
    fn test_client_build_url_with_dataset_id_complex() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token")
            .with_dataset_id("my_dataset");
        let client = ResoClient::with_config(config).unwrap();

        let url = client.build_url("Property/replication");
        assert_eq!(
            url,
            "https://api.example.com/odata/my_dataset/Property/replication"
        );

        let url_with_query = client.build_url("Property?$filter=City%20eq%20%27Austin%27");
        assert_eq!(
            url_with_query,
            "https://api.example.com/odata/my_dataset/Property?$filter=City%20eq%20%27Austin%27"
        );
    }

    #[test]
    fn test_client_with_config_success() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token");
        let result = ResoClient::with_config(config);

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_client_from_env_success() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_TOKEN", "test-token");

        let result = ResoClient::from_env();

        assert!(result.is_ok());
        let client = result.unwrap();
        assert_eq!(client.base_url(), "https://api.example.com/odata");

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_client_from_env_missing_vars() {
        cleanup_env_vars();

        let result = ResoClient::from_env();

        assert!(result.is_err());

        cleanup_env_vars();
    }

    #[test]
    fn test_config_timeout_values() {
        let config1 = ClientConfig::new("https://api.example.com/odata", "token")
            .with_timeout(Duration::from_secs(1));
        assert_eq!(config1.timeout, Duration::from_secs(1));

        let config2 = ClientConfig::new("https://api.example.com/odata", "token")
            .with_timeout(Duration::from_secs(300));
        assert_eq!(config2.timeout, Duration::from_secs(300));

        let config3 = ClientConfig::new("https://api.example.com/odata", "token")
            .with_timeout(Duration::from_millis(500));
        assert_eq!(config3.timeout, Duration::from_millis(500));
    }

    #[test]
    fn test_config_empty_dataset_id() {
        let config =
            ClientConfig::new("https://api.example.com/odata", "token").with_dataset_id("");

        assert_eq!(config.dataset_id, Some("".to_string()));
    }

    #[test]
    fn test_config_special_characters_in_dataset_id() {
        let config = ClientConfig::new("https://api.example.com/odata", "token")
            .with_dataset_id("dataset-with-dashes_and_underscores");

        assert_eq!(
            config.dataset_id,
            Some("dataset-with-dashes_and_underscores".to_string())
        );
    }

    #[test]
    fn test_build_url_empty_path() {
        let config = ClientConfig::new("https://api.example.com/odata", "token");
        let client = ResoClient::with_config(config).unwrap();

        let url = client.build_url("");
        assert_eq!(url, "https://api.example.com/odata/");
    }

    #[test]
    fn test_build_url_with_dataset_empty_path() {
        let config = ClientConfig::new("https://api.example.com/odata", "token")
            .with_dataset_id("my_dataset");
        let client = ResoClient::with_config(config).unwrap();

        let url = client.build_url("");
        assert_eq!(url, "https://api.example.com/odata/my_dataset/");
    }

    #[test]
    #[serial]
    fn test_config_from_env_timeout_zero() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_TOKEN", "token");
        std::env::set_var("RESO_TIMEOUT", "0");

        let config = ClientConfig::from_env().unwrap();

        assert_eq!(config.timeout, Duration::from_secs(0));

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_client_credentials() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_CLIENT_ID", "the-id");
        std::env::set_var("RESO_CLIENT_SECRET", "the-secret");
        std::env::set_var("RESO_TOKEN_URI", "https://auth.example.com/token");
        std::env::set_var("RESO_SCOPE", "api");
        std::env::set_var("RESO_CREDENTIAL_TRANSPORT", "header");
        // RESO_CLIENT_ID wins over RESO_TOKEN
        std::env::set_var("RESO_TOKEN", "ignored");

        let config = ClientConfig::from_env().unwrap();
        match &config.auth {
            Auth::ClientCredentials(c) => {
                assert_eq!(c.client_id, "the-id");
                assert_eq!(c.client_secret, "the-secret");
                assert_eq!(c.token_url, "https://auth.example.com/token");
                assert_eq!(c.scope.as_deref(), Some("api"));
                assert_eq!(c.transport, CredentialTransport::BasicHeader);
            }
            Auth::Bearer(_) => panic!("expected client credentials"),
        }
        assert!(!format!("{:?}", config).contains("the-secret"));

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_client_credentials_missing_secret() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_CLIENT_ID", "the-id");
        std::env::set_var("RESO_TOKEN_URI", "https://auth.example.com/token");

        match ClientConfig::from_env() {
            Err(ResoError::Config(msg)) => assert!(msg.contains("RESO_CLIENT_SECRET")),
            other => panic!("expected Config error, got {:?}", other.map(|_| ())),
        }

        cleanup_env_vars();
    }

    #[test]
    #[serial]
    fn test_config_from_env_bad_transport() {
        cleanup_env_vars();
        std::env::set_var("RESO_BASE_URL", "https://api.example.com/odata");
        std::env::set_var("RESO_CLIENT_ID", "the-id");
        std::env::set_var("RESO_CLIENT_SECRET", "the-secret");
        std::env::set_var("RESO_TOKEN_URI", "https://auth.example.com/token");
        std::env::set_var("RESO_CREDENTIAL_TRANSPORT", "cookie");

        match ClientConfig::from_env() {
            Err(ResoError::Config(msg)) => {
                assert!(msg.contains("RESO_CREDENTIAL_TRANSPORT"));
                assert!(!msg.contains("the-secret"));
            }
            other => panic!("expected Config error, got {:?}", other.map(|_| ())),
        }

        cleanup_env_vars();
    }

    #[test]
    fn test_base_url_no_trailing_slash() {
        let config = ClientConfig::new("https://api.example.com/odata", "token");
        let client = ResoClient::with_config(config).unwrap();

        let base = client.base_url();
        assert!(!base.ends_with('/'));
    }
}
