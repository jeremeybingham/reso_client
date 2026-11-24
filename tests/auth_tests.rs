// Integration tests for OAuth2 client credentials (#21): token fetch, caching, renewal, 401 retry,
// credential transports and secret redaction.
//
// All credentials and data are invented. The token response shape
// (`{access_token, token_type, expires_in}`) mirrors RESO's reference server mock (#35); it does
// not require auth for reads, so 401 handling is only tested here.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reso_client::{
    ClientConfig, ClientCredentials, CredentialTransport, QueryBuilder, ResoClient, ResoError,
};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SECRET: &str = "the-secret";

fn credentials(server: &MockServer) -> ClientCredentials {
    ClientCredentials::new(format!("{}/oauth/token", server.uri()), "the-id", SECRET)
}

fn client(server: &MockServer, credentials: ClientCredentials) -> ResoClient {
    ResoClient::with_config(ClientConfig::with_auth(server.uri(), credentials)).unwrap()
}

fn token_response(token: &str, expires_in: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": token,
        "token_type": "Bearer",
        "expires_in": expires_in,
    }))
}

/// Answers the token endpoint with token-1, token-2, ... in turn.
struct CountingTokens(Mutex<u32>);

impl Respond for CountingTokens {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        token_response(&format!("token-{}", n), serde_json::json!(3600))
    }
}

fn counting_tokens() -> CountingTokens {
    CountingTokens(Mutex::new(0))
}

async fn mount_property_for(server: &MockServer, token: &str) {
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header(
            "authorization",
            format!("Bearer {}", token).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "value": [] })))
        .mount(server)
        .await;
}

async fn run_query(client: &ResoClient) -> reso_client::Result<serde_json::Value> {
    client
        .execute(&QueryBuilder::new("Property").build().unwrap())
        .await
}

#[tokio::test]
async fn fetches_token_with_body_credentials_and_uses_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains("client_id=the-id"))
        .and(body_string_contains("client_secret=the-secret"))
        .and(body_string_contains("scope=api"))
        .respond_with(token_response("token-1", serde_json::json!(3600)))
        .expect(1)
        .mount(&server)
        .await;
    mount_property_for(&server, "token-1").await;

    let client = client(&server, credentials(&server).with_scope("api"));
    run_query(&client).await.unwrap();
}

#[tokio::test]
async fn basic_header_transport() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        // base64("the-id:the-secret")
        .and(header("authorization", "Basic dGhlLWlkOnRoZS1zZWNyZXQ="))
        .respond_with(token_response("token-1", serde_json::json!(3600)))
        .expect(1)
        .mount(&server)
        .await;
    mount_property_for(&server, "token-1").await;

    let creds = credentials(&server).with_transport(CredentialTransport::BasicHeader);
    run_query(&client(&server, creds)).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&requests[0].body).to_string();
    assert!(!body.contains(SECRET), "secret must not be in the body");
}

#[tokio::test]
async fn query_transport() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(query_param("client_id", "the-id"))
        .and(query_param("client_secret", SECRET))
        .respond_with(token_response("token-1", serde_json::json!(3600)))
        .expect(1)
        .mount(&server)
        .await;
    mount_property_for(&server, "token-1").await;

    let creds = credentials(&server).with_transport(CredentialTransport::Query);
    run_query(&client(&server, creds)).await.unwrap();
}

#[tokio::test]
async fn token_is_reused_until_near_expiry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(counting_tokens())
        .expect(1)
        .mount(&server)
        .await;
    mount_property_for(&server, "token-1").await;

    let client = client(&server, credentials(&server));
    for _ in 0..3 {
        run_query(&client).await.unwrap();
    }
}

