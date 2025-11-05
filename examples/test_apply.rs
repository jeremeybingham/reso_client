use reso_client::{QueryBuilder, ResoClient};

/// $apply aggregation test - demonstrates OData aggregation queries.
///
/// The $apply parameter enables powerful aggregations like groupby and count.
/// This is part of the OData v4.0 Aggregation Extensions specification.
///
/// **⚠️ IMPORTANT: Server Support Required**
///
/// Not all RESO servers support the $apply parameter. If your server doesn't
/// support aggregations, you'll receive a 400 Bad Request error. This is normal
/// and expected behavior.
///
/// This test demonstrates:
/// - Grouping by a single field with count
/// - Grouping by multiple fields
/// - Using $filter with $apply
/// - Fallback approach when $apply is not supported
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("📊 RESO Client $apply Aggregation Test\n");
    println!("Testing OData aggregation extensions...\n");
    println!("⚠️  Note: Not all servers support $apply - failures are expected\n");
    println!("   if your server doesn't support OData Aggregation Extensions.\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    let mut successful_tests = 0;
    let mut failed_tests = 0;

    // Test 1: Group by City with count
    println!("📝 Test 1: Group by City with count");
    println!("{}", "=".repeat(70));

    let query1 = QueryBuilder::new("Property")
        .apply("groupby((City), aggregate($count as Count))")
        .build()?;

    println!("Query: {}", query1.to_odata_string());

    match client.execute(&query1).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Query successful! Retrieved {} groups", records.len());
                successful_tests += 1;

                println!("\n📊 Property count by city (showing first 10):");
                println!("{:-<70}", "");

                for (i, record) in records.iter().take(10).enumerate() {
                    let city = record["City"].as_str().unwrap_or("(Unknown)");
                    let count = record["Count"].as_u64().unwrap_or(0);
                    println!("   {}. {:30} {} properties", i + 1, city, count);
                }

                if records.len() > 10 {
                    println!("   ... and {} more cities", records.len() - 10);
                }
            }
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
            println!("💡 This is expected if your server doesn't support $apply");
            failed_tests += 1;
        }
    }

    println!();

    // Test 2: Group by PropertyType with count
    println!("📝 Test 2: Group by PropertyType with count");
    println!("{}", "=".repeat(70));

    let query2 = QueryBuilder::new("Property")
        .apply("groupby((PropertyType), aggregate($count as Count))")
        .build()?;

    println!("Query: {}", query2.to_odata_string());

    match client.execute(&query2).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Query successful! Retrieved {} groups", records.len());
                successful_tests += 1;

                println!("\n📊 Property count by type:");
                println!("{:-<70}", "");

                for record in records.iter() {
                    let prop_type = record["PropertyType"].as_str().unwrap_or("(Unknown)");
                    let count = record["Count"].as_u64().unwrap_or(0);
                    println!("   {:40} {} properties", prop_type, count);
                }
            }
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
            failed_tests += 1;
        }
    }

    println!();

    // Test 3: Group by StandardStatus with count
    println!("📝 Test 3: Group by StandardStatus with count");
    println!("{}", "=".repeat(70));

    let query3 = QueryBuilder::new("Property")
        .apply("groupby((StandardStatus), aggregate($count as Count))")
        .build()?;

    println!("Query: {}", query3.to_odata_string());

    match client.execute(&query3).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Query successful! Retrieved {} groups", records.len());
                successful_tests += 1;

                println!("\n📊 Property count by status:");
                println!("{:-<70}", "");

                for record in records.iter() {
                    let status = record["StandardStatus"].as_str().unwrap_or("(Unknown)");
                    let count = record["Count"].as_u64().unwrap_or(0);
                    println!("   {:40} {} properties", status, count);
                }
            }
        }
        Err(e) => {
            println!("❌ Test 3 failed: {}", e);
            failed_tests += 1;
        }
    }

    println!();

    // Test 4: Group by multiple fields (City and PropertyType)
    println!("📝 Test 4: Group by multiple fields (City, PropertyType)");
    println!("{}", "=".repeat(70));

    let query4 = QueryBuilder::new("Property")
        .apply("groupby((City, PropertyType), aggregate($count as Count))")
        .build()?;

    println!("Query: {}", query4.to_odata_string());

    match client.execute(&query4).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Query successful! Retrieved {} groups", records.len());
                successful_tests += 1;

                println!("\n📊 Property count by city and type (showing first 10):");
                println!("{:-<70}", "");

                for (i, record) in records.iter().take(10).enumerate() {
                    let city = record["City"].as_str().unwrap_or("(Unknown)");
                    let prop_type = record["PropertyType"].as_str().unwrap_or("(Unknown)");
                    let count = record["Count"].as_u64().unwrap_or(0);
                    println!(
                        "   {}. {:20} | {:20} | {} properties",
                        i + 1,
                        city,
                        prop_type,
                        count
                    );
                }

                if records.len() > 10 {
                    println!("   ... and {} more combinations", records.len() - 10);
                }
            }
        }
        Err(e) => {
            println!("❌ Test 4 failed: {}", e);
            failed_tests += 1;
        }
    }

    println!();

    // Test 5: Using $apply with $filter
    println!("📝 Test 5: $apply with $filter (Active properties only)");
    println!("{}", "=".repeat(70));

    let query5 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .apply("groupby((City), aggregate($count as Count))")
        .build()?;

    println!("Query: {}", query5.to_odata_string());

    match client.execute(&query5).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Query successful! Retrieved {} groups", records.len());
                successful_tests += 1;

                println!("\n📊 Active property count by city (showing first 10):");
                println!("{:-<70}", "");

                for (i, record) in records.iter().take(10).enumerate() {
                    let city = record["City"].as_str().unwrap_or("(Unknown)");
                    let count = record["Count"].as_u64().unwrap_or(0);
                    println!("   {}. {:30} {} active properties", i + 1, city, count);
                }
            }
        }
        Err(e) => {
            println!("❌ Test 5 failed: {}", e);
            failed_tests += 1;
        }
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("📊 Test Summary");
    println!("{}", "=".repeat(70));
    println!("Successful tests: {}", successful_tests);
    println!("Failed tests: {}", failed_tests);

    if successful_tests > 0 {
        println!("\n✅ Your server supports $apply aggregations!");
        println!("   You can use groupby and aggregate operations.");
    } else {
        println!("\n⚠️  Your server does not support $apply aggregations.");
        println!("\n💡 Alternative Approach Without $apply:");
        println!("   Instead of using $apply, you can:");
        println!("   1. Use multiple filtered count queries");
        println!("   2. Fetch all data and aggregate client-side");
        println!("   3. Use database views if available");
        println!();
        println!("   Example without $apply:");
        println!("   ```rust");
        println!("   let statuses = [\"Active\", \"Pending\", \"Closed\"];");
        println!("   for status in statuses {{");
        println!("       let query = QueryBuilder::new(\"Property\")");
        println!("           .filter(&format!(\"StandardStatus eq '{{}}'\", status))");
        println!("           .count()");
        println!("           .build()?;");
        println!("       let count = client.execute_count(&query).await?;");
        println!("       println!(\"{{}}: {{}}\", status, count);");
        println!("   }}");
        println!("   ```");
    }

    Ok(())
}
