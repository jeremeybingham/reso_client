//! Secrets read from files are re-read and a rotated secret is used without a restart (#92); no
//! secret appears in the logs, the output or the error text
//!
//! This test has its own binary and installs a global subscriber: server tasks run on other
//! threads, which a scoped subscriber would not reach.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use reso_ingest::cli::run;
use reso_ingest::{log_subscriber, ClientHandle, Config, LogFormat};
use serde_json::json;
use tracing_subscriber::EnvFilter;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const CLIENT_ID: &str = "rotation-client-id";
const OLD_SECRET: &str = "old-secret-4b1d";
const NEW_SECRET: &str = "new-secret-9c2e";

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every log line of this binary, at trace level
fn logs() -> &'static Buffer {
    static LOGS: OnceLock<Buffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = log_subscriber(LogFormat::Json, EnvFilter::new("trace"), move || {
            writer.clone()
        });
        tracing::subscriber::set_global_default(subscriber).unwrap();
        buffer
    })
}

fn assert_no_secrets(text: &str) {
    for secret in [OLD_SECRET, NEW_SECRET] {
        assert!(!text.contains(secret), "a secret leaked:\n{text}");
    }
}

fn assert_no_secrets_logged() {
    let text = String::from_utf8(logs().0.lock().unwrap().clone()).unwrap();
    assert!(!text.is_empty(), "nothing was logged");
    assert_no_secrets(&text);
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("reso-ingest-rotation-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config_text(base: &str, id_file: &Path, secret_file: &Path) -> String {
    format!(
        "[[server]]\nname = \"mls\"\nbase_url = \"{base}\"\n[server.limits]\nmax_retries = 0\n\
         [server.auth]\ntype = \"client_credentials\"\ntoken_url = \"{base}/token\"\n\
         client_id_file = \"{}\"\nclient_secret_file = \"{}\"\n",
        id_file.display(),
        secret_file.display()
    )
}

/// Which client secret a token request carried
fn sent_secret(req: &Request) -> Option<&'static str> {
    let body = String::from_utf8_lossy(&req.body);
    [OLD_SECRET, NEW_SECRET]
        .into_iter()
        .find(|s| body.contains(&format!("client_secret={s}")))
}

/// `$metadata` for any token; `Property` (two records, one page) only for `Bearer token-new`
async fn mount_api(server: &MockServer) {
    Mock::given(method("GET"))
        .respond_with(|req: &Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            let auth = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok());
            if auth != Some("Bearer token-new") {
                return ResponseTemplate::new(401).set_body_json(json!({"error": "expired"}));
            }
            let first = !req.url.query().unwrap_or("").contains("filter");
            let value = if first {
                vec![
                    json!({"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"}),
                    json!({"ListingKey": "K2", "ModificationTimestamp": "2026-01-01T00:00:02Z"}),
                ]
            } else {
                vec![]
            };
            ResponseTemplate::new(200).set_body_json(json!({ "value": value }))
        })
        .mount(server)
        .await;
}

fn token(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": name,
        "token_type": "Bearer",
        "expires_in": 3600,
    }))
}

