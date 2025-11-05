use reso_client::{QueryBuilder, ResoClient};

/// Count-only query test - demonstrates the /$count endpoint.
///
/// The /$count endpoint returns just a single integer count without
/// fetching any actual records. This is much more efficient than using
/// $count=true when you only need the total count.
///
/// Benefits:
/// - Minimal network overhead (returns just a number, not JSON)
/// - Faster response time
/// - Lower server resource usage
/// - Perfect for dashboards and statistics
///
/// This test demonstrates:
/// - Basic count query (total records)
/// - Count with filter (conditional counting)
/// - Multiple count queries (statistics gathering)
/// - Comparison with $count=true approach
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔢 RESO Client Count-Only Query Test\n");
    println!("Testing the /$count endpoint for efficient counting...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Test 1: Basic count - total properties
    println!("📝 Test 1: Count total properties");
    println!("{}", "=".repeat(70));

    let query1 = QueryBuilder::new("Property").count().build()?;

    println!("Query: {}", query1.to_odata_string());

    match client.execute_count(&query1).await {
        Ok(count) => {
            println!("✅ Total properties: {}", count);
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
            return Err(e.into());
        }
    }

    println!();

    // Test 2: Count with filter - active properties
    println!("📝 Test 2: Count active properties");
    println!("{}", "=".repeat(70));

    let query2 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .count()
        .build()?;

    println!("Query: {}", query2.to_odata_string());

    match client.execute_count(&query2).await {
        Ok(count) => {
            println!("✅ Active properties: {}", count);
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
            return Err(e.into());
        }
    }

    println!();

    // Test 3: Multiple counts - status breakdown
    println!("📝 Test 3: Property count by status");
    println!("{}", "=".repeat(70));

    let statuses = ["Active", "Pending", "Closed", "Expired"];
    let mut status_counts = Vec::new();

    for status in &statuses {
        let query = QueryBuilder::new("Property")
            .filter(&format!("StandardStatus eq '{}'", status))
            .count()
            .build()?;

        match client.execute_count(&query).await {
            Ok(count) => {
                status_counts.push((*status, count));
                println!("   {:15} {:>10} properties", status, count);
            }
            Err(e) => {
                println!("   {:15} Error: {}", status, e);
            }
        }
    }

    println!();

    // Test 4: Multiple counts - price ranges
    println!("📝 Test 4: Property count by price range");
    println!("{}", "=".repeat(70));

    let price_ranges = [
        ("Under $200K", "ListPrice lt 200000"),
        (
            "$200K - $500K",
            "ListPrice ge 200000 and ListPrice lt 500000",
        ),
        (
            "$500K - $1M",
            "ListPrice ge 500000 and ListPrice lt 1000000",
        ),
        ("Over $1M", "ListPrice ge 1000000"),
    ];

    for (label, filter) in &price_ranges {
        let query = QueryBuilder::new("Property")
            .filter(*filter)
            .count()
            .build()?;

        match client.execute_count(&query).await {
            Ok(count) => {
                println!("   {:20} {:>10} properties", label, count);
            }
            Err(e) => {
                println!("   {:20} Error: {}", label, e);
            }
        }
    }

    println!();

    // Test 5: Count total members
    println!("📝 Test 5: Count total members");
    println!("{}", "=".repeat(70));

    let query5 = QueryBuilder::new("Member").count().build()?;

    println!("Query: {}", query5.to_odata_string());

    match client.execute_count(&query5).await {
        Ok(count) => {
            println!("✅ Total members: {}", count);
        }
        Err(e) => {
            println!("❌ Test 5 failed: {}", e);
            println!("💡 Member resource may not be available on your server");
        }
    }

    println!();

    // Test 6: Comparison - count-only vs $count=true
    println!("📝 Test 6: Efficiency Comparison");
    println!("{}", "=".repeat(70));

    // Approach 1: Count-only (/$count)
    println!("Approach 1: Count-only endpoint (/$count)");
    let query_count_only = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .count()
        .build()?;

    let start = std::time::Instant::now();
    match client.execute_count(&query_count_only).await {
        Ok(count) => {
            let duration = start.elapsed();
            println!("   ✅ Count: {}", count);
            println!("   ⏱️  Time: {:?}", duration);
            println!("   📦 Response: Single integer (minimal)");
        }
        Err(e) => {
            println!("   ❌ Failed: {}", e);
        }
    }

    println!();

    // Approach 2: $count=true (returns count + records)
    println!("Approach 2: $count=true with query");
    let query_with_count = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .top(1)
        .with_count()
        .build()?;

    let start = std::time::Instant::now();
    match client.execute(&query_with_count).await {
        Ok(response) => {
            let duration = start.elapsed();
            if let Some(count) = response["@odata.count"].as_u64() {
                println!("   ✅ Count: {}", count);
            }
            println!("   ⏱️  Time: {:?}", duration);
            println!("   📦 Response: Full JSON with records (larger)");
        }
        Err(e) => {
            println!("   ❌ Failed: {}", e);
        }
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("✨ Count-only tests completed!");
    println!();
    println!("💡 Key Takeaways:");
    println!("   • Use /$count for maximum efficiency when you only need totals");
    println!("   • Returns a plain integer, not JSON");
    println!("   • Perfect for dashboards, statistics, and analytics");
    println!("   • Much faster than fetching records with $count=true");
    println!();
    println!("📊 Example Use Cases:");
    println!("   • Dashboard widgets (\"123 Active Listings\")");
    println!("   • Quick status checks");
    println!("   • Validation before large queries");
    println!("   • Statistical reports");

    Ok(())
}
