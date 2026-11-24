// examples/test_standard_paging.rs

use reso_client::{QueryBuilder, ResoClient};

/// Standard OData paging: walk a whole result set by following the server's next links.
///
/// The server picks the page size and returns `@odata.nextLink` while more records remain.
/// `execute_page` returns the first page and `next_page` the ones after it, until it returns
/// `None`. `$top` would cap the total, so it is left out here.
///
/// Usage: `cargo run --example test_standard_paging [Resource]` (default `Property`).
/// Against a local RESO reference server: `RESO_BASE_URL=http://localhost:8080 RESO_TOKEN=dummy`.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let resource = std::env::args().nth(1).unwrap_or_else(|| "Property".into());
    let client = ResoClient::from_env()?;

    let query = QueryBuilder::new(&resource).with_count().build()?;
    let mut page = client.execute_page(&query).await?;
    let expected = page.count;

    let mut pages = 1;
    let mut total = page.records.len();
    println!("page 1: {} records", page.records.len());
    while let Some(next) = client.next_page(&page).await? {
        pages += 1;
        total += next.records.len();
        println!("page {}: {} records", pages, next.records.len());
        page = next;
    }

    println!("\n{resource}: {total} records in {pages} pages");
    match expected {
        Some(count) if count as usize == total => println!("matches @odata.count ({count})"),
        Some(count) => println!("@odata.count said {count}"),
        None => println!("server gave no @odata.count"),
    }
    Ok(())
}
