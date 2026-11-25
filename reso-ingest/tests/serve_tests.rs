//! The daemon: feeds on their own intervals, backoff, endpoints (offline, wiremock, real short clock)

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reso_ingest::cli::run;
use reso_ingest::{Config, Daemon, ServeOptions, Shutdown};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityType Name="Member"><Key><PropertyRef Name="MemberKey"/></Key>
<Property Name="MemberKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>
<EntitySet Name="Member" EntityType="org.reso.metadata.Member"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("reso-ingest-serve-{tag}-{}", std::process::id()));
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

/// A server with Property and Member; `Member` answers 500 when `member_fails` is set. Counts the
/// data requests of each resource.
async fn server(member_fails: bool) -> (MockServer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let server = MockServer::start().await;
    let property = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(AtomicUsize::new(0));
    let (p, m) = (Arc::clone(&property), Arc::clone(&member));
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let path = req.url.path();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            let (counter, key) = if path.ends_with("/Member") {
                (&m, "MemberKey")
            } else {
                (&p, "ListingKey")
            };
            counter.fetch_add(1, Ordering::SeqCst);
            if member_fails && path.ends_with("/Member") {
                return ResponseTemplate::new(500).set_body_string("boom");
            }
            if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
            }
            ResponseTemplate::new(200).set_body_json(
                json!({"value": [{key: "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"}]}),
            )
        })
        .mount(&server)
        .await;
    (server, property, member)
}

fn config(url: &str, extra_server: &str, property: &str, member: &str) -> Config {
    Config::parse(&format!(
        r#"
[[server]]
name = "mls"
base_url = "{url}"
{extra_server}
[server.limits]
max_retries = 0
[server.auth]
type = "bearer"
token_env = "SERVE_TEST_TOKEN"
[server.resources.Property]
{property}
[server.resources.Member]
{member}
"#
    ))
    .unwrap()
}

fn options(dir: &TempDir) -> ServeOptions {
    let mut o = ServeOptions::new(dir.0.join("out"));
    o.listen = "127.0.0.1:0".parse().unwrap();
    o
}

async fn get(daemon: &Daemon, path: &str) -> (u16, String) {
    let url = format!("http://{}{path}", daemon.local_addr());
    let r = reqwest::get(url).await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

async fn status(daemon: &Daemon) -> Value {
    serde_json::from_str(&get(daemon, "/status").await.1).unwrap()
}

fn feed<'a>(status: &'a Value, resource: &str) -> &'a Value {
    status["feeds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["resource"] == resource)
        .unwrap()
}

async fn until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !ready().await {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn feeds_run_on_their_own_intervals_and_endpoints_answer() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, property, member) = server(false).await;
    let dir = TempDir::new("intervals");
    let config = config(&srv.uri(), "", "interval = \"100ms\"", "interval = \"1s\"");
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(config, options(&dir), shutdown.clone())
        .await
        .unwrap();

    // Property is polled many times while Member is polled a few times
    until("Property polled five times", async || {
        property.load(Ordering::SeqCst) >= 5
    })
    .await;
    assert!(
        member.load(Ordering::SeqCst) < property.load(Ordering::SeqCst),
        "the slower feed ran as often as the faster one"
    );

    assert_eq!(get(&daemon, "/healthz").await, (200, "ok\n".into()));
    assert_eq!(get(&daemon, "/readyz").await.0, 200);

    until("both feeds have a result", async || {
        let s = status(&daemon).await;
        !feed(&s, "Member")["last_success"].is_null()
            && feed(&s, "Property")["runs"].as_u64() >= Some(2)
    })
    .await;
    let s = status(&daemon).await;
    let p = feed(&s, "Property");
    assert_eq!(p["server"], "mls");
    assert_eq!(p["interval_secs"], 0.1);
    assert_eq!(p["consecutive_failures"], 0);
    assert!(p["last_error"].is_null());
    assert_eq!(feed(&s, "Member")["interval_secs"], 1.0);

    let (code, metrics) = get(&daemon, "/metrics").await;
    assert_eq!(code, 200);
    assert!(
        metrics.contains(r#"reso_ingest_feed_runs_total{server="mls",resource="Property"}"#),
        "{metrics}"
    );
    assert!(
        metrics.contains(r#"reso_ingest_last_run_success{server="mls",resource="Member"} 1"#),
        "{metrics}"
    );
    assert!(
        metrics.contains("reso_ingest_freshness_lag_seconds"),
        "{metrics}"
    );

    // The records of the first syncs are written; the empty polls leave no manifests or markers
    let out = dir.0.join("out");
    let names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n.ends_with(".ndjson")), "{names:?}");
    assert_eq!(
        names.iter().filter(|n| n.starts_with("manifest.")).count(),
        2,
        "one manifest per feed (the first sync of each): {names:?}"
    );
    assert!(!names.iter().any(|n| n.starts_with(".run.")), "{names:?}");

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("the daemon stops after the shutdown request");
}

