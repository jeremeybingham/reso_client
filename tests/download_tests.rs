// Integration tests for Downloader: conditional GET, streaming to disk, retries, token scope (offline, wiremock)

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use reso_client::{
    ClientConfig, ClientCredentials, DownloadConfig, DownloadOutcome, ResoClient, ResoError,
    RetryPolicy, SendToken, Validators,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static DIRS: AtomicU32 = AtomicU32::new(0);

/// A fresh, empty directory under the system temp dir
fn temp_dir() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("reso-dl-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn client(base: &str) -> ResoClient {
    ResoClient::with_config(ClientConfig::new(base, "secret-token")).unwrap()
}

fn fast(retries: u32) -> RetryPolicy {
    RetryPolicy::new(retries).with_backoff(Duration::from_millis(1), Duration::from_millis(5))
}

fn photo() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_bytes(b"JPEGDATA".to_vec())
        .insert_header("Content-Type", "image/jpeg")
        .insert_header("ETag", "\"v1\"")
        .insert_header("Last-Modified", "Tue, 24 Nov 2026 10:00:00 GMT")
}

#[tokio::test]
async fn saves_file_with_headers_and_no_part_left() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/1.jpg"))
        .and(header("Authorization", "Bearer secret-token"))
        .respond_with(photo())
        .expect(1)
        .mount(&server)
        .await;
    let dir = temp_dir();
    let dest = dir.join("sub/1");

    let client = client(&format!("{}/odata", server.uri()));
    let d = client.downloader(DownloadConfig::default()).unwrap();
    let url = format!("{}/files/1.jpg", server.uri());
    let outcome = d
        .download_to(&url, &dest, &Validators::default())
        .await
        .unwrap();

    let DownloadOutcome::Saved(saved) = outcome else {
        panic!("expected Saved");
    };
    assert_eq!(saved.bytes, 8);
    assert_eq!(saved.etag.as_deref(), Some("\"v1\""));
    assert_eq!(saved.content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"JPEGDATA");
    assert!(!dir.join("sub/1.part").exists());
    assert_eq!(d.stats().requests, 1);
    assert_eq!(d.stats().bytes, 8);
    assert_eq!(client.stats().requests, 0, "API counters are separate");
}

#[tokio::test]
async fn sends_validators_and_keeps_file_on_304() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/1.jpg"))
        .and(header("If-None-Match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    let dir = temp_dir();
    let dest = dir.join("1");
    std::fs::write(&dest, b"OLD").unwrap();

    let client = client(&server.uri());
    let d = client.downloader(DownloadConfig::default()).unwrap();
    let validators = Validators {
        etag: Some("\"v1\"".into()),
        last_modified: Some("Tue, 24 Nov 2026 10:00:00 GMT".into()),
    };
    let outcome = d
        .download_to(&format!("{}/files/1.jpg", server.uri()), &dest, &validators)
        .await
        .unwrap();
    assert_eq!(outcome, DownloadOutcome::NotModified);
    assert_eq!(std::fs::read(&dest).unwrap(), b"OLD");
    // wiremock's header matcher splits on commas, so check the date from the recorded request
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("if-modified-since").unwrap(),
        "Tue, 24 Nov 2026 10:00:00 GMT"
    );
}

#[tokio::test]
async fn retries_503_then_saves() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/f"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/f"))
        .respond_with(photo())
        .with_priority(2)
        .mount(&server)
        .await;
    let dest = temp_dir().join("f");

    let client = client(&server.uri());
    let d = client
        .downloader(DownloadConfig::default().with_retry(fast(2)))
        .unwrap();
    let outcome = d
        .download_to(
            &format!("{}/f", server.uri()),
            &dest,
            &Validators::default(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, DownloadOutcome::Saved(_)));
    assert_eq!(d.stats().retries, 1);
    assert_eq!(d.stats().requests, 2);
}

#[tokio::test]
async fn retryable_status_after_retries_is_returned() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let dest = temp_dir().join("f");
    let client = client(&server.uri());
    let d = client
        .downloader(DownloadConfig::default().with_retry(fast(1)))
        .unwrap();
    let err = d
        .download_to(
            &format!("{}/f", server.uri()),
            &dest,
            &Validators::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ResoError::ServerError {
            status_code: 503,
            ..
        }
    ));
    assert_eq!(d.stats().requests, 2);
    assert!(!dest.exists());
}

