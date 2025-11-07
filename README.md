[![Crates.io](https://img.shields.io/crates/v/reso-client.svg)](https://crates.io/crates/reso-client)
[![Documentation](https://docs.rs/reso-client/badge.svg)](https://docs.rs/reso-client)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![CI](https://github.com/jeremeybingham/reso_client/workflows/CI/badge.svg)](https://github.com/jeremeybingham/reso_client/actions)

# RESO Client Library

A Rust client library for [RESO Web API](https://www.reso.org/reso-web-api/) servers using OData 4.0.

## Features

- 🔍 Fluent query builder for OData queries
- 🔐 OAuth bearer token authentication
- 📊 Support for filters, ordering, pagination, and field selection
- 🔢 Count-only queries for efficient record counting ⚠️
- 🗂️ Optional dataset ID path support
- 📖 Metadata retrieval
- 🔄 Replication endpoint support for bulk data transfer (up to 2000 records/request) ⚠️
- ⚡ Async/await with tokio

⚠️ *Some features not supported by the RESO Web API / `actris_ref` test server.*

## Installation via Github

Pending Cargo crate publication, use the Github repository to install - add to your `Cargo.toml`:
```toml
[dependencies]
# Import the RESO client from GitHub
reso-client = { git = "https://github.com/jeremeybingham/reso_client" }
```

## Configuration

### Environment Variables

The client reads configuration from environment variables:

| Variable | Required | Description | Example |
|----------|----------|-------------|---------|
| `RESO_BASE_URL` | Yes | Base URL of the RESO Web API server | `https://api.bridgedataoutput.com/api/v2/OData` |
| `RESO_TOKEN` | Yes | OAuth bearer token for authentication | `your-token-here` |
| `RESO_DATASET_ID` | No | Dataset identifier (see below) | `actris_ref` |
| `RESO_TIMEOUT` | No | HTTP timeout in seconds (default: 30) | `60` |

Create a `.env` file:
```bash
RESO_BASE_URL=https://api.bridgedataoutput.com/api/v2/OData
RESO_TOKEN=your-token-here
RESO_DATASET_ID=actris_ref
RESO_TIMEOUT=30
```

### Dataset ID Explained

Configured to handle the Bridges/ACTRIS RESO Web API Reference Server, which uses a dataset identifier in the URL path. The dataset ID is inserted between the base URL and the resource name.

**Without dataset ID:**
```
https://api.mls.com/OData/Property
```

**With dataset ID:**
```
https://api.mls.com/OData/actris_ref/Property
https://api.mls.com/OData/actris_ref/$metadata
```

When to use:
- **Required**: If your RESO provider's API documentation shows URLs with a dataset/database identifier
- **Optional**: If your provider uses a simple base URL structure

You can set it via environment variable or programmatically:
```rust
// Via environment
let client = ResoClient::from_env()?;

// Via builder
let config = ClientConfig::new("https://api.mls.com/OData", "token")
    .with_dataset_id("actris_ref");
let client = ResoClient::with_config(config)?;
```

## Testing

### Running Tests

The library includes comprehensive test coverage with both unit and integration tests:

```bash
# Run all tests (unit + integration)
cargo test

# Run only unit tests (in src/ modules)
cargo test --lib

# Run only integration tests (in tests/ directory)
cargo test --test '*'

# Run specific test file
cargo test --test queries_tests
```

**Test Organization:**
- **Unit tests**: Internal tests for private functions and implementation details
- **Integration tests**: Public API tests in `tests/` directory
  - `tests/queries_tests.rs` - Comprehensive query building and URL generation tests

### Examples
All examples include detailed comments, error handling, and work with the RESO Web API reference server / `actris_ref` unless otherwise noted. The library includes a comprehensive suite of examples in the `examples` directory demonstrating all major functionality. Assuming you've set your `.env` variables correctly, you can run any example with:

```bash
cargo run --example <example_name>
```

#### Available Examples

**Basic Usage:**
- `test_connectivity` - Test basic API connectivity and authentication
- `test_property` - Property resource queries with filtering and field selection
- `test_member` - Query Member resource for agent/broker information
- `test_metadata` - Fetch and parse OData metadata documents
- `test_core_queries` - Tests the "Core Queries" specified in the [RESO Web API reference documentation](https://transport.reso.org/proposals/web-api-core.html#28-core-query-examples)

**Query Features:**
- `test_filters` - OData filter syntax (comparison, logical operators, string functions)
- `test_select` - Field selection and projection to optimize response size

**Analysis Examples:**
- `analyze_property_fields` - Analyze field usage across 200 active listings to identify which fields are most populated; generates `property_field_analysis_report.json` with recommended field sets (minimal, standard, comprehensive)
- `analyze_active_listings` - Statistical analysis of 200 active residential listings including price analysis, property type distribution, geographic distribution, bedroom/bathroom statistics, size metrics, and photo counts


⚠️ **Server-Specific (currently untestested, requires server support):**

The `$count`, `$apply`, and `$expand` features are not supported by the RESO Web API test server / `actris_ref`. The Replication endpoint is also not supported by default on `actris_ref`. Examples using these features will fail with `404` or `401` errors.

- ⚠️ `test_replication` - Replication endpoint for bulk data transfer (up to 2000 records/request)
- ⚠️ `test_count_only` - Efficient count-only queries using `/$count` endpoint 
- ⚠️ `test_pagination_nextlink` - Server-side pagination with `@odata.nextLink`
- ⚠️ `test_apply` - OData aggregation with `$apply` parameter
- ⚠️ `test_expand` - Navigation property expansion with `$expand` parameter


## Quick Start

### Standard Queries
```rust
use reso_client::{ResoClient, QueryBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create client from environment variables
    let client = ResoClient::from_env()?;

    // Build and execute a query
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin' and ListPrice gt 500000")
        .select(&["ListingKey", "City", "ListPrice"])
        .top(10)
        .build()?;

    let results = client.execute(&query).await?;

    // OData responses have structure: { "value": [...records...], "@odata.count": 123 }
    if let Some(records) = results["value"].as_array() {
        println!("Found {} properties", records.len());
        for record in records {
            println!("{}", serde_json::to_string_pretty(record)?);
        }
    }

    Ok(())
}
```

### Replication Queries (Bulk Data Transfer)
```rust
use reso_client::{ResoClient, ReplicationQueryBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ResoClient::from_env()?;

    // Build replication query (up to 2000 records per request)
    let query = ReplicationQueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .top(2000)
        .build()?;

    let response = client.execute_replication(&query).await?;

    println!("Retrieved {} records", response.record_count);

    // Continue with next link if available
    if response.has_more() {
        let next_response = client.execute_next_link(
            response.next_link().unwrap()
        ).await?;
        println!("Retrieved {} more records", next_response.record_count);
    }

    Ok(())
}
```

## Usage Examples

### Basic Query
```rust
let query = QueryBuilder::new("Property")
    .top(10)
    .build()?;

let results = client.execute(&query).await?;
```

### Filtering

Use OData 4.0 filter syntax:
```rust
// Simple equality
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .build()?;

// Comparison operators
let query = QueryBuilder::new("Property")
    .filter("ListPrice gt 500000 and ListPrice lt 1000000")
    .build()?;

// String functions
let query = QueryBuilder::new("Property")
    .filter("startswith(City, 'San')")
    .build()?;

// Date comparison
let query = QueryBuilder::new("Property")
    .filter("ModificationTimestamp gt 2025-01-01T00:00:00Z")
    .build()?;

// Complex expressions
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin' and (ListPrice gt 500000 or BedroomsTotal ge 4)")
    .build()?;
```

### Field Selection
```rust
let query = QueryBuilder::new("Property")
    .select(&["ListingKey", "City", "ListPrice", "BedroomsTotal"])
    .top(10)
    .build()?;
```

### Sorting
```rust
let query = QueryBuilder::new("Property")
    .order_by("ListPrice", "desc")
    .top(10)
    .build()?;
```

### Pagination
```rust
// First page
let query = QueryBuilder::new("Property")
    .top(20)
    .build()?;

// Second page
let query = QueryBuilder::new("Property")
    .skip(20)
    .top(20)
    .build()?;
```

### Getting Total Count
```rust
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .with_count()
    .top(10)
    .build()?;

let results = client.execute(&query).await?;

// Access the count
if let Some(count) = results["@odata.count"].as_u64() {
    println!("Total matching records: {}", count);
}
```

### Count-Only Queries

Efficiently get just the count without fetching records:
```rust
let query = QueryBuilder::new("Property")
    .filter("City eq 'Austin'")
    .count()  // Returns just the count via /$count endpoint
    .build()?;

let results = client.execute(&query).await?;
let count = results.as_u64().unwrap_or(0);
println!("Total: {}", count);
```

### OData Aggregation (when supported)

**⚠️ Server Compatibility, NOT supported by the RESO Web API reference server / `actris_ref` Note:** The `apply()` method requires server support for OData v4.0 Aggregation Extensions. Not all RESO servers support this feature.

```rust
// Group by field with aggregation (if server supports $apply)
let query = QueryBuilder::new("Property")
    .apply("groupby((StandardStatus), aggregate($count as TotalCount))")
    .build()?;

let results = client.execute(&query).await?;
```

**If your server doesn't support `$apply`**, use multiple filtered queries instead:
**⚠️ This is the method supported by the RESO Web API reference server / `actris_ref`**
```rust
// Workaround: Use $filter for counts by category
let statuses = ["Active", "Pending", "Closed"];

for status in statuses {
    let query = QueryBuilder::new("Property")
        .filter(format!("StandardStatus eq '{}'", status))
        .count()
        .build()?;

    let results = client.execute(&query).await?;
    let count = results.as_u64().unwrap_or(0);
    println!("{}: {}", status, count);
}
```

### Fetching Metadata

Retrieve the OData metadata document:
```rust
let metadata_xml = client.fetch_metadata().await?;
println!("{}", metadata_xml);
```

### Replication Queries

The replication endpoint is designed for bulk data transfer and synchronization of large datasets (>10,000 records). It supports up to 2000 records per request (vs 200 for standard queries) and uses header-based pagination.

**Important notes:**
- Requires MLS authorization
- Results are ordered oldest to newest by default
- No support for `$skip`, `$orderby`, `$apply`, or count options
- Use `$select` to reduce payload size and improve performance

```rust
use reso_client::{ResoClient, ReplicationQueryBuilder};

// Build a replication query
let query = ReplicationQueryBuilder::new("Property")
    .filter("StandardStatus eq 'Active'")
    .select(&["ListingKey", "City", "ListPrice"])
    .top(2000)  // Maximum: 2000
    .build()?;

// Execute the query
let response = client.execute_replication(&query).await?;

println!("Retrieved {} records", response.record_count);

// Process records
for record in &response.records {
    let key = record["ListingKey"].as_str().unwrap_or("");
    let city = record["City"].as_str().unwrap_or("");
    println!("{}: {}", key, city);
}

// Continue with next link if more records available
if let Some(next_link) = response.next_link {
    let next_response = client.execute_next_link(&next_link).await?;
    println!("Retrieved {} more records", next_response.record_count);
}
```

**Fetching all records with pagination:**
```rust
let mut query = ReplicationQueryBuilder::new("Property")
    .top(2000)
    .build()?;

let mut response = client.execute_replication(&query).await?;
let mut all_records = response.records;

// Continue fetching while next link is available
while let Some(next_link) = response.next_link {
    response = client.execute_next_link(&next_link).await?;
    all_records.extend(response.records);
}

println!("Total records fetched: {}", all_records.len());
```

## OData Response Structure

The RESO Web API returns responses in OData format:
```json
{
  "value": [
    {
      "ListingKey": "12345",
      "City": "Austin",
      "ListPrice": 750000
    },
    {
      "ListingKey": "67890",
      "City": "Austin",
      "ListPrice": 850000
    }
  ],
  "@odata.context": "https://api.example.com/odata/$metadata#Property",
  "@odata.count": 42
}
```

Key fields:
- **`value`**: Array of records matching your query
- **`@odata.count`**: Total count (only when `with_count()` is used)
- **`@odata.nextLink`**: URL for next page (for server-side pagination)

Access records:
```rust
let results = client.execute(&query).await?;

if let Some(records) = results["value"].as_array() {
    for record in records {
        let listing_key = record["ListingKey"].as_str();
        let price = record["ListPrice"].as_f64();
        // ... process record
    }
}
```

## Error Handling
```rust
use reso_client::{ResoClient, ResoError};

match client.execute(&query).await {
    Ok(results) => {
        // Process results
    }
    Err(ResoError::Config(msg)) => {
        eprintln!("Configuration error: {}", msg);
    }
    Err(ResoError::Network(msg)) => {
        eprintln!("Network error: {}", msg);
    }
    Err(ResoError::ODataError(msg)) => {
        eprintln!("OData server error: {}", msg);
    }
    Err(e) => {
        eprintln!("Error: {}", e);
    }
}
```

## Advanced Configuration

### Custom Timeout
```rust
use std::time::Duration;

let config = ClientConfig::new("https://api.mls.com/OData", "token")
    .with_timeout(Duration::from_secs(60));
let client = ResoClient::with_config(config)?;
```

### Manual Configuration
```rust
let config = ClientConfig::new(
    "https://api.mls.com/odata",
    "your-bearer-token"
)
.with_dataset_id("actris_ref")
.with_timeout(Duration::from_secs(45));

let client = ResoClient::with_config(config)?;
```

## OData Filter Reference

Common OData 4.0 operators:

| Operator | Description | Example |
|----------|-------------|---------|
| `eq` | Equals | `City eq 'Austin'` |
| `ne` | Not equals | `Status ne 'Closed'` |
| `gt` | Greater than | `ListPrice gt 500000` |
| `ge` | Greater than or equal | `BedroomsTotal ge 3` |
| `lt` | Less than | `ListPrice lt 1000000` |
| `le` | Less than or equal | `BedroomsTotal le 5` |
| `and` | Logical AND | `City eq 'Austin' and ListPrice gt 500000` |
| `or` | Logical OR | `City eq 'Austin' or City eq 'Manor'` |
| `not` | Logical NOT | `not (City eq 'Austin')` |

String functions:
- `startswith(field, 'value')`
- `endswith(field, 'value')`
- `contains(field, 'value')`

Date functions:
- `year(field) eq 2025`
- `month(field) eq 6`
- `day(field) eq 15`

For complete OData 4.0 filter syntax, see: [OData URL Conventions](https://docs.oasis-open.org/odata/odata/v4.0/odata-v4.0-part2-url-conventions.html)

## License

Licensed under the terms of the MIT license. See the file:

- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

## Resources

- [RESO Web API Specification](https://www.reso.org/reso-web-api/)
- [OData 4.0 Protocol](https://www.odata.org/documentation/)
- [RESO Data Dictionary](https://www.reso.org/data-dictionary/)
- [Changelog](CHANGELOG.md)
