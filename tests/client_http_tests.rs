// Integration tests for ResoClient HTTP operations

use reso_client::{ClientConfig, QueryBuilder, ReplicationQueryBuilder, ResoClient, ResoError};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_execute_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("Authorization", "Bearer test-token"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.context": "https://api.example.com/$metadata#Property",
            "value": [
                {"ListingKey": "12345", "City": "Austin", "ListPrice": 500000},
                {"ListingKey": "67890", "City": "Dallas", "ListPrice": 750000}
            ]
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(
        response["@odata.context"],
        "https://api.example.com/$metadata#Property"
    );
    assert_eq!(response["value"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_execute_with_query_params() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("Authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [{"ListingKey": "12345"}]
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .top(10)
        .build()
        .unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_execute_count_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("$count", "true"))
        .and(query_param("$top", "0"))
        .and(header("Authorization", "Bearer test-token"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.count": 42,
            "value": []
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").count().build().unwrap();

    assert_eq!(client.execute_count(&query).await.unwrap(), 42);
}

#[tokio::test]
async fn test_execute_count_keeps_filter() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(query_param("$filter", "City eq 'Austin'"))
        .and(query_param("$count", "true"))
        .and(query_param("$top", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.count": 7,
            "value": []
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .count()
        .build()
        .unwrap();

    assert_eq!(client.execute_count(&query).await.unwrap(), 7);
}

#[tokio::test]
async fn test_execute_count_missing_odata_count() {
    // A server that ignores $count=true returns records and no @odata.count.
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "value": [] })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").count().build().unwrap();

    match client.execute_count(&query).await {
        Err(ResoError::Parse(msg)) => assert!(msg.contains("Failed to parse count")),
        other => panic!("Expected Parse error, got {other:?}"),
    }
}

#[tokio::test]
async fn test_fetch_metadata_success() {
    let mock_server = MockServer::start().await;

    let metadata_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0">
    <edmx:DataServices>
        <Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="RESO">
        </Schema>
    </edmx:DataServices>
</edmx:Edmx>"#;

    Mock::given(method("GET"))
        .and(path("/$metadata"))
        .and(header("Authorization", "Bearer test-token"))
        .and(header("Accept", "application/xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(metadata_xml))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();

    let result = client.fetch_metadata().await;

    assert!(result.is_ok());
    let metadata = result.unwrap();
    assert!(metadata.contains("edmx:Edmx"));
    assert!(metadata.contains("RESO"));
}

#[tokio::test]
async fn test_execute_replication_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property/replication"))
        .and(header("Authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "value": [
                        {"ListingKey": "1"},
                        {"ListingKey": "2"}
                    ]
                }))
                .insert_header(
                    "next",
                    "https://api.example.com/Property/replication?skip=2",
                ),
        )
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = ReplicationQueryBuilder::new("Property").build().unwrap();

    let result = client.execute_replication(&query).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.record_count, 2);
    assert!(response.has_more());
    assert_eq!(
        response.next_link(),
        Some("https://api.example.com/Property/replication?skip=2")
    );
}

#[tokio::test]
async fn test_execute_replication_no_next_link() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property/replication"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [{"ListingKey": "1"}]
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = ReplicationQueryBuilder::new("Property").build().unwrap();

    let result = client.execute_replication(&query).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.record_count, 1);
    assert!(!response.has_more());
    assert_eq!(response.next_link(), None);
}

#[tokio::test]
async fn test_execute_next_link_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property/replication"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [{"ListingKey": "3"}, {"ListingKey": "4"}]
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();

    let next_link = format!("{}/Property/replication?skip=2", mock_server.uri());
    let result = client.execute_next_link(&next_link).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.record_count, 2);
}

#[tokio::test]
async fn test_execute_by_key_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property('12345')"))
        .and(header("Authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ListingKey": "12345",
            "City": "Austin",
            "ListPrice": 500000
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::by_key("Property", "12345").build().unwrap();

    let result = client.execute_by_key(&query).await;

    assert!(result.is_ok());
    let record = result.unwrap();
    assert_eq!(record["ListingKey"], "12345");
    assert_eq!(record["City"], "Austin");
}

#[tokio::test]
async fn test_execute_401_unauthorized() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {
                "code": "Unauthorized",
                "message": "Invalid authentication token"
            }
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "invalid-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::Unauthorized {
            message,
            status_code,
        }) => {
            assert_eq!(status_code, 401);
            assert!(message.contains("Invalid authentication token"));
        }
        _ => panic!("Expected Unauthorized error"),
    }
}

#[tokio::test]
async fn test_execute_403_forbidden() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {
                "code": "Forbidden",
                "message": "Access to this resource is forbidden"
            }
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::Forbidden {
            message,
            status_code,
        }) => {
            assert_eq!(status_code, 403);
            assert!(message.contains("forbidden"));
        }
        _ => panic!("Expected Forbidden error"),
    }
}

#[tokio::test]
async fn test_execute_404_not_found() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/NonExistentResource"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Resource not found"))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("NonExistentResource").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::NotFound {
            message,
            status_code,
        }) => {
            assert_eq!(status_code, 404);
            assert!(message.contains("not found"));
        }
        _ => panic!("Expected NotFound error"),
    }
}

#[tokio::test]
async fn test_execute_429_rate_limited() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "error": {
                "code": "TooManyRequests",
                "message": "Rate limit exceeded"
            }
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::RateLimited {
            message,
            status_code,
        }) => {
            assert_eq!(status_code, 429);
            assert!(message.contains("Rate limit exceeded"));
        }
        _ => panic!("Expected RateLimited error"),
    }
}

