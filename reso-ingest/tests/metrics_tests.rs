//! Metrics of a run (`sync --metrics-file`, `render_prometheus`) after wiremock runs (offline)

use std::collections::BTreeSet;
use std::path::PathBuf;

use reso_ingest::cli::run;
use reso_ingest::{parse_rfc3339, render_prometheus_at};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>
</EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("reso-ingest-metrics-{tag}-{}", std::process::id()));
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

/// `$metadata`, then `Property` answers with `property_status` (200 serves two records)
async fn server(property_status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let path = req.url.path();
            if path.ends_with("$metadata") {
                ResponseTemplate::new(200).set_body_string(METADATA)
            } else if path == "/Property" && property_status == 200 {
                // A replication filter that names the newest record's timestamp gets nothing new
                let caught_up = req.url.query_pairs().any(|(_, v)| v.contains("00:00:02"));
                if caught_up {
                    return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
                }
                ResponseTemplate::new(200).set_body_json(json!({"value": [
                    {"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"},
                    {"ListingKey": "K2", "ModificationTimestamp": "2026-01-01T00:00:02Z"},
                ]}))
            } else if path == "/Property" {
                ResponseTemplate::new(property_status)
            } else {
                ResponseTemplate::new(404)
            }
        })
        .mount(&server)
        .await;
    server
}