#[tokio::test]
async fn forbidden_is_not_retried_and_leaves_old_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("<Error>Request has expired</Error>"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = temp_dir();
    let dest = dir.join("f");
    std::fs::write(&dest, b"OLD").unwrap();

    let client = client(&server.uri());
    let d = client
        .downloader(DownloadConfig::default().with_retry(fast(3)))
        .unwrap();
    let err = d
        .download_to(
            &format!("{}/f", server.uri()),
            &dest,
            &Validators::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ResoError::Forbidden { .. }));
    assert_eq!(std::fs::read(&dest).unwrap(), b"OLD");
    assert!(!dir.join("f.part").exists());
}

#[tokio::test]
async fn token_is_not_sent_to_another_host_by_default() {
    let api = MockServer::start().await;
    let cdn = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(photo())
        .expect(1)
        .mount(&cdn)
        .await;
    let dest = temp_dir().join("f");

    let client = client(&api.uri());
    let d = client.downloader(DownloadConfig::default()).unwrap();
    d.download_to(&format!("{}/f", cdn.uri()), &dest, &Validators::default())
        .await
        .unwrap();
    let requests = cdn.received_requests().await.unwrap();
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn token_is_sent_everywhere_with_always() {
    let api = MockServer::start().await;
    let cdn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("Authorization", "Bearer secret-token"))
        .respond_with(photo())
        .expect(1)
        .mount(&cdn)
        .await;
    let dest = temp_dir().join("f");

    let client = client(&api.uri());
    let d = client
        .downloader(DownloadConfig::default().with_send_token(SendToken::Always))
        .unwrap();
    d.download_to(&format!("{}/f", cdn.uri()), &dest, &Validators::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn rejects_non_http_url() {
    let client = client("https://api.example.com");
    let d = client.downloader(DownloadConfig::default()).unwrap();
    let dest = temp_dir().join("f");
    for url in ["file:///etc/passwd", "relative/path.jpg"] {
        let err = d
            .download_to(url, &dest, &Validators::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ResoError::InvalidQuery(_)), "{url}");
    }
}

#[tokio::test]
async fn refused_connection_is_a_network_error() {
    let client = client("https://api.example.com");
    let d = client
        .downloader(DownloadConfig::default().with_retry(fast(1)))
        .unwrap();
    let dest = temp_dir().join("f");
    let err = d
        .download_to("http://127.0.0.1:1/f", &dest, &Validators::default())
        .await
        .unwrap_err();
    assert!(matches!(err, ResoError::Network(_)));
    assert_eq!(d.stats().retries, 1);
}

#[tokio::test]
async fn download_renews_an_oauth_token_after_401() {
    let server = MockServer::start().await;
    let issued = AtomicU32::new(0);
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(move |_: &wiremock::Request| {
            let n = issued.fetch_add(1, Ordering::SeqCst) + 1;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": format!("token-{n}"),
                "token_type": "Bearer",
                "expires_in": 3600,
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/f"))
        .and(header("Authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/f"))
        .and(header("Authorization", "Bearer token-2"))
        .respond_with(photo())
        .expect(1)
        .mount(&server)
        .await;
    let credentials = ClientCredentials::new(format!("{}/oauth/token", server.uri()), "id", "s");
    let client =
        ResoClient::with_config(ClientConfig::with_auth(server.uri(), credentials)).unwrap();
    let d = client.downloader(DownloadConfig::default()).unwrap();
    let dest = temp_dir().join("f");
    let outcome = d
        .download_to(
            &format!("{}/f", server.uri()),
            &dest,
            &Validators::default(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, DownloadOutcome::Saved(_)));
    assert_eq!(std::fs::read(&dest).unwrap(), b"JPEGDATA");
}

#[tokio::test]
async fn token_scope_decisions() {
    let client = client("https://api.example.com/odata");
    let same = client.downloader(DownloadConfig::default()).unwrap();
    assert!(same.sends_token_to("https://api.example.com/media/1.jpg"));
    assert!(!same.sends_token_to("https://cdn.example.com/1.jpg"));
    assert!(!same.sends_token_to("not a url"));
    let never = client
        .downloader(DownloadConfig::default().with_send_token(SendToken::Never))
        .unwrap();
    assert!(!never.sends_token_to("https://api.example.com/media/1.jpg"));
}
