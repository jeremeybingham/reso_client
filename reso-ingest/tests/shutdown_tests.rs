//! A shutdown request in the middle of a multi-page `sync` (offline, wiremock)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use reso_ingest::cli::{run_with_shutdown, EXIT_INTERRUPTED};
use reso_ingest::Shutdown;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-stop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>
</EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

/// Five records, two per page. The `trigger_at`th `Property` request asks for shutdown while it is
/// being answered, so that page is the one in flight when the request arrives.
async fn server(shutdown: Shutdown, trigger_at: usize) -> MockServer {
    let server = MockServer::start().await;
    let base = server.uri();
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            if requests.fetch_add(1, Ordering::SeqCst) + 1 == trigger_at {
                shutdown.trigger();
            }
            let rows: Vec<(String, String)> = (1..=5)
                .map(|i| (format!("K{i}"), format!("2026-01-01T00:00:0{i}Z")))
                .collect();
            let query: Vec<(String, String)> = req
                .url
                .query_pairs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let from = if let Some((_, v)) = query.iter().find(|(k, _)| k == "from") {
                v.parse().unwrap()
            } else if let Some((_, f)) = query.iter().find(|(k, _)| k == "$filter") {
                rows.iter()
                    .filter(|(_, t)| {
                        let newest = rows.iter().filter(|(_, t)| f.contains(t.as_str())).max();
                        newest.is_some_and(|n| *t <= n.1)
                    })
                    .count()
            } else {
                0
            };
            let end = (from + 2).min(rows.len());
            let value: Vec<Value> = rows[from..end]
                .iter()
                .map(|(k, t)| json!({"ListingKey": k, "ModificationTimestamp": t}))
                .collect();
            let mut body = json!({"value": value});
            if end < rows.len() {
                body["@odata.nextLink"] = json!(format!("{base}/Property?from={end}"));
            }
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    server
}

async fn sync(config: &Path, out: &Path, shutdown: Shutdown) -> (i32, String) {
    let args: Vec<String> = ["sync", "--config", config.to_str().unwrap(), "--out"]
        .iter()
        .map(|s| s.to_string())
        .chain([out.to_str().unwrap().to_string()])
        .collect();
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run_with_shutdown(&args, &mut o, &mut e, shutdown).await;
    (code, String::from_utf8(o).unwrap())
}

fn keys(out: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".ndjson"))
        .collect();
    names.sort();
    names
        .iter()
        .flat_map(|n| {
            std::fs::read_to_string(out.join(n))
                .unwrap()
                .lines()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .map(|l| {
            serde_json::from_str::<Value>(&l).unwrap()["record"]["ListingKey"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn manifests(out: &Path) -> Vec<Value> {
    let mut names: Vec<_> = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("manifest."))
        .collect();
    names.sort();
    names
        .iter()
        .map(|n| serde_json::from_str(&std::fs::read_to_string(out.join(n)).unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn shutdown_mid_run_leaves_output_checkpoint_and_manifest_in_agreement() {
    std::env::set_var("STOP_TEST_TOKEN", "token");
    let first = Shutdown::new();
    let srv = server(first.clone(), 2).await; // the second page request triggers it
    let dir = TempDir::new("mid");
    let config = dir.0.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"STOP_TEST_TOKEN\"\n",
            srv.uri()
        ),
    )
    .unwrap();
    let out = dir.0.join("out");

    let (code, text) = sync(&config, &out, first).await;
    assert_eq!(code, EXIT_INTERRUPTED, "{text}");
    assert!(text.contains("INTERRUPTED"), "{text}");
    // Page one was written and checkpointed; page two was abandoned
    assert_eq!(keys(&out), ["K1", "K2"]);
    let m = &manifests(&out)[0];
    assert_eq!(m["interrupted"], true);
    assert_eq!(m["resources"][0]["interrupted"], true);
    assert_eq!(m["resources"][0]["records"], 2);
    assert_eq!(m["resources"][0]["checkpoint"]["key"], "K2");
    assert!(m["resources"][0]["error"].is_null());
    let saved = std::fs::read_to_string(out.join("state/mls.Property.json")).unwrap();
    assert!(saved.contains("K2") && !saved.contains("K3"), "{saved}");

    // The next run (no shutdown) continues without gaps or repeats
    let (code, text) = sync(&config, &out, Shutdown::new()).await;
    assert_eq!(code, 0, "{text}");
    let mut all = keys(&out); // files are in run-id order, not key order
    all.sort();
    assert_eq!(all, ["K1", "K2", "K3", "K4", "K5"]);
    let m = manifests(&out);
    assert_eq!(m.len(), 2);
    // Manifests are in run-id order, not time order: exactly one of the two is the interrupted one
    assert_eq!(m.iter().filter(|x| x["interrupted"] == true).count(), 1);
}

#[tokio::test]
async fn shutdown_before_the_run_starts_nothing() {
    std::env::set_var("STOP_TEST_TOKEN", "token");
    let early = Shutdown::new();
    early.trigger();
    let srv = server(Shutdown::new(), usize::MAX).await;
    let dir = TempDir::new("early");
    let config = dir.0.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"STOP_TEST_TOKEN\"\n",
            srv.uri()
        ),
    )
    .unwrap();
    let out = dir.0.join("out");
    let (code, _) = sync(&config, &out, early).await;
    assert_eq!(code, EXIT_INTERRUPTED);
    assert!(keys(&out).is_empty());
    assert_eq!(manifests(&out)[0]["interrupted"], true);
}
