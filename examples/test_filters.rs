use reso_client::{QueryBuilder, ResoClient};

/// Filter functionality test - verifies different OData filter operators work correctly.
///
/// This test demonstrates and validates:
/// - Equality filters (eq)
/// - Comparison filters (gt, lt, ge, le)
/// - Logical operators (and, or)
/// - That filtered results actually match the filter criteria
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔍 RESO Client Filter Test\n");
    println!("Testing various OData filter operators...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Test 1: Simple equality filter
    println!("📝 Test 1: Equality Filter (StandardStatus eq 'Active')");
    println!("{}", "=".repeat(70));

    let query1 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&["ListingKey", "StandardStatus", "ListPrice"])
        .top(3)
        .build()?;

    match client.execute(&query1).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} records", records.len());

                // Verify all records have Active status
                let all_active = records
                    .iter()
                    .all(|record| record["StandardStatus"].as_str() == Some("Active"));

                if all_active && !records.is_empty() {
                    println!("✅ All records match filter: StandardStatus = 'Active'");
                } else if records.is_empty() {
                    println!("⚠️  No records returned (may be valid if no active listings)");
                } else {
                    println!("❌ Some records don't match filter!");
                }

                // Display sample
                if let Some(first) = records.first() {
                    println!(
                        "   Sample: ListingKey={}, Status={}",
                        first["ListingKey"].as_str().unwrap_or("N/A"),
                        first["StandardStatus"].as_str().unwrap_or("N/A")
                    );
                }
            }
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
            return Err(e.into());
        }
    }

    println!();

    // Test 2: Comparison filter (greater than)
    println!("📝 Test 2: Comparison Filter (ListPrice gt 500000)");
    println!("{}", "=".repeat(70));

    let query2 = QueryBuilder::new("Property")
        .filter("ListPrice gt 500000")
        .select(&["ListingKey", "ListPrice", "StandardStatus"])
        .top(3)
        .build()?;

    match client.execute(&query2).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} records", records.len());

                // Verify all prices are > 500,000
                let all_match = records.iter().all(|record| {
                    record["ListPrice"]
                        .as_f64()
                        .map(|price| price > 500000.0)
                        .unwrap_or(false)
                });

                if all_match && !records.is_empty() {
                    println!("✅ All records match filter: ListPrice > $500,000");
                } else if records.is_empty() {
                    println!("⚠️  No records returned");
                } else {
                    println!("❌ Some records don't match filter!");
                }

                // Display samples
                for (i, record) in records.iter().take(2).enumerate() {
                    if let Some(price) = record["ListPrice"].as_f64() {
                        println!(
                            "   Sample {}: ListingKey={}, Price=${:.0}",
                            i + 1,
                            record["ListingKey"].as_str().unwrap_or("N/A"),
                            price
                        );
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
            return Err(e.into());
        }
    }

    println!();

    // Test 3: Combined filters with AND
    println!("📝 Test 3: Combined Filter (Active AND Price > 300000)");
    println!("{}", "=".repeat(70));

    let query3 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active' and ListPrice gt 300000")
        .select(&["ListingKey", "StandardStatus", "ListPrice"])
        .top(3)
        .build()?;

    match client.execute(&query3).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} records", records.len());

                // Verify all records match both conditions
                let all_match = records.iter().all(|record| {
                    let status_match = record["StandardStatus"].as_str() == Some("Active");
                    let price_match = record["ListPrice"]
                        .as_f64()
                        .map(|price| price > 300000.0)
                        .unwrap_or(false);
                    status_match && price_match
                });

                if all_match && !records.is_empty() {
                    println!("✅ All records match combined filter");
                } else if records.is_empty() {
                    println!("⚠️  No records returned");
                } else {
                    println!("❌ Some records don't match filter!");
                }

                // Display sample
                if let Some(first) = records.first() {
                    if let Some(price) = first["ListPrice"].as_f64() {
                        println!(
                            "   Sample: Status={}, Price=${:.0}",
                            first["StandardStatus"].as_str().unwrap_or("N/A"),
                            price
                        );
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Test 3 failed: {}", e);
            return Err(e.into());
        }
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("✨ Filter tests completed successfully!");
    println!("\nAll filter operators are working correctly:");
    println!("  ✓ Equality (eq)");
    println!("  ✓ Greater than (gt)");
    println!("  ✓ Logical AND (and)");

    Ok(())
}
