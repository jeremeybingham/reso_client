// src/replication.rs

//! Replication endpoint response types

use serde_json::Value as JsonValue;

/// Response from a replication endpoint query
///
/// The replication endpoint returns records along with a `next` link in the
/// response headers for pagination through large datasets.
///
/// # Example
///
/// ```no_run
/// # use reso_client::{ResoClient, ReplicationQueryBuilder};
/// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
/// let query = ReplicationQueryBuilder::new("Property")
///     .top(2000)
///     .build()?;
///
/// let response = client.execute_replication(&query).await?;
///
/// println!("Retrieved {} records", response.record_count);
///
/// // Continue with next link if available
/// if let Some(next_link) = response.next_link {
///     let next_response = client.execute_next_link(&next_link).await?;
///     println!("Retrieved {} more records", next_response.record_count);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ReplicationResponse {
    /// The array of records returned by the query
    pub records: Vec<JsonValue>,

    /// The next link URL from response headers for pagination
    ///
    /// This is extracted from the `next` header in the HTTP response.
    /// Use this with `execute_next_link()` to fetch the next batch of records.
    pub next_link: Option<String>,

    /// Number of records in this response
    pub record_count: usize,
}

impl ReplicationResponse {
    /// Create a new replication response
    ///
    /// This is typically called internally by the client. You usually don't
    /// need to construct this yourself.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ReplicationResponse;
    /// # use serde_json::json;
    /// let records = vec![json!({"ListingKey": "12345"})];
    /// let response = ReplicationResponse::new(records, None);
    /// assert_eq!(response.record_count, 1);
    /// assert!(!response.has_more());
    /// ```
    pub fn new(records: Vec<JsonValue>, next_link: Option<String>) -> Self {
        let record_count = records.len();
        Self {
            records,
            next_link,
            record_count,
        }
    }

    /// Check if there are more records available
    ///
    /// Returns `true` if there's a next link available for pagination.
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
    /// let response = client.execute_replication(&query).await?;
    ///
    /// if response.has_more() {
    ///     println!("More records available!");
    ///     let next_response = client.execute_next_link(
    ///         response.next_link().unwrap()
    ///     ).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn has_more(&self) -> bool {
        self.next_link.is_some()
    }

    /// Get the next link URL if available
    ///
    /// Returns `Some(&str)` with the URL for the next page, or `None` if
    /// there are no more records.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, ReplicationQueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let query = ReplicationQueryBuilder::new("Property").build()?;
    /// let mut response = client.execute_replication(&query).await?;
    /// let mut all_records = response.records.clone();
    ///
    /// // Fetch all pages
    /// while let Some(next_url) = response.next_link() {
    ///     response = client.execute_next_link(next_url).await?;
    ///     all_records.extend(response.records.clone());
    /// }
    ///
    /// println!("Total records: {}", all_records.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn next_link(&self) -> Option<&str> {
        self.next_link.as_deref()
    }
}