#[tokio::test]
async fn test_execute_500_server_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal server error"))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::ServerError {
            message,
            status_code,
        }) => {
            assert_eq!(status_code, 500);
            assert!(message.contains("Internal server error"));
        }
        _ => panic!("Expected ServerError error"),
    }
}

#[tokio::test]
async fn test_execute_503_service_unavailable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Service temporarily unavailable"))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::ServerError {
            message,
            status_code,
        }) => {
            assert_eq!(status_code, 503);
            assert!(message.contains("unavailable"));
        }
        _ => panic!("Expected ServerError error"),
    }
}

#[tokio::test]
async fn test_execute_400_bad_request() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "code": "InvalidFilter",
                "message": "The filter expression is invalid"
            }
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property")
        .filter("invalid filter")
        .build()
        .unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::ODataError {
            message,
            status_code,
        }) => {
            assert_eq!(status_code, 400);
            assert!(message.contains("filter expression is invalid"));
        }
        _ => panic!("Expected ODataError error"),
    }
}

#[tokio::test]
async fn test_execute_invalid_json_response() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not valid json"))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_err());
    match result {
        Err(ResoError::Parse(msg)) => {
            assert!(msg.contains("Failed to parse JSON"));
        }
        _ => panic!("Expected Parse error"),
    }
}

#[tokio::test]
async fn test_execute_with_dataset_id() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/actris_ref/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [{"ListingKey": "12345"}]
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token").with_dataset_id("actris_ref");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_execute_with_count_in_response() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.count": 100,
            "value": [{"ListingKey": "12345"}]
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").with_count().build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response["@odata.count"], 100);
    assert_eq!(response["value"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_replication_with_link_header() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property/replication"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "value": [{"ListingKey": "1"}]
                }))
                .insert_header("link", "https://api.example.com/next"),
        )
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = ReplicationQueryBuilder::new("Property").build().unwrap();

    let result = client.execute_replication(&query).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert!(response.has_more());
    assert_eq!(response.next_link(), Some("https://api.example.com/next"));
}

#[tokio::test]
async fn test_execute_empty_response_array() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": []
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response["value"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_authorization_header_included() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("Authorization", "Bearer my-secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": []
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "my-secret-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let result = client.execute(&query).await;

    assert!(result.is_ok());
}

// The tests below mirror shapes seen on RESO's reference server (#35), with invented data.

#[tokio::test]
async fn test_execute_body_next_link_is_absolute_with_skip() {
    // The standard OData form: an absolute @odata.nextLink in the body, paged by $skip.
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(wiremock::matchers::query_param("$skip", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [{"ListingKey": "3"}]
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.nextLink": format!("{}/Property?$orderby=ListingKey&$skip=2", mock_server.uri()),
            "value": [{"ListingKey": "1"}, {"ListingKey": "2"}]
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property").build().unwrap();

    let first = client.execute(&query).await.unwrap();
    let link = first["@odata.nextLink"].as_str().unwrap().to_string();
    assert!(link.starts_with("http"));

    let second = client.execute_next_link(&link).await.unwrap();
    assert_eq!(second.record_count, 1);
    assert!(!second.has_more());
}

#[tokio::test]
async fn test_execute_count_only_with_top_zero() {
    // Some servers have no /$count route; $count=true&$top=0 is the portable count-only query.
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(wiremock::matchers::query_param("$count", "true"))
        .and(wiremock::matchers::query_param("$top", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@odata.count": 42,
            "value": []
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property")
        .with_count()
        .top(0)
        .build()
        .unwrap();

    let response = client.execute(&query).await.unwrap();
    assert_eq!(response["@odata.count"], 42);
    assert!(response["value"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_execute_400_nested_odata_error() {
    // OData error body with a numeric-string code and a details list.
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "code": "40000",
                "message": "Invalid query",
                "target": "Query",
                "details": [{"code": "30212", "target": "$filter", "message": "Unknown field: Foo"}]
            }
        })))
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Property")
        .filter("Foo eq 1")
        .build()
        .unwrap();

    match client.execute(&query).await {
        Err(ResoError::ODataError { status_code, .. }) => assert_eq!(status_code, 400),
        other => panic!("Expected ODataError, got {:?}", other),
    }
}

#[tokio::test]
async fn test_execute_404_html_body() {
    // An unknown resource can come back as an HTML error page, not OData JSON.
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/Nope"))
        .respond_with(
            ResponseTemplate::new(404)
                .insert_header("Content-Type", "text/html; charset=utf-8")
                .set_body_string(
                    "<!DOCTYPE html><html><body><pre>Cannot GET /Nope</pre></body></html>",
                ),
        )
        .mount(&mock_server)
        .await;

    let config = ClientConfig::new(mock_server.uri(), "test-token");
    let client = ResoClient::with_config(config).unwrap();
    let query = QueryBuilder::new("Nope").build().unwrap();

    assert!(client.execute(&query).await.is_err());
}

#[tokio::test]
async fn fetch_metadata_model_parses_served_xml() {
    let server = MockServer::start().await;
    let xml = r#"<edmx:Edmx xmlns:edmx="urn:x"><edmx:DataServices><Schema Namespace="n">
        <EntityType Name="Property"><Key><PropertyRef Name="K"/></Key><Property Name="K" Type="Edm.String" Nullable="false"/></EntityType>
        </Schema></edmx:DataServices></edmx:Edmx>"#;
    Mock::given(method("GET"))
        .and(path("/$metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_string(xml))
        .mount(&server)
        .await;
    let client = ResoClient::with_config(ClientConfig::new(server.uri(), "t")).unwrap();
    let model = client.fetch_metadata_model().await.unwrap();
    assert_eq!(model.resource("Property").unwrap().key, vec!["K"]);
}
