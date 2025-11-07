// src/queries.rs

//! Query building for RESO/OData requests

use crate::error::{ResoError, Result};

/// A structured RESO/OData query
#[derive(Debug, Clone)]
pub struct Query {
    resource: String,
    key: Option<String>,
    filter: Option<String>,
    select_fields: Option<Vec<String>>,
    order_by: Option<String>,
    top: Option<u32>,
    skip: Option<u32>,
    count: bool,
    count_only: bool,
    apply: Option<String>,
    expand: Option<Vec<String>>,
}

/// A structured RESO replication query
///
/// Replication queries are used for bulk data transfer and have different
/// constraints than standard queries:
/// - Maximum $top limit: 2000 (vs 200 for standard queries)
/// - No $skip parameter (use next links instead)
/// - No $orderby parameter (ordered oldest to newest by default)
/// - No $apply parameter
/// - No count options
#[derive(Debug, Clone)]
pub struct ReplicationQuery {
    resource: String,
    filter: Option<String>,
    select_fields: Option<Vec<String>>,
    top: Option<u32>,
}

impl Query {
    /// Create a new query for a resource
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            key: None,
            filter: None,
            select_fields: None,
            order_by: None,
            top: None,
            skip: None,
            count: false,
            count_only: false,
            apply: None,
            expand: None,
        }
    }

    /// Convert to OData query string
    ///
    /// Generates the URL path and query parameters according to OData v4.0 specification.
    pub fn to_odata_string(&self) -> String {
        let mut parts = vec![self.resource.clone()];

        // For key access, append ('key') to resource name (e.g., Property('12345'))
        // This is the OData direct key access pattern for single entity retrieval
        if let Some(key) = &self.key {
            parts.push(format!("('{}')", urlencoding::encode(key)));

            let mut params = Vec::new();

            // Key access only supports $select and $expand (per OData spec)
            // Other query options like $filter, $top, $skip are not applicable to single entity access
            if let Some(fields) = &self.select_fields {
                params.push(format!("$select={}", fields.join(",")));
            }

            if let Some(expands) = &self.expand {
                params.push(format!("$expand={}", expands.join(",")));
            }

            if !params.is_empty() {
                parts.push("?".to_string());
                parts.push(params.join("&"));
            }

            return parts.concat();
        }

        // For count-only queries, append /$count and only use filter
        // Returns a plain text count instead of JSON (e.g., "42")
        if self.count_only {
            parts.push("/$count".to_string());

            if let Some(filter) = &self.filter {
                parts.push("?".to_string());
                parts.push(format!("$filter={}", urlencoding::encode(filter)));
            }

            return parts.concat();
        }

        let mut params = Vec::new();

        // $apply
        if let Some(apply) = &self.apply {
            params.push(format!("$apply={}", urlencoding::encode(apply)));
        }

        // $filter
        if let Some(filter) = &self.filter {
            params.push(format!("$filter={}", urlencoding::encode(filter)));
        }

        // $select
        if let Some(fields) = &self.select_fields {
            params.push(format!("$select={}", fields.join(",")));
        }

        // $expand
        if let Some(expands) = &self.expand {
            params.push(format!("$expand={}", expands.join(",")));
        }

        // $orderby
        if let Some(order) = &self.order_by {
            params.push(format!("$orderby={}", urlencoding::encode(order)));
        }

        // $top
        if let Some(top) = self.top {
            params.push(format!("$top={}", top));
        }

        // $skip
        if let Some(skip) = self.skip {
            params.push(format!("$skip={}", skip));
        }

        // $count
        if self.count {
            params.push("$count=true".to_string());
        }

        if !params.is_empty() {
            parts.push("?".to_string());
            parts.push(params.join("&"));
        }

        parts.concat()
    }
}

impl ReplicationQuery {
    /// Create a new replication query for a resource
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            filter: None,
            select_fields: None,
            top: None,
        }
    }

    /// Convert to OData replication query string
    ///
    /// Generates URL path: `{resource}/replication?{params}`
    pub fn to_odata_string(&self) -> String {
        let mut parts = vec![self.resource.clone(), "/replication".to_string()];

        let mut params = Vec::new();

        // $filter
        if let Some(filter) = &self.filter {
            params.push(format!("$filter={}", urlencoding::encode(filter)));
        }

        // $select
        if let Some(fields) = &self.select_fields {
            params.push(format!("$select={}", fields.join(",")));
        }

        // $top
        if let Some(top) = self.top {
            params.push(format!("$top={}", top));
        }

        if !params.is_empty() {
            parts.push("?".to_string());
            parts.push(params.join("&"));
        }

        parts.concat()
    }

    /// Get the resource name
    pub fn resource(&self) -> &str {
        &self.resource
    }
}

/// Fluent query builder
pub struct QueryBuilder {
    query: Query,
}

