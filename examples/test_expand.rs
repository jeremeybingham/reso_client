use reso_client::{QueryBuilder, ResoClient};

/// Expand functionality test - demonstrates expanding related entities.
///
/// The $expand parameter allows you to include related data in a single request,
/// reducing the number of API calls needed. This is especially useful for:
/// - Property -> ListOffice (get office information with the property)
/// - Property -> ListAgent (get agent information with the property)
/// - Member -> Office (get the member's office information)
///
/// This test demonstrates:
/// - Expanding a single related entity
/// - Expanding multiple related entities
/// - Using expand with select
/// - Using expand with key access (singleton queries)
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔗 RESO Client Expand Test\n");
    println!("Testing $expand parameter for related entities...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Test 1: Basic query WITHOUT expand (for comparison)
    println!("📝 Test 1: Query WITHOUT $expand (baseline)");
    println!("{}", "=".repeat(70));

    let query1 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&["ListingKey", "ListPrice", "UnparsedAddress"])
        .top(1)
        .build()?;

    match client.execute(&query1).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} record(s)", records.len());

                if let Some(first) = records.first() {
                    if let Some(obj) = first.as_object() {
                        let field_count = obj.len();
                        println!("📊 Fields in response: {}", field_count);
                        println!("   Fields: {:?}", obj.keys().collect::<Vec<_>>());

                        // Check if ListOffice or ListAgent are present
                        let has_office = obj.contains_key("ListOffice");
                        let has_agent = obj.contains_key("ListAgent");

                        if !has_office && !has_agent {
                            println!("   ⚠️  No expanded entities (expected without $expand)");
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
            return Err(e.into());
        }
    }

    println!();

    // Test 2: Query WITH expand for a single related entity
    println!("📝 Test 2: Query WITH $expand for ListOffice");
    println!("{}", "=".repeat(70));

    let query2 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&["ListingKey", "ListPrice", "UnparsedAddress", "ListOffice"])
        .expand(&["ListOffice"])
        .top(1)
        .build()?;

    match client.execute(&query2).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} record(s)", records.len());

                if let Some(first) = records.first() {
                    println!("\n📋 Property Details:");
                    if let Some(listing_key) = first["ListingKey"].as_str() {
                        println!("   Listing Key: {}", listing_key);
                    }
                    if let Some(price) = first["ListPrice"].as_f64() {
                        println!("   Price: ${:.0}", price);
                    }
                    if let Some(address) = first["UnparsedAddress"].as_str() {
                        println!("   Address: {}", address);
                    }

                    // Check for expanded ListOffice
                    if let Some(office) = first.get("ListOffice") {
                        if office.is_null() {
                            println!("\n   ⚠️  ListOffice is null");
                        } else if let Some(office_obj) = office.as_object() {
                            println!("\n   ✅ ListOffice expanded successfully!");
                            println!("   📍 Office Details:");

                            if let Some(office_key) = office_obj.get("OfficeKey") {
                                println!("      Office Key: {}", office_key);
                            }
                            if let Some(office_name) = office_obj.get("OfficeName") {
                                println!("      Office Name: {}", office_name);
                            }
                            if let Some(office_phone) = office_obj.get("OfficePhone") {
                                println!("      Office Phone: {}", office_phone);
                            }

                            println!("      Total fields in ListOffice: {}", office_obj.len());
                        }
                    } else {
                        println!("\n   ⚠️  ListOffice not found in response");
                        println!("      (Server may not support $expand for this resource)");
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
            println!("\n💡 Note: If $expand is not supported by your server, this is expected.");
            println!("   Not all RESO servers support the $expand parameter.");
        }
    }

    println!();

    // Test 3: Query WITH expand for multiple related entities
    println!("📝 Test 3: Query WITH $expand for multiple entities");
    println!("{}", "=".repeat(70));

    let query3 = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .select(&["ListingKey", "ListPrice", "ListOffice", "ListAgent"])
        .expand(&["ListOffice", "ListAgent"])
        .top(1)
        .build()?;

    match client.execute(&query3).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} record(s)", records.len());

                if let Some(first) = records.first() {
                    let has_office =
                        first.get("ListOffice").is_some() && !first["ListOffice"].is_null();
                    let has_agent =
                        first.get("ListAgent").is_some() && !first["ListAgent"].is_null();

                    println!(
                        "   ListOffice expanded: {}",
                        if has_office {
                            "✅ Yes"
                        } else {
                            "⚠️  No/Null"
                        }
                    );
                    println!(
                        "   ListAgent expanded: {}",
                        if has_agent {
                            "✅ Yes"
                        } else {
                            "⚠️  No/Null"
                        }
                    );

                    if has_office || has_agent {
                        println!("\n   ✅ Successfully expanded multiple related entities!");
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Test 3 failed: {}", e);
            println!("💡 Multiple $expand may not be supported by your server.");
        }
    }

    println!();

    // Test 4: Expand with key access (singleton query)
    println!("📝 Test 4: $expand with Key Access (Singleton Query)");
    println!("{}", "=".repeat(70));

    // First, get a valid listing key
    let find_key_query = QueryBuilder::new("Property")
        .select(&["ListingKey"])
        .top(1)
        .build()?;

    match client.execute(&find_key_query).await {
        Ok(find_response) => {
            if let Some(values) = find_response["value"].as_array() {
                if let Some(first) = values.first() {
                    if let Some(listing_key) = first["ListingKey"].as_str() {
                        println!("Using ListingKey: {}\n", listing_key);

                        // Now query with key access and expand
                        let query4 = QueryBuilder::by_key("Property", listing_key)
                            .select(&["ListingKey", "ListPrice", "UnparsedAddress", "ListOffice"])
                            .expand(&["ListOffice"])
                            .build()?;

                        match client.execute_by_key(&query4).await {
                            Ok(record) => {
                                println!("✅ Key access query with expand successful!");

                                if let Some(listing_key) = record["ListingKey"].as_str() {
                                    println!("   Listing Key: {}", listing_key);
                                }

                                // Check for expanded office
                                if let Some(office) = record.get("ListOffice") {
                                    if !office.is_null() {
                                        println!("   ✅ ListOffice expanded in singleton query");
                                        if let Some(office_obj) = office.as_object() {
                                            println!(
                                                "      Office has {} fields",
                                                office_obj.len()
                                            );
                                        }
                                    } else {
                                        println!("   ⚠️  ListOffice is null");
                                    }
                                }
                            }
                            Err(e) => {
                                println!("❌ Key access with expand failed: {}", e);
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Could not fetch listing key: {}", e);
        }
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("✨ Expand tests completed!");
    println!();
    println!("💡 Key Takeaways:");
    println!("   • $expand reduces API calls by fetching related data in one request");
    println!("   • Always include expanded field names in $select when using both");
    println!("   • Not all servers support $expand - check your API documentation");
    println!("   • Common expansions: ListOffice, ListAgent, CoListAgent, etc.");

    Ok(())
}
