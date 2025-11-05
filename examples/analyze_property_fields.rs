use reso_client::{QueryBuilder, ResoClient};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write as IoWrite;

/// Field Usage Statistics
#[derive(Debug, Clone)]
struct FieldStats {
    name: String,
    populated_count: usize,
    null_count: usize,
    empty_count: usize,
    sample_values: Vec<String>,
}

impl FieldStats {
    fn new(name: String) -> Self {
        Self {
            name,
            populated_count: 0,
            null_count: 0,
            empty_count: 0,
            sample_values: Vec::new(),
        }
    }

    fn population_rate(&self, total: usize) -> f64 {
        if total == 0 {
            return 0.0;
        }
        (self.populated_count as f64 / total as f64) * 100.0
    }

    fn add_sample(&mut self, value: String) {
        if self.sample_values.len() < 3 && !self.sample_values.contains(&value) {
            self.sample_values.push(value);
        }
    }
}

/// Analyze a value and determine if it's populated
fn analyze_value(value: &Value) -> (bool, Option<String>) {
    match value {
        Value::Null => (false, None),
        Value::String(s) if s.is_empty() => (false, None),
        Value::Array(arr) if arr.is_empty() => (false, None),
        Value::Object(obj) if obj.is_empty() => (false, None),
        Value::String(s) => (true, Some(s.clone())),
        Value::Number(n) => (true, Some(n.to_string())),
        Value::Bool(b) => (true, Some(b.to_string())),
        Value::Array(arr) => (true, Some(format!("[{} items]", arr.len()))),
        Value::Object(_) => (true, Some("[object]".to_string())),
    }
}

