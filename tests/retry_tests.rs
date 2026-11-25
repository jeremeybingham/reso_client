// Integration tests for retry, backoff, Retry-After and counters (offline, wiremock)

use std::time::{Duration, Instant};

use reso_client::{ClientConfig, QueryBuilder, RateLimit, ResoClient, ResoError, RetryPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast(retries: u32) -> RetryPolicy {
    RetryPolicy::new(retries).with_backoff(Duration::from_millis(1), Duration::from_millis(5))
}

fn client(server: &MockServer, retry: RetryPolicy) -> ResoClient {
    let config = ClientConfig::new(server.uri(), "t").with_retry(retry);
    ResoClient::with_config(config).unwrap()
}

fn query() -> reso_client::Query {
    QueryBuilder::new("Property").build().unwrap()
}

fn ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": [{"ListingKey": "1"}]}))
}

/// A server that answers `failures` times with `status`, then succeeds
async fn flaky(status: u16, failures: u64, extra: Option<(&str, &str)>) -> MockServer {
    let server = MockServer::start().await;
    let mut fail = ResponseTemplate::new(status);
    if let Some((k, v)) = extra {
        fail = fail.insert_header(k, v);
    }
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(fail)
        .up_to_n_times(failures)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ok())
        .with_priority(2)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn retries_each_retryable_status_then_succeeds() {
    for status in [429, 502, 503, 504] {
        let server = flaky(status, 2, None).await;
        let c = client(&server, fast(3));
        let result = c.execute(&query()).await.unwrap();
        assert_eq!(result["value"][0]["ListingKey"], "1");
        let stats = c.stats();
        assert_eq!((stats.requests, stats.retries), (3, 2), "status {status}");
        assert!(stats.bytes > 0);
    }
}

#[tokio::test]
async fn gives_up_after_max_retries_with_the_original_error() {
    let server = flaky(503, 100, None).await;
    let c = client(&server, fast(2));
    match c.execute(&query()).await {
        Err(ResoError::ServerError { status_code, .. }) => assert_eq!(status_code, 503),
        other => panic!("expected ServerError, got {other:?}"),
    }
    let stats = c.stats();
    assert_eq!((stats.requests, stats.retries), (3, 2));
}

#[tokio::test]
async fn default_policy_never_retries() {
    let server = flaky(503, 1, None).await;
    let c = ResoClient::with_config(ClientConfig::new(server.uri(), "t")).unwrap();
    assert!(c.execute(&query()).await.is_err());
    assert_eq!(c.stats().requests, 1);
    assert_eq!(c.stats().retries, 0);
}

#[tokio::test]
async fn other_client_errors_and_500_are_not_retried() {
    for status in [400, 403, 404, 500] {
        let server = flaky(status, 1, None).await;
        let c = client(&server, fast(3));
        assert!(c.execute(&query()).await.is_err(), "status {status}");
        assert_eq!(c.stats().requests, 1, "status {status}");
        assert_eq!(c.stats().retries, 0, "status {status}");
    }
}

#[tokio::test]
async fn connection_errors_are_retried_then_surface() {
    // Port 1 refuses connections
    let config = ClientConfig::new("http://127.0.0.1:1", "t").with_retry(fast(2));
    let c = ResoClient::with_config(config).unwrap();
    assert!(matches!(
        c.execute(&query()).await,
        Err(ResoError::Network(_))
    ));
    let stats = c.stats();
    assert_eq!((stats.requests, stats.retries), (3, 2));
}

#[tokio::test]
async fn retry_after_seconds_is_honoured() {
    let server = flaky(429, 1, Some(("Retry-After", "1"))).await;
    let c = client(&server, fast(2));
    let start = Instant::now();
    c.execute(&query()).await.unwrap();
    assert!(start.elapsed() >= Duration::from_secs(1));
    assert_eq!(c.stats().retries, 1);
}

#[tokio::test]
async fn retry_after_http_date_is_read() {
    // A date in the past means no wait, so this finishes at once instead of the 1 s of backoff
    let server = flaky(
        503,
        1,
        Some(("Retry-After", "Sun, 06 Nov 1994 08:49:37 GMT")),
    )
    .await;
    let policy = RetryPolicy::new(2).with_backoff(Duration::from_secs(5), Duration::from_secs(5));
    let c = client(&server, policy);
    let start = Instant::now();
    c.execute(&query()).await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn retry_after_longer_than_the_cap_is_not_waited_for() {
    let server = flaky(429, 5, Some(("Retry-After", "3600"))).await;
    let c = client(
        &server,
        fast(3).with_max_retry_after(Duration::from_secs(10)),
    );
    let start = Instant::now();
    assert!(matches!(
        c.execute(&query()).await,
        Err(ResoError::RateLimited { .. })
    ));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(c.stats().requests, 1);
}

#[tokio::test]
async fn paging_and_metadata_also_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/$metadata"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/$metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<edmx/>"))
        .with_priority(2)
        .mount(&server)
        .await;
    let c = client(&server, fast(2));
    assert_eq!(c.fetch_metadata().await.unwrap(), "<edmx/>");
    assert_eq!(c.stats().retries, 1);
}

#[tokio::test]
async fn rate_limit_paces_real_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok())
        .mount(&server)
        .await;
    let config =
        ClientConfig::new(server.uri(), "t").with_rate_limit(RateLimit::default().per_second(2));
    let c = ResoClient::with_config(config).unwrap();
    let start = Instant::now();
    for _ in 0..5 {
        c.execute(&query()).await.unwrap();
    }
    // 5 requests at 2 per second cannot finish in under 2 seconds
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert_eq!(c.stats().requests, 5);
}

#[tokio::test]
async fn responses_are_counted_by_status_class() {
    // 429 twice (throttled), then 200 (ok)
    let server = flaky(429, 2, None).await;
    let c = client(&server, fast(3));
    c.execute(&query()).await.unwrap();
    let s = c.stats();
    assert_eq!(
        (s.ok, s.throttled, s.client_errors, s.server_errors),
        (1, 2, 0, 0)
    );

    // 503 once, then ok; and a plain 404 that is not retried
    let server = flaky(503, 1, None).await;
    let c = client(&server, fast(3));
    c.execute(&query()).await.unwrap();
    let before = c.stats();
    assert_eq!((before.ok, before.server_errors), (1, 1));
    let missing = QueryBuilder::new("Nope").build().unwrap();
    assert!(c.execute(&missing).await.is_err());
    let delta = c.stats().since(&before);
    assert_eq!((delta.requests, delta.client_errors, delta.ok), (1, 1, 0));
}
