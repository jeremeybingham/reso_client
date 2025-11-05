use reso_client::{QueryBuilder, ResoClient};

/// Basic connectivity test - verifies we can connect to the RESO API
/// and retrieve a minimal response.
///
/// This is the simplest possible test to verify:
/// - Environment variables are configured correctly
/// - Network connectivity to the API
/// - Authentication is working
/// - Basic query execution succeeds
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔌 RESO Client Connectivity Test\n");
    println!("Testing basic API connection...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;
    println!("✅ Client created successfully");

    // Build the simplest possible query - just fetch 1 property
    let query = QueryBuilder::new("Property").top(1).build()?;

    println!("📤 Sending query: {:?}", query);

    // Execute the query
    match client.execute(&query).await {
        Ok(response) => {
            println!("✅ Connection successful!");
            println!("\n📊 Response structure:");

            // Check for @odata.context (indicates valid OData response)
            if let Some(context) = response.get("@odata.context") {
                println!("  ✓ OData context: {}", context);
            }

            // Check for value array
            if let Some(values) = response["value"].as_array() {
                println!("  ✓ Received {} record(s)", values.len());

                if let Some(first_record) = values.first() {
                    let field_count = first_record.as_object().map(|obj| obj.len()).unwrap_or(0);
                    println!("  ✓ First record has {} fields", field_count);
                }
            } else {
                println!("  ⚠️  No 'value' array in response");
            }

            println!("\n✨ Connectivity test PASSED!");
            println!("\nYour RESO API connection is working correctly.");
        }
        Err(e) => {
            println!("❌ Connection failed!");
            println!("\nError: {}", e);
            println!("\nTroubleshooting:");
            println!("  1. Check your .env file contains:");
            println!("     RESO_BASE_URL=https://api.mls.com/OData");
            println!("     RESO_TOKEN=your-bearer-token-here");
            println!("  2. Verify your network connection");
            println!("  3. Confirm your API credentials are valid");
            println!("  4. Check the API endpoint URL is correct");
            return Err(e.into());
        }
    }

    Ok(())
}
