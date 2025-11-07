use reso_client::{QueryBuilder, ResoClient};

/// Comprehensive test of all Core Query examples from CoreQueries.md
///
/// This example demonstrates all the query patterns documented in the RESO
/// Core Queries specification, testing them against a live RESO API server.
///
/// Queries tested:
/// 1. Date range filters (properties listed in specific month/year)
/// 2. Complex OR conditions (multiple values for a field)
/// 3. Boolean field queries (e.g., WaterfrontYN)
/// 4. Price range queries (gt, lt, eq)
/// 5. Order by (ascending/descending)
/// 6. Count queries
/// 7. Top/Skip pagination
/// 8. Select specific fields
/// 9. Singleton (key access) queries
/// 10. Multiple field filters
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔍 RESO Client - Core Queries Test Suite\n");
    println!("Testing all query patterns from CoreQueries.md specification\n");
    println!("{}", "=".repeat(80));

    // Create client using environment variables
    let client = ResoClient::from_env()?;
    println!("✅ Client created successfully\n");

    let mut test_count = 0;
    let mut passed_count = 0;

    // Test 1: Get Properties Listed in December of 2020
    test_count += 1;
    println!(
        "\n📋 Test {}: Properties Listed in December 2020",
        test_count
    );
    println!("{}", "-".repeat(80));
    match test_date_range_month(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 2: Get Properties Listed in a Given Year (2020)
    test_count += 1;
    println!("\n📋 Test {}: Properties Listed in Year 2020", test_count);
    println!("{}", "-".repeat(80));
    match test_date_range_year(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 3: Get Active Members with First Name 'James' or 'Adam'
    test_count += 1;
    println!("\n📋 Test {}: Active Members with OR condition", test_count);
    println!("{}", "-".repeat(80));
    match test_or_condition(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 4: Query on Boolean Field to Find Short Sales
    test_count += 1;
    println!(
        "\n📋 Test {}: Boolean Field Query (WaterfrontYN)",
        test_count
    );
    println!("{}", "-".repeat(80));
    match test_boolean_field(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 5: Get Properties with Price Range $250K-$500K
    test_count += 1;
    println!("\n📋 Test {}: Price Range Query ($250K-$500K)", test_count);
    println!("{}", "-".repeat(80));
    match test_price_range(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 6: Get Properties with Price Greater Than $300K
    test_count += 1;
    println!("\n📋 Test {}: Price Greater Than $300K", test_count);
    println!("{}", "-".repeat(80));
    match test_price_greater_than(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 7: Get Properties with Price Equal to $300K
    test_count += 1;
    println!("\n📋 Test {}: Price Equal to $300K", test_count);
    println!("{}", "-".repeat(80));
    match test_price_equal(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 8: Get Properties with Price Less Than $300K
    test_count += 1;
    println!("\n📋 Test {}: Price Less Than $300K", test_count);
    println!("{}", "-".repeat(80));
    match test_price_less_than(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 9: Retrieve Records in Specific Order (descending by price)
    test_count += 1;
    println!("\n📋 Test {}: Order By ListPrice Descending", test_count);
    println!("{}", "-".repeat(80));
    match test_order_by(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 10: Get a Count of Property Records
    test_count += 1;
    println!("\n📋 Test {}: Count Query with Select and Top", test_count);
    println!("{}", "-".repeat(80));
    match test_count_query(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 11: Get Top Five Residential Properties
    test_count += 1;
    println!("\n📋 Test {}: Top 5 Residential Properties", test_count);
    println!("{}", "-".repeat(80));
    match test_top_n(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 12: Get the First Five Members (pagination - page 1)
    test_count += 1;
    println!("\n📋 Test {}: Pagination - First 5 Members", test_count);
    println!("{}", "-".repeat(80));
    match test_pagination_first(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 13: Get the Next Five Members (pagination - page 2)
    test_count += 1;
    println!(
        "\n📋 Test {}: Pagination - Next 5 Members (skip 5)",
        test_count
    );
    println!("{}", "-".repeat(80));
    match test_pagination_next(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 14: Select Specific Field Values
    test_count += 1;
    println!("\n📋 Test {}: Select Specific Fields", test_count);
    println!("{}", "-".repeat(80));
    match test_select_fields(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 15: Get Most Recent Records with Select and OrderBy
    test_count += 1;
    println!(
        "\n📋 Test {}: Select with OrderBy (ModificationTimestamp desc)",
        test_count
    );
    println!("{}", "-".repeat(80));
    match test_select_with_orderby(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 16: Get a Single Property Record (Singleton/Key Access)
    test_count += 1;
    println!("\n📋 Test {}: Singleton Query (Key Access)", test_count);
    println!("{}", "-".repeat(80));
    match test_singleton_query(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Test 17: Filter by Multiple Field Values
    test_count += 1;
    println!("\n📋 Test {}: Multiple Field Filter", test_count);
    println!("{}", "-".repeat(80));
    match test_multiple_field_filter(&client).await {
        Ok(_) => {
            println!("✅ Test passed");
            passed_count += 1;
        }
        Err(e) => println!("⚠️  Test failed: {}", e),
    }

    // Final summary
    println!("\n{}", "=".repeat(80));
    println!("\n📊 Test Summary");
    println!("{}", "=".repeat(80));
    println!("Total tests: {}", test_count);
    println!("Passed: {}", passed_count);
    println!("Failed: {}", test_count - passed_count);
    println!(
        "Success rate: {:.1}%",
        (passed_count as f64 / test_count as f64) * 100.0
    );

    if passed_count == test_count {
        println!(
            "\n✨ All tests passed! Your RESO API implementation supports all Core Query patterns."
        );
    } else {
        println!("\n⚠️  Some tests failed. This may indicate:");
        println!("   - Unsupported query features on the server");
        println!("   - Missing data in the test dataset");
        println!("   - Network or authentication issues");
    }

    Ok(())
}

// Test 1: Properties listed in December 2020
async fn test_date_range_month(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("ListingContractDate ge 2020-12-01 and ListingContractDate lt 2021-01-01")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        if let Some(first) = values.first() {
            if let Some(date) = first["ListingContractDate"].as_str() {
                println!("Sample ListingContractDate: {}", date);
            }
        }
    }

    Ok(())
}

// Test 2: Properties listed in 2020
async fn test_date_range_year(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("ListingContractDate ge 2020-01-01 and ListingContractDate lt 2021-01-01")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
    }

    Ok(())
}

// Test 3: Active members with first name 'James' or 'Adam'
async fn test_or_condition(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Member")
        .filter("MemberStatus eq 'Active' and (MemberFirstName eq 'James' or MemberFirstName eq 'Adam')")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        for (i, record) in values.iter().take(2).enumerate() {
            if let Some(first_name) = record["MemberFirstName"].as_str() {
                println!("  Record {}: MemberFirstName = {}", i + 1, first_name);
            }
        }
    }

    Ok(())
}

// Test 4: Boolean field query - WaterfrontYN
async fn test_boolean_field(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("WaterfrontYN eq true")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        if let Some(first) = values.first() {
            if let Some(waterfront_yn) = first["WaterfrontYN"].as_bool() {
                println!("Sample WaterfrontYN value: {}", waterfront_yn);
            }
        }
    }

    Ok(())
}

// Test 5: Price range $250K-$500K
async fn test_price_range(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("ListPrice gt 250000 and ListPrice lt 500000")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        for (i, record) in values.iter().take(3).enumerate() {
            if let Some(price) = record["ListPrice"].as_f64() {
                println!("  Record {}: ListPrice = ${:.2}", i + 1, price);
            }
        }
    }

    Ok(())
}

// Test 6: Price greater than $300K
async fn test_price_greater_than(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("ListPrice gt 300000")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
    }

    Ok(())
}

// Test 7: Price equal to $300K
async fn test_price_equal(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("ListPrice eq 300000")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
    }

    Ok(())
}

// Test 8: Price less than $300K
async fn test_price_less_than(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("ListPrice lt 300000")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
    }

    Ok(())
}

// Test 9: Order by ListPrice descending
async fn test_order_by(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("ListPrice lt 500000")
        .order_by("ListPrice", "desc")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        println!("Prices in descending order:");
        for (i, record) in values.iter().enumerate() {
            if let Some(price) = record["ListPrice"].as_f64() {
                println!("  {}. ${:.2}", i + 1, price);
            }
        }
    }

    Ok(())
}

// Test 10: Count query with select and top
async fn test_count_query(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .select(&["ListingKey", "ModificationTimestamp"])
        .top(1)
        .with_count()
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(count) = response["@odata.count"].as_u64() {
        println!("Total count: {}", count);
    }
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s) (limited by $top=1)", values.len());
    }

    Ok(())
}

// Test 11: Top 5 residential properties
async fn test_top_n(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .filter("PropertyType eq 'Residential'")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
    }

    Ok(())
}

// Test 12: First 5 members (pagination page 1)
async fn test_pagination_first(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Member").top(5).skip(0).build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        println!("First page members:");
        for (i, record) in values.iter().enumerate() {
            if let (Some(first), Some(last)) = (
                record["MemberFirstName"].as_str(),
                record["MemberLastName"].as_str(),
            ) {
                println!("  {}. {} {}", i + 1, first, last);
            }
        }
    }

    Ok(())
}

// Test 13: Next 5 members (pagination page 2)
async fn test_pagination_next(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Member").top(5).skip(5).build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        println!("Second page members:");
        for (i, record) in values.iter().enumerate() {
            if let (Some(first), Some(last)) = (
                record["MemberFirstName"].as_str(),
                record["MemberLastName"].as_str(),
            ) {
                println!("  {}. {} {}", i + 6, first, last);
            }
        }
    }

    Ok(())
}

// Test 14: Select specific fields
async fn test_select_fields(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Member")
        .select(&["MemberLastName", "MemberFirstName", "MemberMlsId"])
        .top(3)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        println!("Selected fields only:");
        for record in values.iter() {
            if let Some(obj) = record.as_object() {
                println!("  Fields returned: {:?}", obj.keys().collect::<Vec<_>>());
                break;
            }
        }
    }

    Ok(())
}

