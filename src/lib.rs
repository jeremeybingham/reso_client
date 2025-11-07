// src/lib.rs

//! RESO Web API Client Library
//!
//! A Rust client library for [RESO Web API](https://www.reso.org/reso-web-api/)
//! servers that implement the OData 4.0 protocol. This library provides a type-safe,
//! ergonomic interface for querying real estate data from MLS systems.
//!
//! # Features
//!
//! - 🔍 **Fluent Query Builder** - Build complex OData queries with a clean, fluent API
//! - 🔐 **OAuth Authentication** - Built-in support for bearer token authentication
//! - 📊 **Full OData Support** - Filter, sort, paginate, select fields, expand relations
//! - 🔢 **Count Queries** - Efficient record counting via `/$count` endpoint
//! - 🗂️ **Dataset ID Support** - Handle RESO servers that use dataset identifiers
//! - 📖 **Metadata Retrieval** - Fetch and parse OData `$metadata` documents
//! - 🔄 **Replication Endpoint** - Bulk data transfer with up to 2000 records/request
//! - ⚡ **Async/Await** - Built on tokio for high-performance concurrent operations
//! - 🛡️ **Type-Safe Errors** - Comprehensive error types with detailed context
//!
//! # Quick Start
//!
//! ```no_run
//! use reso_client::{ResoClient, QueryBuilder};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create client from environment variables
//!     // Requires: RESO_BASE_URL and RESO_TOKEN
//!     let client = ResoClient::from_env()?;
//!
//!     // Build and execute a query
//!     let query = QueryBuilder::new("Property")
//!         .filter("City eq 'Austin' and ListPrice gt 500000")
//!         .select(&["ListingKey", "City", "ListPrice", "BedroomsTotal"])
//!         .order_by("ListPrice", "desc")
//!         .top(10)
//!         .build()?;
//!
//!     let results = client.execute(&query).await?;
//!
//!     // Access the records from the OData response
//!     if let Some(records) = results["value"].as_array() {
//!         println!("Found {} properties", records.len());
//!         for record in records {
//!             let key = record["ListingKey"].as_str().unwrap_or("");
//!             let price = record["ListPrice"].as_f64().unwrap_or(0.0);
//!             println!("{}: ${}", key, price);
//!         }
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! # Configuration
//!
//! The client can be configured via environment variables or programmatically:
//!
//! ## Environment Variables
//!
//! ```bash
//! RESO_BASE_URL=https://api.bridgedataoutput.com/api/v2/OData
//! RESO_TOKEN=your-oauth-token
//! RESO_DATASET_ID=actris_ref  # Optional
//! RESO_TIMEOUT=30              # Optional, seconds
//! ```
//!
//! ## Manual Configuration
//!
//! ```no_run
//! # use reso_client::{ResoClient, ClientConfig};
//! # use std::time::Duration;
//! let config = ClientConfig::new(
//!     "https://api.mls.com/odata",
//!     "your-bearer-token"
//! )
//! .with_dataset_id("actris_ref")
//! .with_timeout(Duration::from_secs(60));
//!
//! let client = ResoClient::with_config(config)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Common Usage Patterns
//!
//! ## Filtering with OData Expressions
//!
//! ```no_run
//! # use reso_client::{ResoClient, QueryBuilder};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! // Simple equality
//! let query = QueryBuilder::new("Property")
//!     .filter("City eq 'Austin'")
//!     .build()?;
//!
//! // Multiple conditions
//! let query = QueryBuilder::new("Property")
//!     .filter("City eq 'Austin' and ListPrice gt 500000 and BedroomsTotal ge 3")
//!     .build()?;
//!
//! // String functions
//! let query = QueryBuilder::new("Property")
//!     .filter("startswith(City, 'San')")
//!     .build()?;
//!
//! // Date comparison
//! let query = QueryBuilder::new("Property")
//!     .filter("ModificationTimestamp gt 2025-01-01T00:00:00Z")
//!     .build()?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Pagination
//!
//! ```no_run
//! # use reso_client::{ResoClient, QueryBuilder};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! // First page
//! let query = QueryBuilder::new("Property")
//!     .top(20)
//!     .build()?;
//! let page1 = client.execute(&query).await?;
//!
//! // Second page
//! let query = QueryBuilder::new("Property")
//!     .skip(20)
//!     .top(20)
//!     .build()?;
//! let page2 = client.execute(&query).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Getting Total Counts
//!
//! ```no_run
//! # use reso_client::{ResoClient, QueryBuilder};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! // Include count in response (with records)
//! let query = QueryBuilder::new("Property")
//!     .filter("City eq 'Austin'")
//!     .with_count()
//!     .top(10)
//!     .build()?;
//!
//! let results = client.execute(&query).await?;
//! if let Some(count) = results["@odata.count"].as_u64() {
//!     println!("Total matching records: {}", count);
//! }
//!
//! // Count only (no records, more efficient)
//! let query = QueryBuilder::new("Property")
//!     .filter("City eq 'Austin'")
//!     .count()
//!     .build()?;
//!
//! let count = client.execute_count(&query).await?;
//! println!("Total: {}", count);
//! # Ok(())
//! # }
//! ```
//!
//! ## Bulk Data with Replication
//!
//! ```no_run
//! # use reso_client::{ResoClient, ReplicationQueryBuilder};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! let query = ReplicationQueryBuilder::new("Property")
//!     .filter("StandardStatus eq 'Active'")
//!     .select(&["ListingKey", "City", "ListPrice"])
//!     .top(2000)  // Max: 2000 for replication
//!     .build()?;
//!
//! let mut response = client.execute_replication(&query).await?;
//! let mut all_records = response.records;
//!
//! // Continue fetching while next link is available
//! while let Some(next_link) = response.next_link {
//!     response = client.execute_next_link(&next_link).await?;
//!     all_records.extend(response.records);
//! }
//!
//! println!("Total records fetched: {}", all_records.len());
//! # Ok(())
//! # }
//! ```
//!
//! ## Error Handling
//!
//! ```no_run
//! # use reso_client::{ResoClient, QueryBuilder, ResoError};
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = ResoClient::from_env()?;
//! let query = QueryBuilder::new("Property").top(10).build()?;
//!
//! match client.execute(&query).await {
//!     Ok(results) => {
//!         println!("Success!");
//!     }
//!     Err(ResoError::Unauthorized { message, .. }) => {
//!         eprintln!("Authentication failed: {}", message);
//!     }
//!     Err(ResoError::NotFound { message, .. }) => {
//!         eprintln!("Resource not found: {}", message);
//!     }
//!     Err(ResoError::RateLimited { message, .. }) => {
//!         eprintln!("Rate limited: {}", message);
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
//!
//! # Stability
//!
//! This library is pre-1.0, meaning the API may change between minor versions.
//! We follow semantic versioning, but breaking changes may occur in 0.x releases.
//! We strive to keep changes minimal and well-documented in the CHANGELOG.
//!
//! # Resources
//!
//! - [RESO Web API Specification](https://www.reso.org/reso-web-api/)
//! - [OData 4.0 Protocol](https://www.odata.org/documentation/)
//! - [RESO Data Dictionary](https://www.reso.org/data-dictionary/)

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