impl QueryBuilder {
    /// Create a new query builder for a resource
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// let query = QueryBuilder::new("Property")
    ///     .filter("City eq 'Austin' and ListPrice gt 500000")
    ///     .top(10)
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            query: Query::new(resource),
        }
    }

    /// Create a query builder for direct key access
    ///
    /// Direct key access is more efficient than using filters for single-record lookups.
    /// Returns a single object instead of an array wrapped in `{"value": [...]}`.
    ///
    /// Key access supports `$select` and `$expand`, but not `$filter`, `$top`, `$skip`,
    /// `$orderby`, or `$apply`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// // Basic key access
    /// let query = QueryBuilder::by_key("Property", "12345")
    ///     .build()?;
    ///
    /// // With select
    /// let query = QueryBuilder::by_key("Property", "12345")
    ///     .select(&["ListingKey", "City", "ListPrice"])
    ///     .build()?;
    ///
    /// // With expand
    /// let query = QueryBuilder::by_key("Property", "12345")
    ///     .expand(&["ListOffice", "ListAgent"])
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn by_key(resource: impl Into<String>, key: impl Into<String>) -> Self {
        let mut query = Query::new(resource);
        query.key = Some(key.into());
        Self { query }
    }

    /// Add an OData filter expression
    ///
    /// Pass a complete OData filter string. The library does not parse or validate
    /// the filter - it simply URL-encodes it and adds it to the query.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// // Simple equality
    /// let query = QueryBuilder::new("Property")
    ///     .filter("City eq 'Austin'")
    ///     .build()?;
    ///
    /// // Complex conditions
    /// let query = QueryBuilder::new("Property")
    ///     .filter("City eq 'Austin' and ListPrice gt 500000")
    ///     .build()?;
    ///
    /// // Enumeration with 'has' operator
    /// let query = QueryBuilder::new("Property")
    ///     .filter("Appliances has PropertyEnums.Appliances'Dishwasher'")
    ///     .build()?;
    ///
    /// // Collection operations
    /// let query = QueryBuilder::new("Property")
    ///     .filter("OpenHouse/any(x:x/OpenHouseDate eq 2025-06-01)")
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn filter(mut self, expression: impl Into<String>) -> Self {
        self.query.filter = Some(expression.into());
        self
    }

    /// Add an OData apply expression for aggregations
    ///
    /// **⚠️ Server Compatibility Required:** This feature requires server support for
    /// OData v4.0 Aggregation Extensions. Not all RESO servers support `$apply`.
    /// If unsupported, the server will return a 400 error.
    ///
    /// Pass a complete OData apply string. The library does not parse or validate
    /// the apply expression - it simply URL-encodes it and adds it to the query.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// // Group by city with count
    /// let query = QueryBuilder::new("Property")
    ///     .apply("groupby((City), aggregate($count as Count))")
    ///     .build()?;
    ///
    /// // Group by multiple fields
    /// let query = QueryBuilder::new("Property")
    ///     .apply("groupby((City, PropertyType), aggregate($count as Count))")
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Alternative for servers without $apply support
    ///
    /// If your server doesn't support aggregation, use multiple filtered queries instead:
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, QueryBuilder};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let statuses = ["Active", "Pending", "Closed"];
    /// for status in statuses {
    ///     let query = QueryBuilder::new("Property")
    ///         .filter(format!("StandardStatus eq '{}'", status))
    ///         .count()
    ///         .build()?;
    ///
    ///     let response = client.execute(&query).await?;
    ///     let count = response.as_u64().unwrap_or(0);
    ///     println!("{}: {}", status, count);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn apply(mut self, expression: impl Into<String>) -> Self {
        self.query.apply = Some(expression.into());
        self
    }

    /// Select specific fields
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// let query = QueryBuilder::new("Property")
    ///     .select(&["ListingKey", "City", "ListPrice"])
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn select(mut self, fields: &[&str]) -> Self {
        self.query.select_fields = Some(fields.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Expand related entities
    ///
    /// The `$expand` parameter allows you to include related data in a single request,
    /// reducing the number of API calls needed. Common examples include expanding
    /// ListOffice or ListAgent for Property resources.
    ///
    /// **Note:** When using `$select`, you must include the expanded field names
    /// in the select list, otherwise the expansion will be ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// // Expand a single related entity
    /// let query = QueryBuilder::new("Property")
    ///     .expand(&["ListOffice"])
    ///     .build()?;
    ///
    /// // Expand multiple related entities
    /// let query = QueryBuilder::new("Property")
    ///     .expand(&["ListOffice", "ListAgent"])
    ///     .build()?;
    ///
    /// // When using select, include expanded fields
    /// let query = QueryBuilder::new("Property")
    ///     .select(&["ListingKey", "City", "ListPrice", "ListOffice", "ListAgent"])
    ///     .expand(&["ListOffice", "ListAgent"])
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn expand(mut self, fields: &[&str]) -> Self {
        self.query.expand = Some(fields.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Order by a field
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// let query = QueryBuilder::new("Property")
    ///     .order_by("ListPrice", "desc")
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn order_by(mut self, field: &str, direction: &str) -> Self {
        self.query.order_by = Some(format!("{} {}", field, direction));
        self
    }

    /// Limit number of results
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// let query = QueryBuilder::new("Property")
    ///     .top(10)
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn top(mut self, n: u32) -> Self {
        self.query.top = Some(n);
        self
    }

    /// Skip results (for pagination)
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// let query = QueryBuilder::new("Property")
    ///     .skip(100)
    ///     .top(10)
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn skip(mut self, n: u32) -> Self {
        self.query.skip = Some(n);
        self
    }

    /// Include count in response
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// let query = QueryBuilder::new("Property")
    ///     .filter("City eq 'Austin'")
    ///     .with_count()
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn with_count(mut self) -> Self {
        self.query.count = true;
        self
    }

    /// Create a count-only query
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::QueryBuilder;
    /// let query = QueryBuilder::new("Property")
    ///     .filter("City eq 'Austin'")
    ///     .count()
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn count(mut self) -> Self {
        self.query.count_only = true;
        self
    }

    /// Build the query
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Key access is used with incompatible parameters ($filter, $top, $skip, $orderby, $apply, $count)
    pub fn build(self) -> Result<Query> {
        // Validate key access doesn't use incompatible parameters
        if self.query.key.is_some() {
            if self.query.filter.is_some() {
                return Err(ResoError::InvalidQuery(
                    "Key access cannot be used with $filter".to_string(),
                ));
            }
            if self.query.top.is_some() {
                return Err(ResoError::InvalidQuery(
                    "Key access cannot be used with $top".to_string(),
                ));
            }
            if self.query.skip.is_some() {
                return Err(ResoError::InvalidQuery(
                    "Key access cannot be used with $skip".to_string(),
                ));
            }
            if self.query.order_by.is_some() {
                return Err(ResoError::InvalidQuery(
                    "Key access cannot be used with $orderby".to_string(),
                ));
            }
            if self.query.apply.is_some() {
                return Err(ResoError::InvalidQuery(
                    "Key access cannot be used with $apply".to_string(),
                ));
            }
            if self.query.count || self.query.count_only {
                return Err(ResoError::InvalidQuery(
                    "Key access cannot be used with $count".to_string(),
                ));
            }
        }

        Ok(self.query)
    }
}

/// Fluent replication query builder
///
/// Builds queries for the replication endpoint which is designed for
/// bulk data transfer and full dataset synchronization.
///
/// # Constraints
///
/// - Maximum $top: 2000 (vs 200 for standard queries)
/// - No $skip: Use next links from response headers instead
/// - No $orderby: Results ordered oldest to newest by default
/// - No $apply: Aggregations not supported
///
/// # Examples
///
/// ```
/// # use reso_client::ReplicationQueryBuilder;
/// // Basic replication query
/// let query = ReplicationQueryBuilder::new("Property")
///     .top(2000)
///     .build()?;
///
/// // With filter and select
/// let query = ReplicationQueryBuilder::new("Property")
///     .filter("StandardStatus eq 'Active'")
///     .select(&["ListingKey", "City", "ListPrice"])
///     .top(1000)
///     .build()?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct ReplicationQueryBuilder {
    query: ReplicationQuery,
}

impl ReplicationQueryBuilder {
    /// Create a new replication query builder for a resource
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ReplicationQueryBuilder;
    /// let query = ReplicationQueryBuilder::new("Property")
    ///     .top(2000)
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            query: ReplicationQuery::new(resource),
        }
    }

    /// Add an OData filter expression
    ///
    /// Pass a complete OData filter string. The library does not parse or validate
    /// the filter - it simply URL-encodes it and adds it to the query.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ReplicationQueryBuilder;
    /// let query = ReplicationQueryBuilder::new("Property")
    ///     .filter("StandardStatus eq 'Active'")
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn filter(mut self, expression: impl Into<String>) -> Self {
        self.query.filter = Some(expression.into());
        self
    }

    /// Select specific fields
    ///
    /// Using $select is highly recommended for replication queries to reduce
    /// payload size and improve performance.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ReplicationQueryBuilder;
    /// let query = ReplicationQueryBuilder::new("Property")
    ///     .select(&["ListingKey", "City", "ListPrice"])
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn select(mut self, fields: &[&str]) -> Self {
        self.query.select_fields = Some(fields.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Limit number of results (maximum: 2000)
    ///
    /// The replication endpoint allows up to 2000 records per request,
    /// compared to 200 for standard queries.
    ///
    /// # Errors
    ///
    /// Returns an error if `n` exceeds 2000.
    ///
    /// # Examples
    ///
    /// ```
    /// # use reso_client::ReplicationQueryBuilder;
    /// let query = ReplicationQueryBuilder::new("Property")
    ///     .top(2000)
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn top(mut self, n: u32) -> Self {
        self.query.top = Some(n);
        self
    }

    /// Build the replication query
    ///
    /// # Errors
    ///
    /// Returns an error if validation fails (e.g., top > 2000).
    pub fn build(self) -> Result<ReplicationQuery> {
        // Validate top limit - replication endpoint allows up to 2000 records per request
        // This limit is higher than standard queries (200) because replication is designed
        // for bulk data transfer and full dataset synchronization
        if let Some(top) = self.query.top {
            if top > 2000 {
                return Err(ResoError::InvalidQuery(format!(
                    "Replication queries support maximum $top of 2000, got {}",
                    top
                )));
            }
        }

        Ok(self.query)
    }
}
