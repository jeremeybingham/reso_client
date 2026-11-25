//! JSON log lines of a `sync` run carry `run_id`, `server` and `resource`, and hold no secrets or
//! record contents (#96)
//!
//! This test has its own binary and installs a global subscriber: server tasks run on other
//! threads, which a scoped subscriber would not reach.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use reso_ingest::cli::run;
use reso_ingest::{log_subscriber, LogFormat};
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "s3cr3t-token-for-logs";
const PERSONAL: &str = "agent.private@example.com";

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Member"><Key><PropertyRef Name="MemberKey"/></Key>
<Property Name="MemberKey" Type="Edm.String" Nullable="false"/>
<Property Name="MemberEmail" Type="Edm.String"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Member" EntityType="org.reso.metadata.Member"/></EntityContainer>
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

/// The values of `field` in the `spans` list of a JSON log line
fn span_field<'a>(line: &'a Value, field: &str) -> Option<&'a str> {
    line["spans"]
        .as_array()?
        .iter()
        .find_map(|s| s.get(field).and_then(Value::as_str))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_writes_json_lines_with_run_server_and_resource() {
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = log_subscriber(LogFormat::Json, EnvFilter::new("debug"), move || {
        writer.clone()
    });
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            let first = !req.url.query().unwrap_or("").contains("filter");
            let value = if first {
                vec![json!({
                    "MemberKey": "M1",
                    "MemberEmail": PERSONAL,
                    "ModificationTimestamp": "2026-01-01T00:00:01Z"
                })]
            } else {
                vec![]
            };
            ResponseTemplate::new(200).set_body_json(json!({ "value": value }))
        })
        .mount(&server)
        .await;

    let dir: PathBuf =
        std::env::temp_dir().join(format!("reso-ingest-logs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls-a\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"RESO_LOG_TEST_TOKEN\"\n",
            server.uri()
        ),
    )
    .unwrap();
    std::env::set_var("RESO_LOG_TEST_TOKEN", SECRET);

    let args: Vec<String> = [
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--out",
        dir.join("out").to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err).await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));

    let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(!text.contains(SECRET), "token in logs");
    assert!(!text.contains(PERSONAL), "record contents in logs");
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect();
    assert!(!lines.is_empty());

    let synced = lines
        .iter()
        .find(|l| l["fields"]["message"] == "resource synced")
        .expect("a `resource synced` line");
    assert_eq!(synced["level"], "INFO");
    assert_eq!(synced["fields"]["records"], 1);
    assert_eq!(span_field(synced, "server"), Some("mls-a"));
    assert_eq!(span_field(synced, "resource"), Some("Member"));
    let run_id = span_field(synced, "run_id").expect("run_id on the line");
    assert!(String::from_utf8_lossy(&out).contains(run_id));

    // Events from reso-client inside the resource (the page queries) carry all three too
    let query = lines
        .iter()
        .find(|l| {
            l["target"]
                .as_str()
                .is_some_and(|t| t.starts_with("reso_client"))
                && span_field(l, "resource").is_some()
        })
        .expect("a reso_client line inside the resource span");
    assert_eq!(span_field(query, "server"), Some("mls-a"));
    assert_eq!(span_field(query, "run_id"), Some(run_id));
}
