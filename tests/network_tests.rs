// Network options (#91): User-Agent, proxy (explicit, with credentials, bypassed, from the
// environment), extra CA certificates and connection-pool settings.
//
// Every test is `#[serial]`: one sets `HTTP_PROXY`, which every client built at the same time
// would pick up.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reso_client::{
    ClientConfig, DownloadConfig, NetworkOptions, QueryBuilder, ResoClient, ResoError, Validators,
};
use serial_test::serial;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(base: String, network: NetworkOptions) -> ResoClient {
    ResoClient::with_config(ClientConfig::new(base, "t").with_network(network)).unwrap()
}

async fn property_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": []})))
        .mount(&server)
        .await;
    server
}

async fn get_property(client: &ResoClient) -> reso_client::Result<serde_json::Value> {
    client
        .execute(&QueryBuilder::new("Property").build().unwrap())
        .await
}

// --- User-Agent ---

#[tokio::test]
#[serial]
async fn default_user_agent_names_the_crate_and_version() {
    let server = MockServer::start().await;
    let agent = format!("reso-client/{}", env!("CARGO_PKG_VERSION"));
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("User-Agent", agent.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": []})))
        .expect(1)
        .mount(&server)
        .await;
    get_property(&client(server.uri(), NetworkOptions::default()))
        .await
        .unwrap();
}

#[tokio::test]
#[serial]
async fn custom_user_agent_goes_with_api_token_and_file_requests() {
    let server = MockServer::start().await;
    let agent = "acme-ingest/1.0 (data@acme.example)";
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("User-Agent", agent))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": []})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(header("User-Agent", agent))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"access_token": "a", "token_type": "Bearer", "expires_in": 3600}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/photo/1.jpg"))
        .and(header("User-Agent", agent))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"jpeg".to_vec(), "image/jpeg"))
        .expect(1)
        .mount(&server)
        .await;

    let credentials =
        reso_client::ClientCredentials::new(format!("{}/token", server.uri()), "id", "secret");
    let config = ClientConfig::with_auth(server.uri(), credentials)
        .with_network(NetworkOptions::default().with_user_agent(agent));
    let client = ResoClient::with_config(config).unwrap();
    get_property(&client).await.unwrap();

    let dir = std::env::temp_dir().join(format!("reso-net-ua-{}", std::process::id()));
    let downloader = client.downloader(DownloadConfig::default()).unwrap();
    downloader
        .download_to(
            &format!("{}/photo/1.jpg", server.uri()),
            &dir.join("1.jpg"),
            &Validators::default(),
        )
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

// --- Proxy ---

/// What the test proxy saw: one request line and its headers per proxied request
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<ProxiedRequest>>>);

/// A request line and its header lines
type ProxiedRequest = (String, Vec<String>);

impl Seen {
    fn requests(&self) -> Vec<ProxiedRequest> {
        self.0.lock().unwrap().clone()
    }
}

/// A small forward proxy for plain HTTP: records each request, rewrites the absolute URL to a
/// path, forwards it with `Connection: close` and copies the answer back
fn test_proxy() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Seen::default();
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let record = record.clone();
            std::thread::spawn(move || proxy_one(stream, record));
        }
    });
    (format!("http://{addr}"), seen)
}

fn proxy_one(mut client: TcpStream, seen: Seen) {
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let request_line = line.trim_end().to_owned();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        headers.push(h.trim_end().to_owned());
    }
    seen.0
        .lock()
        .unwrap()
        .push((request_line.clone(), headers.clone()));

    // "GET http://host:port/path HTTP/1.1" -> connect to host:port, send "GET /path HTTP/1.1"
    let mut parts = request_line.splitn(3, ' ');
    let (verb, url, version) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let mut upstream = TcpStream::connect(host).unwrap();
    let mut head = format!("{verb} {path} {version}\r\n");
    for h in &headers {
        let name = h.split(':').next().unwrap_or("").to_ascii_lowercase();
        if name != "connection" && !name.starts_with("proxy-") {
            head.push_str(h);
            head.push_str("\r\n");
        }
    }
    head.push_str("Connection: close\r\n\r\n");
    upstream.write_all(head.as_bytes()).unwrap();
    let mut answer = Vec::new();
    let _ = upstream.read_to_end(&mut answer);
    let _ = client.write_all(&answer);
}

#[tokio::test]
#[serial]
async fn requests_go_through_the_proxy() {
    let server = property_server().await;
    let (proxy, seen) = test_proxy();
    let client = client(server.uri(), NetworkOptions::default().with_proxy(&proxy));

    get_property(&client).await.unwrap();

    let requests = seen.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].0,
        format!("GET {}/Property HTTP/1.1", server.uri())
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn proxy_credentials_become_basic_proxy_authorization() {
    let server = property_server().await;
    let (proxy, seen) = test_proxy();
    let with_user = proxy.replace("http://", "http://alice:s3cret@");
    let client = client(
        server.uri(),
        NetworkOptions::default().with_proxy(with_user),
    );

    get_property(&client).await.unwrap();

    // base64("alice:s3cret")
    let headers = &seen.requests()[0].1;
    assert!(
        headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("proxy-authorization: Basic YWxpY2U6czNjcmV0")),
        "{headers:?}"
    );
    // ...and the credentials never reach the server
    let received = server.received_requests().await.unwrap();
    assert!(received[0].headers.get("proxy-authorization").is_none());
}

