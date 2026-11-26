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
    // Property is still polled every 100 ms: a poll in progress holds its `.run.` marker, so take
    // a listing between polls (each poll removes its marker when it ends)
    let out = dir.0.join("out");
    let list = || -> Vec<String> {
        std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    let mut names = list();
    until("a listing between polls", async || {
        names = list();
        !names.iter().any(|n| n.starts_with(".run."))
    })
    .await;
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
    let started = Instant::now();
    let daemon = Daemon::start(config, options(&dir), shutdown.clone())
        .await
        .unwrap();

    until("Member failed four times", async || {
        feed(&status(&daemon).await, "Member")["consecutive_failures"].as_u64() >= Some(4)
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
    // Doubling waits: after failure i the feed waits 100 ms * 2^i, so m failed requests take at
    // least 100 ms * (2^m - 2). A slow machine only adds time, so this holds under any load,
    // while a feed without backoff (one request per 100 ms) breaks it from the third request.
    let failed = member.load(Ordering::SeqCst) as u32;
    let elapsed = started.elapsed();
    let floor = Duration::from_millis(100) * ((1u32 << failed.min(20)) - 2);
    assert!(
        elapsed >= floor,
        "{failed} Member requests in {elapsed:?}, backoff needs at least {floor:?}"
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
    let (srv, _property, member) = server(false).await;
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
    // Property runs once (after a start-up delay of up to 10 s) and then waits an hour. Count its
    // runs, not its requests: one sync is two requests (the records, then an empty page past them).
    let s = status(&daemon).await;
    let runs = feed(&s, "Property")["runs"].as_u64().unwrap();
    assert!(runs <= 1, "Property ran {runs} times");
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

#[tokio::test]
async fn feeds_of_one_server_count_their_own_requests() {
    // Both feeds share the server's client and sync at the same time (slow answers, short
    // intervals). Property's answers are padded to 20 kB and Member's are small, so a Member sync
    // that counted Property's traffic would show it in its bytes (#109).
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let srv = MockServer::start().await;
    // Arrival time of each data request, and whether it was Member's
    let arrivals = Arc::new(std::sync::Mutex::new(Vec::<(Instant, bool)>::new()));
    let seen = Arc::clone(&arrivals);
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let path = req.url.path();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            let member = path.ends_with("/Member");
            seen.lock().unwrap().push((Instant::now(), member));
            let (key, pad) = if member {
                ("MemberKey", String::new())
            } else {
                ("ListingKey", "x".repeat(20_000))
            };
            let value = if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                json!([])
            } else {
                json!([{key: "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"}])
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"value": value, "pad": pad}))
                .set_delay(DELAY)
        })
        .mount(&srv)
        .await;
    const DELAY: Duration = Duration::from_millis(150);
    // Requests of the two feeds open at the same time: a Member request that arrived while a
    // Property request was still delayed, or the other way round
    let overlaps = || {
        let a = arrivals.lock().unwrap();
        a.iter()
            .filter(|(t, m)| {
                a.iter()
                    .any(|(u, n)| n != m && u <= t && t.duration_since(*u) < DELAY)
            })
            .count()
    };
    let dir = TempDir::new("own-counts");
    // A rate limit, so /metrics reports the client's live use of it
    let config = Config::parse(&format!(
        r#"
[[server]]
name = "mls"
base_url = "{}"
[server.limits]
max_retries = 0
per_hour = 100000
[server.auth]
type = "bearer"
token_env = "SERVE_TEST_TOKEN"
[server.resources.Property]
interval = "100ms"
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

    let bytes = |m: &str, resource: &str| -> f64 {
        let prefix =
            format!("reso_ingest_last_run_http_bytes{{server=\"mls\",resource=\"{resource}\"}} ");
        m.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{resource}: {m}"))
    };
    let mut checked = 0;
    until("several overlapping syncs of both feeds", async || {
        let s = status(&daemon).await;
        if feed(&s, "Member")["runs"].as_u64() < Some(1)
            || feed(&s, "Property")["runs"].as_u64() < Some(1)
        {
            return false;
        }
        // Every request that has arrived was counted by the limiter before it was sent
        let arrived = arrivals.lock().unwrap().len() as f64;
        let metrics = get(&daemon, "/metrics").await.1;
        assert!(bytes(&metrics, "Member") < 20_000.0, "{metrics}");
        assert!(bytes(&metrics, "Property") >= 20_000.0, "{metrics}");
        // Both feeds' requests and the $metadata fetches, read from the shared client now
        let hour = "reso_ingest_rate_limit_used{server=\"mls\",unit=\"requests\",window=\"hour\"} ";
        let used: f64 = metrics
            .lines()
            .find_map(|l| l.strip_prefix(hour))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{metrics}"));
        assert!(used >= arrived, "{metrics}");
        assert!(
            metrics.contains(
                "reso_ingest_rate_limit{server=\"mls\",unit=\"requests\",window=\"hour\"} 100000\n"
            ),
            "{metrics}"
        );
        checked += 1;
        checked >= 10 && overlaps() >= 5
    })
    .await;

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .unwrap();
}

#[tokio::test]
async fn reload_adds_pauses_resumes_restarts_and_removes_feeds() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, property, member) = server(false).await;
    let dir = TempDir::new("reload");
    let url = srv.uri();
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(
        config(&url, "", "interval = \"100ms\"", ""),
        options(&dir),
        shutdown.clone(),
    )
    .await
    .unwrap();
    until("Property polled", async || {
        property.load(Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(member.load(Ordering::SeqCst), 0);

    // A new feed
    let s = daemon
        .reload(&config(
            &url,
            "",
            "interval = \"100ms\"",
            "interval = \"100ms\"",
        ))
        .unwrap();
    assert_eq!(s.added, ["mls/Member"]);
    assert!(s.removed.is_empty() && s.restarted.is_empty(), "{s:?}");
    until("Member polled", async || member.load(Ordering::SeqCst) >= 2).await;
    let st = status(&daemon).await;
    assert_eq!(st["reloads"], 1);
    assert!(st["last_reload_error"].is_null());

    // Pause Property: it stops, stays listed, and the other feed goes on
    let s = daemon
        .reload(&config(
            &url,
            "",
            "interval = \"100ms\"\npaused = true",
            "interval = \"100ms\"",
        ))
        .unwrap();
    assert_eq!(s.paused, ["mls/Property"]);
    assert!(s.removed.is_empty() && s.added.is_empty(), "{s:?}");
    until("Property paused", async || {
        feed(&status(&daemon).await, "Property")["state"] == "paused"
    })
    .await;
    let runs_before = feed(&status(&daemon).await, "Property")["runs"].clone();
    // A sync in progress may finish
    tokio::time::sleep(Duration::from_millis(150)).await;
    let calls = property.load(Ordering::SeqCst);
    let members = member.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        property.load(Ordering::SeqCst),
        calls,
        "a paused feed is not polled"
    );
    assert!(
        member.load(Ordering::SeqCst) > members,
        "the other feed keeps running"
    );
    let st = status(&daemon).await;
    assert_eq!(
        feed(&st, "Property")["runs"],
        runs_before,
        "counts are kept"
    );
    let metrics = get(&daemon, "/metrics").await.1;
    assert!(
        metrics.contains(r#"reso_ingest_feed_paused{server="mls",resource="Property"} 1"#),
        "{metrics}"
    );

    // Resume it
    let s = daemon
        .reload(&config(
            &url,
            "",
            "interval = \"100ms\"",
            "interval = \"100ms\"",
        ))
        .unwrap();
    assert_eq!(s.resumed, ["mls/Property"]);
    until("Property polled again", async || {
        property.load(Ordering::SeqCst) > calls
    })
    .await;

    // A new interval restarts that feed alone; the same file again changes nothing
    let s = daemon
        .reload(&config(
            &url,
            "",
            "interval = \"200ms\"",
            "interval = \"100ms\"",
        ))
        .unwrap();
    assert_eq!(s.restarted, ["mls/Property"]);
    let s = daemon
        .reload(&config(
            &url,
            "",
            "interval = \"200ms\"",
            "interval = \"100ms\"",
        ))
        .unwrap();
    assert!(s.is_empty(), "{s:?}");
    assert_eq!(
        feed(&status(&daemon).await, "Property")["interval_secs"],
        0.2
    );

    // A server setting restarts every feed of the server
    let s = daemon
        .reload(&config(
            &url,
            "page_size = 50",
            "interval = \"200ms\"",
            "interval = \"100ms\"",
        ))
        .unwrap();
    assert_eq!(s.restarted, ["mls/Member", "mls/Property"]);

    // Remove Member's interval: no longer scheduled, no longer listed
    let s = daemon
        .reload(&config(&url, "page_size = 50", "interval = \"200ms\"", ""))
        .unwrap();
    assert_eq!(s.removed, ["mls/Member"]);
    let st = status(&daemon).await;
    assert_eq!(st["feeds"].as_array().unwrap().len(), 1, "{st}");
    let members = member.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        member.load(Ordering::SeqCst) <= members + 1,
        "a removed feed stops"
    );

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("the daemon stops after the shutdown request");
    assert!(
        daemon.reload(&config(&url, "", "", "")).is_err(),
        "no reload after shutdown"
    );
}

#[tokio::test]
async fn a_daemon_can_start_with_every_feed_paused_and_a_bad_server_keeps_running() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    std::env::remove_var("SERVE_TEST_UNSET");
    let (srv, property, _) = server(false).await;
    let dir = TempDir::new("allpaused");
    let url = srv.uri();
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(
        config(&url, "paused = true", "interval = \"100ms\"", ""),
        options(&dir),
        shutdown.clone(),
    )
    .await
    .unwrap();
    assert_eq!(feed(&status(&daemon).await, "Property")["state"], "paused");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(property.load(Ordering::SeqCst), 0);

    let s = daemon
        .reload(&config(&url, "", "interval = \"100ms\"", ""))
        .unwrap();
    assert_eq!(s.resumed, ["mls/Property"]);
    until("Property polled", async || {
        property.load(Ordering::SeqCst) >= 1
    })
    .await;

    // A changed server whose secret is missing: an error, and the running feed is kept
    let broken = config(&url, "page_size = 5", "interval = \"100ms\"", "")
        .servers
        .into_iter()
        .map(|mut s| {
            s.auth = reso_ingest::AuthConfig::Bearer {
                token_env: Some("SERVE_TEST_UNSET".into()),
                token_file: None,
            };
            s
        })
        .collect();
    let mut bad = config(&url, "", "", "");
    bad.servers = broken;
    let e = daemon.reload(&bad).unwrap_err().to_string();
    assert!(e.contains("SERVE_TEST_UNSET"), "{e}");
    let st = status(&daemon).await;
    assert!(st["last_reload_error"]
        .as_str()
        .unwrap()
        .contains("SERVE_TEST_UNSET"));
    let calls = property.load(Ordering::SeqCst);
    until("Property still polled", async || {
        property.load(Ordering::SeqCst) > calls
    })
    .await;

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .unwrap();
}

