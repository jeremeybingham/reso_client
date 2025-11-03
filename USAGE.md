# USAGE.md - reso-client Library Reference

**Target Audience:** Developers and LLM agents integrating reso-client into Rust applications.

## Installation
```toml
[dependencies]
reso-client = "0.1.0"
tokio = { version = "1", features = ["full"] }
```

## Core Types
```rust
use reso_client::{
    ResoClient,      // HTTP client for RESO API
    ClientConfig,    // Configuration builder
    QueryBuilder,    // OData query builder
    Query,           // Compiled query
    ResoError,       // Error type
    Result,          // Result<T, ResoError>
    JsonValue,       // Re-export of serde_json::Value
};
```

## Client Creation

### From Environment Variables
```rust
// Requires: RESO_BASE_URL, RESO_TOKEN
// Optional: RESO_DATASET_ID, RESO_TIMEOUT
let client = ResoClient::from_env()?;
```

### Manual Configuration
```rust
use std::time::Duration;

let config = ClientConfig::new("https://api.mls.com/OData", "bearer_token")
    .with_dataset_id("dataset_id")     // Optional
    .with_timeout(Duration::from_secs(60)); // Optional, default 30s

let client = ResoClient::with_config(config)?;
```

## Query Building

### Basic Query Structure
```rust
let query = QueryBuilder::new("Resource")  // Required: resource name
    .filter("expression")                  // Optional: OData filter
    .select(&["field1", "field2"])        // Optional: field projection
    .order_by("field", "asc|desc")        // Optional: sort order
    .top(100)                              // Optional: limit results
    .skip(200)                             // Optional: skip results
    .with_count()                          // Optional: include total count
    .build()?;                             // Returns Result<Query>
```

### Common Resources

- `Property` - Real estate listings
- `Member` - MLS members/agents
- `Office` - MLS offices
- `Media` - Photos and documents
- `OpenHouse` - Open house events

### Filter Syntax (OData 4.0)
```rust
// Comparison operators
"ListPrice gt 500000"
"BedroomsTotal ge 3"
"City eq 'Austin'"
"Status ne 'Closed'"

// Logical operators
"City eq 'Austin' and ListPrice gt 500000"
"City eq 'Austin' or City eq 'Dallas'"
"not (Status eq 'Closed')"

// String functions
"startswith(City, 'San')"
"endswith(City, 'ville')"
"contains(City, 'Spring')"

// Date comparison (ISO 8601 format)
"ModificationTimestamp gt 2025-01-01T00:00:00Z"
"ListingContractDate ge 2025-01-01"

// Parentheses for grouping
"(City eq 'Austin' or City eq 'Dallas') and ListPrice gt 500000"
```

### Field Selection
```rust
// Select specific fields to reduce response size
.select(&["ListingKey", "City", "ListPrice", "BedroomsTotal"])

// Without select, returns all fields
```

### Sorting
```rust
.order_by("ListPrice", "desc")  // Descending
.order_by("City", "asc")        // Ascending
```

### Pagination
```rust
// Page 1: records 0-99
.top(100)

// Page 2: records 100-199
.skip(100).top(100)

// Page 3: records 200-299
.skip(200).top(100)
```

### Count Total Records
```rust
.with_count()  // Adds @odata.count to response
```

### Count-Only Queries
```rust
// Efficient way to get just the count without fetching records
.count()  // Returns just the count via /$count endpoint
```

### OData Aggregation with $apply

**⚠️ Server Compatibility Required**

The `apply()` method supports OData aggregation via the `$apply` parameter. However, **this feature requires server support** for OData v4.0 Aggregation Extensions.

**Not all RESO servers support `$apply`**. If your server doesn't support aggregation, you'll receive a 400 error:
```
{"error":{"code":400,"message":"Invalid parameter - $apply"}}
```

#### Using apply() (when server supports it)
```rust
// Group by field with count
.apply("groupby((StandardStatus), aggregate($count as TotalCount))")

// Group by multiple fields
.apply("groupby((City, PropertyType), aggregate($count as Count))")
```

#### Workaround: Using $filter for counts when $apply is not supported

If your server doesn't support `$apply`, use multiple queries with `$filter` instead:

```rust
// Get counts by status using separate queries
let statuses = ["Active", "Pending", "Closed", "Expired"];

for status in statuses {
    let query = QueryBuilder::new("Property")
        .filter(&format!("StandardStatus eq '{}'", status))
        .count()
        .build()?;

    let response = client.execute(&query).await?;
    let count = response.as_u64().unwrap_or(0);
    println!("   {}: {}", status, count);
}
```

