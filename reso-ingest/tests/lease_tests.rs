//! One writer per feed: a sync skips a feed another process holds (offline, wiremock)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reso_ingest::cli::run;
use reso_ingest::{FileStateStore, Lease, LeaseOutcome, StateStore};
use serde_json::{json, Value};
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

async fn sync(config: &str, out: &std::path::Path) -> (i32, String) {
    std::env::set_var("LEASE_TEST_TOKEN", "t");
    let args: Vec<String> = ["sync", "--config", config, "--out", out.to_str().unwrap()]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run(&args, &mut o, &mut e).await;
    (code, String::from_utf8(e).unwrap())
}

fn manifest(out: &std::path::Path) -> Value {
    let mut names: Vec<_> = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("manifest.") && n.ends_with(".json"))
        .collect();
    // Ids sort by start time, but file names do not (`X-02.json` sorts before `X.json`)
    names.sort_by_key(|n| {
        n.trim_start_matches("manifest.")
            .trim_end_matches(".json")
            .to_string()
    });
    serde_json::from_str(&std::fs::read_to_string(out.join(names.last().unwrap())).unwrap())
        .unwrap()
}

#[tokio::test]
async fn a_held_feed_is_skipped_and_a_free_one_is_synced() {
    let srv = server().await;
    let dir =
        TempDir(std::env::temp_dir().join(format!("reso-ingest-lease-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let config = dir.0.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"LEASE_TEST_TOKEN\"\n",
            srv.uri()
        ),
    )
    .unwrap();
    let out = dir.0.join("out");
    let config = config.to_str().unwrap();

    // Another process holds mls/Property
    let LeaseOutcome::Acquired(lease) = Lease::acquire(
        &(Arc::new(FileStateStore::new(out.join("state"))) as Arc<dyn StateStore>),
        "mls",
        "Property",
        Duration::from_secs(60),
    )
    .await
    .unwrap() else {
        panic!("acquire");
    };
    let (code, err) = sync(config, &out).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("another process is syncing this feed"),
        "{err}"
    );
    let m = manifest(&out);
    assert_eq!(m["resources"][0]["records"], 0);
    assert!(m["resources"][0]["error"].is_null());
    assert!(m["resources"][0]["warnings"][0]
        .as_str()
        .unwrap()
        .starts_with("skipped"));

    // Released: the next run syncs, and releases its own lease when it ends
    lease.release().await;
    let (code, err) = sync(config, &out).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(manifest(&out)["resources"][0]["records"], 1);
    assert!(!out.join("state/mls.Property.lock").exists());
}