#[tokio::test]
async fn serve_command_reloads_its_config_directory() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, property, member) = server(false).await;
    let dir = TempDir::new("cmdreload");
    let conf = dir.0.join("servers.d");
    std::fs::create_dir(&conf).unwrap();
    let write = |property: &str, member: &str| {
        let text = format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"SERVE_TEST_TOKEN\"\n[server.resources.Property]\n{property}\n[server.resources.Member]\n{member}\n",
            srv.uri()
        );
        std::fs::write(conf.join("mls.toml"), text).unwrap();
    };
    write("interval = \"100ms\"", "");
    // A free port
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let shutdown = Shutdown::new();
    let args: Vec<String> = [
        "serve",
        "--config",
        conf.to_str().unwrap(),
        "--out",
        dir.0.join("out").to_str().unwrap(),
        "--listen",
        &format!("127.0.0.1:{port}"),
        "--reload-every",
        "100ms",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let command = reso_ingest::cli::run_with_shutdown(&args, &mut out, &mut err, shutdown.clone());
    let driver = async {
        until("Property polled", async || {
            property.load(Ordering::SeqCst) >= 1
        })
        .await;

        write(
            "interval = \"100ms\"\npaused = true",
            "interval = \"100ms\"",
        );
        until("Member added by the reload", async || {
            member.load(Ordering::SeqCst) >= 1
        })
        .await;
        let st: Value = serde_json::from_str(
            &reqwest::get(format!("http://127.0.0.1:{port}/status"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(feed(&st, "Property")["state"], "paused", "{st}");

        // A broken file is logged and the running configuration kept
        std::fs::write(conf.join("mls.toml"), "[[server]\nbroken").unwrap();
        let members = member.load(Ordering::SeqCst);
        until("Member still polled", async || {
            member.load(Ordering::SeqCst) > members + 1
        })
        .await;
        shutdown.trigger();
    };
    let (code, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(command, driver)
    })
    .await
    .unwrap();
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, 0, "{err}");
}

/// `serve --ui` (#161): the pages show ok, failing and paused feeds, escape what servers send and
/// are not there without `ui`
#[cfg(feature = "ui")]
#[tokio::test]
async fn status_pages_show_every_feed_and_escape_server_text() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            let path = req.url.path();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            if path.ends_with("/Member") {
                return ResponseTemplate::new(500).set_body_string("<script>alert(1)</script>");
            }
            if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
            }
            ResponseTemplate::new(200).set_body_json(
                json!({"value": [{"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"}]}),
            )
        })
        .mount(&srv)
        .await;
    let dir = TempDir::new("ui");
    let config = Config::parse(&format!(
        r#"
[[server]]
name = "mls"
base_url = "{}"
[server.limits]
max_retries = 0
[server.auth]
type = "bearer"
token_env = "SERVE_TEST_TOKEN"
[server.resources.Property]
interval = "300ms"
[server.resources.Member]
interval = "300ms"
[server.resources.Office]
interval = "1h"
paused = true
"#,
        srv.uri()
    ))
    .unwrap();
    let mut opts = options(&dir);
    opts.ui = true;
    opts.grafana_url = Some("http://127.0.0.1:3000/".into());
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(config.clone(), opts, shutdown.clone())
        .await
        .unwrap();
    until("both feeds ran", async || {
        let s = status(&daemon).await;
        feed(&s, "Member")["runs"].as_u64() >= Some(1)
            && feed(&s, "Property")["runs"].as_u64() >= Some(1)
    })
    .await;
    let error = feed(&status(&daemon).await, "Member")["last_error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.contains("<script>"),
        "the error carries the server's text: {error}"
    );

    let url = format!("http://{}/", daemon.local_addr());
    let r = reqwest::get(&url).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");
    let index = r.text().await.unwrap();
    assert!(index.contains("3 feeds"), "{index}");
    assert!(index.contains("1 failing</span>, 1 paused"), "{index}");
    assert!(
        index.contains(r#"<tr class="fail"><td><a href="/feed/mls/Member">"#),
        "{index}"
    );
    assert!(
        index.contains(r#"<tr class="paused"><td><a href="/feed/mls/Office">"#),
        "{index}"
    );
    assert!(
        index.contains(r#"<tr><td><a href="/feed/mls/Property">"#),
        "{index}"
    );
    assert!(index.contains(r#"http-equiv="refresh" content="15""#));
    // --grafana-url: each row links to the kit's dashboard filtered to the feed
    assert!(index.contains("<th>History</th>"), "{index}");
    assert!(
        index.contains(
            r#"<td><a href="http://127.0.0.1:3000/d/reso-ingest-serve?var-server=mls&amp;var-resource=Property" rel="noreferrer">Grafana</a></td></tr>"#
        ),
        "{index}"
    );

    let (code, member) = get(&daemon, "/feed/mls/Member").await;
    assert_eq!(code, 200);
    assert!(
        member.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{member}"
    );
    assert!(!member.contains("<script>"), "{member}");
    assert!(member.contains("<td>failed</td>"), "{member}");
    assert!(
        member.contains("<dt>History</dt><dd><a href=\"http://127.0.0.1:3000/d/reso-ingest-serve?var-server=mls&amp;var-resource=Member\""),
        "{member}"
    );

    let (_, property) = get(&daemon, "/feed/mls/Property").await;
    assert!(
        property.contains("2026-01-01T00:00:01Z"),
        "checkpoint shown: {property}"
    );
    assert!(property.contains("<td>ok</td>"), "{property}");

    assert_eq!(get(&daemon, "/feed/mls/Nope").await.0, 404);
    let (code, runs) = get(&daemon, "/runs").await;
    assert_eq!(code, 200);
    assert!(
        runs.contains(r#"href="/feed/mls/Member""#)
            && runs.contains(r#"href="/feed/mls/Property""#),
        "{runs}"
    );
    assert!(!runs.contains("<script>"), "{runs}");
    shutdown.trigger();
    daemon.wait().await;

    // Without `ui` the pages are not routed
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(config, options(&dir), shutdown.clone())
        .await
        .unwrap();
    for path in ["/", "/runs", "/feed/mls/Property"] {
        assert_eq!(get(&daemon, path).await.0, 404, "{path}");
    }
    assert_eq!(get(&daemon, "/status").await.0, 200);
    shutdown.trigger();
    daemon.wait().await;
}

/// `--grafana-url` needs `ui` and an http(s) URL: anything else is a start-up error, so a
/// `javascript:` URL never reaches a page
#[tokio::test]
async fn a_bad_grafana_url_is_a_startup_error() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, _, _) = server(false).await;
    let dir = TempDir::new("grafana");
    for (ui, url, want) in [
        (false, "http://127.0.0.1:3000", "needs --ui"),
        (true, "javascript:alert(1)", "http:// or https://"),
        (true, "127.0.0.1:3000", "http:// or https://"),
    ] {
        if ui && !cfg!(feature = "ui") {
            continue;
        }
        let mut opts = options(&dir);
        opts.ui = ui;
        opts.grafana_url = Some(url.into());
        let e = Daemon::start(
            config(&srv.uri(), "", "interval = \"1h\"", ""),
            opts,
            Shutdown::new(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(e.contains(want), "{url}: {e}");
    }
}

/// Without the `ui` feature, `--ui` is a start-up error rather than a silent no-op
#[cfg(not(feature = "ui"))]
#[tokio::test]
async fn ui_without_the_feature_is_a_startup_error() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, _, _) = server(false).await;
    let dir = TempDir::new("noui");
    let mut opts = options(&dir);
    opts.ui = true;
    let e = Daemon::start(
        config(&srv.uri(), "", "interval = \"1h\"", ""),
        opts,
        Shutdown::new(),
    )
    .await
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("`ui` feature"), "{e}");
}

/// The Prometheus + Grafana kit (`deploy/observability/`, #161) uses only series `serve` exports,
/// and a feed that succeeded has `reso_ingest_feed_last_success_timestamp_seconds`
#[tokio::test]
async fn observability_kit_uses_only_metrics_serve_exports() {
    std::env::set_var("SERVE_TEST_TOKEN", "t");
    let (srv, _, _) = server(false).await;
    let dir = TempDir::new("kit");
    let shutdown = Shutdown::new();
    let daemon = Daemon::start(
        config(&srv.uri(), "", "interval = \"100ms\"", ""),
        options(&dir),
        shutdown.clone(),
    )
    .await
    .unwrap();
    until("Property succeeded", async || {
        !feed(&status(&daemon).await, "Property")["last_success"].is_null()
    })
    .await;
    let metrics = get(&daemon, "/metrics").await.1;
    shutdown.trigger();
    daemon.wait().await;

    // `serve --grafana-url` links to this dashboard by uid with these two variables
    let dashboard: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/observability/grafana/dashboards/reso-ingest-serve.json"
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(dashboard["uid"], "reso-ingest-serve");
    let vars: Vec<&str> = dashboard["templating"]["list"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert!(
        vars.contains(&"server") && vars.contains(&"resource"),
        "{vars:?}"
    );

    let sample = metrics
        .lines()
        .find(|l| {
            l.starts_with(
                r#"reso_ingest_feed_last_success_timestamp_seconds{server="mls",resource="Property"} "#,
            )
        })
        .unwrap_or_else(|| panic!("{metrics}"));
    let t: f64 = sample.rsplit(' ').next().unwrap().parse().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!((now - t).abs() < 60.0, "{sample}");

    let mut exported: std::collections::BTreeSet<String> = metrics
        .lines()
        .filter_map(|l| l.strip_prefix("# TYPE "))
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect();
    // Only with `--media`, or with rate limits in [server.limits]
    for name in [
        "reso_ingest_rate_limit",
        "reso_ingest_rate_limit_used",
        "reso_ingest_media_files_total",
        "reso_ingest_media_bytes_total",
        "reso_ingest_media_pending",
        "reso_ingest_media_index_ok",
    ] {
        exported.insert(name.into());
    }
    let kit = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/observability");
    let dashboard: Value = serde_json::from_slice(
        &std::fs::read(kit.join("grafana/dashboards/reso-ingest-serve.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(dashboard["uid"], "reso-ingest-serve");
    let alerts = std::fs::read_to_string(kit.join("alerts.yml")).unwrap();
    for (what, text) in [("dashboard", dashboard.to_string()), ("alerts", alerts)] {
        let used: std::collections::BTreeSet<&str> = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| w.starts_with("reso_ingest_"))
            .collect();
        assert!(!used.is_empty(), "{what} uses no reso_ingest_ metric");
        for name in used {
            assert!(exported.contains(name), "{what} uses unknown metric {name}");
        }
    }
}
