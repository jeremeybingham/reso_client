// src/queries.rs

//! Query building for RESO/OData requests

use crate::error::Result;

/// A structured RESO/OData query
#[derive(Debug, Clone)]
pub struct Query {
    resource: String,
    filter: Option<String>,
    select_fields: Option<Vec<String>>,
    order_by: Option<String>,
    top: Option<u32>,
    skip: Option<u32>,
    count: bool,
    count_only: bool,
}

impl Query {
    /// Create a new query for a resource
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            filter: None,
            select_fields: None,
            order_by: None,
            top: None,
            skip: None,
            count: false,
            count_only: false,
        }
    }

    /// Convert to OData query string
    pub fn to_odata_string(&self) -> String {
        let mut parts = vec![self.resource.clone()];

        // For count-only queries, append /$count and only use filter
        if self.count_only {
            parts.push("/$count".to_string());

            if let Some(filter) = &self.filter {
                parts.push("?".to_string());
                parts.push(format!("$filter={}", urlencoding::encode(filter)));
            }

            return parts.concat();
        }

        let mut params = Vec::new();

        // $filter
        if let Some(filter) = &self.filter {
            params.push(format!("$filter={}", urlencoding::encode(filter)));
        }

        // $select
        if let Some(fields) = &self.select_fields {
            params.push(format!("$select={}", fields.join(",")));
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
    pub fn build(self) -> Result<Query> {
        Ok(self.query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_builder_basic() {
        let query = QueryBuilder::new("Property").top(10).build().unwrap();

        assert_eq!(query.to_odata_string(), "Property?$top=10");
    }

    #[test]
    fn test_query_resource_only() {
        let query = QueryBuilder::new("Property").build().unwrap();

        assert_eq!(query.to_odata_string(), "Property");
    }

    #[test]
    fn test_query_with_filter() {
        let query = QueryBuilder::new("Property")
            .filter("City eq 'Austin'")
            .build()
            .unwrap();

        let url = query.to_odata_string();
        assert!(url.starts_with("Property?"));
        assert!(url.contains("$filter=City%20eq%20%27Austin%27"));
    }

    #[test]
    fn test_query_with_select() {
        let query = QueryBuilder::new("Property")
            .select(&["ListingKey", "City", "ListPrice"])
            .build()
            .unwrap();

        let url = query.to_odata_string();
        assert!(url.contains("$select=ListingKey,City,ListPrice"));
    }

    #[test]
    fn test_query_with_orderby() {
        let query = QueryBuilder::new("Property")
            .order_by("ListPrice", "desc")
            .build()
            .unwrap();

        let url = query.to_odata_string();
        assert!(url.contains("$orderby=ListPrice%20desc"));
    }

    #[test]
    fn test_query_with_skip() {
        let query = QueryBuilder::new("Property").skip(20).build().unwrap();

        let url = query.to_odata_string();
        assert!(url.contains("$skip=20"));
    }

    #[test]
    fn test_query_with_count() {
        let query = QueryBuilder::new("Property").with_count().build().unwrap();

        let url = query.to_odata_string();
        assert!(url.contains("$count=true"));
    }

    #[test]
    fn test_query_with_multiple_params() {
        let query = QueryBuilder::new("Property")
            .filter("City eq 'Austin'")
            .select(&["ListingKey", "City"])
            .top(5)
            .skip(10)
            .order_by("ListPrice", "desc")
            .with_count()
            .build()
            .unwrap();

        let url = query.to_odata_string();

        // Verify all parameters are present
        assert!(url.starts_with("Property?"));
        assert!(url.contains("$filter="));
        assert!(url.contains("$select=ListingKey,City"));
        assert!(url.contains("$top=5"));
        assert!(url.contains("$skip=10"));
        assert!(url.contains("$orderby="));
        assert!(url.contains("$count=true"));
    }

    #[test]
    fn test_query_filter_url_encoding() {
        let query = QueryBuilder::new("Property")
            .filter("City eq 'San Francisco' and ListPrice gt 1000000")
            .build()
            .unwrap();

        let url = query.to_odata_string();
        // Spaces and quotes should be URL encoded
        assert!(url.contains("%20")); // Space encoded
        assert!(url.contains("%27")); // Single quote encoded
    }

    #[test]
    fn test_query_complex_filter() {
        let query = QueryBuilder::new("Property")
            .filter("(City eq 'Austin' or City eq 'Dallas') and ListPrice gt 500000")
            .build()
            .unwrap();

        let url = query.to_odata_string();
        assert!(url.contains("$filter="));
        assert!(url.contains("Austin"));
    }

    #[test]
    fn test_query_direct_construction() {
        let query = Query::new("Member");
        assert_eq!(query.to_odata_string(), "Member");
    }

    #[test]
    fn test_query_pagination() {
        // First page
        let query1 = QueryBuilder::new("Property").top(20).build().unwrap();
        assert_eq!(query1.to_odata_string(), "Property?$top=20");

        // Second page
        let query2 = QueryBuilder::new("Property")
            .skip(20)
            .top(20)
            .build()
            .unwrap();
        let url = query2.to_odata_string();
        assert!(url.contains("$skip=20"));
        assert!(url.contains("$top=20"));
    }
}
