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
    fn build_url(&self, path: &str) -> String {
        match &self.config.dataset_id {
            Some(dataset_id) => format!("{}/{}/{}", self.config.base_url, dataset_id, path),
            None => format!("{}/{}", self.config.base_url, path),
        }
    }

    /// Execute a query and return raw JSON
    pub async fn execute(&self, query: &crate::queries::Query) -> Result<serde_json::Value> {
        use tracing::{debug, info};

        let url = self.build_url(&query.to_odata_string());

        info!("Executing query: {}", url);

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ResoError::Network(e.to_string()))?;

        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ResoError::ODataError(format!(
                "Request failed with status {}: {}",
                status, body
            )));
        }

        let json = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| ResoError::Parse(format!("Failed to parse JSON: {}", e)))?;

        debug!(
            "Query result: {} records",
            json.get("value")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0)
        );

        Ok(json)
    }

    /// Fetch $metadata XML
    pub async fn fetch_metadata(&self) -> Result<String> {
        use tracing::info;

        let url = self.build_url("$metadata");

        info!("Fetching metadata from: {}", url);

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/xml")
            .send()
            .await
            .map_err(|e| ResoError::Network(e.to_string()))?;

        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ResoError::ODataError(format!(
                "Metadata request failed with status {}: {}",
                status, body
            )));
        }

        response
            .text()
            .await
            .map_err(|e| ResoError::Parse(format!("Failed to read metadata: {}", e)))
    }
}