This approach is more widely compatible and works with all RESO servers that support basic filtering.

## Executing Queries

### Standard Execution
```rust
let response: JsonValue = client.execute(&query).await?;

// Response structure:
// {
//   "value": [...],              // Array of records
//   "@odata.count": 123,         // Total count (if with_count() used)
//   "@odata.nextLink": "...",    // Next page URL (if server paginates)
//   "@odata.context": "..."      // Metadata context
// }
```

### Accessing Response Data
```rust
// Get records array
if let Some(records) = response["value"].as_array() {
    for record in records {
        let key = record["ListingKey"].as_str().unwrap_or("");
        let price = record["ListPrice"].as_f64().unwrap_or(0.0);
        let city = record["City"].as_str().unwrap_or("");
    }
}

// Get total count (when with_count() used)
if let Some(count) = response["@odata.count"].as_u64() {
    println!("Total records: {}", count);
}

// Check for next page
if let Some(next_link) = response["@odata.nextLink"].as_str() {
    println!("More results available at: {}", next_link);
}
```

### Metadata Retrieval
```rust
let metadata_xml: String = client.fetch_metadata().await?;
// Returns XML schema document describing available resources and fields
```

## Error Handling

### Error Types
```rust
enum ResoError {
    Config(String),       // Configuration errors (missing env vars, invalid config)
    Network(String),      // HTTP/network errors
    ODataError(String),   // API returned error (4xx, 5xx)
    Parse(String),        // JSON parsing errors
    InvalidQuery(String), // Query construction errors
}
```

### Pattern Matching
```rust
match client.execute(&query).await {
    Ok(response) => {
        // Process response
    },
    Err(ResoError::Config(msg)) => {
        // Handle configuration error
    },
    Err(ResoError::Network(msg)) => {
        // Handle network error, retry logic
    },
    Err(ResoError::ODataError(msg)) => {
        // Handle API error (check filter syntax, permissions)
    },
    Err(ResoError::Parse(msg)) => {
        // Handle parsing error
    },
    Err(e) => {
        // Generic error handling
    },
}
```

## Complete Examples

### Simple Query
```rust
use reso_client::{ResoClient, QueryBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ResoClient::from_env()?;
    
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .top(10)
        .build()?;
    
    let response = client.execute(&query).await?;
    
    if let Some(records) = response["value"].as_array() {
        println!("Found {} properties", records.len());
    }
    
    Ok(())
}
```

### Filtered Query with Specific Fields
```rust
let query = QueryBuilder::new("Property")
    .filter("ListPrice gt 500000 and BedroomsTotal ge 3")
    .select(&["ListingKey", "City", "ListPrice", "BedroomsTotal"])
    .order_by("ListPrice", "desc")
    .top(50)
    .build()?;

let response = client.execute(&query).await?;
```

### Paginated Query with Count
```rust
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .with_count()
    .skip(0)
    .top(100)
    .build()?;

let response = client.execute(&query).await?;

let total = response["@odata.count"].as_u64().unwrap_or(0);
let records = response["value"].as_array().unwrap_or(&vec![]).len();

println!("Showing {} of {} total records", records, total);
```

### Processing All Records
```rust
let mut skip = 0;
let page_size = 100;
let mut all_records = Vec::new();

loop {
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .skip(skip)
        .top(page_size)
        .build()?;
    
    let response = client.execute(&query).await?;
    
    let records = response["value"]
        .as_array()
        .ok_or("No value field")?;
    
    if records.is_empty() {
        break;
    }
    
    all_records.extend(records.iter().cloned());
    skip += page_size;
}

println!("Retrieved {} total records", all_records.len());
```

### Date Range Query
```rust
let query = QueryBuilder::new("Property")
    .filter("ModificationTimestamp gt 2025-01-01T00:00:00Z and ModificationTimestamp lt 2025-02-01T00:00:00Z")
    .select(&["ListingKey", "ModificationTimestamp", "City"])
    .order_by("ModificationTimestamp", "desc")
    .build()?;
```

### Complex Filter with Multiple Conditions
```rust
let query = QueryBuilder::new("Property")
    .filter("(City eq 'Austin' or City eq 'Dallas') and ListPrice gt 500000 and ListPrice lt 2000000 and BedroomsTotal ge 3 and Status eq 'Active'")
    .select(&["ListingKey", "City", "ListPrice", "BedroomsTotal"])
    .order_by("ListPrice", "asc")
    .top(100)
    .build()?;
```

## Dataset ID Usage

Some RESO providers require a dataset identifier in the URL path.

### URL Structure

**Without dataset ID:**
```
https://api.mls.com/OData/Property?$filter=...
```

