use reso_client::{ReplicationQueryBuilder, ResoClient};

/// Replication pagination test - demonstrates using next links for bulk data transfer.
///
/// The replication endpoint is designed for fetching large datasets efficiently.
/// Instead of using $skip for pagination, it provides a "next" link in the
/// response headers that points to the next batch of records.
///
/// Key differences from standard queries:
/// - Maximum $top: 2000 (vs 200 for standard queries)
/// - No $skip parameter (use next links instead)
/// - No $orderby (ordered oldest to newest by default)
/// - Results include a next link for pagination
///
/// This test demonstrates:
/// - Initial replication query
/// - Following next links to fetch all data
/// - Tracking progress through large datasets
/// - Handling end of data (no more next links)
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 RESO Client Replication Pagination Test\n");
    println!("Testing pagination using next links for bulk data transfer...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;
    println!("✅ Client created successfully\n");

    // Configuration for this test
    let batch_size = 100; // Using smaller batches for testing
    let max_batches = 5; // Limit batches for testing (remove in production)

    println!("📋 Test Configuration:");
    println!("   Batch size: {} records", batch_size);
    println!("   Max batches: {} (for testing)", max_batches);
    println!();

    // Test 1: Basic replication with pagination
    println!("📝 Test 1: Fetch multiple batches using next links");
    println!("{}", "=".repeat(70));

    // Build initial replication query
    let query = ReplicationQueryBuilder::new("Property")
        .select(&[
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "ListPrice",
            "ModificationTimestamp",
        ])
        .top(batch_size)
        .build()?;

    println!("Initial query: {}", query.to_odata_string());
    println!();

    // Execute the first batch
    match client.execute_replication(&query).await {
        Ok(mut response) => {
            let mut total_records = response.record_count;
            let mut batch_count = 1;

            println!(
                "📦 Batch #{}: Retrieved {} records",
                batch_count, response.record_count
            );

            // Show sample from first batch
            if !response.records.is_empty() {
                println!("   Sample record:");
                if let Some(first) = response.records.first() {
                    if let Some(key) = first["ListingKey"].as_str() {
                        println!("      ListingKey: {}", key);
                    }
                    if let Some(status) = first["StandardStatus"].as_str() {
                        println!("      Status: {}", status);
                    }
                    if let Some(price) = first["ListPrice"].as_f64() {
                        println!("      Price: ${:.0}", price);
                    }
                }
            }

            // Check for next link
            if let Some(next_link) = &response.next_link {
                println!("   ✅ Next link available");
                println!("   🔗 {}", next_link);
            } else {
                println!("   ⚠️  No next link (end of data or single batch)");
            }

            println!();

            // Follow next links to fetch more batches
            while response.has_more() && batch_count < max_batches {
                if let Some(next_link) = response.next_link() {
                    println!("📤 Fetching next batch...");

                    match client.execute_next_link(next_link).await {
                        Ok(next_response) => {
                            batch_count += 1;
                            total_records += next_response.record_count;

                            println!(
                                "📦 Batch #{}: Retrieved {} records",
                                batch_count, next_response.record_count
                            );

                            if next_response.has_more() {
                                println!("   ✅ Next link available (continuing...)");
                            } else {
                                println!("   ✅ No more next links (reached end of data)");
                            }

                            println!();

                            response = next_response;
                        }
                        Err(e) => {
                            println!("   ❌ Failed to fetch next batch: {}", e);
                            break;
                        }
                    }
                }
            }

            // Summary
            println!("{}", "=".repeat(70));
            println!("📊 Pagination Summary:");
            println!("{}", "-".repeat(70));
            println!("   Total batches fetched: {}", batch_count);
            println!("   Total records retrieved: {}", total_records);
            println!(
                "   Average batch size: {:.1}",
                total_records as f64 / batch_count as f64
            );

            if batch_count >= max_batches && response.has_more() {
                println!();
                println!("   ⚠️  Stopped after {} batches (test limit)", max_batches);
                println!("      More data is available via next link");
                println!("      Remove max_batches limit to fetch all data");
            } else if !response.has_more() {
                println!();
                println!("   ✅ All available data has been fetched");
            }

            println!();
            println!("✅ Test 1 completed successfully!");
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
            println!();
            println!("💡 Troubleshooting:");
            println!("   1. Verify replication endpoint is available on your server");
            println!("   2. Check if you have permission to access replication");
            println!("   3. Ensure RESO_BASE_URL and RESO_TOKEN are correct");
            return Err(e.into());
        }
    }

    println!();

    // Test 2: Replication with filter
    println!("📝 Test 2: Filtered replication with pagination");
    println!("{}", "=".repeat(70));

    let filtered_query = ReplicationQueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&["ListingKey", "StandardStatus", "ListPrice"])
        .top(batch_size)
        .build()?;

    println!("Query with filter: {}", filtered_query.to_odata_string());
    println!();

    match client.execute_replication(&filtered_query).await {
        Ok(response) => {
            println!(
                "📦 First batch: Retrieved {} active properties",
                response.record_count
            );

            // Verify filter worked
            if !response.records.is_empty() {
                let all_active = response
                    .records
                    .iter()
                    .all(|r| r["StandardStatus"].as_str() == Some("Active"));

                if all_active {
                    println!("   ✅ All records match filter (StandardStatus = Active)");
                } else {
                    println!("   ⚠️  Some records don't match filter");
                }
            }

            if response.has_more() {
                println!("   ✅ More batches available via next link");
            } else {
                println!("   ⚠️  No more data (single batch)");
            }

            println!();
            println!("✅ Test 2 completed successfully!");
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
        }
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("✨ Replication pagination tests completed!");
    println!();
    println!("💡 Key Takeaways:");
    println!("   • Replication supports up to 2000 records per batch");
    println!("   • Use next links (not $skip) for pagination");
    println!("   • Next link is provided in response headers");
    println!("   • Perfect for bulk data transfer and synchronization");
    println!("   • Results are ordered oldest to newest by default");
    println!();
    println!("📝 Example: Fetch all properties");
    println!("   ```rust");
    println!("   let query = ReplicationQueryBuilder::new(\"Property\")");
    println!("       .select(&[\"ListingKey\", \"ModificationTimestamp\"])");
    println!("       .top(2000)");
    println!("       .build()?;");
    println!();
    println!("   let mut response = client.execute_replication(&query).await?;");
    println!("   let mut all_records = response.records.clone();");
    println!();
    println!("   while let Some(next_link) = response.next_link() {{");
    println!("       response = client.execute_next_link(next_link).await?;");
    println!("       all_records.extend(response.records);");
    println!("   }}");
    println!();
    println!("   println!(\"Total records: {{}}\", all_records.len());");
    println!("   ```");

    Ok(())
}