#[tokio::test]
#[serial]
async fn no_proxy_hosts_bypass_the_proxy() {
    let server = property_server().await;
    let (proxy, seen) = test_proxy();
    let client = client(
        server.uri(),
        NetworkOptions::default()
            .with_proxy(&proxy)
            .with_no_proxy("localhost,127.0.0.1"),
    );

    get_property(&client).await.unwrap();

    assert!(seen.requests().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// Sets a variable for the life of the guard; tests that use it are `#[serial]`
struct EnvVar(&'static str);

impl EnvVar {
    fn set(name: &'static str, value: &str) -> Self {
        std::env::set_var(name, value);
        Self(name)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

#[tokio::test]
#[serial]
async fn http_proxy_variable_is_honoured_unless_turned_off() {
    let server = property_server().await;
    let (proxy, seen) = test_proxy();
    let _var = EnvVar::set("HTTP_PROXY", &proxy);
    // A NO_PROXY from the machine running the tests would hide the variable
    let _no = EnvVar::set("NO_PROXY", "");

    get_property(&client(server.uri(), NetworkOptions::default()))
        .await
        .unwrap();
    assert_eq!(seen.requests().len(), 1, "HTTP_PROXY was not used");

    get_property(&client(
        server.uri(),
        NetworkOptions::default().without_system_proxy(),
    ))
    .await
    .unwrap();
    assert_eq!(
        seen.requests().len(),
        1,
        "without_system_proxy still used HTTP_PROXY"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

// --- Plain and TLS server that counts connections ---

const BODY: &str = r#"{"value":[]}"#;

/// Answer every request on `stream` with an empty `value` array, keeping the connection open
fn serve_keep_alive(stream: impl Read + Write) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        loop {
            let mut h = String::new();
            match reader.read_line(&mut h) {
                Ok(0) | Err(_) => return,
                Ok(_) if h == "\r\n" => break,
                Ok(_) => line.push_str(&h),
            }
        }
        let answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{BODY}",
            BODY.len()
        );
        let stream = reader.get_mut();
        if stream.write_all(answer.as_bytes()).is_err() || stream.flush().is_err() {
            return;
        }
    }
}

/// A keep-alive HTTP server; returns its URL and the number of connections accepted so far
fn counting_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || serve_keep_alive(stream));
        }
    });
    (format!("http://{addr}"), count)
}

#[tokio::test]
#[serial]
async fn idle_connections_are_reused_by_default() {
    let (url, connections) = counting_server();
    let client = client(url, NetworkOptions::default());
    for _ in 0..3 {
        get_property(&client).await.unwrap();
    }
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial]
async fn pool_of_zero_idle_connections_opens_one_per_request() {
    let (url, connections) = counting_server();
    let client = client(
        url,
        NetworkOptions::default().with_pool_max_idle_per_host(0),
    );
    for _ in 0..3 {
        get_property(&client).await.unwrap();
    }
    assert_eq!(connections.load(Ordering::SeqCst), 3);
}

#[tokio::test]
#[serial]
async fn idle_timeout_drops_connections_left_idle_longer() {
    let (url, connections) = counting_server();
    let client = client(
        url,
        NetworkOptions::default().with_pool_idle_timeout(Duration::from_millis(100)),
    );
    get_property(&client).await.unwrap();
    get_property(&client).await.unwrap();
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(400)).await;
    get_property(&client).await.unwrap();
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

const CA_PEM: &str = include_str!("fixtures/tls/ca.pem");

/// An HTTPS server whose certificate is signed by the test CA in `tests/fixtures/tls`
fn tls_server() -> String {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certs = CertificateDer::pem_slice_iter(include_bytes!("fixtures/tls/server.pem"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("fixtures/tls/server.key")).unwrap();
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let config = config.clone();
            std::thread::spawn(move || {
                let Ok(conn) = rustls::ServerConnection::new(config) else {
                    return;
                };
                serve_keep_alive(rustls::StreamOwned::new(conn, stream));
            });
        }
    });
    format!("https://localhost:{}", addr.port())
}

#[tokio::test]
#[serial]
async fn private_ca_is_refused_without_the_extra_certificate() {
    let url = tls_server();
    let error = get_property(&client(url, NetworkOptions::default()))
        .await
        .unwrap_err();
    assert!(matches!(error, ResoError::Network(_)), "{error:?}");
}

#[tokio::test]
#[serial]
async fn extra_ca_certificate_is_trusted() {
    let url = tls_server();
    let client = client(url, NetworkOptions::default().with_ca_pem(CA_PEM));
    let value = get_property(&client).await.unwrap();
    assert_eq!(value["value"], serde_json::json!([]));
}

#[tokio::test]
#[serial]
async fn extra_ca_from_a_file_is_trusted() {
    let url = tls_server();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls/ca.pem");
    let network = NetworkOptions::default().with_ca_file(path).unwrap();
    get_property(&client(url, network)).await.unwrap();
}

#[tokio::test]
#[serial]
async fn unreachable_proxy_error_does_not_show_its_credentials() {
    let server = property_server().await;
    let network = NetworkOptions::default().with_proxy("http://alice:s3cret@127.0.0.1:1");
    let error = get_property(&client(server.uri(), network))
        .await
        .unwrap_err();
    let text = format!("{error} {error:?}");
    assert!(!text.contains("s3cret"), "{text}");
}