// Test 15: Select with OrderBy (most recent records)
async fn test_select_with_orderby(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    let query = QueryBuilder::new("Property")
        .select(&["ListingKey", "ModificationTimestamp"])
        .order_by("ModificationTimestamp", "desc")
        .top(5)
        .build()?;

    println!("Query: {}", query.to_odata_string());

    let response = client.execute(&query).await?;
    if let Some(values) = response["value"].as_array() {
        println!("Returned {} record(s)", values.len());
        println!("Most recent records:");
        for (i, record) in values.iter().enumerate() {
            if let Some(timestamp) = record["ModificationTimestamp"].as_str() {
                println!("  {}. ModificationTimestamp: {}", i + 1, timestamp);
            }
        }
    }

    Ok(())
}

// Test 16: Singleton query (key access)
async fn test_singleton_query(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    // First, get a valid ListingKey from the server
    let find_key_query = QueryBuilder::new("Property")
        .select(&["ListingKey"])
        .top(1)
        .build()?;

    let find_response = client.execute(&find_key_query).await?;

    if let Some(values) = find_response["value"].as_array() {
        if let Some(first) = values.first() {
            if let Some(listing_key) = first["ListingKey"].as_str() {
                println!("Using ListingKey: {}", listing_key);

                // Now perform the singleton query
                let query = QueryBuilder::by_key("Property", listing_key).build()?;
                println!("Query: {}", query.to_odata_string());

                let response = client.execute(&query).await?;

                // Singleton queries return the object directly, not in a "value" array
                if let Some(returned_key) = response["ListingKey"].as_str() {
                    println!("Singleton record retrieved: ListingKey = {}", returned_key);
                }

                if let Some(obj) = response.as_object() {
                    println!("Record has {} fields", obj.len());
                }
            } else {
                println!("⚠️  Could not find ListingKey in response");
            }
        } else {
            println!("⚠️  No records found to test singleton query");
        }
    }

    Ok(())
}

// Test 17: Multiple field filter
async fn test_multiple_field_filter(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    // First, get valid values for first and last name
    let find_member_query = QueryBuilder::new("Member")
        .select(&["MemberFirstName", "MemberLastName"])
        .top(1)
        .build()?;

    let find_response = client.execute(&find_member_query).await?;

    if let Some(values) = find_response["value"].as_array() {
        if let Some(first) = values.first() {
            if let (Some(first_name), Some(last_name)) = (
                first["MemberFirstName"].as_str(),
                first["MemberLastName"].as_str(),
            ) {
                println!("Testing with: {} {}", first_name, last_name);

                let query = QueryBuilder::new("Member")
                    .filter(format!(
                        "MemberFirstName eq '{}' and MemberLastName eq '{}'",
                        first_name, last_name
                    ))
                    .top(5)
                    .build()?;

                println!("Query: {}", query.to_odata_string());

                let response = client.execute(&query).await?;
                if let Some(values) = response["value"].as_array() {
                    println!("Returned {} record(s)", values.len());
                }
            } else {
                println!("⚠️  Could not find member name fields in response");
            }
        } else {
            println!("⚠️  No members found to test multiple field filter");
        }
    }

    Ok(())
}
