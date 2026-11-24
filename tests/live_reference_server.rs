// Live tests against a local RESO reference server (tools.reso.org, `reso-reference-server`).
//
// Ignored by default; CI never runs them. Start the server and run:
//   RESO_LIVE_URL=http://localhost:8080 cargo test --test live_reference_server -- --ignored
// The server needs no token for reads; any string is accepted. The seeded DD 2.0 data is assumed
// (Property has 50 rows). Nothing from the server is stored in this repository (RESO licence).

use reso_client::{ClientConfig, QueryBuilder, ResoClient};

fn live_client() -> ResoClient {
    let url =
        std::env::var("RESO_LIVE_URL").expect("set RESO_LIVE_URL, e.g. http://localhost:8080");
    ResoClient::with_config(ClientConfig::new(url, "dummy")).unwrap()
}

#[tokio::test]
#[ignore]
async fn live_property_query() {
    let client = live_client();
    let query = QueryBuilder::new("Property").top(5).build().unwrap();
    let response = client.execute(&query).await.unwrap();
    assert_eq!(response["value"].as_array().unwrap().len(), 5);
}

#[tokio::test]
#[ignore]
async fn live_count_matches_page() {
    let client = live_client();
    let query = QueryBuilder::new("Property")
        .with_count()
        .top(0)
        .build()
        .unwrap();
    let response = client.execute(&query).await.unwrap();
    assert!(response["@odata.count"].as_u64().unwrap() > 0);
    assert!(response["value"].as_array().unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn live_server_driven_paging() {
    // Lookup has thousands of rows, so the default page of 100 carries an absolute nextLink.
    let client = live_client();
    let query = QueryBuilder::new("Lookup").build().unwrap();
    let first = client.execute(&query).await.unwrap();
    let link = first["@odata.nextLink"].as_str().expect("nextLink");
    assert!(link.starts_with("http"));
    assert!(link.contains("$skip=100"));
    let second = client.execute_next_link(link).await.unwrap();
    assert_eq!(second.record_count, 100);
}

#[tokio::test]
#[ignore]
async fn live_metadata() {
    let xml = live_client().fetch_metadata().await.unwrap();
    assert!(xml.contains("EntityType"));
}

#[tokio::test]
#[ignore]
async fn live_bad_filter_is_odata_error() {
    let client = live_client();
    let query = QueryBuilder::new("Property")
        .filter("NoSuchField eq 1")
        .build()
        .unwrap();
    assert!(client.execute(&query).await.is_err());
}

#[tokio::test]
#[ignore]
async fn live_walk_all_pages_matches_count() {
    // Walk Lookup (thousands of rows, default page 100) to the end with next_page (#19) and
    // compare with $count; keys must not repeat.
    let client = live_client();
    let query = QueryBuilder::new("Lookup").with_count().build().unwrap();
    let mut page = client.execute_page(&query).await.unwrap();
    let expected = page.count.expect("@odata.count");
    let mut keys = std::collections::HashSet::new();
    let mut pages = 1;
    loop {
        for record in &page.records {
            assert!(keys.insert(record["LookupKey"].to_string()), "repeated key");
        }
        match client.next_page(&page).await.unwrap() {
            Some(next) => {
                page = next;
                pages += 1;
            }
            None => break,
        }
    }
    assert!(pages > 1);
    assert_eq!(keys.len() as u64, expected);
}

#[tokio::test]
#[ignore]
async fn live_client_credentials_each_transport() {
    // The reference server's mock `POST /oauth/token` accepts any credentials (#35).
    use reso_client::{ClientCredentials, CredentialTransport};
    let url =
        std::env::var("RESO_LIVE_URL").expect("set RESO_LIVE_URL, e.g. http://localhost:8080");
    for transport in [
        CredentialTransport::Body,
        CredentialTransport::BasicHeader,
        CredentialTransport::Query,
    ] {
        let creds = ClientCredentials::new(format!("{}/oauth/token", url), "any-id", "any-secret")
            .with_transport(transport);
        let client = ResoClient::with_config(ClientConfig::with_auth(&url, creds)).unwrap();
        let query = QueryBuilder::new("Property").top(1).build().unwrap();
        let response = client.execute(&query).await.unwrap();
        assert_eq!(
            response["value"].as_array().unwrap().len(),
            1,
            "{:?}",
            transport
        );
    }
}
