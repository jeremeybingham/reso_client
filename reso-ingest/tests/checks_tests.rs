//! Run checks: `$metadata` changes, data problems, count comparison and the webhook (offline, wiremock)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use reso_client::{ClientConfig, ResoClient};
use reso_ingest::cli::run;
use reso_ingest::{count_warning, CheckingWriter, NdjsonWriter, RecordWriter};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-chk-{tag}-{}", std::process::id()));
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

fn metadata(price_type: &str, extra: &str, status: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EnumType Name="Status">{status}</EnumType>
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ListPrice" Type="{price_type}"/>{extra}
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#
    )
}

#[derive(Default)]
struct Feed {
    metadata: String,
    records: Vec<Value>,
    /// What `$count` answers; none means the server has no count
    count: Option<u64>,
    /// Ignore the filter and return the records again on every run
    ignore_filter: bool,
}

async fn server(feed: Arc<Mutex<Feed>>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let feed = feed.lock().unwrap();
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(feed.metadata.clone());
            }
            let query: Vec<(String, String)> = req
                .url
                .query_pairs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if query.iter().any(|(k, _)| k == "$count") {
                return match feed.count {
                    Some(n) => ResponseTemplate::new(200)
                        .set_body_json(json!({"@odata.count": n, "value": []})),
                    None => ResponseTemplate::new(400).set_body_string("no count"),
                };
            }
            let filtered = query.iter().any(|(k, _)| k == "$filter");
            if filtered && !feed.ignore_filter {
                return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
            }
            ResponseTemplate::new(200).set_body_json(json!({"value": feed.records}))
        })
        .mount(&server)
        .await;
    server
}

fn rec(key: Option<&str>, ts: &str) -> Value {
    match key {
        Some(k) => json!({"ListingKey": k, "ListPrice": 1, "ModificationTimestamp": ts}),
        None => json!({"ListPrice": 1, "ModificationTimestamp": ts}),
    }
}

fn write_config(dir: &TempDir, url: &str, top: &str, extra: &str) -> String {
    let text = format!(
        "{top}\n[[server]]\nname = \"mls\"\nbase_url = \"{url}\"\n{extra}\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"CHECKS_TEST_TOKEN\"\n"
    );
    let path = dir.path("servers.toml");
    std::fs::write(&path, text).unwrap();
    path.to_str().unwrap().to_string()
}

