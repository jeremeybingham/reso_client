# RESO Client Library

A Rust client library for [RESO Web API](https://www.reso.org/reso-web-api/) servers using OData 4.0.

## Features

- 🔍 Fluent query builder for OData queries
- 🔐 OAuth bearer token authentication
- 📊 Support for filters, ordering, pagination, and field selection
- 🔢 Count-only queries for efficient record counting
- 📈 OData aggregation support via `$apply` (requires server support)
- 🗂️ Optional dataset ID path support
- 📖 Metadata retrieval
- ⚡ Async/await with tokio
- ✅ Comprehensive test coverage

## Quick Start
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

**⚠️ Server Compatibility Note:** The `apply()` method requires server support for OData v4.0 Aggregation Extensions. Not all RESO servers support this feature.

```rust
// Group by field with aggregation (if server supports $apply)
let query = QueryBuilder::new("Property")
    .apply("groupby((StandardStatus), aggregate($count as TotalCount))")
    .build()?;

let results = client.execute(&query).await?;
```

**If your server doesn't support `$apply`**, use multiple filtered queries instead:
```rust
// Workaround: Use $filter for counts by category
let statuses = ["Active", "Pending", "Closed"];

for status in statuses {
    let query = QueryBuilder::new("Property")
        .filter(&format!("StandardStatus eq '{}'", status))
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

## Testing

### Running Tests

The library includes comprehensive unit tests:
```bash
cargo test
```

### Examples and Integration Tests

For working examples and integration tests against real RESO servers, see the separate examples repository:

**[reso-client-examples](https://github.com/jeremeybingham/reso-client-examples)** *(coming soon)*

This repository includes:
- Simple usage examples for common queries
- Complete integration tests against real RESO servers
- Advanced usage patterns and best practices
- Step-by-step tutorials for getting started

## Development

### Prerequisites

- Rust 1.70 or later
- Environment variables configured (see Configuration section)

### Running Quality Checks
```bash
# Run tests
cargo test

# Check formatting
cargo fmt --check

# Run clippy
cargo clippy -- -D warnings

# Build documentation
cargo doc --open
```

## License

Licensed under the terms of the MIT license. See the file:

- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

## Resources

- [RESO Web API Specification](https://www.reso.org/reso-web-api/)
- [OData 4.0 Protocol](https://www.odata.org/documentation/)
- [RESO Data Dictionary](https://www.reso.org/data-dictionary/)
- [Changelog](CHANGELOG.md)