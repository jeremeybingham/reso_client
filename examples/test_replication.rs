use reso_client::{ReplicationQueryBuilder, ResoClient};

/// Test replication endpoint with 2000 records
///
/// This example demonstrates:
/// - Using the replication endpoint for bulk data transfer
/// - Fetching up to 2000 records (vs 200 for standard queries)
/// - Using $select to keep response size small
/// - Verifying the expected number of records were returned
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 RESO Client Replication Query Test\n");
    println!("Testing replication endpoint with 2000 records...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;
    println!("✅ Client created successfully");

    // Build a replication query to fetch 2000 properties
    // Using select to only fetch a few fields to keep response size manageable
    let query = ReplicationQueryBuilder::new("Property")
        .select(&[
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "ListPrice",
            "ModificationTimestamp",
        ])
        .top(2000)
        .build()?;

    println!("📤 Sending replication query for 2000 records with minimal fields");
    println!(
        "   Fields: ListingKey, ListingId, StandardStatus, ListPrice, ModificationTimestamp\n"
    );

    // Execute the replication query
    match client.execute_replication(&query).await {
        Ok(response) => {
            println!("✅ Replication query successful!");
            println!("\n📊 Response details:");
            println!("  ✓ Records received: {}", response.record_count);

            // Verify we got the expected number of records
            if response.record_count == 2000 {
                println!("  ✓ SUCCESS: Received exactly 2000 records as requested");
            } else {
                println!("  ⚠️  Note: Received {} records (may be fewer than 2000 if dataset is smaller)", response.record_count);
            }

            // Check if there's a next link for pagination
            if let Some(next_link) = &response.next_link {
                println!("  ✓ Next link available for pagination");
                println!("    (Use client.execute_next_link() to fetch more records)");
                println!("    Next: {}", next_link);
            } else {
                println!("  ✓ No more records available (end of dataset)");
            }

            // Display a few sample records
            if !response.records.is_empty() {
                println!("\n📝 Sample records (showing first 3):");
                println!("{}", "=".repeat(80));

                for (index, record) in response.records.iter().take(3).enumerate() {
                    println!("\n  Record #{}:", index + 1);
                    if let Some(listing_key) = record["ListingKey"].as_str() {
                        println!("    Listing Key:  {}", listing_key);
                    }
                    if let Some(listing_id) = record["ListingId"].as_str() {
                        println!("    Listing ID:   {}", listing_id);
                    }
                    if let Some(status) = record["StandardStatus"].as_str() {
                        println!("    Status:       {}", status);
                    }
                    if let Some(price) = record["ListPrice"].as_f64() {
                        println!("    List Price:   ${:.2}", price);
                    }
                    if let Some(modified) = record["ModificationTimestamp"].as_str() {
                        println!("    Modified:     {}", modified);
                    }
                }
                println!("\n{}", "=".repeat(80));
            }

            println!("\n✨ Replication test completed successfully!");

            // Final verification message
            if response.record_count == 2000 {
                println!(
                    "\n✅ TEST PASSED: Successfully fetched 2000 records via replication endpoint"
                );
            } else {
                println!("\n✅ TEST COMPLETED: Fetched {} records (dataset may contain fewer than 2000 total records)", response.record_count);
            }
        }
        Err(e) => {
            println!("❌ Replication query failed!");
            println!("\nError: {}", e);
            println!("\nTroubleshooting:");
            println!("  1. Check your .env file contains:");
            println!("     RESO_BASE_URL=https://api.mls.com/OData");
            println!("     RESO_TOKEN=your-bearer-token-here");
            println!("  2. Verify the server supports replication endpoint");
            println!("  3. Check if you have permission to access the replication endpoint");
            println!("  4. Verify your network connection");
            return Err(e.into());
        }
    }

    Ok(())
}
