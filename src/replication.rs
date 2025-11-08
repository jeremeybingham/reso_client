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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_new_with_records_and_next_link() {
        let records = vec![
            json!({"ListingKey": "12345", "City": "Austin"}),
            json!({"ListingKey": "67890", "City": "Dallas"}),
        ];
        let next_link = Some("https://api.example.com/next".to_string());

        let response = ReplicationResponse::new(records.clone(), next_link.clone());

        assert_eq!(response.records.len(), 2);
        assert_eq!(response.record_count, 2);
        assert_eq!(response.next_link, next_link);
        assert!(response.has_more());
    }

    #[test]
    fn test_new_with_records_no_next_link() {
        let records = vec![
            json!({"ListingKey": "12345"}),
            json!({"ListingKey": "67890"}),
            json!({"ListingKey": "11111"}),
        ];

        let response = ReplicationResponse::new(records.clone(), None);

        assert_eq!(response.records.len(), 3);
        assert_eq!(response.record_count, 3);
        assert_eq!(response.next_link, None);
        assert!(!response.has_more());
    }

    #[test]
    fn test_new_with_empty_records() {
        let records: Vec<JsonValue> = vec![];
        let next_link = Some("https://api.example.com/next".to_string());

        let response = ReplicationResponse::new(records, next_link.clone());

        assert_eq!(response.records.len(), 0);
        assert_eq!(response.record_count, 0);
        assert_eq!(response.next_link, next_link);
        assert!(response.has_more());
    }

    #[test]
    fn test_new_with_empty_records_no_next_link() {
        let records: Vec<JsonValue> = vec![];

        let response = ReplicationResponse::new(records, None);

        assert_eq!(response.records.len(), 0);
        assert_eq!(response.record_count, 0);
        assert_eq!(response.next_link, None);
        assert!(!response.has_more());
    }

    #[test]
    fn test_new_with_single_record() {
        let records = vec![json!({"ListingKey": "12345", "City": "Austin"})];

        let response = ReplicationResponse::new(records, None);

        assert_eq!(response.records.len(), 1);
        assert_eq!(response.record_count, 1);
        assert!(!response.has_more());
    }

    #[test]
    fn test_new_with_large_batch() {
        let mut records = Vec::new();
        for i in 0..2000 {
            records.push(json!({"ListingKey": format!("{}", i)}));
        }
        let next_link = Some("https://api.example.com/next?skip=2000".to_string());

        let response = ReplicationResponse::new(records, next_link.clone());

        assert_eq!(response.records.len(), 2000);
        assert_eq!(response.record_count, 2000);
        assert_eq!(response.next_link, next_link);
        assert!(response.has_more());
    }

    #[test]
    fn test_has_more_true() {
        let records = vec![json!({"key": "value"})];
        let response =
            ReplicationResponse::new(records, Some("https://api.example.com/next".to_string()));

        assert!(response.has_more());
    }

    #[test]
    fn test_has_more_false() {
        let records = vec![json!({"key": "value"})];
        let response = ReplicationResponse::new(records, None);

        assert!(!response.has_more());
    }

    #[test]
    fn test_next_link_some() {
        let records = vec![json!({"key": "value"})];
        let url = "https://api.example.com/Property/replication?skip=1000".to_string();
        let response = ReplicationResponse::new(records, Some(url.clone()));

        assert_eq!(response.next_link(), Some(url.as_str()));
    }

    #[test]
    fn test_next_link_none() {
        let records = vec![json!({"key": "value"})];
        let response = ReplicationResponse::new(records, None);

        assert_eq!(response.next_link(), None);
    }

    #[test]
    fn test_record_count_accuracy() {
        let test_cases = vec![0, 1, 10, 100, 500, 1000, 2000];

        for count in test_cases {
            let mut records = Vec::new();
            for i in 0..count {
                records.push(json!({"id": i}));
            }

            let response = ReplicationResponse::new(records.clone(), None);

            assert_eq!(
                response.record_count, count,
                "Record count mismatch for {} records",
                count
            );
            assert_eq!(
                response.records.len(),
                count,
                "Records length mismatch for {} records",
                count
            );
        }
    }

    #[test]
    fn test_clone_trait() {
        let records = vec![json!({"key": "value"})];
        let original =
            ReplicationResponse::new(records, Some("https://api.example.com/next".to_string()));

        let cloned = original.clone();

        assert_eq!(cloned.records.len(), original.records.len());
        assert_eq!(cloned.record_count, original.record_count);
        assert_eq!(cloned.next_link, original.next_link);
    }

    #[test]
    fn test_debug_trait() {
        let records = vec![json!({"key": "value"})];
        let response =
            ReplicationResponse::new(records, Some("https://api.example.com/next".to_string()));

        let debug_str = format!("{:?}", response);
        assert!(debug_str.contains("ReplicationResponse"));
        assert!(debug_str.contains("records"));
        assert!(debug_str.contains("next_link"));
    }

    #[test]
    fn test_next_link_with_query_params() {
        let records = vec![json!({"key": "value"})];
        let url = "https://api.example.com/Property/replication?$filter=City%20eq%20%27Austin%27&$skip=1000&$top=2000".to_string();
        let response = ReplicationResponse::new(records, Some(url.clone()));

        assert_eq!(response.next_link(), Some(url.as_str()));
        assert!(response.has_more());
    }

    #[test]
    fn test_complex_json_records() {
        let records = vec![
            json!({
                "ListingKey": "12345",
                "City": "Austin",
                "ListPrice": 500000.50,
                "BedroomsTotal": 3,
                "BathroomsTotalInteger": 2,
                "Photos": ["photo1.jpg", "photo2.jpg"],
                "ListOffice": {
                    "OfficeKey": "OFF123",
                    "OfficeName": "Example Realty"
                }
            }),
            json!({
                "ListingKey": "67890",
                "City": "Dallas",
                "ListPrice": 750000.00,
                "BedroomsTotal": 4,
                "BathroomsTotalInteger": 3
            }),
        ];

        let response = ReplicationResponse::new(records.clone(), None);

        assert_eq!(response.records.len(), 2);
        assert_eq!(response.record_count, 2);

        // Verify we can access nested fields
        let first_record = &response.records[0];
        assert_eq!(first_record["City"], "Austin");
        assert_eq!(first_record["ListPrice"], 500000.50);
        assert_eq!(first_record["ListOffice"]["OfficeKey"], "OFF123");
    }

    #[test]
    fn test_next_link_empty_string() {
        let records = vec![json!({"key": "value"})];
        let response = ReplicationResponse::new(records, Some("".to_string()));

        assert_eq!(response.next_link(), Some(""));
        assert!(response.has_more());
    }

    #[test]
    fn test_records_ownership() {
        let records = vec![
            json!({"ListingKey": "12345"}),
            json!({"ListingKey": "67890"}),
        ];

        let response = ReplicationResponse::new(records, None);

        // Verify we can access records multiple times (ownership is maintained)
        assert_eq!(response.records.len(), 2);
        assert_eq!(response.records[0]["ListingKey"], "12345");
        assert_eq!(response.records[1]["ListingKey"], "67890");
    }
}
