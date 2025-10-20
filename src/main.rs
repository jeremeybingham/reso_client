// main.rs - Example usage of the RESO API Client

mod reso_client;

use reso_client::{ResoApiClient, ResoApiConfig};
use serde::{Deserialize, Serialize};
use std::env;

/// Example Property struct (simplified)
/// You can generate full structs using the RESO Data Dictionary parser
#[derive(Debug, Serialize, Deserialize)]
pub struct Property {
    #[serde(rename = "ListingKey")]
    pub listing_key: Option<String>,
    
    #[serde(rename = "ListPrice")]
    pub list_price: Option<f64>,
    
    #[serde(rename = "StandardStatus")]
    pub standard_status: Option<String>,
    
    #[serde(rename = "City")]
    pub city: Option<String>,
    
    #[serde(rename = "StateOrProvince")]
    pub state_or_province: Option<String>,
    
    #[serde(rename = "PostalCode")]
    pub postal_code: Option<String>,
    
    #[serde(rename = "BedroomsTotal")]
    pub bedrooms_total: Option<i32>,
    
    #[serde(rename = "BathroomsTotalInteger")]
    pub bathrooms_total: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("RESO Web API Client Example");
    println!("============================\n");

    // Try to load .env file
    match dotenvy::dotenv() {
        Ok(path) => println!("✓ Loaded .env file from: {:?}", path),
        Err(_) => println!("ℹ No .env file found, using system environment variables"),
    }

    // Load configuration from environment variables
    println!("\n=== Configuration ===");
    
    let base_url = env::var("RESO_BASE_URL")
        .unwrap_or_else(|_| {
            println!("Using default base URL (RESO_BASE_URL not set)");
            "https://api.bridgedataoutput.com/api/v2".to_string()
        });
    println!("Base URL: {}", base_url);
    
    let server_token = match env::var("RESO_SERVER_TOKEN") {
        Ok(token) => {
            println!("Server Token: {}...", &token.chars().take(10).collect::<String>());
            token
        }
        Err(_) => {
            eprintln!("\n❌ ERROR: RESO_SERVER_TOKEN environment variable not set!");
            eprintln!("\nPlease set your credentials:");
            eprintln!("  Option 1 - Create a .env file:");
            eprintln!("    RESO_SERVER_TOKEN=your_token_here");
            eprintln!("    RESO_DATASET_ID=your_dataset_id");
            eprintln!("\n  Option 2 - Export environment variables:");
            eprintln!("    export RESO_SERVER_TOKEN=\"your_token_here\"");
            eprintln!("    export RESO_DATASET_ID=\"your_dataset_id\"");
            eprintln!("\n  Option 3 - Run with inline variables:");
            eprintln!("    RESO_SERVER_TOKEN=\"token\" RESO_DATASET_ID=\"id\" cargo run");
            std::process::exit(1);
        }
    };
    
    let dataset_id = match env::var("RESO_DATASET_ID") {
        Ok(id) => {
            println!("Dataset ID: {}", id);
            id
        }
        Err(_) => {
            eprintln!("\n❌ ERROR: RESO_DATASET_ID environment variable not set!");
            eprintln!("\nPlease set your dataset ID using one of the methods above.");
            std::process::exit(1);
        }
    };
    
    println!("\n");

    // Create API client
    let config = ResoApiConfig::new(base_url, server_token)
        .with_dataset(dataset_id);
    
    let client = ResoApiClient::new(config);

    // Example 1: Get metadata
    println!("=== Fetching Metadata ===");
    match client.get_metadata().await {
        Ok(metadata) => {
            println!("Metadata retrieved successfully!");
            println!("First 500 characters:\n{}\n", &metadata[..metadata.len().min(500)]);
        }
        Err(e) => eprintln!("Error fetching metadata: {}\n", e),
    }

    // Example 2: Query properties with filters
    println!("=== Querying Active Properties ===");
    let query = "$filter=StandardStatus eq 'Active'&$top=5&$orderby=ListPrice desc";
    
    match client.query::<Property>("Property", Some(query)).await {
        Ok(response) => {
            println!("Found {} properties", response.value.len());
            if let Some(count) = response.count {
                println!("Total count: {}", count);
            }
            
            for (i, property) in response.value.iter().enumerate() {
                println!("\n{}. Property {}", i + 1, 
                    property.listing_key.as_deref().unwrap_or("N/A"));
                println!("   Price: ${}", 
                    property.list_price.map(|p| p.to_string()).unwrap_or("N/A".to_string()));
                println!("   Location: {}, {}", 
                    property.city.as_deref().unwrap_or("N/A"),
                    property.state_or_province.as_deref().unwrap_or("N/A"));
                println!("   Beds/Baths: {}/{}",
                    property.bedrooms_total.map(|b| b.to_string()).unwrap_or("N/A".to_string()),
                    property.bathrooms_total.map(|b| b.to_string()).unwrap_or("N/A".to_string()));
            }
        }
        Err(e) => eprintln!("Error querying properties: {}", e),
    }

    // Example 3: Query with $select to get specific fields only
    println!("\n=== Querying with $select ===");
    let query = "$select=ListingKey,ListPrice,City&$top=3";
    
    match client.query::<Property>("Property", Some(query)).await {
        Ok(response) => {
            println!("Retrieved {} properties with selected fields", response.value.len());
            for property in &response.value {
                println!("  {} - {} - ${}", 
                    property.listing_key.as_deref().unwrap_or("N/A"),
                    property.city.as_deref().unwrap_or("N/A"),
                    property.list_price.map(|p| p.to_string()).unwrap_or("N/A".to_string()));
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }

    // Example 4: Query by price range
    println!("\n=== Querying by Price Range ===");
    let query = "$filter=ListPrice ge 200000 and ListPrice le 500000&$top=5";
    
    match client.query::<Property>("Property", Some(query)).await {
        Ok(response) => {
            println!("Found {} properties between $200k-$500k", response.value.len());
        }
        Err(e) => eprintln!("Error: {}", e),
    }

    // Example 5: Query other resources (if available)
    println!("\n=== Querying Members ===");
    match client.query::<serde_json::Value>("Member", Some("$top=3")).await {
        Ok(response) => {
            println!("Retrieved {} members", response.value.len());
        }
        Err(e) => eprintln!("Note: Member resource may not be available: {}", e),
    }

    println!("\n=== Examples Complete ===");
    Ok(())
}