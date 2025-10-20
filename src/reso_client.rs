use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// Custom error type for RESO API operations
#[derive(Debug)]
pub enum ResoApiError {
    HttpError(reqwest::Error),
    AuthError(String),
    ParseError(String),
}

impl fmt::Display for ResoApiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ResoApiError::HttpError(e) => write!(f, "HTTP error: {}", e),
            ResoApiError::AuthError(msg) => write!(f, "Authentication error: {}", msg),
            ResoApiError::ParseError(msg) => write!(f, "Parse error: {}", msg),
        }
    }
}

impl Error for ResoApiError {}

impl From<reqwest::Error> for ResoApiError {
    fn from(err: reqwest::Error) -> Self {
        ResoApiError::HttpError(err)
    }
}

/// RESO Web API Client configuration
#[derive(Debug, Clone)]
pub struct ResoApiConfig {
    /// Base URL for the API (e.g., "https://api.bridgedataoutput.com/api/v2")
    pub base_url: String,
    /// Server token for authentication (Bearer token)
    pub server_token: String,
    /// Dataset ID (provided by your MLS)
    pub dataset_id: Option<String>,
}

impl ResoApiConfig {
    pub fn new(base_url: String, server_token: String) -> Self {
        Self {
            base_url,
            server_token,
            dataset_id: None,
        }
    }

    pub fn with_dataset(mut self, dataset_id: String) -> Self {
        self.dataset_id = Some(dataset_id);
        self
    }
}

/// RESO Web API Client
pub struct ResoApiClient {
    config: ResoApiConfig,
    client: Client,
}

impl ResoApiClient {
    /// Create a new RESO API client
    pub fn new(config: ResoApiConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    /// Build the full URL (without authentication - that goes in headers)
    fn build_url(&self, path: &str) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        let url = format!("{}/{}", base, path);
        
        // Debug: print URL
        eprintln!("🔗 Request URL: {}", url);
        
        url
    }
    
    /// Build authorization header value
    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.server_token)
    }

    /// Build OData URL for a specific resource
    pub fn build_odata_url(&self, resource: &str) -> Result<String, ResoApiError> {
        let dataset_id = self.config.dataset_id.as_ref()
            .ok_or_else(|| ResoApiError::AuthError("Dataset ID not configured".to_string()))?;
        
        Ok(self.build_url(&format!("OData/{}/{}", dataset_id, resource)))
    }

    /// Get metadata from the RESO API
    pub async fn get_metadata(&self) -> Result<String, ResoApiError> {
        let dataset_id = self.config.dataset_id.as_ref()
            .ok_or_else(|| ResoApiError::AuthError("Dataset ID not configured".to_string()))?;
        
        let url = self.build_url(&format!("OData/{}/$metadata", dataset_id));
        
        eprintln!("📡 Fetching metadata...");
        let response = self.client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        let status = response.status();
        eprintln!("📊 Response status: {}", status);
        
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            eprintln!("❌ Response body: {}", body);
            return Err(ResoApiError::AuthError(
                format!("Failed to fetch metadata: {} - {}", status, body)
            ));
        }

        Ok(response.text().await?)
    }

    /// Query a resource with optional OData query parameters
    pub async fn query<T>(&self, resource: &str, query_params: Option<&str>) -> Result<ODataResponse<T>, ResoApiError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut url = self.build_odata_url(resource)?;
        
        if let Some(params) = query_params {
            url.push_str(&format!("?{}", params));
        }

        let response = self.client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        let status = response.status();
        
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ResoApiError::AuthError(
                format!("Query failed: {} - {}", status, body)
            ));
        }

        let odata_response: ODataResponse<T> = response.json().await
            .map_err(|e| ResoApiError::ParseError(e.to_string()))?;

        Ok(odata_response)
    }

    /// Get a specific record by ID
    pub async fn get_by_id<T>(&self, resource: &str, id: &str) -> Result<T, ResoApiError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{}({})", self.build_odata_url(resource)?, id);

        let response = self.client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(ResoApiError::AuthError(
                format!("Get by ID failed: {}", response.status())
            ));
        }

        let record: T = response.json().await
            .map_err(|e| ResoApiError::ParseError(e.to_string()))?;

        Ok(record)
    }

    /// Raw GET request to any endpoint
    pub async fn get_raw(&self, path: &str) -> Result<Response, ResoApiError> {
        let url = self.build_url(path);
        let response = self.client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await?;
        Ok(response)
    }
}

/// OData response wrapper
#[derive(Debug, Serialize, Deserialize)]
pub struct ODataResponse<T> {
    #[serde(rename = "@odata.context")]
    pub context: Option<String>,
    
    #[serde(rename = "@odata.count")]
    pub count: Option<i64>,
    
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    
    #[serde(rename = "value")]
    pub value: Vec<T>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_creation() {
        let config = ResoApiConfig::new(
            "https://api.bridgedataoutput.com/api/v2".to_string(),
            "your_token_here".to_string(),
        ).with_dataset("test_dataset".to_string());

        assert_eq!(config.dataset_id, Some("test_dataset".to_string()));
    }

    #[test]
    fn test_url_building() {
        let config = ResoApiConfig::new(
            "https://api.bridgedataoutput.com/api/v2".to_string(),
            "test_token".to_string(),
        ).with_dataset("abc123".to_string());

        let client = ResoApiClient::new(config);
        let url = client.build_odata_url("Property").unwrap();
        
        assert!(url.contains("OData/abc123/Property"));
        assert!(!url.contains("access_token"));
    }
}