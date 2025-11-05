use reso_client::ResoClient;

/// $metadata exploration test - demonstrates fetching and parsing OData metadata.
///
/// The $metadata endpoint returns an XML document describing the entire API schema:
/// - Available resources (Property, Member, Office, etc.)
/// - Entity fields and their data types
/// - Relationships between entities
/// - Enumerations and their values
/// - Validation rules and constraints
///
/// This is extremely useful for:
/// - API discovery and documentation
/// - Building dynamic UIs
/// - Validation before queries
/// - Understanding available fields
///
/// This test demonstrates:
/// - Fetching the $metadata XML document
/// - Basic parsing to extract resource names
/// - Identifying key information about the API schema
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("📋 RESO Client $metadata Test\n");
    println!("Fetching and exploring OData metadata...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;
    println!("✅ Client created successfully\n");

    // Fetch the metadata
    println!("📤 Fetching $metadata from server...");
    println!("{}", "=".repeat(70));

    match client.fetch_metadata().await {
        Ok(metadata_xml) => {
            println!("✅ Metadata fetched successfully!\n");

            // Basic statistics about the XML
            let line_count = metadata_xml.lines().count();
            let char_count = metadata_xml.len();
            let kb_size = char_count as f64 / 1024.0;

            println!("📊 Metadata Document Statistics:");
            println!("{}", "-".repeat(70));
            println!("   Size: {:.2} KB ({} characters)", kb_size, char_count);
            println!("   Lines: {}", line_count);

            println!();
            println!("🔍 Analyzing metadata content...");
            println!("{}", "-".repeat(70));

            // Count entity types (resources)
            let entity_type_count = metadata_xml.matches("<EntityType").count();
            println!("   Entity Types (Resources): {}", entity_type_count);

            // Count complex types
            let complex_type_count = metadata_xml.matches("<ComplexType").count();
            println!("   Complex Types: {}", complex_type_count);

            // Count enum types
            let enum_type_count = metadata_xml.matches("<EnumType").count();
            println!("   Enumeration Types: {}", enum_type_count);

            // Try to identify common resources
            println!();
            println!("📦 Common Resources Available:");
            println!("{}", "-".repeat(70));

            let common_resources = [
                "Property",
                "Member",
                "Office",
                "Media",
                "OpenHouse",
                "Team",
                "TeamMember",
            ];

            for resource in &common_resources {
                let pattern = format!("Name=\"{}\"", resource);
                if metadata_xml.contains(&pattern) {
                    println!("   ✅ {}", resource);
                } else {
                    println!("   ⚠️  {} (not found or different name)", resource);
                }
            }

            // Look for RESO Data Dictionary version
            println!();
            println!("📖 RESO Data Dictionary Information:");
            println!("{}", "-".repeat(70));

            if metadata_xml.contains("reso.org") {
                println!("   ✅ RESO Data Dictionary annotations found");

                // Try to find version info
                if let Some(start) = metadata_xml.find("DD Wiki Page") {
                    let snippet =
                        &metadata_xml[start..start.min(metadata_xml.len()).min(start + 200)];
                    println!(
                        "   Sample annotation: {}",
                        snippet.lines().next().unwrap_or("")
                    );
                }
            } else {
                println!("   ⚠️  RESO Data Dictionary annotations not found");
            }

            // Show a sample of the XML structure
            println!();
            println!("📄 Sample XML Structure (first 20 lines):");
            println!("{}", "-".repeat(70));

            for (i, line) in metadata_xml.lines().take(20).enumerate() {
                println!("   {:3} | {}", i + 1, line.trim_start());
            }

            if line_count > 20 {
                println!("   ... ({} more lines)", line_count - 20);
            }

            // Look for specific Property fields as examples
            println!();
            println!("🏠 Sample Property Fields (searching for common fields):");
            println!("{}", "-".repeat(70));

            let common_fields = [
                "ListingKey",
                "ListingId",
                "StandardStatus",
                "ListPrice",
                "City",
                "StateOrProvince",
                "BedroomsTotal",
                "BathroomsTotalInteger",
                "LivingArea",
            ];

            for field in &common_fields {
                let pattern = format!("Name=\"{}\"", field);
                if metadata_xml.contains(&pattern) {
                    println!("   ✅ {}", field);

                    // Try to find the data type
                    if let Some(pos) = metadata_xml.find(&pattern) {
                        let snippet =
                            &metadata_xml[pos..pos.min(metadata_xml.len()).min(pos + 150)];
                        if let Some(type_start) = snippet.find("Type=\"") {
                            let type_snippet = &snippet[type_start + 6..];
                            if let Some(type_end) = type_snippet.find('"') {
                                let data_type = &type_snippet[..type_end];
                                println!("      Type: {}", data_type);
                            }
                        }
                    }
                }
            }

            // Success summary
            println!();
            println!("{}", "=".repeat(70));
            println!("✨ Metadata analysis complete!");
            println!();
            println!("💡 What you can do with metadata:");
            println!("   • Discover all available resources and fields");
            println!("   • Validate field names before building queries");
            println!("   • Generate documentation automatically");
            println!("   • Build dynamic forms based on schema");
            println!("   • Understand data types and relationships");
            println!();
            println!("💾 To save metadata to a file:");
            println!("   cargo run --example test_metadata > metadata.xml");
            println!();
            println!("🔍 To explore further:");
            println!("   • Use an XML viewer or browser to open metadata.xml");
            println!("   • Search for specific EntityType or field names");
            println!("   • Look for NavigationProperty elements for relationships");
        }
        Err(e) => {
            println!("❌ Failed to fetch metadata: {}", e);
            println!();
            println!("💡 Troubleshooting:");
            println!("   1. Check your authentication token is valid");
            println!("   2. Verify the base URL is correct");
            println!("   3. Ensure the server supports the $metadata endpoint");
            println!("   4. Check network connectivity");
            return Err(e.into());
        }
    }

    Ok(())
}
