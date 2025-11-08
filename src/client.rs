// src/client.rs

//! Client configuration and connection management

use crate::error::{ResoError, Result};
use reqwest::Client;
use std::time::Duration;

/// Configuration for RESO client
///
/// Holds all configuration needed to connect to a RESO Web API server,
/// including the base URL, authentication token, optional dataset ID,
/// and HTTP timeout settings.
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
/// ```
#[derive(Clone)]
pub struct ClientConfig {
    /// Base URL of the RESO Web API server
    pub base_url: String,

    /// OAuth bearer token
    pub token: String,

    /// Optional dataset ID (inserted between base_url and resource)
    pub dataset_id: Option<String>,

    /// HTTP timeout duration
    pub timeout: Duration,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("dataset_id", &self.dataset_id)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl ClientConfig {
    /// Create configuration from environment variables
    ///
    /// Expects:
    /// - `RESO_BASE_URL` - Base URL of the RESO server (e.g., `https://api.mls.com/api/v2/OData`)
    /// - `RESO_TOKEN` - OAuth bearer token
    /// - `RESO_DATASET_ID` (optional) - Dataset ID inserted in URL path
    /// - `RESO_TIMEOUT` (optional) - Timeout in seconds (default: 30)
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

        let token = std::env::var("RESO_TOKEN")
            .map_err(|_| ResoError::Config("RESO_TOKEN not set".into()))?;

        let dataset_id = std::env::var("RESO_DATASET_ID").ok();

        let timeout_secs = std::env::var("RESO_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            dataset_id,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    /// Create configuration manually
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
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            dataset_id: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// Set dataset ID
    ///
    /// Some RESO servers require a dataset identifier in the URL path.
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
}

/// RESO Web API client
pub struct ResoClient {
    config: ClientConfig,
    http_client: Client,
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

    /// Build full URL with optional dataset_id
    ///
    /// Some RESO servers require a dataset ID in the URL path between the base URL
    /// and the resource/query path (e.g., `https://api.mls.com/odata/{dataset_id}/Property`).
    /// This method handles both cases transparently.
    fn build_url(&self, path: &str) -> String {
        match &self.config.dataset_id {
            Some(dataset_id) => format!("{}/{}/{}", self.config.base_url, dataset_id, path),
            None => format!("{}/{}", self.config.base_url, path),
        }
    }

    /// Send an authenticated GET request and handle error responses
    ///
    /// This helper method encapsulates the common pattern of:
    /// 1. Sending a GET request with Authorization header
    /// 2. Checking the response status
    /// 3. Converting error responses to appropriate ResoError variants
    async fn send_authenticated_request(
        &self,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::Response> {
        let response = self
            .http_client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", accept)
            .send()
            .await
            .map_err(|e| ResoError::Network(e.to_string()))?;

        let status = response.status();

        // Check for error responses and extract the body for detailed error information
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            // from_status() parses OData error format if present and maps to appropriate error variant
            return Err(ResoError::from_status(status.as_u16(), &body));
        }

        Ok(response)
    }

    /// Parse JSON response from a successful request
    async fn parse_json_response(response: reqwest::Response) -> Result<serde_json::Value> {
        response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| ResoError::Parse(format!("Failed to parse JSON: {}", e)))
    }

    /// Parse text response from a successful request
    async fn parse_text_response(response: reqwest::Response) -> Result<String> {
        response
            .text()
            .await
            .map_err(|e| ResoError::Parse(format!("Failed to read response: {}", e)))
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
        let json = Self::parse_json_response(response).await?;

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
        Self::parse_json_response(response).await
    }

    /// Execute a count-only query and return the count as an integer
    ///
    /// Uses the OData `/$count` endpoint to efficiently get just the count
    /// without fetching any records. More efficient than using `with_count()`
    /// when you only need the count.
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

        let response = self.send_authenticated_request(&url, "text/plain").await?;
        let text = Self::parse_text_response(response).await?;

        let count = text
            .trim()
            .parse::<u64>()
            .map_err(|e| ResoError::Parse(format!("Failed to parse count '{}': {}", text, e)))?;

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
        Self::parse_text_response(response).await
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

        let response = self
            .send_authenticated_request(&url, "application/json")
            .await?;

        // Extract next link from response headers before consuming response
        // The replication endpoint uses the "next" header (preferred) or "link" header
        // to indicate more records are available. This must be extracted before reading
        // the response body since consuming the response moves ownership.
        let next_link = response
            .headers()
            .get("next")
            .or_else(|| response.headers().get("link"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        debug!("Next link from headers: {:?}", next_link);

        let json = Self::parse_json_response(response).await?;

        // Extract records from OData response envelope
        // OData wraps result arrays in a "value" field: {"value": [...], "@odata.context": "..."}
        let records = json
            .get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        debug!("Retrieved {} records", records.len());

        Ok(crate::replication::ReplicationResponse::new(
            records, next_link,
        ))
    }

    /// Execute a next link from a previous replication response
    ///
    /// Takes the full URL from a previous replication response's `next_link`
    /// field and fetches the next batch of records.
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

        let response = self
            .send_authenticated_request(next_link, "application/json")
            .await?;

        // Extract next link from response headers before consuming response
        // The replication endpoint uses the "next" header (preferred) or "link" header
        // to indicate more records are available. This must be extracted before reading
        // the response body since consuming the response moves ownership.
        let next_link = response
            .headers()
            .get("next")
            .or_else(|| response.headers().get("link"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        debug!("Next link from headers: {:?}", next_link);

        let json = Self::parse_json_response(response).await?;

        // Extract records from OData response envelope
        // OData wraps result arrays in a "value" field: {"value": [...], "@odata.context": "..."}
        let records = json
            .get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        debug!("Retrieved {} records", records.len());

        Ok(crate::replication::ReplicationResponse::new(
            records, next_link,
        ))
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
    }

    #[test]
    fn test_config_new_basic() {
        let config = ClientConfig::new("https://api.example.com/odata", "test-token");

        assert_eq!(config.base_url, "https://api.example.com/odata");
        assert_eq!(config.token, "test-token");
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
        assert_eq!(config.token, "my-test-token");
        assert_eq!(config.dataset_id, None);
        assert_eq!(config.timeout, Duration::from_secs(30));

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
        assert_eq!(cloned.token, config.token);
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
    fn test_base_url_no_trailing_slash() {
        let config = ClientConfig::new("https://api.example.com/odata", "token");
        let client = ResoClient::with_config(config).unwrap();

        let base = client.base_url();
        assert!(!base.ends_with('/'));
    }
}
