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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_error_body_valid_odata_error() {
        let body = r#"{"error": {"code": "InvalidFilter", "message": "The filter expression is invalid"}}"#;
        let result = ResoError::parse_error_body(body);
        assert_eq!(
            result,
            "The filter expression is invalid (code: InvalidFilter)"
        );
    }

    #[test]
    fn test_parse_error_body_valid_odata_error_no_code() {
        let body = r#"{"error": {"code": "", "message": "Something went wrong"}}"#;
        let result = ResoError::parse_error_body(body);
        assert_eq!(result, "Something went wrong");
    }

    #[test]
    fn test_parse_error_body_malformed_json() {
        let body = "This is not JSON at all";
        let result = ResoError::parse_error_body(body);
        assert_eq!(result, "This is not JSON at all");
    }

    #[test]
    fn test_parse_error_body_wrong_json_structure() {
        let body = r#"{"message": "Error without proper structure"}"#;
        let result = ResoError::parse_error_body(body);
        assert_eq!(result, r#"{"message": "Error without proper structure"}"#);
    }

    #[test]
    fn test_parse_error_body_long_body_truncation() {
        let long_body = "x".repeat(600);
        let result = ResoError::parse_error_body(&long_body);
        assert!(result.len() <= 515); // 500 + "... (truncated)"
        assert!(result.ends_with("... (truncated)"));
    }

    #[test]
    fn test_parse_error_body_exact_500_chars() {
        let body = "x".repeat(500);
        let result = ResoError::parse_error_body(&body);
        assert_eq!(result, body);
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn test_from_status_401_unauthorized() {
        let error = ResoError::from_status(401, "Authentication failed");
        match error {
            ResoError::Unauthorized {
                message,
                status_code,
            } => {
                assert_eq!(message, "Authentication failed");
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected Unauthorized error"),
        }
    }

    #[test]
    fn test_from_status_403_forbidden() {
        let error = ResoError::from_status(403, "Access denied");
        match error {
            ResoError::Forbidden {
                message,
                status_code,
            } => {
                assert_eq!(message, "Access denied");
                assert_eq!(status_code, 403);
            }
            _ => panic!("Expected Forbidden error"),
        }
    }

    #[test]
    fn test_from_status_404_not_found() {
        let error = ResoError::from_status(404, "Resource not found");
        match error {
            ResoError::NotFound {
                message,
                status_code,
            } => {
                assert_eq!(message, "Resource not found");
                assert_eq!(status_code, 404);
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[test]
    fn test_from_status_429_rate_limited() {
        let error = ResoError::from_status(429, "Too many requests");
        match error {
            ResoError::RateLimited {
                message,
                status_code,
            } => {
                assert_eq!(message, "Too many requests");
                assert_eq!(status_code, 429);
            }
            _ => panic!("Expected RateLimited error"),
        }
    }

    #[test]
    fn test_from_status_500_server_error() {
        let error = ResoError::from_status(500, "Internal server error");
        match error {
            ResoError::ServerError {
                message,
                status_code,
            } => {
                assert_eq!(message, "Internal server error");
                assert_eq!(status_code, 500);
            }
            _ => panic!("Expected ServerError error"),
        }
    }

    #[test]
    fn test_from_status_503_server_error() {
        let error = ResoError::from_status(503, "Service unavailable");
        match error {
            ResoError::ServerError {
                message,
                status_code,
            } => {
                assert_eq!(message, "Service unavailable");
                assert_eq!(status_code, 503);
            }
            _ => panic!("Expected ServerError error"),
        }
    }

    #[test]
    fn test_from_status_400_odata_error() {
        let error = ResoError::from_status(400, "Bad request");
        match error {
            ResoError::ODataError {
                message,
                status_code,
            } => {
                assert_eq!(message, "Bad request");
                assert_eq!(status_code, 400);
            }
            _ => panic!("Expected ODataError error"),
        }
    }

    #[test]
    fn test_from_status_with_odata_json() {
        let body = r#"{"error": {"code": "InvalidQuery", "message": "Query syntax error"}}"#;
        let error = ResoError::from_status(400, body);
        match error {
            ResoError::ODataError {
                message,
                status_code,
            } => {
                assert_eq!(message, "Query syntax error (code: InvalidQuery)");
                assert_eq!(status_code, 400);
            }
            _ => panic!("Expected ODataError error"),
        }
    }

    #[test]
    fn test_odata_error_response_deserialization() {
        let json = r#"{"error": {"code": "TestCode", "message": "Test message"}}"#;
        let result: std::result::Result<ODataErrorResponse, _> = serde_json::from_str(json);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.error.code, "TestCode");
        assert_eq!(response.error.message, "Test message");
    }

    #[test]
    fn test_odata_error_response_missing_code() {
        let json = r#"{"error": {"message": "Test message"}}"#;
        let result: std::result::Result<ODataErrorResponse, _> = serde_json::from_str(json);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.error.code, ""); // Default value
        assert_eq!(response.error.message, "Test message");
    }

    #[test]
    fn test_error_display_config() {
        let error = ResoError::Config("Missing RESO_TOKEN".to_string());
        assert_eq!(
            format!("{}", error),
            "Configuration error: Missing RESO_TOKEN"
        );
    }

    #[test]
    fn test_error_display_network() {
        let error = ResoError::Network("Connection timeout".to_string());
        assert_eq!(format!("{}", error), "Network error: Connection timeout");
    }

    #[test]
    fn test_error_display_unauthorized() {
        let error = ResoError::Unauthorized {
            message: "Invalid token".to_string(),
            status_code: 401,
        };
        assert_eq!(format!("{}", error), "Unauthorized (401): Invalid token");
    }

    #[test]
    fn test_error_display_forbidden() {
        let error = ResoError::Forbidden {
            message: "No access".to_string(),
            status_code: 403,
        };
        assert_eq!(format!("{}", error), "Forbidden (403): No access");
    }

    #[test]
    fn test_error_display_not_found() {
        let error = ResoError::NotFound {
            message: "Property not found".to_string(),
            status_code: 404,
        };
        assert_eq!(format!("{}", error), "Not Found (404): Property not found");
    }

    #[test]
    fn test_error_display_rate_limited() {
        let error = ResoError::RateLimited {
            message: "Slow down".to_string(),
            status_code: 429,
        };
        assert_eq!(format!("{}", error), "Rate Limited (429): Slow down");
    }

    #[test]
    fn test_error_display_server_error() {
        let error = ResoError::ServerError {
            message: "Database error".to_string(),
            status_code: 500,
        };
        assert_eq!(format!("{}", error), "Server Error (500): Database error");
    }

    #[test]
    fn test_error_display_odata_error() {
        let error = ResoError::ODataError {
            message: "Invalid filter".to_string(),
            status_code: 400,
        };
        assert_eq!(format!("{}", error), "OData error (400): Invalid filter");
    }

    #[test]
    fn test_error_display_parse() {
        let error = ResoError::Parse("JSON parse failed".to_string());
        assert_eq!(format!("{}", error), "Parse error: JSON parse failed");
    }

    #[test]
    fn test_error_display_invalid_query() {
        let error = ResoError::InvalidQuery("Cannot use $filter with key access".to_string());
        assert_eq!(
            format!("{}", error),
            "Invalid query: Cannot use $filter with key access"
        );
    }

    #[test]
    fn test_error_debug_trait() {
        let error = ResoError::Config("test".to_string());
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("Config"));
        assert!(debug_str.contains("test"));
    }
}