/// Generate recommendations based on field statistics
fn generate_recommendations(stats: &HashMap<String, FieldStats>, total_records: usize) -> Value {
    let mut core_fields = Vec::new();
    let mut highly_useful = Vec::new();
    let mut moderately_useful = Vec::new();
    let mut rarely_used = Vec::new();
    let mut never_used = Vec::new();

    // Core fields that should always be included
    let core_field_names = vec![
        "ListingKey",
        "ListingId",
        "StandardStatus",
        "MlsStatus",
        "ListPrice",
        "UnparsedAddress",
        "City",
        "StateOrProvince",
        "PostalCode",
        "PropertyType",
        "PropertySubType",
        "ModificationTimestamp",
        "ListingContractDate",
    ];

    for (field_name, field_stat) in stats.iter() {
        let rate = field_stat.population_rate(total_records);

        if core_field_names.contains(&field_name.as_str()) {
            core_fields.push(json!({
                "field": field_name,
                "populationRate": format!("{:.2}%", rate),
                "reason": "Essential field for property identification and status"
            }));
        } else if rate >= 80.0 {
            highly_useful.push(json!({
                "field": field_name,
                "populationRate": format!("{:.2}%", rate),
                "reason": "Highly populated across listings"
            }));
        } else if rate >= 40.0 {
            moderately_useful.push(json!({
                "field": field_name,
                "populationRate": format!("{:.2}%", rate),
                "reason": "Moderately populated, useful for many listings"
            }));
        } else if rate > 0.0 {
            rarely_used.push(json!({
                "field": field_name,
                "populationRate": format!("{:.2}%", rate),
                "reason": "Rarely populated, consider for specialized queries only"
            }));
        } else {
            never_used.push(json!({
                "field": field_name,
                "populationRate": "0.00%",
                "reason": "Never populated in this sample"
            }));
        }
    }

    json!({
        "coreFields": core_fields,
        "highlyUseful": highly_useful,
        "moderatelyUseful": moderately_useful,
        "rarelyUsed": rarely_used,
        "neverUsed": never_used,
        "summary": {
            "totalFieldsAnalyzed": stats.len(),
            "coreFieldsCount": core_fields.len(),
            "highlyUsefulCount": highly_useful.len(),
            "moderatelyUsefulCount": moderately_useful.len(),
            "rarelyUsedCount": rarely_used.len(),
            "neverUsedCount": never_used.len()
        }
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔍 Property Field Usage Analyzer\n");
    println!("Analyzing 200 active listings to determine field usage patterns...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Build a query to fetch 200 active properties
    // We don't use select() so we get ALL fields
    let query = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .top(200)
        .build()?;

    println!("📊 Fetching 200 active properties...");

    // Execute the query
    let response = client.execute(&query).await?;

    // Parse the response
    let records = response["value"]
        .as_array()
        .ok_or("No records found in response")?;

    println!("✅ Retrieved {} property records\n", records.len());

    if records.is_empty() {
        println!("⚠️  No active properties found. Cannot perform analysis.");
        return Ok(());
    }

    // Initialize field statistics
    let mut field_stats: HashMap<String, FieldStats> = HashMap::new();
    let total_records = records.len();

    println!(
        "🔬 Analyzing field usage across {} properties...",
        total_records
    );

    // Analyze each record
    for record in records {
        if let Value::Object(obj) = record {
            for (field_name, field_value) in obj.iter() {
                // Skip OData metadata fields
                if field_name.starts_with("@odata") {
                    continue;
                }

                let stats = field_stats
                    .entry(field_name.clone())
                    .or_insert_with(|| FieldStats::new(field_name.clone()));

                let (is_populated, sample_value) = analyze_value(field_value);

                if is_populated {
                    stats.populated_count += 1;
                    if let Some(val) = sample_value {
                        stats.add_sample(val);
                    }
                } else if matches!(field_value, Value::String(s) if s.is_empty()) {
                    stats.empty_count += 1;
                } else {
                    stats.null_count += 1;
                }
            }
        }
    }

    println!("✅ Analysis complete!\n");

    // Sort fields by population rate
    let mut sorted_stats: Vec<_> = field_stats.values().cloned().collect();
    sorted_stats.sort_by(|a, b| {
        b.population_rate(total_records)
            .partial_cmp(&a.population_rate(total_records))
            .unwrap()
    });

    // Display summary statistics
    println!("{}", "=".repeat(80));
    println!("📈 FIELD USAGE SUMMARY");
    println!("{}", "=".repeat(80));
    println!();

    println!("Total records analyzed: {}", total_records);
    println!("Total fields found: {}", field_stats.len());
    println!();

    // Count fields by usage category
    let highly_used = sorted_stats
        .iter()
        .filter(|s| s.population_rate(total_records) >= 80.0)
        .count();
    let moderately_used = sorted_stats
        .iter()
        .filter(|s| {
            let rate = s.population_rate(total_records);
            rate >= 40.0 && rate < 80.0
        })
        .count();
    let rarely_used = sorted_stats
        .iter()
        .filter(|s| {
            let rate = s.population_rate(total_records);
            rate > 0.0 && rate < 40.0
        })
        .count();
    let never_used = sorted_stats
        .iter()
        .filter(|s| s.population_rate(total_records) == 0.0)
        .count();

    println!("Fields by usage category:");
    println!("  • Highly used (≥80%):       {} fields", highly_used);
    println!("  • Moderately used (40-79%): {} fields", moderately_used);
    println!("  • Rarely used (1-39%):      {} fields", rarely_used);
    println!("  • Never used (0%):          {} fields", never_used);
    println!();

    // Display top 20 most populated fields
    println!("{}", "=".repeat(80));
    println!("🌟 TOP 20 MOST POPULATED FIELDS");
    println!("{}", "=".repeat(80));
    println!();

    for (i, stat) in sorted_stats.iter().take(20).enumerate() {
        let rate = stat.population_rate(total_records);
        println!(
            "{:2}. {:40} {:6.2}% ({}/{} populated)",
            i + 1,
            stat.name,
            rate,
            stat.populated_count,
            total_records
        );
        if !stat.sample_values.is_empty() {
            println!("    Sample values: {}", stat.sample_values.join(", "));
        }
        println!();
    }

    // Generate detailed JSON report
    println!("{}", "=".repeat(80));
    println!("📄 GENERATING DETAILED JSON REPORT");
    println!("{}", "=".repeat(80));
    println!();

    let mut field_details = Vec::new();
    for stat in &sorted_stats {
        let rate = stat.population_rate(total_records);
        field_details.push(json!({
            "fieldName": stat.name,
            "populatedCount": stat.populated_count,
            "nullCount": stat.null_count,
            "emptyCount": stat.empty_count,
            "populationRate": format!("{:.2}", rate),
            "sampleValues": stat.sample_values
        }));
    }

    let recommendations = generate_recommendations(&field_stats, total_records);

    let report = json!({
        "metadata": {
            "analysisDate": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string(),
            "totalRecordsAnalyzed": total_records,
            "totalFieldsAnalyzed": field_stats.len(),
            "dataSource": "ACTRIS Reference Server",
            "filter": "StandardStatus eq 'Active'",
            "sampleSize": 200
        },
        "summary": {
            "highlyUsedFields": highly_used,
            "moderatelyUsedFields": moderately_used,
            "rarelyUsedFields": rarely_used,
            "neverUsedFields": never_used
        },
        "fieldAnalysis": field_details,
        "recommendations": recommendations,
        "suggestedMinimalFieldSet": [
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "MlsStatus",
            "ListPrice",
            "UnparsedAddress",
            "StreetNumber",
            "StreetName",
            "City",
            "StateOrProvince",
            "PostalCode",
            "PropertyType",
            "PropertySubType",
            "BedroomsTotal",
            "BathroomsTotalInteger",
            "LivingArea",
            "LotSizeSquareFeet",
            "LotSizeAcres",
            "YearBuilt",
            "ListingContractDate",
            "ModificationTimestamp",
            "PhotosCount",
            "PublicRemarks"
        ],
        "suggestedStandardFieldSet": [
            "ListingKey",
            "ListingId",
            "StandardStatus",
            "MlsStatus",
            "ListPrice",
            "UnparsedAddress",
            "StreetNumber",
            "StreetName",
            "StreetSuffix",
            "UnitNumber",
            "City",
            "StateOrProvince",
            "PostalCode",
            "CountyOrParish",
            "PropertyType",
            "PropertySubType",
            "BedroomsTotal",
            "BathroomsTotalInteger",
            "BathroomsFull",
            "BathroomsHalf",
            "LivingArea",
            "BuildingAreaTotal",
            "LotSizeSquareFeet",
            "LotSizeAcres",
            "YearBuilt",
            "Cooling",
            "Heating",
            "ParkingTotal",
            "GarageSpaces",
            "Appliances",
            "ListingContractDate",
            "CloseDate",
            "ModificationTimestamp",
            "PhotosCount",
            "Media",
            "PublicRemarks",
            "Directions",
            "ListAgentKey",
            "ListAgentFullName",
            "ListOfficeKey",
            "ListOfficeName"
        ],
        "suggestedComprehensiveFieldSet": recommendations["highlyUseful"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|item| item["field"].as_str().unwrap_or(""))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });

    // Write report to file
    let filename = "property_field_analysis_report.json";
    let mut file = File::create(filename)?;
    file.write_all(serde_json::to_string_pretty(&report)?.as_bytes())?;

    println!("✅ Report saved to: {}", filename);
    println!();

    // Display key recommendations
    println!("{}", "=".repeat(80));
    println!("💡 KEY RECOMMENDATIONS");
    println!("{}", "=".repeat(80));
    println!();

    println!(
        "Based on the analysis of {} active listings:",
        total_records
    );
    println!();

    println!("1. MINIMAL FIELD SET (for basic listings):");
    println!("   Use when you need only essential property information.");
    println!("   Includes: 23 core fields");
    println!();

    println!("2. STANDARD FIELD SET (for typical applications):");
    println!("   Use for most real estate applications and user interfaces.");
    println!("   Includes: 42 commonly used fields");
    println!();

    println!("3. COMPREHENSIVE FIELD SET (for detailed analysis):");
    println!("   Use when you need extensive property details.");
    println!(
        "   Includes: {} highly populated fields (≥80% usage)",
        highly_used
    );
    println!();

    println!("4. FIELDS TO CONSIDER OMITTING:");
    println!(
        "   {} fields are never populated in this sample",
        never_used
    );
    println!(
        "   {} fields are rarely used (<40% population)",
        rarely_used
    );
    println!("   Consider omitting these unless specifically needed.");
    println!();

    println!("{}", "=".repeat(80));
    println!("✨ Analysis complete!");
    println!();
    println!("📊 Full detailed report available in: {}", filename);
    println!();
    println!("Next steps:");
    println!("  1. Review the JSON report for detailed field-by-field analysis");
    println!("  2. Choose appropriate field set based on your use case");
    println!("  3. Use QueryBuilder.select() to request only needed fields");
    println!("  4. Monitor field usage patterns as data evolves");

    Ok(())
}