**With dataset ID:**
```
https://api.mls.com/OData/dataset_id/Property?$filter=...
```

### Configuration
```rust
// Via environment variable
// RESO_DATASET_ID=actris_ref
let client = ResoClient::from_env()?;

// Via builder
let config = ClientConfig::new("https://api.mls.com/OData", "token")
    .with_dataset_id("actris_ref");
let client = ResoClient::with_config(config)?;
```

## Environment Variables

| Variable | Required | Description | Example |
|----------|----------|-------------|---------|
| `RESO_BASE_URL` | Yes | Base API URL | `https://api.bridgedataoutput.com/api/v2/OData` |
| `RESO_TOKEN` | Yes | Bearer token | `your_token_here` |
| `RESO_DATASET_ID` | No | Dataset identifier | `actris_ref` |
| `RESO_TIMEOUT` | No | Timeout (seconds) | `60` (default: 30) |

## Thread Safety

- `ResoClient` is `Send + Sync` and can be shared across threads
- Recommended: Create one client and clone/share via `Arc<ResoClient>`
- `Query` and `QueryBuilder` are not thread-safe (create per-thread)
```rust
use std::sync::Arc;

let client = Arc::new(ResoClient::from_env()?);

// Clone Arc for each thread/task
let client_clone = Arc::clone(&client);
tokio::spawn(async move {
    let query = QueryBuilder::new("Property").top(10).build()?;
    let response = client_clone.execute(&query).await?;
    // Process response
    Ok::<_, ResoError>(())
});
```

## Performance Tips

1. **Use `select()`** to request only needed fields - reduces bandwidth and parsing time
2. **Use pagination** with `top()` and `skip()` for large result sets
3. **Reuse `ResoClient`** - HTTP connection pooling is automatic
4. **Use `with_count()` only when needed** - adds overhead on server side
5. **Filter on server side** - always prefer `filter()` over client-side filtering
6. **Batch requests** - make concurrent queries when possible (client is async)

## Security Notes

- Bearer tokens are automatically redacted in debug output
- Never log or print `ClientConfig` or raw token values
- Use environment variables or secure configuration management for tokens
- Tokens are sent in `Authorization: Bearer <token>` header (HTTPS required)

## Common Patterns

### Check if Record Exists
```rust
let query = QueryBuilder::new("Property")
    .filter("ListingKey eq '12345'")
    .top(1)
    .build()?;

let response = client.execute(&query).await?;
let exists = response["value"].as_array()
    .map(|arr| !arr.is_empty())
    .unwrap_or(false);
```

### Get Single Record by Key
```rust
let query = QueryBuilder::new("Property")
    .filter("ListingKey eq '12345'")
    .top(1)
    .build()?;

let response = client.execute(&query).await?;
let record = response["value"]
    .as_array()
    .and_then(|arr| arr.first());
```

### Count Records Matching Filter
```rust
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .with_count()
    .top(0)  // Don't return records, just count
    .build()?;

let response = client.execute(&query).await?;
let count = response["@odata.count"].as_u64().unwrap_or(0);
```

## API Reference Summary

### ClientConfig Methods
- `from_env() -> Result<Self>`
- `new(base_url, token) -> Self`
- `with_dataset_id(id) -> Self`
- `with_timeout(duration) -> Self`

### ResoClient Methods
- `from_env() -> Result<Self>`
- `with_config(config) -> Result<Self>`
- `base_url(&self) -> &str`
- `execute(&self, query: &Query) -> Result<JsonValue>`
- `fetch_metadata(&self) -> Result<String>`

### QueryBuilder Methods
- `new(resource) -> Self`
- `filter(expression) -> Self`
- `select(fields: &[&str]) -> Self`
- `order_by(field, direction) -> Self`
- `top(n: u32) -> Self`
- `skip(n: u32) -> Self`
- `with_count() -> Self`
- `count() -> Self`
- `apply(expression) -> Self` ⚠️ Requires server support for OData aggregation
- `build() -> Result<Query>`

### Query Methods
- `new(resource) -> Self`
- `to_odata_string(&self) -> String`

## URL Encoding

- Filter expressions are automatically URL-encoded
- Order-by expressions are automatically URL-encoded
- Resource names are NOT encoded (use valid identifiers)
- Field names in `select()` are NOT encoded (use valid identifiers)

## Limitations

- No support for `$expand` (navigation properties)
- `$apply` (aggregation) requires server support for OData v4.0 Aggregation Extensions
- No support for batch requests (`$batch`)
- No built-in retry logic (implement at application level)
- No response caching (implement at application level)