#[tokio::test]
async fn the_next_token_request_uses_a_rotated_secret_file() {
    logs();
    let dir = TempDir::new("handle");
    let (id_file, secret_file) = (dir.path("id"), dir.path("secret"));
    std::fs::write(&id_file, format!("{CLIENT_ID}\n")).unwrap();
    std::fs::write(&secret_file, format!("{OLD_SECRET}\n")).unwrap();

    let server = MockServer::start().await;
    let seen: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let record = Arc::clone(&seen);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(move |req: &Request| {
            let secret = sent_secret(req).unwrap_or("none");
            record.lock().unwrap().push(secret);
            assert!(String::from_utf8_lossy(&req.body).contains(CLIENT_ID));
            token(&format!("token-{secret}"))
        })
        .mount(&server)
        .await;
    mount_api(&server).await;

    let config = Config::parse(&config_text(&server.uri(), &id_file, &secret_file)).unwrap();
    let handle = ClientHandle::new(&config.servers[0]).unwrap();
    let first = handle.client();
    first.fetch_metadata_model().await.unwrap();
    // Unchanged secrets: the same client, its token cached
    let same = handle.client();
    assert!(Arc::ptr_eq(&first, &same));
    same.fetch_metadata_model().await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [OLD_SECRET]);

    // Rotated mid-run: the next client asks for a token with the new secret
    std::fs::write(&secret_file, NEW_SECRET).unwrap();
    let rotated = handle.client();
    assert!(!Arc::ptr_eq(&first, &rotated));
    rotated.fetch_metadata_model().await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [OLD_SECRET, NEW_SECRET]);

    // A file that cannot be read keeps the current client
    std::fs::remove_file(&secret_file).unwrap();
    assert!(Arc::ptr_eq(&rotated, &handle.client()));
    assert!(handle.renew(&rotated).is_none());
    // A client handed out before a rotation another task already picked up: renew gives the newer one
    assert!(Arc::ptr_eq(&handle.renew(&first).unwrap(), &rotated));
    assert_no_secrets(&format!("{handle:?}"));
    assert_no_secrets_logged();
}

#[tokio::test]
async fn sync_retries_a_resource_once_after_a_401_when_the_secret_rotated() {
    logs();
    let dir = TempDir::new("sync");
    let (id_file, secret_file) = (dir.path("id"), dir.path("secret"));
    std::fs::write(&id_file, CLIENT_ID).unwrap();
    std::fs::write(&secret_file, OLD_SECRET).unwrap();

    // The vendor rotates after the first token: the old secret is then refused, and the mounted
    // file is updated at that moment
    let server = MockServer::start().await;
    let old_requests = Arc::new(AtomicU32::new(0));
    let rotate_file = secret_file.clone();
    let counter = Arc::clone(&old_requests);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(move |req: &Request| match sent_secret(req) {
            Some(NEW_SECRET) => token("token-new"),
            Some(_) if counter.fetch_add(1, Ordering::SeqCst) == 0 => token("token-old"),
            _ => {
                std::fs::write(&rotate_file, format!("{NEW_SECRET}\n")).unwrap();
                ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_client"}))
            }
        })
        .mount(&server)
        .await;
    mount_api(&server).await;

    let config = dir.path("servers.toml");
    std::fs::write(&config, config_text(&server.uri(), &id_file, &secret_file)).unwrap();
    let out = dir.path("out");
    let args: Vec<String> = [
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = run(&args, &mut stdout, &mut stderr).await;
    let (stdout, stderr) = (
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("2 records"), "{stdout}");
    // token-old, then the refused renewal, then the new secret
    assert_eq!(old_requests.load(Ordering::SeqCst), 2);
    assert_no_secrets(&stdout);
    assert_no_secrets(&stderr);
    for entry in std::fs::read_dir(&out).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default();
        assert_no_secrets(&text);
    }
    assert_no_secrets_logged();
}

#[tokio::test]
async fn sync_fails_after_a_401_when_the_secret_did_not_change() {
    logs();
    let dir = TempDir::new("stale");
    let (id_file, secret_file) = (dir.path("id"), dir.path("secret"));
    std::fs::write(&id_file, CLIENT_ID).unwrap();
    std::fs::write(&secret_file, OLD_SECRET).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_client"})))
        .expect(1)
        .mount(&server)
        .await;
    mount_api(&server).await;

    let config = dir.path("servers.toml");
    std::fs::write(&config, config_text(&server.uri(), &id_file, &secret_file)).unwrap();
    let out = dir.path("out");
    let args: Vec<String> = [
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = run(&args, &mut stdout, &mut stderr).await;
    let text = format!(
        "{}{}",
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap()
    );
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("invalid_client"), "{text}");
    assert_no_secrets(&text);
    assert_no_secrets_logged();
}
