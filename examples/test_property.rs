use reso_client::{QueryBuilder, ResoClient, ResoError};

/// Format a number with thousand separators
fn format_number(n: f64) -> String {
    let s = format!("{:.0}", n);
    let bytes: Vec<_> = s.bytes().rev().collect();
    let chunks: Vec<_> = bytes
        .chunks(3)
        .map(|chunk| chunk.iter().rev().copied().collect::<Vec<_>>())
        .collect();
    let result: Vec<_> = chunks
        .iter()
        .rev()
        .map(|chunk| String::from_utf8(chunk.clone()).unwrap())
        .collect();
    result.join(",")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🏠 RESO Client Property Search Example\n");
    println!("Searching for 5 active properties...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Build a query to fetch properties in Austin
    let query = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&[
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "ListPrice",
            "UnparsedAddress",
            "PropertyType",
            "PropertySubType",
            "BedroomsTotal",
            "BathroomsTotalInteger",
            "BathroomsFull",
            "BathroomsHalf",
            "LivingArea",
            "BuildingAreaTotal",
            "LotSizeSquareFeet",
            "LotSizeAcres",
            "YearBuilt",
        ])
        .top(5)
        .build()?;

    // Execute the query and handle the response
    match client.execute(&query).await {
        Ok(response) => {
            // Access the OData response payload
            if let Some(records) = response["value"].as_array() {
                if records.is_empty() {
                    println!("No properties found matching the criteria.");
                    return Ok(());
                }

                println!("✅ Found {} properties\n", records.len());
                println!("{}", "=".repeat(80));

                // Display each property
                for (index, record) in records.iter().enumerate() {
                    println!("\n📍 Property #{}", index + 1);
                    println!("{}", "-".repeat(80));

                    // Basic information
                    if let Some(listing_key) = record["ListingKey"].as_str() {
                        println!("  Listing Key:    {}", listing_key);
                    }
                    if let Some(listing_id) = record["ListingId"].as_str() {
                        println!("  Listing ID:     {}", listing_id);
                    }
                    if let Some(status) = record["StandardStatus"].as_str() {
                        println!("  Status:         {}", status);
                    }

                    // Address
                    if let Some(address) = record["UnparsedAddress"].as_str() {
                        println!("  Address:        {}", address);
                    }

                    // Price
                    if let Some(price) = record["ListPrice"].as_f64() {
                        println!("  List Price:     ${}", format_number(price));
                    }

                    // Property type
                    if let Some(prop_type) = record["PropertyType"].as_str() {
                        print!("  Property Type:  {}", prop_type);
                        if let Some(sub_type) = record["PropertySubType"].as_str() {
                            print!(" ({})", sub_type);
                        }
                        println!();
                    }

                    // Bedrooms and bathrooms
                    let bedrooms = record["BedroomsTotal"].as_i64();
                    let bathrooms_total = record["BathroomsTotalInteger"].as_i64();
                    let bathrooms_full = record["BathroomsFull"].as_i64();
                    let bathrooms_half = record["BathroomsHalf"].as_i64();

                    if bedrooms.is_some() || bathrooms_total.is_some() {
                        print!("  Bed/Bath:       ");
                        if let Some(beds) = bedrooms {
                            print!("{} bed", beds);
                        }
                        if let Some(baths) = bathrooms_total {
                            print!(" / {} bath", baths);
                        }
                        if let (Some(full), Some(half)) = (bathrooms_full, bathrooms_half) {
                            print!(" ({} full, {} half)", full, half);
                        }
                        println!();
                    }

                    // Living area
                    if let Some(living_area) = record["LivingArea"].as_f64() {
                        println!("  Living Area:    {} sq ft", format_number(living_area));
                    }
                    if let Some(building_area) = record["BuildingAreaTotal"].as_f64() {
                        println!("  Building Area:  {} sq ft", format_number(building_area));
                    }

                    // Lot size
                    if let Some(lot_sqft) = record["LotSizeSquareFeet"].as_f64() {
                        print!("  Lot Size:       {} sq ft", format_number(lot_sqft));
                        if let Some(lot_acres) = record["LotSizeAcres"].as_f64() {
                            print!(" ({:.2} acres)", lot_acres);
                        }
                        println!();
                    } else if let Some(lot_acres) = record["LotSizeAcres"].as_f64() {
                        println!("  Lot Size:       {:.2} acres", lot_acres);
                    }

                    // Year built
                    if let Some(year) = record["YearBuilt"].as_i64() {
                        println!("  Year Built:     {}", year);
                    }
                }

                println!("\n{}", "=".repeat(80));
                println!("\n✨ Query completed successfully!");
            } else {
                println!("⚠️  No properties found or invalid response format");
            }
        }
        Err(ResoError::Config(msg)) => {
            eprintln!("❌ Configuration error: {}", msg);
            eprintln!("\nMake sure your .env file contains:");
            eprintln!("  RESO_BASE_URL=https://api.mls.com/OData");
            eprintln!("  RESO_TOKEN=your-bearer-token-here");
            eprintln!("  RESO_DATASET_ID=dataset  # Optional");
        }
        Err(ResoError::Network(msg)) => {
            eprintln!("❌ Network error: {}", msg);
            eprintln!("\nCheck your internet connection and API endpoint.");
        }
        Err(ResoError::ODataError {
            message,
            status_code,
        }) => {
            eprintln!("❌ API error ({}): {}", status_code, message);
            eprintln!("\nThe query may be invalid or the API may have returned an error.");
        }
        Err(ResoError::Parse(msg)) => {
            eprintln!("❌ Parsing error: {}", msg);
            eprintln!("\nThe API response could not be parsed correctly.");
        }
        Err(e) => {
            eprintln!("❌ Unexpected error: {}", e);
        }
    }

    Ok(())
}
