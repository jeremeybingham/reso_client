//! The access log: one append-only line per resource pull, no secrets (offline, wiremock; #93)

use std::path::{Path, PathBuf};

use reso_ingest::cli::run;
use reso_ingest::{AccessEntry, ACCESS_LOG};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
            }
            ResponseTemplate::new(200).set_body_json(
                json!({"value": [{"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"}]}),
            )
        })
        .mount(&server)
        .await;
    server
}

async fn broken() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            ResponseTemplate::new(500)
        })
        .mount(&server)
        .await;
    server
}

const SECRET: &str = "audit-secret-value-123";

async fn sync(config: &str, out: &Path) -> (i32, String) {
    std::env::set_var("AUDIT_TEST_TOKEN", SECRET);
    let args: Vec<String> = ["sync", "--config", config, "--out", out.to_str().unwrap()]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run(&args, &mut o, &mut e).await;
    (code, String::from_utf8(e).unwrap())
}

fn server_toml(name: &str, uri: &str) -> String {
    format!(
        "[[server]]\nname = \"{name}\"\nbase_url = \"{uri}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"AUDIT_TEST_TOKEN\"\n"
    )
}

fn lines(out: &Path) -> (String, Vec<AccessEntry>) {
    let text = std::fs::read_to_string(out.join(ACCESS_LOG)).unwrap();
    let entries = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (text, entries)
}

#[tokio::test]
async fn each_pull_appends_one_line_and_a_failed_pull_is_logged() {
    let (good, bad) = (server().await, broken().await);
    let dir =
        TempDir(std::env::temp_dir().join(format!("reso-ingest-audit-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let config = dir.0.join("servers.toml");
    std::fs::write(
        &config,
        server_toml("good", &good.uri()) + &server_toml("bad", &bad.uri()),
    )
    .unwrap();
    let out = dir.0.join("out");
    let config = config.to_str().unwrap();

    let (code, err) = sync(config, &out).await;
    assert_eq!(code, 1, "the bad server fails: {err}");
    let (first_text, first) = lines(&out);
    assert_eq!(first.len(), 2, "{first_text}");
    assert!(!first_text.contains(SECRET));
    let ok = first.iter().find(|e| e.server == "good").unwrap();
    assert_eq!(ok.resource, "Property");
    assert_eq!(ok.status, "ok");
    assert_eq!(ok.credential, "env:AUDIT_TEST_TOKEN");
    assert_eq!(ok.records, 1);
    assert_eq!(ok.window_start, None, "a full load has no start");
    assert_eq!(ok.window_end.as_deref(), Some("2026-01-01T00:00:01Z"));
    assert!(ok.bytes_received > 0 && ok.bytes_written > 0);
    let failed = first.iter().find(|e| e.server == "bad").unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.error.is_some());

    // A second run appends after the first run's lines and leaves them as they were
    let (code, err) = sync(config, &out).await;
    assert_eq!(code, 1, "{err}");
    let (text, all) = lines(&out);
    assert!(text.starts_with(&first_text));
    assert_eq!(all.len(), 4);
    assert_ne!(all[2].run_id, all[0].run_id);
    let again = all[2..].iter().find(|e| e.server == "good").unwrap();
    assert_eq!(again.window_start.as_deref(), Some("2026-01-01T00:00:01Z"));
}
