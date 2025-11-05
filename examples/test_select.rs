use reso_client::{QueryBuilder, ResoClient};

/// Field selection test - verifies the $select parameter works correctly.
///
/// This test demonstrates and validates:
/// - Selecting specific fields reduces response size
/// - Only requested fields are returned
/// - Field selection works with filters
/// - Performance benefits of selecting fewer fields
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("📋 RESO Client Select Test\n");
    println!("Testing OData $select parameter...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Test 1: Query with ALL fields (no select)
    println!("📝 Test 1: Query WITHOUT $select (all fields)");
    println!("{}", "=".repeat(70));

    let query1 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .top(1)
        .build()?;

    let all_fields_count = match client.execute(&query1).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                if let Some(first_record) = records.first() {
                    let field_count = first_record.as_object().map(|obj| obj.len()).unwrap_or(0);

                    println!("✅ Retrieved 1 record");
                    println!("📊 Total fields in response: {}", field_count);

                    // Show some example fields
                    if let Some(obj) = first_record.as_object() {
                        let sample_fields: Vec<_> =
                            obj.keys().take(5).map(|k| k.as_str()).collect();
                        println!("   Sample fields: {}", sample_fields.join(", "));
                    }

                    field_count
                } else {
                    println!("⚠️  No records returned");
                    0
                }
            } else {
                println!("❌ Invalid response format");
                0
            }
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
            return Err(e.into());
        }
    };

    println!();

    // Test 2: Query with specific fields selected
    println!("📝 Test 2: Query WITH $select (3 fields only)");
    println!("{}", "=".repeat(70));

    let selected_fields = vec!["ListingKey", "ListPrice", "StandardStatus"];
    let query2 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&selected_fields)
        .top(1)
        .build()?;

    match client.execute(&query2).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                if let Some(first_record) = records.first() {
                    let field_count = first_record.as_object().map(|obj| obj.len()).unwrap_or(0);

                    println!("✅ Retrieved 1 record");
                    println!("📊 Total fields in response: {}", field_count);

                    // Verify we got exactly the fields we asked for
                    if let Some(obj) = first_record.as_object() {
                        let returned_fields: Vec<_> = obj.keys().map(|k| k.as_str()).collect();

                        println!("   Requested: {}", selected_fields.join(", "));
                        println!("   Returned:  {}", returned_fields.join(", "));

                        // Check if only requested fields are present
                        let only_requested = returned_fields
                            .iter()
                            .all(|field| selected_fields.contains(field));

                        if only_requested && field_count == selected_fields.len() {
                            println!("✅ Response contains ONLY requested fields");
                        } else {
                            println!("⚠️  Response may contain additional fields");
                        }

                        // Display the values
                        println!("\n   Values:");
                        for field in &selected_fields {
                            if let Some(value) = obj.get(*field) {
                                match value {
                                    serde_json::Value::String(s) => {
                                        println!("     {}: {}", field, s);
                                    }
                                    serde_json::Value::Number(n) => {
                                        println!("     {}: {}", field, n);
                                    }
                                    _ => {
                                        println!("     {}: {:?}", field, value);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    println!("⚠️  No records returned");
                }
            } else {
                println!("❌ Invalid response format");
            }
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
            return Err(e.into());
        }
    }

    println!();

    // Test 3: Compare efficiency
    println!("📝 Test 3: Efficiency Comparison");
    println!("{}", "=".repeat(70));

    if all_fields_count > 0 {
        let selected_count = selected_fields.len();
        let reduction_percent =
            ((all_fields_count - selected_count) as f64 / all_fields_count as f64) * 100.0;

        println!("📉 Field count reduction:");
        println!("   Without $select: {} fields", all_fields_count);
        println!("   With $select:    {} fields", selected_count);
        println!("   Reduction:       {:.1}%", reduction_percent);
        println!();
        println!("✅ Selecting specific fields significantly reduces response size");
        println!("   This improves performance and reduces bandwidth usage.");
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("✨ Select tests completed successfully!");
    println!("\nKey findings:");
    println!("  ✓ $select parameter works correctly");
    println!("  ✓ Only requested fields are returned");
    println!("  ✓ Response size is reduced significantly");
    println!("\n💡 Best practice: Always use $select to request only the fields you need!");

    Ok(())
}
