use reso_client::{QueryBuilder, ResoClient};

/// ACTRIS Reference Server Test Suite
///
/// This test suite is designed specifically for the ACTRIS reference server
/// and demonstrates testing various RESO resources along with ACTRIS-specific
/// custom fields.
///
/// ACTRIS Custom Fields Found in Metadata:
/// - Member: ACTRIS_REF_LastHumanModificationTimestamp
/// - Office: ACTRIS_REF_LastHumanModificationTimestamp, ACTRIS_REF_OfficeBrokerFullName,
///           ACTRIS_REF_OfficeManagerFullName, ACTRIS_REF_SyndicateNotTo
/// - OpenHouse: ACTRIS_REF_LastHumanModificationTimestamp
/// - Property: ACTRIS_REF_LastHumanModificationTimestamp, ACTRIS_REF_TaxFilledSqftTotal
/// - PropertyRooms: ACTRIS_REF_RoomTypeDescription
/// - PropertyUnitTypes: ACTRIS_REF_Deposit
///
/// Resources Available:
/// - Property (main real estate listings)
/// - Member (agents/brokers)
/// - Office (brokerage offices)
/// - OpenHouse (open house events)
/// - Media (photos/videos)
/// - HistoryTransactional (listing history)
/// - PropertyGreenVerification (green certifications)
/// - PropertyRooms (room details)
/// - PropertyUnitTypes (multi-unit properties)
///
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🏢 ACTRIS Reference Server Test Suite\n");
    println!("Testing various resources and ACTRIS-specific custom fields...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Test 1: Property with ACTRIS custom fields
    println!("📝 Test 1: Property Resource with ACTRIS Custom Fields");
    println!("{}", "=".repeat(80));

    let property_query = QueryBuilder::new("Property")
        .select(&[
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "ListPrice",
            "UnparsedAddress",
            "City",
            "StateOrProvince",
            "PostalCode",
            "PropertyType",
            "BedroomsTotal",
            "BathroomsTotalInteger",
            "LivingArea",
            "YearBuilt",
            "ModificationTimestamp",
            "ACTRIS_REF_LastHumanModificationTimestamp",
            "ACTRIS_REF_TaxFilledSqftTotal",
        ])
        .top(3)
        .build()?;

    match client.execute(&property_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} property record(s)\n", records.len());

                for (i, record) in records.iter().enumerate() {
                    println!("   Property #{}:", i + 1);

                    if let Some(key) = record["ListingKey"].as_str() {
                        println!("      Listing Key: {}", key);
                    }
                    if let Some(id) = record["ListingId"].as_str() {
                        println!("      Listing ID: {}", id);
                    }
                    if let Some(status) = record["StandardStatus"].as_str() {
                        println!("      Status: {}", status);
                    }
                    if let Some(price) = record["ListPrice"].as_f64() {
                        println!("      Price: ${:.0}", price);
                    }
                    if let Some(address) = record["UnparsedAddress"].as_str() {
                        println!("      Address: {}", address);
                    }

                    // ACTRIS Custom Fields
                    if let Some(timestamp) =
                        record["ACTRIS_REF_LastHumanModificationTimestamp"].as_str()
                    {
                        println!("      🔹 ACTRIS Last Human Mod: {}", timestamp);
                    }
                    if let Some(tax_sqft) = record["ACTRIS_REF_TaxFilledSqftTotal"].as_i64() {
                        println!("      🔹 ACTRIS Tax Filled SqFt: {}", tax_sqft);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 1 failed: {}", e);
        }
    }

    println!();

    // Test 2: Member resource with ACTRIS custom fields
    println!("📝 Test 2: Member Resource with ACTRIS Custom Fields");
    println!("{}", "=".repeat(80));

    let member_query = QueryBuilder::new("Member")
        .select(&[
            "MemberKey",
            "MemberMlsId",
            "MemberFirstName",
            "MemberLastName",
            "MemberStatus",
            "ModificationTimestamp",
            "ACTRIS_REF_LastHumanModificationTimestamp",
        ])
        .top(3)
        .build()?;

    match client.execute(&member_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} member record(s)\n", records.len());

                for (i, record) in records.iter().enumerate() {
                    println!("   Member #{}:", i + 1);

                    if let Some(key) = record["MemberKey"].as_str() {
                        println!("      Member Key: {}", key);
                    }

                    let first = record["MemberFirstName"].as_str().unwrap_or("N/A");
                    let last = record["MemberLastName"].as_str().unwrap_or("N/A");
                    println!("      Name: {} {}", first, last);

                    if let Some(status) = record["MemberStatus"].as_str() {
                        println!("      Status: {}", status);
                    }

                    // ACTRIS Custom Field
                    if let Some(timestamp) =
                        record["ACTRIS_REF_LastHumanModificationTimestamp"].as_str()
                    {
                        println!("      🔹 ACTRIS Last Human Mod: {}", timestamp);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 2 failed: {}", e);
        }
    }

    println!();

    // Test 3: Office resource with ACTRIS custom fields
    println!("📝 Test 3: Office Resource with ACTRIS Custom Fields");
    println!("{}", "=".repeat(80));

    let office_query = QueryBuilder::new("Office")
        .select(&[
            "OfficeKey",
            "OfficeMlsId",
            "OfficeName",
            "OfficeStatus",
            "OfficeCity",
            "OfficeStateOrProvince",
            "ACTRIS_REF_LastHumanModificationTimestamp",
            "ACTRIS_REF_OfficeBrokerFullName",
            "ACTRIS_REF_OfficeManagerFullName",
        ])
        .top(3)
        .build()?;

    match client.execute(&office_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} office record(s)\n", records.len());

                for (i, record) in records.iter().enumerate() {
                    println!("   Office #{}:", i + 1);

                    if let Some(key) = record["OfficeKey"].as_str() {
                        println!("      Office Key: {}", key);
                    }
                    if let Some(name) = record["OfficeName"].as_str() {
                        println!("      Office Name: {}", name);
                    }
                    if let Some(city) = record["OfficeCity"].as_str() {
                        print!("      Location: {}", city);
                        if let Some(state) = record["OfficeStateOrProvince"].as_str() {
                            print!(", {}", state);
                        }
                        println!();
                    }

                    // ACTRIS Custom Fields
                    if let Some(broker) = record["ACTRIS_REF_OfficeBrokerFullName"].as_str() {
                        println!("      🔹 ACTRIS Broker: {}", broker);
                    }
                    if let Some(manager) = record["ACTRIS_REF_OfficeManagerFullName"].as_str() {
                        println!("      🔹 ACTRIS Manager: {}", manager);
                    }
                    if let Some(timestamp) =
                        record["ACTRIS_REF_LastHumanModificationTimestamp"].as_str()
                    {
                        println!("      🔹 ACTRIS Last Human Mod: {}", timestamp);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 3 failed: {}", e);
        }
    }

    println!();

    // Test 4: OpenHouse resource with ACTRIS custom fields
    println!("📝 Test 4: OpenHouse Resource with ACTRIS Custom Fields");
    println!("{}", "=".repeat(80));

    let openhouse_query = QueryBuilder::new("OpenHouse")
        .select(&[
            "OpenHouseKey",
            "ListingKey",
            "ListingId",
            "OpenHouseDate",
            "OpenHouseStartTime",
            "OpenHouseEndTime",
            "OpenHouseStatus",
            "OpenHouseType",
            "OpenHouseRemarks",
            "ACTRIS_REF_LastHumanModificationTimestamp",
        ])
        .top(3)
        .build()?;

    match client.execute(&openhouse_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} open house record(s)\n", records.len());

                for (i, record) in records.iter().enumerate() {
                    println!("   Open House #{}:", i + 1);

                    if let Some(key) = record["OpenHouseKey"].as_str() {
                        println!("      Open House Key: {}", key);
                    }
                    if let Some(listing_id) = record["ListingId"].as_str() {
                        println!("      Listing ID: {}", listing_id);
                    }
                    if let Some(date) = record["OpenHouseDate"].as_str() {
                        println!("      Date: {}", date);
                    }
                    if let Some(status) = record["OpenHouseStatus"].as_str() {
                        println!("      Status: {}", status);
                    }
                    if let Some(oh_type) = record["OpenHouseType"].as_str() {
                        println!("      Type: {}", oh_type);
                    }

                    // ACTRIS Custom Field
                    if let Some(timestamp) =
                        record["ACTRIS_REF_LastHumanModificationTimestamp"].as_str()
                    {
                        println!("      🔹 ACTRIS Last Human Mod: {}", timestamp);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 4 failed: {}", e);
        }
    }

    println!();

    // Test 5: Media resource
    println!("📝 Test 5: Media Resource");
    println!("{}", "=".repeat(80));

    let media_query = QueryBuilder::new("Media")
        .select(&[
            "MediaKey",
            "ResourceName",
            "ResourceRecordKey",
            "MediaType",
            "Order",
            "PreferredPhotoYN",
            "ModificationTimestamp",
        ])
        .filter("ResourceName eq 'Property'")
        .top(5)
        .build()?;

    match client.execute(&media_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} media record(s)\n", records.len());

                for (i, record) in records.iter().enumerate() {
                    println!("   Media #{}:", i + 1);

                    if let Some(key) = record["MediaKey"].as_str() {
                        println!("      Media Key: {}", key);
                    }
                    if let Some(resource) = record["ResourceName"].as_str() {
                        println!("      Resource: {}", resource);
                    }
                    if let Some(record_key) = record["ResourceRecordKey"].as_str() {
                        println!("      Record Key: {}", record_key);
                    }
                    if let Some(media_type) = record["MediaType"].as_str() {
                        println!("      Media Type: {}", media_type);
                    }
                    if let Some(order) = record["Order"].as_i64() {
                        println!("      Order: {}", order);
                    }
                    if let Some(preferred) = record["PreferredPhotoYN"].as_bool() {
                        println!("      Preferred Photo: {}", preferred);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 5 failed: {}", e);
        }
    }

    println!();

    // Test 6: Filter with ACTRIS custom field
    println!("📝 Test 6: Filter Using ACTRIS Custom Fields");
    println!("{}", "=".repeat(80));

    let actris_filter_query = QueryBuilder::new("Property")
        .filter("ACTRIS_REF_LastHumanModificationTimestamp ne null")
        .select(&[
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "ModificationTimestamp",
            "ACTRIS_REF_LastHumanModificationTimestamp",
        ])
        .top(3)
        .build()?;

    match client.execute(&actris_filter_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!(
                    "✅ Retrieved {} properties with ACTRIS timestamps\n",
                    records.len()
                );

                for (i, record) in records.iter().enumerate() {
                    println!("   Property #{}:", i + 1);

                    if let Some(listing_id) = record["ListingId"].as_str() {
                        println!("      Listing ID: {}", listing_id);
                    }
                    if let Some(mod_ts) = record["ModificationTimestamp"].as_str() {
                        println!("      System Mod Time: {}", mod_ts);
                    }
                    if let Some(actris_ts) =
                        record["ACTRIS_REF_LastHumanModificationTimestamp"].as_str()
                    {
                        println!("      🔹 ACTRIS Human Mod: {}", actris_ts);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 7 failed: {}", e);
        }
    }

    println!();

    // Test 7: PropertyRooms with ACTRIS custom field
    println!("📝 Test 7: PropertyRooms Resource with ACTRIS Custom Fields");
    println!("{}", "=".repeat(80));

    let rooms_query = QueryBuilder::new("PropertyRooms")
        .select(&[
            "ListingKey",
            "RoomType",
            "RoomLevel",
            "ACTRIS_REF_RoomTypeDescription",
        ])
        .top(3)
        .build()?;

    match client.execute(&rooms_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!("✅ Retrieved {} property room record(s)\n", records.len());

                for (i, record) in records.iter().enumerate() {
                    println!("   Room #{}:", i + 1);

                    if let Some(key) = record["PropertyRoomsKey"].as_str() {
                        println!("      Room Key: {}", key);
                    }
                    if let Some(room_type) = record["RoomType"].as_str() {
                        println!("      Room Type: {}", room_type);
                    }
                    if let Some(level) = record["RoomLevel"].as_str() {
                        println!("      Level: {}", level);
                    }

                    // ACTRIS Custom Field
                    if let Some(description) = record["ACTRIS_REF_RoomTypeDescription"].as_str() {
                        println!("      🔹 ACTRIS Description: {}", description);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 8 failed: {}", e);
            println!("   Note: PropertyRooms may not have data in this server");
        }
    }

    println!();

    // Test 8: Complex filter on Property
    println!("📝 Test 8: Complex Query - Active Properties with Details");
    println!("{}", "=".repeat(80));

    let complex_query = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active' and ListPrice ne null")
        .select(&[
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "ListPrice",
            "UnparsedAddress",
            "City",
            "PropertyType",
            "BedroomsTotal",
            "LivingArea",
        ])
        .order_by("ListPrice", "desc")
        .top(3)
        .build()?;

    match client.execute(&complex_query).await {
        Ok(response) => {
            if let Some(records) = response["value"].as_array() {
                println!(
                    "✅ Retrieved {} active property(ies) sorted by price\n",
                    records.len()
                );

                for (i, record) in records.iter().enumerate() {
                    println!("   Property #{}:", i + 1);

                    if let Some(listing_id) = record["ListingId"].as_str() {
                        println!("      Listing ID: {}", listing_id);
                    }
                    if let Some(price) = record["ListPrice"].as_f64() {
                        println!("      Price: ${:.0}", price);
                    }
                    if let Some(address) = record["UnparsedAddress"].as_str() {
                        println!("      Address: {}", address);
                    }
                    if let Some(prop_type) = record["PropertyType"].as_str() {
                        println!("      Type: {}", prop_type);
                    }
                    if let Some(beds) = record["BedroomsTotal"].as_i64() {
                        println!("      Bedrooms: {}", beds);
                    }

                    println!();
                }
            }
        }
        Err(e) => {
            println!("❌ Test 9 failed: {}", e);
        }
    }

    println!();

    // Summary
    println!("{}", "=".repeat(80));
    println!("✨ ACTRIS Reference Server Test Suite Completed!");
    println!();
    println!("📊 Test Summary:");
    println!("   • Property resource with ACTRIS custom fields");
    println!("   • Member resource with ACTRIS timestamp tracking");
    println!("   • Office resource with ACTRIS broker/manager fields");
    println!("   • OpenHouse resource with ACTRIS timestamps");
    println!("   • Media resource (standard RESO)");
    println!("   • Filtering on ACTRIS custom fields");
    println!("   • PropertyRooms with ACTRIS descriptions");
    println!("   • Complex multi-field filtering and sorting");
    println!();
    println!("🔹 ACTRIS Custom Fields Tested:");
    println!("   • ACTRIS_REF_LastHumanModificationTimestamp");
    println!("   • ACTRIS_REF_TaxFilledSqftTotal");
    println!("   • ACTRIS_REF_OfficeBrokerFullName");
    println!("   • ACTRIS_REF_OfficeManagerFullName");
    println!("   • ACTRIS_REF_RoomTypeDescription");
    println!();
    println!("💡 Additional Resources Available:");
    println!("   • PropertyGreenVerification");
    println!("   • PropertyUnitTypes (has ACTRIS_REF_Deposit field)");
    println!("   • HistoryTransactional");
    println!("   • Field (metadata exploration)");

    Ok(())
}