async fn sync(dir: &TempDir, url: &str, extra: &[&str]) -> (i32, String) {
    std::env::set_var("METRICS_TEST_TOKEN", "t");
    let config = dir.0.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{url}\"\n[server.limits]\nmax_retries = 0\nper_hour = 1000\nbytes_per_day = 100000000\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"METRICS_TEST_TOKEN\"\n"
        ),
    )
    .unwrap();
    let mut args: Vec<String> = [
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--out",
        dir.0.join("out").to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(extra.iter().map(|s| s.to_string()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err).await;
    (code, String::from_utf8(err).unwrap())
}

fn sample(text: &str, name: &str, labels: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with(&format!("{name}{{")) && l.contains(labels))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

const FEED: &str = "server=\"mls\",resource=\"Property\"";

#[tokio::test]
async fn metrics_file_has_lag_volumes_and_request_counts() {
    let srv = server(200).await;
    let dir = TempDir::new("ok");
    let file = dir.0.join("prom").join("reso.prom");
    let (code, err) = sync(
        &dir,
        &srv.uri(),
        &["--metrics-file", file.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!file.with_extension("prom.tmp").exists());

    let get = |name: &str| sample(&text, name, FEED).unwrap_or_else(|| panic!("{name}\n{text}"));
    assert_eq!(get("reso_ingest_last_run_records"), 2.0);
    assert_eq!(get("reso_ingest_last_run_success"), 1.0);
    assert!(get("reso_ingest_last_run_requests") >= 2.0);
    assert!(get("reso_ingest_last_run_http_bytes") > 0.0);
    assert!(get("reso_ingest_last_run_file_bytes") > 0.0);
    assert!(get("reso_ingest_last_run_duration_seconds") > 0.0);
    assert!(get("reso_ingest_last_run_records_per_second") > 0.0);
    assert!(get("reso_ingest_last_success_timestamp_seconds") > 1.7e9);
    // The newest record is from 2026-01-01T00:00:02Z, so the lag is that far behind now
    let lag = get("reso_ingest_freshness_lag_seconds");
    let expected = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - parse_rfc3339("2026-01-01T00:00:02Z").unwrap();
    assert!(
        (lag - expected).abs() < 120.0,
        "lag {lag}, expected {expected}"
    );
    assert!(sample(&text, "reso_ingest_last_run_responses", "class=\"2xx\"").unwrap() >= 2.0);
    assert_eq!(
        sample(&text, "reso_ingest_last_run_responses", "class=\"429\"").unwrap(),
        0.0
    );
    assert!(text.contains("# TYPE reso_ingest_freshness_lag_seconds gauge"));

    // The rate-limit windows of the server's client at the end of its sync (#109): the pull's
    // requests plus the $metadata fetch; the bytes window counts every body; no per-second limit
    let hour = "server=\"mls\",unit=\"requests\",window=\"hour\"";
    let used = sample(&text, "reso_ingest_rate_limit_used", hour).unwrap();
    assert_eq!(used, get("reso_ingest_last_run_requests") + 1.0, "{text}");
    assert_eq!(sample(&text, "reso_ingest_rate_limit", hour), Some(1000.0));
    let day_bytes = "server=\"mls\",unit=\"bytes\",window=\"day\"";
    assert!(
        sample(&text, "reso_ingest_rate_limit_used", day_bytes).unwrap()
            > get("reso_ingest_last_run_http_bytes")
    );
    assert_eq!(
        sample(&text, "reso_ingest_rate_limit", day_bytes),
        Some(100_000_000.0)
    );
    assert!(!text.contains("window=\"second\""), "{text}");

    // A run with nothing new keeps reporting the lag from the saved checkpoint
    let (code, err) = sync(
        &dir,
        &srv.uri(),
        &["--metrics-file", file.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(sample(&text, "reso_ingest_freshness_lag_seconds", FEED).is_some());

    // The manifest carries the same numbers
    let manifest = std::fs::read_dir(dir.0.join("out"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("manifest.")
        })
        .max()
        .unwrap();
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    assert!(m["resources"][0]["stats"]["requests"].as_u64().unwrap() >= 1);
    let budget = &m["rate_budget"];
    assert_eq!(budget.as_array().unwrap().len(), 2, "{budget}");
    assert_eq!(budget[0]["server"], "mls");
    assert_eq!(budget[0]["unit"], "requests");
    assert_eq!(budget[0]["window"], "hour");
    assert_eq!(budget[0]["limit"], 1000);
    assert!(budget[0]["used"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn throttled_feed_shows_429s_failure_and_no_success_time() {
    let srv = server(429).await;
    let dir = TempDir::new("throttled");
    let file = dir.0.join("reso.prom");
    let (code, _) = sync(
        &dir,
        &srv.uri(),
        &["--metrics-file", file.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 1);
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        sample(&text, "reso_ingest_last_run_success", FEED),
        Some(0.0)
    );
    assert!(sample(&text, "reso_ingest_last_run_responses", "class=\"429\"").unwrap() >= 1.0);
    assert!(sample(&text, "reso_ingest_last_success_timestamp_seconds", FEED).is_none());
    // Nothing was written, so there is no checkpoint and no lag to report
    assert!(sample(&text, "reso_ingest_freshness_lag_seconds", FEED).is_none());
}

#[tokio::test]
async fn failed_server_is_reported_and_unwritable_metrics_file_fails_the_run() {
    let dir = TempDir::new("failed");
    let file = dir.0.join("reso.prom");
    // Nothing listens on port 1
    let (code, _) = sync(
        &dir,
        "http://127.0.0.1:1",
        &["--metrics-file", file.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 1);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("reso_ingest_server_failed{server=\"mls\"} 1"),
        "{text}"
    );

    let srv = server(200).await;
    let dir = TempDir::new("badfile");
    let blocker = dir.0.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let bad = blocker.join("reso.prom");
    let (code, err) = sync(&dir, &srv.uri(), &["--metrics-file", bad.to_str().unwrap()]).await;
    assert_eq!(code, 1);
    assert!(err.contains("error:"), "{err}");
}

#[tokio::test]
async fn shipped_dashboard_and_alerts_use_only_exported_metrics() {
    let srv = server(200).await;
    let dir = TempDir::new("monitoring");
    let file = dir.0.join("reso.prom");
    let (code, err) = sync(
        &dir,
        &srv.uri(),
        &["--metrics-file", file.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let _ = render_prometheus_at; // part of the public API
    let mut exported: BTreeSet<String> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("# TYPE "))
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect();
    // Families that only appear when something went wrong (a failed server, a 429 run)
    exported.insert("reso_ingest_server_failed".into());
    exported.insert("reso_ingest_schema_changes".into());

    let monitoring = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("monitoring");
    let dashboard: serde_json::Value =
        serde_json::from_slice(&std::fs::read(monitoring.join("grafana-dashboard.json")).unwrap())
            .unwrap();
    let alerts = std::fs::read_to_string(monitoring.join("alert-rules.yml")).unwrap();
    for (what, text) in [("dashboard", dashboard.to_string()), ("alerts", alerts)] {
        let used: BTreeSet<&str> = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| w.starts_with("reso_ingest_"))
            .collect();
        assert!(!used.is_empty(), "{what} uses no reso_ingest_ metric");
        for name in used {
            assert!(exported.contains(name), "{what} uses unknown metric {name}");
        }
    }
}

#[test]
fn every_shipped_alert_links_a_runbook_that_exists() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let runbooks = std::fs::read_to_string(root.join("../docs/OPERATIONS.md")).unwrap();
    let base = "https://github.com/jeremeybingham/reso_client/blob/main/docs/OPERATIONS.md#";
    // The rules for every run, and the `serve` rules of the Prometheus + Grafana kit (#161)
    for file in [
        "monitoring/alert-rules.yml",
        "../deploy/observability/alerts.yml",
    ] {
        let alerts = std::fs::read_to_string(root.join(file)).unwrap();
        let names: Vec<&str> = alerts
            .lines()
            .filter_map(|l| l.trim().strip_prefix("- alert: "))
            .collect();
        assert!(!names.is_empty(), "{file}");
        let urls = alerts.matches("runbook_url:").count();
        assert_eq!(
            urls,
            names.len(),
            "{file}: every alert needs one runbook_url"
        );
        for name in names {
            let url = format!("runbook_url: \"{base}{}\"", name.to_lowercase());
            assert!(alerts.contains(&url), "{name} has no runbook_url {url}");
            assert!(
                runbooks.lines().any(|l| l == format!("### {name}")),
                "docs/OPERATIONS.md has no runbook section for {name}"
            );
        }
    }
}
