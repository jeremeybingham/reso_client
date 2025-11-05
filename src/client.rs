// src/client.rs

//! Client configuration and connection management

use crate::error::{ResoError, Result};
use reqwest::Client;
use std::time::Duration;

/// Configuration for RESO client
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
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            dataset_id: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// Set dataset ID
    pub fn with_dataset_id(mut self, dataset_id: impl Into<String>) -> Self {
        self.dataset_id = Some(dataset_id.into());
        self
    }

    /// Set custom timeout
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
