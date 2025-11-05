// src/lib.rs

//! RESO Web API Client Library
//!
//! This library provides a Rust interface for communicating with RESO Web API
//! (OData 4.0) compliant servers.
//!
//! # Quick Start
//!
//! ```no_run
//! use reso_client::{ResoClient, QueryBuilder};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create client (uses environment variables)
//!     let client = ResoClient::from_env()?;
//!     
//!     // Build and execute a query
//!     let query = QueryBuilder::new("Property")
//!         .filter("City eq 'Austin'")
//!         .top(10)
//!         .build()?;
//!     
//!     let results = client.execute(&query).await?;
//!     
//!     // Access the records from the OData response
//!     if let Some(records) = results["value"].as_array() {
//!         println!("Found {} properties", records.len());
//!     }
//!     
//!     Ok(())
//! }
//! ```

pub mod client;
pub mod error;
pub mod queries;
pub mod replication;

// Re-export main types for convenience
pub use client::{ClientConfig, ResoClient};
pub use error::{ResoError, Result};
pub use queries::{Query, QueryBuilder, ReplicationQuery, ReplicationQueryBuilder};
pub use replication::ReplicationResponse;

// Re-export serde_json for convenience
pub use serde_json::Value as JsonValue;
