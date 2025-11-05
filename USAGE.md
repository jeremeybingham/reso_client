# USAGE.md - reso-client Library Reference

**Target Audience:** Developers and LLM agents integrating reso-client into Rust applications.

## Table of Contents

- [What is RESO?](#what-is-reso)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Installation](#installation)
  - [Configuration](#configuration)
- [Working Examples](#working-examples)
- [Core Concepts](#core-concepts)
- [API Reference](#api-reference)
- [Quick Reference Card](#quick-reference-card)
- [Troubleshooting](#troubleshooting)

## What is RESO?

**RESO (Real Estate Standards Organization)** is a non-profit organization that creates data standards for the real estate industry. The **RESO Web API** is a standardized REST API specification built on **OData 4.0** that allows applications to access Multiple Listing Service (MLS) data.

### Key Concepts

- **MLS (Multiple Listing Service)**: Database of real estate listings maintained by real estate brokers
- **OData**: Open standard protocol for building and consuming RESTful APIs, used by RESO
- **Resources**: Entities like Property, Member (agents), Office, Media, OpenHouse
- **Bearer Token**: OAuth authentication token required for API access
- **Dataset ID**: Some providers require a dataset identifier in the URL path

### This Library

`reso-client` is a Rust client library that provides:
- Type-safe query building for OData/RESO APIs
- Automatic URL encoding and query parameter formatting
- Support for standard queries and bulk replication
- Comprehensive error handling
- Async/await support with tokio

## Getting Started

### Prerequisites

1. **Rust toolchain** - Install from [rustup.rs](https://rustup.rs/)
2. **RESO API credentials** - Obtain from your MLS provider:
   - **Base URL** - The OData endpoint URL (e.g., `https://api.mls.com/api/v2/OData`)
   - **Bearer Token** - OAuth authentication token
   - **Dataset ID** (optional) - Some providers require this

### How to Obtain RESO Credentials

Contact your MLS provider or data vendor to request:
1. API access credentials (bearer token)
2. Base URL endpoint
3. Documentation on available resources and fields
4. Rate limits and usage guidelines

Common RESO data providers include:
- Bridge Interactive (BridgeDataOutput)
- CoreLogic
- Rapattoni
- FBS (Flexmls)
- And many MLS-specific providers

## Working Examples

The library includes comprehensive examples in the `examples/` directory demonstrating all functionality:

- **Basic**: `test_connectivity`, `test_property`, `test_member`
- **Query Features**: `test_filters`, `test_select`, `test_count_only`, `test_pagination_nextlink`
- **Analysis Examples**: `analyze_property_fields`, `analyze_active_listings`
- **Advanced**: `test_replication`, `test_metadata`, `test_core_queries`
- **Server-Specific**: `test_apply`, `test_expand` (requires server support)

Run any example with: `cargo run --example <name>`

All examples include detailed comments and demonstrate error handling patterns.

### Analysis Examples

**Property Field Usage Analyzer** (`analyze_property_fields`):
Analyzes 200 active listings to determine which fields are most populated across the dataset. Generates a detailed JSON report (`property_field_analysis_report.json`) containing:
- Field-by-field population statistics
- Recommended field sets (minimal, standard, comprehensive)
- Sample values for each field
- Fields categorized by usage rate (highly used ≥80%, moderately used 40-79%, rarely used <40%, never used)

This example helps you optimize your queries by selecting only the fields that contain meaningful data.

**Active Listings Statistical Analysis** (`analyze_active_listings`):
Queries 200 active residential listings and performs comprehensive statistical analysis:
- Price analysis (average, median, min, max)
- Property type distribution
- Geographic distribution by state and city
- Bedroom and bathroom statistics
- Size statistics (living area, lot size in sq ft and acres)
- Year built and property age analysis
- Photo count statistics

This example demonstrates practical use of the RESO client for market analysis and data insights using a suggested minimal field set.

## Installation
```toml
[dependencies]
# Import the RESO client from GitHub
reso-client = { git = "https://github.com/jeremeybingham/reso_client" }
tokio = { version = "1", features = ["full"] }
```

## Core Types
```rust
use reso_client::{
    ResoClient,                 // HTTP client for RESO API
    ClientConfig,               // Configuration builder
    QueryBuilder,               // OData query builder
    Query,                      // Compiled query
    ReplicationQueryBuilder,    // Replication query builder
    ReplicationQuery,           // Compiled replication query
    ReplicationResponse,        // Replication response with records and next link
    ResoError,                  // Error type
    Result,                     // Result<T, ResoError>
    JsonValue,                  // Re-export of serde_json::Value
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

### Direct Key Access

For efficient single-record lookups, use direct key access instead of filters:

```rust
// Instead of using filter:
// .filter("ListingKey eq '12345'")

// Use direct key access (more efficient):
let query = QueryBuilder::by_key("Property", "12345")
    .build()?;

// Returns a single record object, not wrapped in {"value": [...]}
let record = client.execute_by_key(&query).await?;

// With field selection:
let query = QueryBuilder::by_key("Property", "12345")
    .select(&["ListingKey", "City", "ListPrice"])
    .build()?;

// With expand (see below):
let query = QueryBuilder::by_key("Property", "12345")
    .expand(&["ListOffice", "ListAgent"])
    .build()?;
```

**Key access limitations:**
- Only supports `$select` and `$expand`
- Cannot use `$filter`, `$top`, `$skip`, `$orderby`, `$apply`, or `$count`
- Key access queries use `execute_by_key()` instead of `execute()`

### Expanding Related Entities

The `$expand` parameter allows you to include related data in a single request:

```rust
// Expand a single related entity
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .expand(&["ListOffice"])
    .top(10)
    .build()?;

// Expand multiple related entities
let query = QueryBuilder::new("Property")
    .expand(&["ListOffice", "ListAgent"])
    .top(10)
    .build()?;

// When using select with expand, include the expanded field names
let query = QueryBuilder::new("Property")
    .select(&["ListingKey", "City", "ListPrice", "ListOffice", "ListAgent"])
    .expand(&["ListOffice", "ListAgent"])
    .top(10)
    .build()?;

let response = client.execute(&query).await?;

// Access expanded data
if let Some(records) = response["value"].as_array() {
    for record in records {
        let city = record["City"].as_str().unwrap_or("");

        // Access expanded ListOffice data
        if let Some(office) = record.get("ListOffice") {
            let office_name = office["OfficeName"].as_str().unwrap_or("");
            println!("City: {}, Office: {}", city, office_name);
        }
    }
}
```

**Note:** Not all RESO servers support `$expand`. The RESO Web API reference server (`actris_ref`) does not support this feature.

### Count Total Records
```rust
.with_count()  // Adds @odata.count to response
```

### Count-Only Queries
```rust
// Efficient way to get just the count without fetching records
.count()  // Returns just the count via /$count endpoint

// Method 1: Using execute() - returns JSON number
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .count()
    .build()?;

let response = client.execute(&query).await?;
let count = response.as_u64().unwrap_or(0);
println!("Total: {}", count);

// Method 2: Using execute_count() - returns u64 directly
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .count()
    .build()?;

let count = client.execute_count(&query).await?;
println!("Total: {}", count);
```

### OData Aggregation with $apply

**⚠️ Server Compatibility Required, NOT supported by the RESO Web API reference server / `actris_ref`**

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
**⚠️ Use this method for the RESO Web API reference server / `actris_ref`**

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

## Replication Queries

Replication queries are designed for bulk data transfer and dataset synchronization. They differ from standard queries in several ways:

### Key Differences from Standard Queries

| Feature | Standard Query | Replication Query |
|---------|---------------|-------------------|
| Maximum `$top` | 200 | 2000 |
| `$skip` support | Yes | No (use next links) |
| `$orderby` support | Yes | No (ordered oldest→newest) |
| `$count` support | Yes | No |
| `$apply` support | Optional | No |
| Pagination | Query params | Header-based (next link) |
| Authorization | Standard | Requires MLS authorization |

### Building Replication Queries

```rust
use reso_client::{ResoClient, ReplicationQueryBuilder};

// Basic replication query
let query = ReplicationQueryBuilder::new("Property")
    .top(2000)  // Maximum: 2000
    .build()?;

// With filter
let query = ReplicationQueryBuilder::new("Property")
    .filter("StandardStatus eq 'Active'")
    .top(1000)
    .build()?;

// With select (highly recommended for performance)
let query = ReplicationQueryBuilder::new("Property")
    .filter("StandardStatus eq 'Active'")
    .select(&["ListingKey", "City", "ListPrice", "ModificationTimestamp"])
    .top(2000)
    .build()?;
```

### Executing Replication Queries

```rust
// Execute the query
let response = client.execute_replication(&query).await?;

// Response structure:
// ReplicationResponse {
//     records: Vec<JsonValue>,      // Array of records
//     next_link: Option<String>,     // URL for next batch
//     record_count: usize,           // Number of records in this batch
// }

// Access records
for record in &response.records {
    let key = record["ListingKey"].as_str().unwrap_or("");
    let city = record["City"].as_str().unwrap_or("");
    println!("{}: {}", key, city);
}

// Check if more records are available
if response.has_more() {
    println!("More records available");
}

// Get next link
if let Some(next_link) = response.next_link() {
    let next_response = client.execute_next_link(next_link).await?;
}
```

### Pagination with Next Links

Unlike standard queries that use `$skip`, replication uses header-based pagination:

```rust
let query = ReplicationQueryBuilder::new("Property")
    .top(2000)
    .build()?;

let mut response = client.execute_replication(&query).await?;
let mut total_count = response.record_count;

// Continue fetching while next link exists
while let Some(next_link) = response.next_link {
    response = client.execute_next_link(&next_link).await?;
    total_count += response.record_count;

    println!("Fetched batch of {} records (total: {})",
             response.record_count, total_count);
}

println!("Total records fetched: {}", total_count);
```

### Complete Replication Example

```rust
use reso_client::{ResoClient, ReplicationQueryBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ResoClient::from_env()?;

    // Build query for active properties
    let query = ReplicationQueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&["ListingKey", "City", "ListPrice", "ModificationTimestamp"])
        .top(2000)
        .build()?;

    // Execute and collect all records
    let mut response = client.execute_replication(&query).await?;
    let mut all_records = response.records;

    println!("Initial batch: {} records", response.record_count);

    // Fetch remaining batches
    while let Some(next_link) = response.next_link {
        response = client.execute_next_link(&next_link).await?;
        all_records.extend(response.records);
        println!("Fetched batch: {} records (total: {})",
                 response.record_count, all_records.len());
    }

    println!("Replication complete: {} total records", all_records.len());

    Ok(())
}
```

### Best Practices for Replication

1. **Use `$select`** - Always specify needed fields to reduce payload size
2. **Handle pagination** - Use `next_link` to fetch all records
3. **Monitor progress** - Track `record_count` for each batch
4. **Filter wisely** - Use filters to replicate only changed records
5. **Error handling** - Implement retry logic for network failures
6. **Rate limiting** - Consider adding delays between requests for large datasets

### When to Use Replication

Use replication queries when:
- Fetching large datasets (>10,000 records)
- Performing initial data synchronization
- Creating data backups or mirrors
- Transferring complete datasets between systems

Use standard queries when:
- Searching for specific records
- Displaying paginated results to users
- Getting counts or aggregations
- Needing custom sort orders

## Error Handling

### Error Types
```rust
enum ResoError {
    // Configuration errors (missing env vars, invalid config)
    Config(String),

    // HTTP/network errors (connection failures, timeouts)
    Network(String),

    // 401 Unauthorized - Invalid or missing authentication token
    Unauthorized { message: String, status_code: u16 },

    // 403 Forbidden - Valid credentials but insufficient permissions
    Forbidden { message: String, status_code: u16 },

    // 404 Not Found - Resource or endpoint not found
    NotFound { message: String, status_code: u16 },

    // 429 Too Many Requests - Rate limit exceeded
    RateLimited { message: String, status_code: u16 },

    // 5xx Server Error - Server-side error
    ServerError { message: String, status_code: u16 },

    // Generic OData server error for other status codes
    ODataError { message: String, status_code: u16 },

    // JSON parsing errors
    Parse(String),

    // Query construction errors
    InvalidQuery(String),
}
```

### Pattern Matching
```rust
match client.execute(&query).await {
    Ok(response) => {
        // Process response
    },
    Err(ResoError::Config(msg)) => {
        eprintln!("Configuration error: {}", msg);
        // Check environment variables or ClientConfig
    },
    Err(ResoError::Network(msg)) => {
        eprintln!("Network error: {}", msg);
        // Implement retry logic, check network connectivity
    },
    Err(ResoError::Unauthorized { message, status_code }) => {
        eprintln!("Unauthorized ({}): {}", status_code, message);
        // Check bearer token validity, request new token
    },
    Err(ResoError::Forbidden { message, status_code }) => {
        eprintln!("Forbidden ({}): {}", status_code, message);
        // Check API permissions with MLS provider
    },
    Err(ResoError::NotFound { message, status_code }) => {
        eprintln!("Not Found ({}): {}", status_code, message);
        // Check resource name, endpoint path, dataset ID
    },
    Err(ResoError::RateLimited { message, status_code }) => {
        eprintln!("Rate Limited ({}): {}", status_code, message);
        // Implement backoff strategy, reduce request frequency
    },
    Err(ResoError::ServerError { message, status_code }) => {
        eprintln!("Server Error ({}): {}", status_code, message);
        // Retry with exponential backoff, contact provider if persistent
    },
    Err(ResoError::ODataError { message, status_code }) => {
        eprintln!("OData Error ({}): {}", status_code, message);
        // Check filter syntax, query parameters
    },
    Err(ResoError::Parse(msg)) => {
        eprintln!("Parse error: {}", msg);
        // Unexpected response format, check API version compatibility
    },
    Err(ResoError::InvalidQuery(msg)) => {
        eprintln!("Invalid query: {}", msg);
        // Fix query construction (e.g., key access with incompatible params)
    },
}
```

### Retry Logic Example
```rust
use std::time::Duration;
use tokio::time::sleep;

async fn execute_with_retry(
    client: &ResoClient,
    query: &Query,
    max_retries: u32,
) -> Result<JsonValue, ResoError> {
    let mut retries = 0;

    loop {
        match client.execute(query).await {
            Ok(response) => return Ok(response),
            Err(e) => {
                // Retry on network errors and rate limits
                let should_retry = matches!(e,
                    ResoError::Network(_) |
                    ResoError::RateLimited { .. } |
                    ResoError::ServerError { .. }
                );

                if should_retry && retries < max_retries {
                    retries += 1;
                    let backoff = Duration::from_secs(2_u64.pow(retries));
                    eprintln!("Retry {}/{} after {:?}", retries, max_retries, backoff);
                    sleep(backoff).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
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

Some RESO providers, including the RESO Web API reference server, require a dataset identifier in the URL path.

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
- `execute(&self, query: &Query) -> Result<JsonValue>` - Execute standard query
- `execute_by_key(&self, query: &Query) -> Result<JsonValue>` - Execute direct key access query
- `execute_count(&self, query: &Query) -> Result<u64>` - Execute count-only query, returns integer
- `execute_replication(&self, query: &ReplicationQuery) -> Result<ReplicationResponse>` - Execute replication query
- `execute_next_link(&self, next_link: &str) -> Result<ReplicationResponse>` - Fetch next replication batch
- `fetch_metadata(&self) -> Result<String>` - Fetch OData metadata XML

### QueryBuilder Methods
- `new(resource) -> Self` - Create standard query builder
- `by_key(resource, key) -> Self` - Create key access query builder
- `filter(expression) -> Self` - Add OData filter
- `select(fields: &[&str]) -> Self` - Select specific fields
- `expand(fields: &[&str]) -> Self` - Expand related entities (not supported by all servers)
- `order_by(field, direction) -> Self` - Sort results
- `top(n: u32) -> Self` - Limit results
- `skip(n: u32) -> Self` - Skip results for pagination
- `with_count() -> Self` - Include total count in response
- `count() -> Self` - Count-only query via /$count endpoint
- `apply(expression) -> Self` - OData aggregation (⚠️ requires server support)
- `build() -> Result<Query>` - Build the query

### Query Methods
- `new(resource) -> Self`
- `to_odata_string(&self) -> String`

### ReplicationQueryBuilder Methods
- `new(resource) -> Self`
- `filter(expression) -> Self`
- `select(fields: &[&str]) -> Self`
- `top(n: u32) -> Self` (maximum: 2000)
- `build() -> Result<ReplicationQuery>`

### ReplicationQuery Methods
- `new(resource) -> Self`
- `to_odata_string(&self) -> String`
- `resource(&self) -> &str`

### ReplicationResponse Methods
- `new(records, next_link) -> Self`
- `has_more(&self) -> bool`
- `next_link(&self) -> Option<&str>`

### ReplicationResponse Fields
- `records: Vec<JsonValue>` - Array of records
- `next_link: Option<String>` - URL for next batch
- `record_count: usize` - Number of records in this batch

## URL Encoding

- Filter expressions are automatically URL-encoded
- Order-by expressions are automatically URL-encoded
- Resource names are NOT encoded (use valid identifiers)
- Field names in `select()` are NOT encoded (use valid identifiers)

## Using from Other Languages

While this library is written in Rust, it can be used from other programming languages through various approaches:

### Rust Applications (Native)

Simply add to your `Cargo.toml`:
```toml
[dependencies]
reso-client = { git = "https://github.com/jeremeybingham/reso_client" }
```

### Python (via PyO3)

You can create Python bindings using [PyO3](https://pyo3.rs/):

```rust
// Example wrapper (not included in this library)
use pyo3::prelude::*;
use reso_client::{ResoClient, QueryBuilder};

#[pyclass]
struct PyResoClient {
    client: ResoClient,
}

#[pymethods]
impl PyResoClient {
    #[new]
    fn new() -> PyResult<Self> {
        let client = ResoClient::from_env()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
        Ok(Self { client })
    }

    fn query(&self, py: Python, resource: &str, filter: Option<&str>) -> PyResult<PyObject> {
        // Implementation here
        todo!()
    }
}
```

### JavaScript/Node.js (via NAPI-RS)

Create Node.js bindings using [napi-rs](https://napi.rs/):

```rust
// Example wrapper (not included in this library)
#[napi]
pub struct ResoClientNode {
    client: ResoClient,
}

#[napi]
impl ResoClientNode {
    #[napi(constructor)]
    pub fn new(base_url: String, token: String) -> napi::Result<Self> {
        let config = ClientConfig::new(base_url, token);
        let client = ResoClient::with_config(config)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Self { client })
    }

    #[napi]
    pub async fn execute(&self, resource: String) -> napi::Result<String> {
        // Implementation here
        todo!()
    }
}
```

### C/C++ (via FFI)

Create C-compatible bindings:

```rust
// Example FFI wrapper (not included in this library)
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

#[no_mangle]
pub extern "C" fn reso_client_new(
    base_url: *const c_char,
    token: *const c_char,
) -> *mut ResoClient {
    // Implementation here
    todo!()
}

#[no_mangle]
pub extern "C" fn reso_client_execute(
    client: *mut ResoClient,
    query: *const c_char,
) -> *mut c_char {
    // Implementation here
    todo!()
}
```

### WebAssembly (WASM)

Compile to WebAssembly for browser/WASM runtime use:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
wasm-bindgen = "0.2"
```

```rust
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct ResoClientWasm {
    client: ResoClient,
}

#[wasm_bindgen]
impl ResoClientWasm {
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: String, token: String) -> Result<ResoClientWasm, JsValue> {
        // Implementation here
        todo!()
    }
}
```

### Command-Line Interface

Create a CLI wrapper for use from shell scripts or any language that can call external programs:

```rust
// Example CLI (not included in this library)
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(short, long)]
    resource: String,

    #[arg(short, long)]
    filter: Option<String>,

    #[arg(long)]
    output: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let client = ResoClient::from_env()?;

    let query = QueryBuilder::new(&args.resource);
    // Build and execute query, output JSON
    Ok(())
}
```

Then use from any language:
```bash
reso-cli --resource Property --filter "City eq 'Austin'" --output results.json
```

### HTTP Service Wrapper

Create a REST API service that wraps this library:

```rust
// Example using axum (not included in this library)
use axum::{routing::post, Json, Router};

async fn query(Json(payload): Json<QueryRequest>) -> Json<serde_json::Value> {
    let client = ResoClient::from_env().unwrap();
    let query = QueryBuilder::new(&payload.resource)
        .filter(&payload.filter)
        .build()
        .unwrap();

    let response = client.execute(&query).await.unwrap();
    Json(response)
}

#[tokio::main]
async fn main() {
    let app = Router::new().route("/query", post(query));
    // Start server
}
```

Then call from any language via HTTP:
```bash
curl -X POST http://localhost:3000/query \
  -H "Content-Type: application/json" \
  -d '{"resource": "Property", "filter": "City eq '\''Austin'\''"}'
```

**Note:** The above examples are illustrative. Creating production-ready language bindings requires additional error handling, memory management, and thorough testing.

## Limitations

### Library Limitations
- No support for batch requests (`$batch`)
- No built-in response caching (implement at application level)
- No built-in retry logic for standard queries (see error handling examples)

### Server-Dependent Features

Some features depend on server support and may not work with all RESO providers:

- **`$expand`** (navigation properties) - Not supported by RESO Web API reference server (`actris_ref`)
- **`$apply`** (aggregation) - Requires OData v4.0 Aggregation Extensions (not supported by `actris_ref`)
- **Replication endpoint** - Requires MLS authorization and has limited parameter support:
  - No `$skip` (use next links)
  - No `$orderby` (ordered oldest to newest)
  - No `$apply` or count options
  - Maximum `$top` of 2000

### Best Practices for Compatibility

To maximize compatibility across different RESO servers:
1. Test features with your specific provider before deploying
2. Use filter-based workarounds for aggregation when `$apply` is not supported
3. Implement graceful fallbacks for unsupported features
4. Consult your provider's documentation for supported OData features

## Troubleshooting

### Common Issues and Solutions

#### 401 Unauthorized Error
**Symptom:** `Unauthorized (401): Invalid credentials`

**Solutions:**
1. Verify bearer token is correct and not expired
2. Check `RESO_TOKEN` environment variable
3. Request new token from MLS provider
4. Ensure token has no leading/trailing whitespace

#### 403 Forbidden Error
**Symptom:** `Forbidden (403): Access denied`

**Solutions:**
1. Verify your account has permissions for the requested resource
2. Check if your account has access to the specific dataset ID
3. Contact MLS provider to verify API access level
4. Ensure you're not accessing restricted resources

#### 404 Not Found Error
**Symptom:** `Not Found (404): Resource not found`

**Solutions:**
1. Verify the resource name is correct (case-sensitive)
2. Check if `RESO_DATASET_ID` is required and correctly set
3. Verify base URL is correct
4. Check if the endpoint exists on your server (e.g., `/replication`)
5. Use `fetch_metadata()` to discover available resources

#### Invalid Parameter Errors
**Symptom:** `{"error":{"code":400,"message":"Invalid parameter - $apply"}}`

**Solutions:**
1. Check if your server supports the parameter (see limitations)
2. For `$apply`, use filter-based workaround (see OData Aggregation section)
3. For `$expand`, verify server supports navigation properties
4. Review OData 4.0 specification for correct syntax

#### Connection Timeout
**Symptom:** `Network error: operation timed out`

**Solutions:**
1. Increase timeout: `ClientConfig::new(...).with_timeout(Duration::from_secs(60))`
2. Reduce query size (use `$top` with smaller value)
3. Use `$select` to request fewer fields
4. Check network connectivity
5. Verify server is not experiencing downtime

#### Rate Limiting
**Symptom:** `Rate Limited (429): Too many requests`

**Solutions:**
1. Implement exponential backoff (see error handling examples)
2. Add delays between requests
3. Reduce request frequency
4. Contact provider about rate limit increases
5. Use replication endpoint for bulk data (if available)

#### Empty Results
**Symptom:** Query succeeds but returns no records

**Solutions:**
1. Verify filter syntax is correct (OData uses `eq` not `=`)
2. Check string values are properly quoted: `City eq 'Austin'`
3. Verify field names exist (use `fetch_metadata()`)
4. Test with simpler filter to isolate issue
5. Check if data exists matching your criteria

#### Parse Errors
**Symptom:** `Parse error: Failed to parse JSON`

**Solutions:**
1. Check server is returning valid JSON
2. Verify you're using correct execution method:
   - `execute()` for standard queries
   - `execute_by_key()` for key access
   - `execute_count()` for count-only
3. Check API version compatibility
4. Examine raw response (enable debug logging)

### Debug Logging

Enable tracing to see detailed request/response information:

```rust
// Add to Cargo.toml
[dependencies]
tracing-subscriber = "0.3"

// In your code
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Your code here
    let client = ResoClient::from_env()?;
    // ...
}
```

This will show:
- Request URLs
- Query parameters
- Response status codes
- Record counts

### Testing Connectivity

Use this minimal example to test basic connectivity:

```rust
use reso_client::{ResoClient, QueryBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ResoClient::from_env()?;

    // Test 1: Fetch metadata
    println!("Testing metadata fetch...");
    let metadata = client.fetch_metadata().await?;
    println!("✓ Metadata fetched ({} bytes)", metadata.len());

    // Test 2: Simple query
    println!("\nTesting simple query...");
    let query = QueryBuilder::new("Property").top(1).build()?;
    let response = client.execute(&query).await?;
    println!("✓ Query succeeded");

    if let Some(records) = response["value"].as_array() {
        println!("✓ Got {} record(s)", records.len());
    }

    Ok(())
}
```

## Quick Reference Card

### Minimal Complete Example

```rust
use reso_client::{ResoClient, QueryBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create client
    let client = ResoClient::from_env()?;

    // 2. Build query
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin' and ListPrice gt 500000")
        .select(&["ListingKey", "City", "ListPrice"])
        .top(10)
        .build()?;

    // 3. Execute
    let response = client.execute(&query).await?;

    // 4. Process results
    if let Some(records) = response["value"].as_array() {
        for record in records {
            println!("{}", serde_json::to_string_pretty(record)?);
        }
    }

    Ok(())
}
```

### Environment Setup

```bash
# Required
export RESO_BASE_URL="https://api.bridgedataoutput.com/api/v2/OData"
export RESO_TOKEN="your-bearer-token"

# Optional
export RESO_DATASET_ID="actris_ref"
export RESO_TIMEOUT="30"
```

### Common Filter Patterns

```rust
// Equality
"City eq 'Austin'"

// Comparison
"ListPrice gt 500000"
"BedroomsTotal ge 3"

// Multiple conditions
"City eq 'Austin' and ListPrice gt 500000"

// OR condition
"City eq 'Austin' or City eq 'Dallas'"

// String functions
"startswith(City, 'San')"
"contains(PostalCode, '78')"

// Date comparison
"ModificationTimestamp gt 2025-01-01T00:00:00Z"

// Parentheses for grouping
"(City eq 'Austin' or City eq 'Dallas') and ListPrice gt 500000"
```

### Query Patterns Cheat Sheet

```rust
// Basic query
QueryBuilder::new("Property").top(10).build()?

// Filtered query
QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .build()?

// With field selection
QueryBuilder::new("Property")
    .select(&["ListingKey", "City", "ListPrice"])
    .top(10)
    .build()?

// Sorted
QueryBuilder::new("Property")
    .order_by("ListPrice", "desc")
    .top(10)
    .build()?

// Pagination
QueryBuilder::new("Property")
    .skip(100)
    .top(100)
    .build()?

// With count
QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .with_count()
    .build()?

// Count only
QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .count()
    .build()?
let count = client.execute_count(&query).await?;

// Direct key access
QueryBuilder::by_key("Property", "12345")
    .select(&["ListingKey", "City"])
    .build()?
let record = client.execute_by_key(&query).await?;

// Replication
ReplicationQueryBuilder::new("Property")
    .filter("StandardStatus eq 'Active'")
    .select(&["ListingKey", "City"])
    .top(2000)
    .build()?
let response = client.execute_replication(&query).await?;
```

### Response Structures

```rust
// Standard query response
{
  "value": [...],              // Array of records
  "@odata.count": 123,         // Total count (with with_count())
  "@odata.nextLink": "...",    // Next page URL
  "@odata.context": "..."      // Metadata context
}

// Count-only response (via execute_count)
42  // u64 integer

// Key access response (via execute_by_key)
{
  "ListingKey": "12345",
  "City": "Austin",
  ...
}  // Single record object

// Replication response
ReplicationResponse {
    records: Vec<JsonValue>,
    next_link: Option<String>,
    record_count: usize,
}
```

### Common Resources

- `Property` - Real estate listings
- `Member` - MLS members/agents
- `Office` - MLS offices
- `Media` - Photos and documents
- `OpenHouse` - Open house events

### Error Types Quick Reference

- `Config` - Missing env vars, invalid config
- `Network` - Connection failures, timeouts
- `Unauthorized (401)` - Invalid token
- `Forbidden (403)` - Insufficient permissions
- `NotFound (404)` - Invalid resource/endpoint
- `RateLimited (429)` - Too many requests
- `ServerError (5xx)` - Server-side error
- `ODataError` - Generic API error
- `Parse` - Invalid response format
- `InvalidQuery` - Query construction error

