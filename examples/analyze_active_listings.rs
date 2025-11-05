use reso_client::{QueryBuilder, ResoClient};
use std::collections::HashMap;

/// Format a number with thousand separators
fn format_number(n: f64) -> String {
    let s = format!("{:.0}", n);
    let bytes: Vec<_> = s.bytes().rev().collect();
    let chunks: Vec<_> = bytes
        .chunks(3)
        .map(|chunk| chunk.iter().rev().copied().collect::<Vec<_>>())
        .collect();
    let result: Vec<_> = chunks
        .iter()
        .rev()
        .map(|chunk| String::from_utf8(chunk.clone()).unwrap())
        .collect();
    result.join(",")
}

/// Calculate median from a sorted vector
fn calculate_median(sorted_values: &[f64]) -> f64 {
    let len = sorted_values.len();
    if len == 0 {
        return 0.0;
    }
    if len.is_multiple_of(2) {
        (sorted_values[len / 2 - 1] + sorted_values[len / 2]) / 2.0
    } else {
        sorted_values[len / 2]
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🏠 Active Listings Analysis\n");
    println!("Analyzing 200 active listings using minimal field set...\n");

    // Create client using environment variables
    let client = ResoClient::from_env()?;

    // Build query with suggested minimal field set
    let query = QueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active' and PropertyType eq 'Residential'")
        .select(&[
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
            "PublicRemarks",
        ])
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

    // Initialize analysis variables
    let mut prices: Vec<f64> = Vec::new();
    let mut property_types: HashMap<String, usize> = HashMap::new();
    let mut cities: HashMap<String, usize> = HashMap::new();
    let mut states: HashMap<String, usize> = HashMap::new();
    let mut bedrooms: Vec<i64> = Vec::new();
    let mut bathrooms: Vec<i64> = Vec::new();
    let mut living_areas: Vec<f64> = Vec::new();
    let mut lot_sizes_sqft: Vec<f64> = Vec::new();
    let mut lot_sizes_acres: Vec<f64> = Vec::new();
    let mut years_built: Vec<i64> = Vec::new();
    let mut photos_counts: Vec<i64> = Vec::new();

    // Analyze each record
    for record in records {
        // Price analysis
        if let Some(price) = record["ListPrice"].as_f64() {
            if price > 0.0 {
                prices.push(price);
            }
        }

        // Property type distribution
        if let Some(prop_type) = record["PropertyType"].as_str() {
            *property_types.entry(prop_type.to_string()).or_insert(0) += 1;
        }

        // Geographic distribution
        if let Some(city) = record["City"].as_str() {
            *cities.entry(city.to_string()).or_insert(0) += 1;
        }
        if let Some(state) = record["StateOrProvince"].as_str() {
            *states.entry(state.to_string()).or_insert(0) += 1;
        }

        // Bedrooms and bathrooms
        if let Some(beds) = record["BedroomsTotal"].as_i64() {
            if beds > 0 {
                bedrooms.push(beds);
            }
        }
        if let Some(baths) = record["BathroomsTotalInteger"].as_i64() {
            if baths > 0 {
                bathrooms.push(baths);
            }
        }

        // Living area
        if let Some(area) = record["LivingArea"].as_f64() {
            if area > 0.0 {
                living_areas.push(area);
            }
        }

        // Lot size
        if let Some(lot_sqft) = record["LotSizeSquareFeet"].as_f64() {
            if lot_sqft > 0.0 {
                lot_sizes_sqft.push(lot_sqft);
            }
        }
        if let Some(lot_acres) = record["LotSizeAcres"].as_f64() {
            if lot_acres > 0.0 {
                lot_sizes_acres.push(lot_acres);
            }
        }

        // Year built
        if let Some(year) = record["YearBuilt"].as_i64() {
            if year > 1800 && year <= 2025 {
                years_built.push(year);
            }
        }

        // Photos count
        if let Some(photos) = record["PhotosCount"].as_i64() {
            photos_counts.push(photos);
        }
    }

    // Sort data for median calculations
    prices.sort_by(|a, b| a.partial_cmp(b).unwrap());
    living_areas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    lot_sizes_sqft.sort_by(|a, b| a.partial_cmp(b).unwrap());
    lot_sizes_acres.sort_by(|a, b| a.partial_cmp(b).unwrap());
    years_built.sort();
    bedrooms.sort();
    bathrooms.sort();
    photos_counts.sort();

    // Display analysis results
    println!("{}", "=".repeat(80));
    println!("📈 PRICE ANALYSIS");
    println!("{}", "=".repeat(80));
    println!();

    if !prices.is_empty() {
        let avg_price = prices.iter().sum::<f64>() / prices.len() as f64;
        let median_price = calculate_median(&prices);
        let min_price = prices.first().unwrap();
        let max_price = prices.last().unwrap();

        println!("  Total properties with prices: {}", prices.len());
        println!(
            "  Average price:                ${}",
            format_number(avg_price)
        );
        println!(
            "  Median price:                 ${}",
            format_number(median_price)
        );
        println!(
            "  Minimum price:                ${}",
            format_number(*min_price)
        );
        println!(
            "  Maximum price:                ${}",
            format_number(*max_price)
        );
    } else {
        println!("  No price data available");
    }

    println!();
    println!("{}", "=".repeat(80));
    println!("🏘️  PROPERTY TYPE DISTRIBUTION");
    println!("{}", "=".repeat(80));
    println!();

    if !property_types.is_empty() {
        let mut sorted_types: Vec<_> = property_types.iter().collect();
        sorted_types.sort_by(|a, b| b.1.cmp(a.1));

        for (prop_type, count) in sorted_types.iter().take(10) {
            let percentage = (**count as f64 / records.len() as f64) * 100.0;
            println!("  {:30} {:4} ({:5.2}%)", prop_type, count, percentage);
        }
    } else {
        println!("  No property type data available");
    }

    println!();
    println!("{}", "=".repeat(80));
    println!("📍 GEOGRAPHIC DISTRIBUTION");
    println!("{}", "=".repeat(80));
    println!();

    if !states.is_empty() {
        println!("  By State:");
        let mut sorted_states: Vec<_> = states.iter().collect();
        sorted_states.sort_by(|a, b| b.1.cmp(a.1));

        for (state, count) in sorted_states.iter().take(5) {
            let percentage = (**count as f64 / records.len() as f64) * 100.0;
            println!("    {:20} {:4} ({:5.2}%)", state, count, percentage);
        }
        println!();
    }

    if !cities.is_empty() {
        println!("  Top Cities:");
        let mut sorted_cities: Vec<_> = cities.iter().collect();
        sorted_cities.sort_by(|a, b| b.1.cmp(a.1));

        for (city, count) in sorted_cities.iter().take(10) {
            let percentage = (**count as f64 / records.len() as f64) * 100.0;
            println!("    {:30} {:4} ({:5.2}%)", city, count, percentage);
        }
    } else {
        println!("  No geographic data available");
    }

    println!();
    println!("{}", "=".repeat(80));
    println!("🛏️  BEDROOM & BATHROOM STATISTICS");
    println!("{}", "=".repeat(80));
    println!();

    if !bedrooms.is_empty() {
        let avg_beds = bedrooms.iter().sum::<i64>() as f64 / bedrooms.len() as f64;
        let median_beds = if bedrooms.len().is_multiple_of(2) {
            (bedrooms[bedrooms.len() / 2 - 1] + bedrooms[bedrooms.len() / 2]) as f64 / 2.0
        } else {
            bedrooms[bedrooms.len() / 2] as f64
        };

        println!("  Bedrooms:");
        println!("    Average:   {:.1}", avg_beds);
        println!("    Median:    {:.1}", median_beds);
        println!(
            "    Range:     {} - {}",
            bedrooms.first().unwrap(),
            bedrooms.last().unwrap()
        );
        println!();
    }

    if !bathrooms.is_empty() {
        let avg_baths = bathrooms.iter().sum::<i64>() as f64 / bathrooms.len() as f64;
        let median_baths = if bathrooms.len().is_multiple_of(2) {
            (bathrooms[bathrooms.len() / 2 - 1] + bathrooms[bathrooms.len() / 2]) as f64 / 2.0
        } else {
            bathrooms[bathrooms.len() / 2] as f64
        };

        println!("  Bathrooms:");
        println!("    Average:   {:.1}", avg_baths);
        println!("    Median:    {:.1}", median_baths);
        println!(
            "    Range:     {} - {}",
            bathrooms.first().unwrap(),
            bathrooms.last().unwrap()
        );
    }

    println!();
    println!("{}", "=".repeat(80));
    println!("📏 SIZE STATISTICS");
    println!("{}", "=".repeat(80));
    println!();

    if !living_areas.is_empty() {
        let avg_area = living_areas.iter().sum::<f64>() / living_areas.len() as f64;
        let median_area = calculate_median(&living_areas);

        println!("  Living Area:");
        println!("    Average:   {} sq ft", format_number(avg_area));
        println!("    Median:    {} sq ft", format_number(median_area));
        println!(
            "    Range:     {} - {} sq ft",
            format_number(*living_areas.first().unwrap()),
            format_number(*living_areas.last().unwrap())
        );
        println!();
    }

    if !lot_sizes_sqft.is_empty() {
        let avg_lot = lot_sizes_sqft.iter().sum::<f64>() / lot_sizes_sqft.len() as f64;
        let median_lot = calculate_median(&lot_sizes_sqft);

        println!("  Lot Size (Square Feet):");
        println!("    Average:   {} sq ft", format_number(avg_lot));
        println!("    Median:    {} sq ft", format_number(median_lot));
        println!(
            "    Range:     {} - {} sq ft",
            format_number(*lot_sizes_sqft.first().unwrap()),
            format_number(*lot_sizes_sqft.last().unwrap())
        );
        println!();
    }

    if !lot_sizes_acres.is_empty() {
        let avg_acres = lot_sizes_acres.iter().sum::<f64>() / lot_sizes_acres.len() as f64;
        let median_acres = calculate_median(&lot_sizes_acres);

        println!("  Lot Size (Acres):");
        println!("    Average:   {:.2} acres", avg_acres);
        println!("    Median:    {:.2} acres", median_acres);
        println!(
            "    Range:     {:.2} - {:.2} acres",
            lot_sizes_acres.first().unwrap(),
            lot_sizes_acres.last().unwrap()
        );
    }

    println!();
    println!("{}", "=".repeat(80));
    println!("🏗️  YEAR BUILT STATISTICS");
    println!("{}", "=".repeat(80));
    println!();

    if !years_built.is_empty() {
        let avg_year = years_built.iter().sum::<i64>() as f64 / years_built.len() as f64;
        let median_year = if years_built.len().is_multiple_of(2) {
            (years_built[years_built.len() / 2 - 1] + years_built[years_built.len() / 2]) as f64
                / 2.0
        } else {
            years_built[years_built.len() / 2] as f64
        };

        let current_year = 2025;
        let avg_age = current_year as f64 - avg_year;

        println!("  Average year built:  {:.0}", avg_year);
        println!("  Median year built:   {:.0}", median_year);
        println!(
            "  Range:               {} - {}",
            years_built.first().unwrap(),
            years_built.last().unwrap()
        );
        println!("  Average age:         {:.0} years", avg_age);
    } else {
        println!("  No year built data available");
    }

    println!();
    println!("{}", "=".repeat(80));
    println!("📸 PHOTOS STATISTICS");
    println!("{}", "=".repeat(80));
    println!();

    if !photos_counts.is_empty() {
        let avg_photos = photos_counts.iter().sum::<i64>() as f64 / photos_counts.len() as f64;
        let median_photos = if photos_counts.len().is_multiple_of(2) {
            (photos_counts[photos_counts.len() / 2 - 1] + photos_counts[photos_counts.len() / 2])
                as f64
                / 2.0
        } else {
            photos_counts[photos_counts.len() / 2] as f64
        };

        let properties_with_photos = photos_counts.iter().filter(|&&x| x > 0).count();

        println!(
            "  Properties with photos:  {} ({:.1}%)",
            properties_with_photos,
            (properties_with_photos as f64 / photos_counts.len() as f64) * 100.0
        );
        println!("  Average photos per listing:  {:.1}", avg_photos);
        println!("  Median photos per listing:   {:.1}", median_photos);
        println!(
            "  Range:                       {} - {}",
            photos_counts.first().unwrap(),
            photos_counts.last().unwrap()
        );
    } else {
        println!("  No photos data available");
    }

    println!();
    println!("{}", "=".repeat(80));
    println!("✨ Analysis Complete!");
    println!("{}", "=".repeat(80));
    println!();
    println!("This analysis used the suggested minimal field set:");
    println!("  • Basic identifiers (ListingKey, ListingId)");
    println!("  • Status fields (StandardStatus, MlsStatus)");
    println!("  • Pricing (ListPrice)");
    println!("  • Address components (UnparsedAddress, StreetNumber, StreetName, etc.)");
    println!("  • Property characteristics (PropertyType, PropertySubType, etc.)");
    println!("  • Physical attributes (BedroomsTotal, BathroomsTotalInteger, etc.)");
    println!("  • Dates (ListingContractDate, ModificationTimestamp)");
    println!("  • Media (PhotosCount, PublicRemarks)");
    println!();

    Ok(())
}
