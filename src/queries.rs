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
        }
    }
    
    /// Convert to OData query string
    pub fn to_odata_string(&self) -> String {
        let mut parts = vec![self.resource.clone()];
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
            params.push(format!("$orderby={}", order));
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
    
    /// Build the query
    pub fn build(self) -> Result<Query> {
        Ok(self.query)
    }
}