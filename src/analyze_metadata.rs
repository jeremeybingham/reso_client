// analyze_metadata.rs - Tool to fetch and analyze your RESO metadata
// Run this to understand what entities are available in your MLS

mod reso_client;
mod metadata_parser;

use reso_client::{ResoApiClient, ResoApiConfig};
use metadata_parser::{MetadataParser, EntityType};
use std::env;
use std::fs;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("RESO Metadata Analyzer");
    println!("======================\n");

    // Load environment
    dotenvy::dotenv().ok();

    let base_url = env::var("RESO_BASE_URL")
        .unwrap_or_else(|_| "https://api.bridgedataoutput.com/api/v2".to_string());
    let server_token = env::var("RESO_SERVER_TOKEN")?;
    let dataset_id = env::var("RESO_DATASET_ID")?;

    // Create client
    let config = ResoApiConfig::new(base_url, server_token)
        .with_dataset(dataset_id);
    let client = ResoApiClient::new(config);

    // Fetch metadata
    println!("📡 Fetching metadata from RESO API...");
    let metadata_xml = client.get_metadata().await?;
    println!("✓ Metadata received ({} bytes)\n", metadata_xml.len());

    // Save raw metadata for inspection
    fs::write("metadata.xml", &metadata_xml)?;
    println!("💾 Saved raw metadata to metadata.xml\n");

    // Parse metadata
    println!("🔍 Parsing metadata...");
    let schema = MetadataParser::parse(&metadata_xml)?;
    println!("✓ Parsed successfully!\n");

    // Print summary
    MetadataParser::print_summary(&schema);

    // Find RESO standard resources
    println!("\n📋 RESO Standard Resources Available:");
    let resources = MetadataParser::find_reso_resources(&schema);
    if resources.is_empty() {
        println!("  ⚠ No standard RESO resources found");
    } else {
        for resource in &resources {
            println!("  ✓ {}", resource);
        }
    }

    // Analyze Property entity in detail
    if let Some(property) = schema.entities.get("Property") {
        println!("\n🏠 Property Entity Analysis:");
        println!("   Total fields: {}", property.properties.len());
        
        println!("\n   Key fields:");
        let key_fields = vec![
            "ListingKey", "ListPrice", "StandardStatus", "City", 
            "StateOrProvince", "PostalCode", "BedroomsTotal", 
            "BathroomsTotalInteger", "ListingId", "ModificationTimestamp"
        ];
        
        for field_name in key_fields {
            if let Some(prop) = property.properties.iter().find(|p| p.name == field_name) {
                println!("   ✓ {} : {} (nullable: {})", 
                    prop.name, prop.property_type, prop.nullable);
            }
        }

        // Generate sample struct
        println!("\n📝 Generated Rust Struct Preview:");
        println!("{}", "=".repeat(60));
        let struct_code = MetadataParser::generate_struct(property);
        let lines: Vec<&str> = struct_code.lines().collect();
        for line in lines.iter().take(20) {
            println!("{}", line);
        }
        if lines.len() > 20 {
            println!("   ... ({} more fields)", lines.len() - 20);
        }
        println!("{}", "=".repeat(60));

        // Save full struct
        fs::write("generated_property.rs", struct_code)?;
        println!("\n💾 Saved full Property struct to generated_property.rs");
    }

    // Generate structs for all RESO resources
    println!("\n🔧 Generating structs for all RESO resources...");
    let mut generated_code = String::new();
    generated_code.push_str("// Generated RESO entity structs\n");
    generated_code.push_str("// Auto-generated from $metadata\n\n");
    generated_code.push_str("use serde::{Deserialize, Serialize};\n");
    generated_code.push_str("use chrono::{DateTime, NaiveDate, NaiveTime, Utc};\n\n");

    for resource in &resources {
        if let Some(entity) = schema.entities.get(resource) {
            generated_code.push_str(&MetadataParser::generate_struct(entity));
            generated_code.push_str("\n");
            println!("  ✓ Generated {}", resource);
        }
    }

    fs::write("generated_entities.rs", generated_code)?;
    println!("\n💾 Saved all entities to generated_entities.rs");

    // List all unique EDM types used
    println!("\n📊 Data Types Used:");
    let mut types = std::collections::HashSet::new();
    for entity in schema.entities.values() {
        for prop in &entity.properties {
            types.insert(prop.property_type.clone());
        }
    }
    let mut types_vec: Vec<_> = types.into_iter().collect();
    types_vec.sort();
    for t in types_vec.iter().take(15) {
        println!("  • {}", t);
    }
    if types_vec.len() > 15 {
        println!("  ... and {} more", types_vec.len() - 15);
    }

    println!("\n✅ Analysis complete!");
    println!("\nNext steps:");
    println!("1. Review generated_entities.rs for your entity structs");
    println!("2. Check metadata.xml for the full schema");
    println!("3. Use these structs in your queries");
    
    Ok(())
}