#[tokio::test]
async fn token_is_renewed_after_ninety_percent_of_its_lifetime() {
    let server = MockServer::start().await;
    // expires_in 1 second (sent as a string, as some servers do): renewed after 0.9 s.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(token_response("token-1", serde_json::json!("1")))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(token_response("token-2", serde_json::json!(3600)))
        .expect(1)
        .mount(&server)
        .await;
    mount_property_for(&server, "token-1").await;
    mount_property_for(&server, "token-2").await;

    let client = client(&server, credentials(&server));
    run_query(&client).await.unwrap();
    tokio::time::sleep(Duration::from_millis(950)).await;
    run_query(&client).await.unwrap();
    run_query(&client).await.unwrap();

    let bearer_tokens: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/Property")
        .map(|r| r.headers["authorization"].to_str().unwrap().to_string())
        .collect();
    assert_eq!(
        bearer_tokens,
        ["Bearer token-1", "Bearer token-2", "Bearer token-2"]
    );
}

#[tokio::test]
async fn concurrent_callers_trigger_one_fetch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(counting_tokens().with_delay())
        .expect(1)
        .mount(&server)
        .await;
    mount_property_for(&server, "token-1").await;

    let client = Arc::new(client(&server, credentials(&server)));
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let client = Arc::clone(&client);
            tokio::spawn(async move { run_query(&client).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}

/// Wraps CountingTokens with a delay so concurrent callers overlap the fetch.
struct Delayed(CountingTokens);

impl Respond for Delayed {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0
            .respond(request)
            .set_delay(Duration::from_millis(200))
    }
}

impl CountingTokens {
    fn with_delay(self) -> Delayed {
        Delayed(self)
    }
}

#[tokio::test]
async fn a_401_renews_the_token_and_retries_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(counting_tokens())
        .expect(2)
        .mount(&server)
        .await;
    // The server has revoked token-1 early.
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    mount_property_for(&server, "token-2").await;

    let client = client(&server, credentials(&server));
    run_query(&client).await.unwrap();
    // The renewed token is cached for later requests.
    run_query(&client).await.unwrap();
}

#[tokio::test]
async fn a_second_401_returns_the_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(counting_tokens())
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": { "code": "Unauthorized", "message": "Account disabled" }
        })))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server, credentials(&server));
    match run_query(&client).await {
        Err(ResoError::Unauthorized { message, .. }) => {
            assert!(message.contains("Account disabled"))
        }
        other => panic!("expected Unauthorized, got {:?}", other),
    }
}

#[tokio::test]
async fn static_bearer_token_401_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("authorization", "Bearer static-token"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let client = ResoClient::with_config(ClientConfig::new(server.uri(), "static-token")).unwrap();
    assert!(matches!(
        run_query(&client).await,
        Err(ResoError::Unauthorized { .. })
    ));
}

#[tokio::test]
async fn token_endpoint_error_reports_oauth_fields_not_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_client",
            "error_description": "Unknown client",
            "echo": SECRET,
        })))
        .mount(&server)
        .await;

    let client = client(&server, credentials(&server));
    match run_query(&client).await {
        Err(err @ ResoError::TokenRequest(_)) => {
            let text = err.to_string();
            assert!(text.contains("400"));
            assert!(text.contains("invalid_client"));
            assert!(text.contains("Unknown client"));
            assert!(!text.contains(SECRET));
        }
        other => panic!("expected TokenRequest, got {:?}", other),
    }
}

#[tokio::test]
async fn token_response_without_access_token_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token_type": "Bearer"
        })))
        .mount(&server)
        .await;

    let client = client(&server, credentials(&server));
    assert!(matches!(
        run_query(&client).await,
        Err(ResoError::TokenRequest(_))
    ));
}

#[tokio::test]
async fn network_error_on_query_transport_hides_the_secret() {
    // Port 1 on loopback has no listener, so the connection is refused. (A dropped wiremock
    // server is no good here: wiremock pools servers and hands the port to another test.)
    let creds = ClientCredentials::new("http://127.0.0.1:1/oauth/token", "the-id", SECRET)
        .with_transport(CredentialTransport::Query);
    let client = ResoClient::with_config(ClientConfig::with_auth(
        "https://api.example.invalid",
        creds,
    ))
    .unwrap();

    match run_query(&client).await {
        Err(err @ ResoError::TokenRequest(_)) => {
            let text = format!("{} {:?}", err, err);
            assert!(!text.contains(SECRET), "secret leaked: {}", text);
        }
        other => panic!("expected TokenRequest, got {:?}", other),
    }
}
