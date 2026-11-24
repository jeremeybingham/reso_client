// Integration tests for server-driven paging (#19): Page, execute_page, next_page, fetch_page.
//
// All data is invented. The body-link shape (absolute, $skip) mirrors RESO's reference server (#35);
// the header forms mirror Bridge's `next` header and RFC 8288 `Link` headers.

use reso_client::{ClientConfig, QueryBuilder, ResoClient, ResoError};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_for(server: &MockServer) -> ResoClient {
    ResoClient::with_config(ClientConfig::new(server.uri(), "test-token")).unwrap()
}

fn records(keys: &[&str]) -> serde_json::Value {
    keys.iter()
        .map(|k| serde_json::json!({ "ListingKey": k }))
        .collect()
}

/// Walk every page and return the keys in order.
async fn walk(client: &ResoClient) -> Vec<String> {
    let query = QueryBuilder::new("Property").build().unwrap();
    let mut page = client.execute_page(&query).await.unwrap();
    let mut keys = Vec::new();
    loop {
        keys.extend(
            page.records
                .iter()
                .map(|r| r["ListingKey"].as_str().unwrap().to_string()),
        );
        match client.next_page(&page).await.unwrap() {
            Some(next) => page = next,
            None => break,
        }
    }
    keys
}

#[tokio::test]
async fn walks_three_pages_by_body_next_link() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param_is_missing("$skip"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.context": format!("{base}/$metadata#Property"),
            "@odata.count": 5,
            "@odata.nextLink": format!("{base}/Property?$count=true&$skip=2"),
            "value": records(&["1", "2"])
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("$skip", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.nextLink": format!("{base}/Property?$count=true&$skip=4"),
            "value": records(&["3", "4"])
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("$skip", "4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": records(&["5"])
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server);
    assert_eq!(walk(&client).await, ["1", "2", "3", "4", "5"]);

    let first = client
        .execute_page(&QueryBuilder::new("Property").build().unwrap())
        .await
        .unwrap();
    assert_eq!(first.count, Some(5));
    assert!(first.has_more());
    assert!(first.body.get("@odata.context").is_some());
}

#[tokio::test]
async fn walks_by_relative_body_link() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("$skiptoken", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": records(&["2"])
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.nextLink": "Property?$skiptoken=abc",
            "value": records(&["1"])
        })))
        .mount(&server)
        .await;

    assert_eq!(walk(&client_for(&server)).await, ["1", "2"]);
}

#[tokio::test]
async fn walks_by_link_header_only() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": records(&["2"])
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "value": records(&["1"]) }))
                .insert_header(
                    "link",
                    format!(
                        "<{base}/Property>; rel=\"first\", <{base}/Property?page=2>; rel=\"next\""
                    ),
                ),
        )
        .mount(&server)
        .await;

    assert_eq!(walk(&client_for(&server)).await, ["1", "2"]);
}

#[tokio::test]
async fn body_link_wins_over_header() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "@odata.nextLink": format!("{base}/Property?$skip=1"),
                    "value": records(&["1"])
                }))
                .insert_header("link", format!("<{base}/Property?page=2>; rel=\"next\"")),
        )
        .mount(&server)
        .await;

    let page = client_for(&server)
        .execute_page(&QueryBuilder::new("Property").build().unwrap())
        .await
        .unwrap();
    assert_eq!(
        page.next_link.as_deref(),
        Some(format!("{base}/Property?$skip=1").as_str())
    );
}

#[tokio::test]
async fn malformed_link_header_ends_paging() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "value": records(&["1"]) }))
                .insert_header("link", format!("<{base}/Property?page=2; rel=\"next\"")),
        )
        .mount(&server)
        .await;

    let client = client_for(&server);
    let page = client
        .execute_page(&QueryBuilder::new("Property").build().unwrap())
        .await
        .unwrap();
    assert!(!page.has_more());
    assert!(client.next_page(&page).await.unwrap().is_none());
}

#[tokio::test]
async fn refuses_next_link_on_another_server() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.nextLink": "https://elsewhere.example.com/Property?$skip=1",
            "value": records(&["1"])
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let page = client
        .execute_page(&QueryBuilder::new("Property").build().unwrap())
        .await
        .unwrap();
    let err = client.next_page(&page).await.unwrap_err();
    assert!(matches!(err, ResoError::Parse(ref m) if m.contains("different server")));
    assert!(client
        .execute_next_link("https://elsewhere.example.com/Property")
        .await
        .is_err());
}

#[tokio::test]
async fn refuses_next_link_that_repeats_the_page() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.nextLink": format!("{base}/Property"),
            "value": records(&["1"])
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let page = client
        .execute_page(&QueryBuilder::new("Property").build().unwrap())
        .await
        .unwrap();
    assert!(matches!(
        client.next_page(&page).await,
        Err(ResoError::Parse(_))
    ));
}

#[tokio::test]
async fn execute_next_link_reads_body_link() {
    // Regression (#19): execute_next_link used to read only headers, so a standard result set
    // stopped after the second page.
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("$skip", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.nextLink": format!("{base}/Property?$skip=2"),
            "value": records(&["2"])
        })))
        .mount(&server)
        .await;

    let response = client_for(&server)
        .execute_next_link(&format!("{base}/Property?$skip=1"))
        .await
        .unwrap();
    assert_eq!(response.record_count, 1);
    assert_eq!(
        response.next_link(),
        Some(format!("{base}/Property?$skip=2").as_str())
    );
}

#[tokio::test]
async fn fetch_page_resumes_from_saved_link() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("$skip", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": records(&["101"])
        })))
        .mount(&server)
        .await;

    let page = client_for(&server)
        .fetch_page(&format!("{base}/Property?$skip=100"))
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert!(!page.has_more());
}