#[tokio::test]
async fn a_failing_feed_backs_off_and_does_not_stop_the_others() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, property, member) = server(true).await;
    let dir = TempDir::new("backoff");
    let config = config(
        &srv.uri(),
        "",
        "interval = \"100ms\"",
        "interval = \"100ms\"",
    );
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(config, options(&dir), shutdown.clone())
        .await
        .unwrap();

    until("Member failed twice", async || {
        feed(&status(&daemon).await, "Member")["consecutive_failures"].as_u64() >= Some(2)
    })
    .await;
    until("Property polled ten times", async || {
        property.load(Ordering::SeqCst) >= 10
    })
    .await;
    let s = status(&daemon).await;
    let m = feed(&s, "Member");
    assert!(m["last_error"].as_str().unwrap().contains("500"), "{m}");
    assert!(m["last_success"].is_null());
    assert_eq!(feed(&s, "Property")["consecutive_failures"], 0);
    // Doubling waits: the failing feed was asked far less often than the healthy one
    assert!(
        member.load(Ordering::SeqCst) * 2 < property.load(Ordering::SeqCst),
        "member {} property {}",
        member.load(Ordering::SeqCst),
        property.load(Ordering::SeqCst)
    );
    let metrics = get(&daemon, "/metrics").await.1;
    assert!(
        metrics
            .contains(r#"reso_ingest_feed_consecutive_failures{server="mls",resource="Member"}"#),
        "{metrics}"
    );
    assert!(
        metrics.contains(r#"reso_ingest_last_run_success{server="mls",resource="Member"} 0"#),
        "{metrics}"
    );

    shutdown.trigger();
    daemon.wait().await;
}

#[tokio::test]
async fn readiness_follows_the_state_directory() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, _, _) = server(false).await;
    let dir = TempDir::new("ready");
    let config = config(&srv.uri(), "", "interval = \"1h\"", "interval = \"1h\"");
    let opts = options(&dir);
    let state = dir.0.join("out").join("state");
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(config, opts, shutdown.clone()).await.unwrap();
    assert_eq!(get(&daemon, "/readyz").await.0, 200);

    // The state directory becomes a file: nothing can be saved
    std::fs::remove_dir_all(&state).unwrap();
    std::fs::write(&state, "not a directory").unwrap();
    let (code, body) = get(&daemon, "/readyz").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(get(&daemon, "/healthz").await.0, 200); // still alive

    shutdown.trigger();
    daemon.wait().await;
}

#[tokio::test]
async fn server_interval_covers_included_resources() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, property, member) = server(false).await;
    let dir = TempDir::new("include");
    let config = Config::parse(&format!(
        r#"
[[server]]
name = "mls"
base_url = "{}"
include = ["Property", "Member"]
interval = "1h"
concurrency = 1
[server.limits]
max_retries = 0
[server.auth]
type = "bearer"
token_env = "SERVE_TEST_TOKEN"
[server.resources.Member]
interval = "100ms"
"#,
        srv.uri()
    ))
    .unwrap();
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(config, options(&dir), shutdown.clone())
        .await
        .unwrap();
    until("Member polled four times", async || {
        member.load(Ordering::SeqCst) >= 4
    })
    .await;
    // Property runs once (after a start-up delay of up to 10 s) and then waits an hour
    assert!(property.load(Ordering::SeqCst) <= 1);
    let s = status(&daemon).await;
    assert_eq!(feed(&s, "Property")["interval_secs"], 3600.0);
    assert_eq!(s["feeds"].as_array().unwrap().len(), 2);
    shutdown.trigger();
    daemon.wait().await;
}

#[tokio::test]
async fn nothing_scheduled_is_a_startup_error() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let dir = TempDir::new("none");
    let config = Config::parse(
        r#"
[[server]]
name = "mls"
base_url = "http://127.0.0.1:1"
[server.auth]
type = "bearer"
token_env = "SERVE_TEST_TOKEN"
"#,
    )
    .unwrap();
    let err = Daemon::start(config, options(&dir), Shutdown::new())
        .await
        .err()
        .expect("no feeds");
    assert!(err.to_string().contains("interval"), "{err}");

    // The command line reports it as a usage error
    let file = dir.0.join("servers.toml");
    std::fs::write(
        &file,
        "[[server]]\nname = \"mls\"\nbase_url = \"http://127.0.0.1:1\"\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"SERVE_TEST_TOKEN\"\n",
    )
    .unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args: Vec<String> = [
        "serve",
        "--config",
        file.to_str().unwrap(),
        "--listen",
        "127.0.0.1:0",
    ]
    .map(String::from)
    .into();
    assert_eq!(run(&args, &mut out, &mut err).await, 2);
    assert!(String::from_utf8_lossy(&err).contains("interval"));
}