async fn cli(args: &[&str]) -> (i32, String, String) {
    std::env::set_var("CHECKS_TEST_TOKEN", "t");
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err).await;
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// The manifest of the newest run in `out`
fn last_manifest(out: &std::path::Path) -> Value {
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
    let text = std::fs::read_to_string(out.join(names.last().unwrap())).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn warnings(manifest: &Value) -> Vec<String> {
    manifest["resources"][0]["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn schema_changes_are_found_against_the_previous_run() {
    let feed = Arc::new(Mutex::new(Feed {
        metadata: metadata(
            "Edm.Decimal",
            "",
            r#"<Member Name="Active"/><Member Name="Pending"/>"#,
        ),
        records: vec![rec(Some("K1"), "2026-01-01T00:00:01Z")],
        ..Feed::default()
    }));
    let srv = server(feed.clone()).await;
    let dir = TempDir::new("schema");
    let config = write_config(&dir, &srv.uri(), "", "");
    let out = dir.path("out");
    let args = [
        "sync",
        "--config",
        &config,
        "--out",
        out.to_str().unwrap(),
        "--metrics-file",
    ];
    let metrics = dir.path("m.prom");
    let args: Vec<&str> = args
        .iter()
        .copied()
        .chain([metrics.to_str().unwrap()])
        .collect();

    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 0, "{err}");
    let m = last_manifest(&out);
    assert_eq!(
        m["schema_changes"],
        json!([]),
        "the first run is a baseline"
    );

    // Unchanged: still nothing
    let (code, ..) = cli(&args).await;
    assert_eq!(code, 0);
    assert_eq!(last_manifest(&out)["schema_changes"], json!([]));

    feed.lock().unwrap().metadata = metadata(
        "Edm.String",
        r#"<Property Name="Fresh" Type="Edm.Int32"/>"#,
        r#"<Member Name="Active"/><Member Name="Closed"/>"#,
    );
    let (code, ..) = cli(&args).await;
    assert_eq!(code, 0, "schema changes are warnings, not failures");
    let m = last_manifest(&out);
    assert_eq!(
        m["schema_changes"],
        json!([{"server": "mls", "changes": [
            "Property.ListPrice: type Edm.Decimal -> Edm.String",
            "Property.Fresh: field added (Edm.Int32)",
            "enum Status: value Pending removed",
            "enum Status: value Closed added",
        ]}])
    );
    let text = std::fs::read_to_string(&metrics).unwrap();
    assert!(
        text.contains("reso_ingest_schema_changes{server=\"mls\"} 4"),
        "{text}"
    );

    // Reported once: the snapshot moved on
    let (code, ..) = cli(&args).await;
    assert_eq!(code, 0);
    assert_eq!(last_manifest(&out)["schema_changes"], json!([]));
}

#[tokio::test]
async fn data_problems_and_a_count_mismatch_are_warnings() {
    let feed = Arc::new(Mutex::new(Feed {
        metadata: metadata("Edm.Decimal", "", r#"<Member Name="Active"/>"#),
        records: vec![
            rec(Some("K1"), "2026-01-01T00:00:01Z"),
            rec(Some("K1"), "2026-01-01T00:00:02Z"),
            rec(Some("K2"), "2026-01-01T00:00:03Z"),
        ],
        count: Some(9),
        ..Feed::default()
    }));
    let srv = server(feed.clone()).await;
    let dir = TempDir::new("data");
    let config = write_config(&dir, &srv.uri(), "", "");
    let out = dir.path("out");
    let metrics = dir.path("m.prom");
    let args = [
        "sync",
        "--config",
        &config,
        "--out",
        out.to_str().unwrap(),
        "--metrics-file",
        metrics.to_str().unwrap(),
    ];

    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 0, "findings do not fail the run: {err}");
    let w = warnings(&last_manifest(&out));
    assert!(
        w.contains(&"1 records repeat a `ListingKey` already in the same page".to_string()),
        "{w:?}"
    );
    assert!(
        w.contains(&"3 records written but the server counts 9".to_string()),
        "{w:?}"
    );
    let text = std::fs::read_to_string(&metrics).unwrap();
    assert!(
        text.contains("reso_ingest_last_run_warnings{server=\"mls\",resource=\"Property\"} 2"),
        "{text}"
    );

    // The second run resumes from the checkpoint, so there is no count check; a server that
    // hands back records older than the checkpoint stops the resource with an error
    {
        let mut f = feed.lock().unwrap();
        f.ignore_filter = true;
        f.records = vec![rec(Some("K1"), "2025-12-31T00:00:00Z")];
    }
    let (code, out_text, _) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(
        out_text.contains("server ignored $orderby or $filter"),
        "{out_text}"
    );
}

#[tokio::test]
async fn a_record_without_a_key_stops_the_resource() {
    let feed = Arc::new(Mutex::new(Feed {
        metadata: metadata("Edm.Decimal", "", r#"<Member Name="Active"/>"#),
        records: vec![
            rec(Some("K1"), "2026-01-01T00:00:01Z"),
            rec(None, "2026-01-01T00:00:02Z"),
        ],
        ..Feed::default()
    }));
    let srv = server(feed).await;
    let dir = TempDir::new("nokey");
    let config = write_config(&dir, &srv.uri(), "", "");
    let out = dir.path("out");
    let (code, out_text, _) =
        cli(&["sync", "--config", &config, "--out", out.to_str().unwrap()]).await;
    assert_eq!(code, 1);
    assert!(
        out_text.contains("Record has no string or number ListingKey"),
        "{out_text}"
    );
}

#[tokio::test]
async fn a_matching_count_and_a_server_without_count_give_no_warning() {
    let feed = Arc::new(Mutex::new(Feed {
        metadata: metadata("Edm.Decimal", "", r#"<Member Name="Active"/>"#),
        records: vec![rec(Some("K1"), "2026-01-01T00:00:01Z")],
        count: Some(1),
        ..Feed::default()
    }));
    let srv = server(feed.clone()).await;
    let client = ResoClient::with_config(ClientConfig::new(srv.uri(), "t")).unwrap();
    assert_eq!(count_warning(&client, "Property", None, 1).await, None);
    assert_eq!(
        count_warning(&client, "Property", None, 2).await.as_deref(),
        Some("2 records written but the server counts 1")
    );
    feed.lock().unwrap().count = None;
    assert_eq!(count_warning(&client, "Property", None, 2).await, None);
}

#[test]
fn checking_writer_passes_records_through_and_counts_repeats_within_a_page() {
    let dir = TempDir::new("writer");
    let mut inner = NdjsonWriter::new(dir.path("out"), "mls", "Property", "r1", 1 << 20).unwrap();
    let mut w = CheckingWriter::new(&mut inner, "ListingKey");
    let batch = [
        rec(Some("A"), "2026-01-01T00:00:10Z"),
        rec(Some("A"), "2026-01-01T00:00:11Z"),
        rec(Some("B"), "2026-01-01T00:00:12Z"),
    ];
    w.write_batch(&batch, "2026-01-02T00:00:00Z").unwrap();
    // The same key on another page is normal (an update between pages)
    w.write_batch(&batch[..1], "2026-01-02T00:00:00Z").unwrap();
    assert_eq!(
        w.warnings(),
        ["1 records repeat a `ListingKey` already in the same page"]
    );
    let files = w.finish().unwrap();
    assert_eq!(files[0].records, 4, "every record was written, unchanged");
}

#[tokio::test]
async fn the_webhook_gets_the_run_summary() {
    let feed = Arc::new(Mutex::new(Feed {
        metadata: metadata("Edm.Decimal", "", r#"<Member Name="Active"/>"#),
        records: vec![
            rec(Some("K1"), "2026-01-01T00:00:01Z"),
            rec(Some("K1"), "2026-01-01T00:00:02Z"),
        ],
        ..Feed::default()
    }));
    let srv = server(feed).await;
    let hook = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&hook)
        .await;
    let dir = TempDir::new("hook");
    let config = write_config(
        &dir,
        &srv.uri(),
        "webhook_url_env = \"CHECKS_TEST_HOOK\"",
        "",
    );
    let out = dir.path("out");
    std::env::set_var("CHECKS_TEST_HOOK", format!("{}/secret/path", hook.uri()));

    let (code, _, err) = cli(&["sync", "--config", &config, "--out", out.to_str().unwrap()]).await;
    assert_eq!(code, 0, "{err}");
    let posts = hook.received_requests().await.unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].url.path(), "/secret/path");
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(body["clean"], true);
    assert_eq!(
        (body["resources"].clone(), body["records"].clone()),
        (json!(1), json!(2))
    );
    assert_eq!(body["failures"], json!([]));
    assert_eq!(
        body["warnings"],
        json!(["mls/Property: 1 records repeat a `ListingKey` already in the same page"])
    );
    assert!(body["manifest"].as_str().unwrap().contains("manifest."));
    assert!(body["run_id"].is_string() && body["finished_at"].is_string());
}

#[tokio::test]
async fn a_failing_or_missing_webhook_fails_the_run_without_leaking_the_url() {
    let feed = Arc::new(Mutex::new(Feed {
        metadata: metadata("Edm.Decimal", "", r#"<Member Name="Active"/>"#),
        records: vec![rec(Some("K1"), "2026-01-01T00:00:01Z")],
        ..Feed::default()
    }));
    let srv = server(feed).await;
    let hook = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&hook)
        .await;
    let dir = TempDir::new("hookfail");
    let config = write_config(
        &dir,
        &srv.uri(),
        "webhook_url_env = \"CHECKS_TEST_HOOK_BAD\"",
        "",
    );
    let out = dir.path("out");
    let args = ["sync", "--config", &config, "--out", out.to_str().unwrap()];

    std::env::set_var(
        "CHECKS_TEST_HOOK_BAD",
        format!("{}/token-in-path", hook.uri()),
    );
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(err.contains("answered 500"), "{err}");
    assert!(!err.contains("token-in-path"), "{err}");

    std::env::remove_var("CHECKS_TEST_HOOK_BAD");
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(err.contains("CHECKS_TEST_HOOK_BAD is not set"), "{err}");
}

#[tokio::test]
async fn delete_limits_come_from_the_config_and_can_be_released_from_the_command_line() {
    let feed = Arc::new(Mutex::new(Feed {
        metadata: metadata("Edm.Decimal", "", r#"<Member Name="Active"/>"#),
        records: (1..=4)
            .map(|i| rec(Some(&format!("K{i}")), &format!("2026-01-01T00:00:0{i}Z")))
            .collect(),
        ..Feed::default()
    }));
    let srv = server(feed.clone()).await;
    let dir = TempDir::new("limits");
    let config = write_config(&dir, &srv.uri(), "", "deletes = { max_percent = 50 }");
    let out = dir.path("out");
    let args = [
        "sync",
        "--config",
        &config,
        "--out",
        out.to_str().unwrap(),
        "--deletes",
    ];

    let (code, ..) = cli(&args).await;
    assert_eq!(code, 0);
    // The key pull is an unfiltered query: it sees what the feed has now
    feed.lock().unwrap().records.truncate(1);
    let (code, ..) = cli(&args).await;
    assert_eq!(code, 0, "held deletes are a warning");
    let m = last_manifest(&out);
    assert_eq!(m["resources"][0]["deletes"], 0);
    assert_eq!(m["resources"][0]["deletes_held"], 3);
    assert!(warnings(&m)[0].contains("3 deletes held"));

    let mut released: Vec<&str> = args.to_vec();
    released.push("--allow-mass-deletes");
    let (code, ..) = cli(&released).await;
    assert_eq!(code, 0);
    let m = last_manifest(&out);
    assert_eq!(
        (
            m["resources"][0]["deletes"].clone(),
            m["resources"][0]["deletes_held"].clone()
        ),
        (json!(3), json!(0))
    );
}

#[test]
fn config_rejects_bad_limits_and_a_literal_webhook_url() {
    use reso_ingest::Config;
    let server = "[[server]]\nname = \"a\"\nbase_url = \"http://x\"\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"T\"\n";
    let ok = Config::parse(&format!("webhook_url_env = \"HOOK\"\n{server}")).unwrap();
    assert_eq!(ok.webhook_url_env.as_deref(), Some("HOOK"));
    assert!(Config::parse(&format!(
        "webhook_url_env = \"https://hooks.example/x\"\n{server}"
    ))
    .is_err());
    for bad in ["max_percent = 0", "max_percent = 101", "max_deletes = 0"] {
        let text = format!(
            "[[server]]\nname = \"a\"\nbase_url = \"http://x\"\ndeletes = {{ {bad} }}\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"T\"\n"
        );
        assert!(Config::parse(&text).is_err(), "{bad}");
    }
}
