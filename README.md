# RESO Web API Client for Rust

A Rust client for connecting to the RESO Web API via Bridge Interactive using OAuth Bearer token authentication.

## Quick Start

### 1. Get Credentials

Register with a RESO Web API provider and obtain:
- **Server Token** (Bearer token)
- **Dataset ID** (from your MLS)

### 2. Configure

Create a `.env` file in your project root:

```env
RESO_BASE_URL=https://api.resowebprovider.com/api/
RESO_SERVER_TOKEN=your_server_token_here
RESO_DATASET_ID=your_dataset_id_here
```

**Important:** Add `.env` to `.gitignore`!

### 3. Run

```bash
cargo run
```

## Usage

### Basic Query

```rust
use reso_client::{ResoApiClient, ResoApiConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ResoApiConfig::new(
        "https://api.resowebprovider.com/api/".to_string(),
        "your_token".to_string()
    ).with_dataset("your_dataset_id".to_string());
    
    let client = ResoApiClient::new(config);
    
    // Get metadata
    let metadata = client.get_metadata().await?;
    
    // Query properties
    let query = "$filter=StandardStatus eq 'Active'&$top=10";
    let response = client.query::<Property>("Property", Some(query)).await?;
    
    Ok(())
}
```

## OData Query Examples

```rust
// Filter by status and price
"$filter=StandardStatus eq 'Active' and ListPrice lt 500000&$top=10"

// Select specific fields
"$select=ListingKey,ListPrice,City&$top=20"

// Sort by price
"$orderby=ListPrice desc&$top=5"

// Price range with location
"$filter=(City eq 'Boston' or City eq 'Cambridge') and BedroomsTotal ge 3"
```

## Common OData Operators

| Operator | Example |
|----------|---------|
| `eq` | `City eq 'Boston'` |
| `ne` | `Status ne 'Sold'` |
| `gt/ge` | `ListPrice ge 200000` |
| `lt/le` | `ListPrice lt 500000` |
| `and/or` | `Beds ge 3 and Baths ge 2` |
| `contains` | `contains(City, 'Boston')` |

## Available Resources

Query any RESO resource:
- **Property** - Listings
- **Member** - Agents
- **Office** - Offices
- **OpenHouse** - Open houses
- **Media** - Photos/media

Check metadata to see all available resources for your MLS.

## Error Handling

```rust
match client.query::<Property>("Property", None).await {
    Ok(response) => {
        // Process response
    }
    Err(e) => {
        eprintln!("Error: {}", e);
    }
}
```

## Authentication

The client uses OAuth 2.0 Bearer token authentication via the `Authorization` header:

```
Authorization: Bearer {your_server_token}
```

All requests are made over HTTPS to the RESO Web API endpoints.
