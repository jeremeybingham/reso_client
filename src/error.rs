// src/error.rs

//! Error types for the RESO client library

use thiserror::Error;

/// Result type alias
pub type Result<T> = std::result::Result<T, ResoError>;

/// RESO client errors
#[derive(Debug, Error)]
pub enum ResoError {
    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),
    
    /// Network/HTTP error
    #[error("Network error: {0}")]
    Network(String),
    
    /// OData server error
    #[error("OData error: {0}")]
    ODataError(String),
    
    /// Parsing error
    #[error("Parse error: {0}")]
    Parse(String),
    
    /// Invalid query
    #[error("Invalid query: {0}")]
    InvalidQuery(String),
}