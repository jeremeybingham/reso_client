// src/error.rs

//! Error types for the RESO client library
//!
//! This module defines all error types that can occur when using the RESO client.
//! Errors are categorized by their source and include detailed context.
//!
//! # Examples
//!
//! ```no_run
//! # use reso_client::{ResoClient, QueryBuilder, ResoError};
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = ResoClient::from_env()?;
//! let query = QueryBuilder::new("Property")
//!     .filter("City eq 'Austin'")
//!     .build()?;
//!
//! match client.execute(&query).await {
//!     Ok(results) => {
//!         println!("Success!");
//!     }
//!     Err(ResoError::Unauthorized { message, .. }) => {
//!         eprintln!("Auth failed: {}", message);
//!     }
//!     Err(ResoError::NotFound { message, .. }) => {
//!         eprintln!("Resource not found: {}", message);
//!     }
//!     Err(ResoError::Network(msg)) => {
//!         eprintln!("Network error: {}", msg);
//!     }
//!     Err(e) => {
//!         eprintln!("Other error: {}", e);
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use thiserror::Error;

/// Result type alias for RESO client operations
///
/// # Examples
///
/// ```
/// # use reso_client::Result;
/// fn parse_value(s: &str) -> Result<i32> {
///     s.parse().map_err(|_| {
///         reso_client::ResoError::Parse("Invalid integer".to_string())
///     })
/// }
/// ```
pub type Result<T> = std::result::Result<T, ResoError>;

/// OData error response format
///
/// The RESO Web API may return structured error responses in this format:
/// ```json
/// {
///   "error": {
///     "code": "ErrorCode",
///     "message": "Error description"
///   }
/// }
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ODataErrorResponse {
    pub error: ODataErrorDetail,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ODataErrorDetail {
    #[serde(default)]
    pub code: String,
    pub message: String,
}

/// RESO client errors
///
/// All errors that can occur when using the RESO client library.
/// Each variant includes contextual information to help diagnose issues.
///
/// # Examples
///
/// ```no_run
/// # use reso_client::{ResoClient, ResoError};
/// # async fn example() {
/// let result = ResoClient::from_env();
/// match result {
///     Err(ResoError::Config(msg)) => {
///         eprintln!("Configuration error: {}", msg);
///         eprintln!("Make sure RESO_BASE_URL and RESO_TOKEN are set");
///     }
///     Ok(client) => {
///         println!("Client created successfully");
///     }
///     _ => {}
/// }
/// # }
/// ```
#[derive(Debug, Error)]
pub enum ResoError {
    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// Network/HTTP error
    #[error("Network error: {0}")]
    Network(String),

    /// 401 Unauthorized - Invalid or missing authentication token
    #[error("Unauthorized (401): {message}")]
    Unauthorized { message: String, status_code: u16 },

    /// 403 Forbidden - Valid credentials but insufficient permissions
    #[error("Forbidden (403): {message}")]
    Forbidden { message: String, status_code: u16 },

    /// 404 Not Found - Resource or endpoint not found
    #[error("Not Found (404): {message}")]
    NotFound { message: String, status_code: u16 },

    /// 429 Too Many Requests - Rate limit exceeded
    #[error("Rate Limited (429): {message}")]
    RateLimited { message: String, status_code: u16 },

    /// 5xx Server Error - Server-side error
    #[error("Server Error ({status_code}): {message}")]
    ServerError { message: String, status_code: u16 },

    /// Generic OData server error for other status codes
    #[error("OData error ({status_code}): {message}")]
    ODataError { message: String, status_code: u16 },

    /// Parsing error
    #[error("Parse error: {0}")]
    Parse(String),

    /// Invalid query
    #[error("Invalid query: {0}")]
    InvalidQuery(String),
}

impl ResoError {
    /// Parse error response body and extract OData error message if present
    ///
    /// OData servers may return structured errors in a specific JSON format.
    /// This method attempts to parse that format and extract meaningful error details.
    pub(crate) fn parse_error_body(body: &str) -> String {
        // Try to parse as OData error format: {"error": {"code": "...", "message": "..."}}
        if let Ok(odata_error) = serde_json::from_str::<ODataErrorResponse>(body) {
            if !odata_error.error.code.is_empty() {
                return format!(
                    "{} (code: {})",
                    odata_error.error.message, odata_error.error.code
                );
            }
            return odata_error.error.message;
        }

        // If not OData format or parsing failed, return the body as-is
        // Truncate if too long to avoid overwhelming error messages
        if body.len() > 500 {
            format!("{}... (truncated)", &body[..500])
        } else {
            body.to_string()
        }
    }

    /// Create an appropriate error from HTTP status code and response body
    pub(crate) fn from_status(status_code: u16, body: &str) -> Self {
        let message = Self::parse_error_body(body);

        match status_code {
            401 => ResoError::Unauthorized {
                message,
                status_code,
            },
            403 => ResoError::Forbidden {
                message,
                status_code,
            },
            404 => ResoError::NotFound {
                message,
                status_code,
            },
            429 => ResoError::RateLimited {
                message,
                status_code,
            },
            500..=599 => ResoError::ServerError {
                message,
                status_code,
            },
            _ => ResoError::ODataError {
                message,
                status_code,
            },
        }
    }
}
