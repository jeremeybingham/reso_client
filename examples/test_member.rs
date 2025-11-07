use reso_client::{QueryBuilder, ResoClient};

/// Member resource test - demonstrates working with the Member resource.
///
/// The Member resource represents real estate agents, brokers, and other
/// MLS participants. This is separate from the Property resource and contains
/// information about the people and entities involved in real estate transactions.
///
/// Common Member fields:
/// - MemberKey (unique identifier)
/// - MemberMlsId (MLS ID)
/// - MemberFirstName, MemberLastName
/// - MemberStatus (Active, Inactive, etc.)
/// - MemberType (Agent, Broker, etc.)
/// - OfficeKey (link to Office resource)
///
/// This test demonstrates:
/// - Basic Member queries
/// - Filtering by status and name
/// - Selecting specific fields
/// - Counting members
/// - Using Member data for lookups
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("👥 RESO Client Member Resource Test\n");
    println!("Testing queries against the Member resource...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Test 1: Basic Member query
    println!("📝 Test 1: Fetch sample members");
    println!("{}", "=".repeat(70));

    let query1 = QueryBuilder::new("Member")
        .select(&[
            "MemberKey",
            "MemberMlsId",
            "MemberFirstName",
            "MemberLastName",
            "MemberStatus",
        ])
        .top(5)
        .build()?;

    match client.execute(&query1).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} member(s)\n", records.len());

                if records.is_empty() {
                    println!("⚠️  No members found");
                } else {
                    for (i, record) in records.iter().enumerate() {
                        println!("   Member #{}:", i + 1);

                        if let Some(key) = record["MemberKey"].as_str() {
                            println!("      Member Key: {}", key);
                        }
                        if let Some(mls_id) = record["MemberMlsId"].as_str() {
                            println!("      MLS ID: {}", mls_id);
                        }

                        let first = record["MemberFirstName"].as_str().unwrap_or("(Unknown)");
                        let last = record["MemberLastName"].as_str().unwrap_or("(Unknown)");
                        println!("      Name: {} {}", first, last);

                        if let Some(status) = record["MemberStatus"].as_str() {
                            println!("      Status: {}", status);
                        }

                        println!();
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
            println!("💡 The Member resource may not be available on your server");
            return Err(e.into());
        }
    }

    // Test 2: Filter by active members
    println!("📝 Test 2: Filter for active members only");
    println!("{}", "=".repeat(70));

    let query2 = QueryBuilder::new("Member")
        .filter("MemberStatus eq 'Active'")
        .select(&[
            "MemberKey",
            "MemberFirstName",
            "MemberLastName",
            "MemberStatus",
        ])
        .top(5)
        .build()?;

    match client.execute(&query2).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} active member(s)\n", records.len());

                // Verify all are active
                let all_active = records
                    .iter()
                    .all(|r| r["MemberStatus"].as_str() == Some("Active"));

                if all_active && !records.is_empty() {
                    println!("   ✅ All members have Status = 'Active'");
                }

                // Show sample
                if let Some(first) = records.first() {
                    let first_name = first["MemberFirstName"].as_str().unwrap_or("(Unknown)");
                    let last_name = first["MemberLastName"].as_str().unwrap_or("(Unknown)");
                    println!("   Sample: {} {} (Active)", first_name, last_name);
                }
            }
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
        }
    }

    println!();

    // Test 3: Search by name
    println!("📝 Test 3: Search members by first name");
    println!("{}", "=".repeat(70));

    // First, get a sample name to search for
    let sample_query = QueryBuilder::new("Member")
        .select(&["MemberFirstName"])
        .top(1)
        .build()?;

    match client.execute(&sample_query).await {
        Ok(sample_response) => {
            if let Some(values) = sample_response["value"].as_array() {
                if let Some(first) = values.first() {
                    if let Some(search_name) = first["MemberFirstName"].as_str() {
                        println!("Searching for members with first name: {}\n", search_name);

                        let query3 = QueryBuilder::new("Member")
                            .filter(format!("MemberFirstName eq '{}'", search_name))
                            .select(&[
                                "MemberKey",
                                "MemberFirstName",
                                "MemberLastName",
                                "MemberMlsId",
                            ])
                            .top(10)
                            .build()?;

                        match client.execute(&query3).await {
                            Ok(response) => {
                                if let Some(records) = response["value"].as_array() {
                                    println!(
                                        "   ✅ Found {} member(s) with first name '{}'",
                                        records.len(),
                                        search_name
                                    );

                                    for (i, record) in records.iter().take(5).enumerate() {
                                        let first = record["MemberFirstName"]
                                            .as_str()
                                            .unwrap_or("(Unknown)");
                                        let last = record["MemberLastName"]
                                            .as_str()
                                            .unwrap_or("(Unknown)");
                                        let mls_id =
                                            record["MemberMlsId"].as_str().unwrap_or("N/A");
                                        println!(
                                            "      {}. {} {} (MLS ID: {})",
                                            i + 1,
                                            first,
                                            last,
                                            mls_id
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                println!("   ❌ Search failed: {}", e);
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Could not get sample name: {}", e);
        }
    }

    println!();

    // Test 4: Count total members
    println!("📝 Test 4: Count total members - ⚠️ ($count will fail with 404 on actris_ref)");
    println!("{}", "=".repeat(70));

    let query4 = QueryBuilder::new("Member").count().build()?;

    match client.execute_count(&query4).await {
        Ok(count) => {
            println!("✅ Total members in system: {}", count);
        }
        Err(e) => {
            println!("❌ Test 4 failed: {}", e);
        }
    }

    println!();

    // Test 5: Count by member status
    println!("📝 Test 5: Count members by status - ⚠️ ($count will fail with 404 on actris_ref)");
    println!("{}", "=".repeat(70));

    let statuses = ["Active", "Inactive", "Suspended"];

    for status in &statuses {
        let query = QueryBuilder::new("Member")
            .filter(format!("MemberStatus eq '{}'", status))
            .count()
            .build()?;

        match client.execute_count(&query).await {
            Ok(count) => {
                println!("   {:15} {:>10} members", status, count);
            }
            Err(e) => {
                println!("   {:15} Error: {}", status, e);
            }
        }
    }

    println!();

    // Test 6: Order by last name
    println!("📝 Test 6: Members ordered by last name");
    println!("{}", "=".repeat(70));

    let query6 = QueryBuilder::new("Member")
        .select(&["MemberKey", "MemberFirstName", "MemberLastName"])
        .order_by("MemberLastName", "asc")
        .top(5)
        .build()?;

    match client.execute(&query6).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!(
                    "✅ Retrieved {} member(s) ordered by last name\n",
                    records.len()
                );

                for (i, record) in records.iter().enumerate() {
                    let first = record["MemberFirstName"].as_str().unwrap_or("(Unknown)");
                    let last = record["MemberLastName"].as_str().unwrap_or("(Unknown)");
                    println!("   {}. {}, {}", i + 1, last, first);
                }
            }
        }
        Err(e) => {
            println!("❌ Test 6 failed: {}", e);
        }
    }

    println!();

    // Test 7: Key access - fetch specific member
    println!("📝 Test 7: Fetch specific member by key");
    println!("{}", "=".repeat(70));

    // First, get a valid member key
    let key_query = QueryBuilder::new("Member")
        .select(&["MemberKey"])
        .top(1)
        .build()?;

    match client.execute(&key_query).await {
        Ok(key_response) => {
            if let Some(values) = key_response["value"].as_array() {
                if let Some(first) = values.first() {
                    if let Some(member_key) = first["MemberKey"].as_str() {
                        println!("Using MemberKey: {}\n", member_key);

                        let query7 = QueryBuilder::by_key("Member", member_key)
                            .select(&["MemberKey", "MemberFirstName", "MemberLastName"])
                            .build()?;

                        match client.execute_by_key(&query7).await {
                            Ok(record) => {
                                println!("✅ Member record retrieved:");
                                let first =
                                    record["MemberFirstName"].as_str().unwrap_or("(Unknown)");
                                let last = record["MemberLastName"].as_str().unwrap_or("(Unknown)");

                                println!("   Name: {} {}", first, last);
                            }
                            Err(e) => {
                                println!("❌ Key access failed: {}", e);
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("❌ Could not get member key: {}", e);
        }
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("✨ Member resource tests completed!");
    println!();
    println!("💡 Common Use Cases for Member Resource:");
    println!("   • Agent/broker lookup by name or MLS ID");
    println!("   • Validating listing agents");
    println!("   • Building agent directories");
    println!("   • Contact information retrieval");
    println!("   • Office roster management");
    println!();
    println!("🔗 Related Resources:");
    println!("   • Office - Organization information");
    println!("   • Property - Links to ListAgent, CoListAgent");
    println!("   • Team - Team membership information");

    Ok(())
}
