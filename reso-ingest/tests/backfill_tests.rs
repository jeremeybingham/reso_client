//! `sync --backfill` (#88): progress, stop and resume, windows, and incremental syncs alongside
//! (offline, wiremock)

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reso_ingest::cli::{run, run_with_shutdown, EXIT_INTERRUPTED};
use reso_ingest::{Config, Shutdown};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("reso-ingest-backfill-{tag}-{}", std::process::id()));
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
<EntityType Name="Member"><Key><PropertyRef Name="MemberKey"/></Key>
<Property Name="MemberKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C">
<EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>
<EntitySet Name="Member" EntityType="org.reso.metadata.Member"/>
</EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

const PROPERTIES: usize = 60;
const MEMBERS: usize = 5;
const PAGE: usize = 10;

/// Timestamp of record `i`: one per minute, so each is unique and sorts as text
fn ts(i: usize) -> String {
    format!("2026-01-01T{:02}:{:02}:00Z", i / 60, i % 60)
}

/// The indexes of the records `filter` lets through. Understands the terms the crate sends on
/// `ModificationTimestamp` (`ge`, `lt`, `gt`; `eq` only appears beside a `gt` on the same, unique
/// timestamp, so it is skipped), all joined by `and`.
fn matching(filter: Option<&str>, n: usize) -> Vec<usize> {
    let Some(f) = filter else {
        return (0..n).collect();
    };
    let words: Vec<String> = f
        .replace(['(', ')'], " ")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut terms = Vec::new();
    for w in words.windows(3) {
        if w[0] == "ModificationTimestamp" && ["ge", "lt", "gt"].contains(&w[1].as_str()) {
            terms.push((w[1].clone(), w[2].clone()));
        }
    }
    (0..n)
        .filter(|&i| {
            let t = ts(i);
            terms.iter().all(|(op, v)| match op.as_str() {
                "ge" => t >= *v,
                "lt" => t < *v,
                _ => t > *v,
            })
        })
        .collect()
}

struct Fake {
    server: MockServer,
    /// `Property` page requests answered
    property_pages: Arc<AtomicUsize>,
}

/// `Property` (60 records) and `Member` (5), ten per page, `$count` answered. `Property` pages wait
/// `delay`; the `stop_at`th `Property` page request triggers `shutdown`.
async fn fake(delay: Duration, shutdown: Option<(Shutdown, usize)>) -> Fake {
    let server = MockServer::start().await;
    let base = server.uri();
    let pages = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&pages);
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let path = req.url.path().to_string();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            let q: Vec<(String, String)> = req
                .url
                .query_pairs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let get = |name: &str| q.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            let (resource, n, key) = if path.ends_with("/Member") {
                ("Member", MEMBERS, "MemberKey")
            } else {
                ("Property", PROPERTIES, "ListingKey")
            };
            let rows = matching(get("$filter").as_deref(), n);
            if get("$count").as_deref() == Some("true") {
                return ResponseTemplate::new(200)
                    .set_body_json(json!({"@odata.count": rows.len(), "value": []}));
            }
            let mut delay_this = Duration::ZERO;
            if resource == "Property" {
                let k = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if let Some((stop, at)) = &shutdown {
                    if k == *at {
                        stop.trigger();
                    }
                }
                delay_this = delay;
            }
            let skip: usize = get("skip").map_or(0, |s| s.parse().unwrap());
            let end = (skip + PAGE).min(rows.len());
            let value: Vec<Value> = rows[skip..end]
                .iter()
                .map(|&i| json!({key: format!("{}{i:03}", &key[..1]), "ModificationTimestamp": ts(i)}))
                .collect();
            let mut body = json!({"value": value});
            if end < rows.len() {
                let mut next = req.url.clone();
                next.query_pairs_mut()
                    .clear()
                    .extend_pairs(q.iter().filter(|(k, _)| k != "skip"))
                    .append_pair("skip", &end.to_string());
                body["@odata.nextLink"] = json!(format!("{base}{}?{}", next.path(), next.query().unwrap()));
            }
            ResponseTemplate::new(200)
                .set_body_json(body)
                .set_delay(delay_this)
        })
        .mount(&server)
        .await;
    Fake {
        server,
        property_pages: pages,
    }
}

fn config(dir: &Path, base: &str, extra: &str) -> PathBuf {
    let path = dir.join("servers.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[[server]]
name = "mls"
base_url = "{base}"
page_size = {PAGE}

[server.auth]
type = "bearer"
token_env = "RESO_BACKFILL_TEST_TOKEN"

[server.limits]
per_second = 1000
{extra}
"#
        ),
    )
    .unwrap();
    std::env::set_var("RESO_BACKFILL_TEST_TOKEN", "t");
    path
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

async fn sync(extra: &[&str], conf: &Path, out: &Path) -> (i32, String, String) {
    sync_with(extra, conf, out, Shutdown::new()).await
}

async fn sync_with(
    extra: &[&str],
    conf: &Path,
    out: &Path,
    shutdown: Shutdown,
) -> (i32, String, String) {
    let mut a = args(&[
        "sync",
        "--config",
        conf.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    a.extend(args(extra));
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run_with_shutdown(&a, &mut o, &mut e, shutdown).await;
    (
        code,
        String::from_utf8(o).unwrap(),
        String::from_utf8(e).unwrap(),
    )
}

/// Every record key in the NDJSON data files of `resource` under `out`, with repeats
fn keys(out: &Path, resource: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![out.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "ndjson")
                && p.to_string_lossy().contains(&format!(".{resource}."))
            {
                for line in std::fs::read_to_string(&p).unwrap().lines() {
                    let v: Value = serde_json::from_str(line).unwrap();
                    let r = &v["record"];
                    let k = r.get("ListingKey").or_else(|| r.get("MemberKey")).unwrap();
                    found.push(k.as_str().unwrap().to_string());
                }
            }
        }
    }
    found
}

/// The manifest of the newest run in `out`
fn last_manifest(out: &Path) -> Value {
    let names: Vec<PathBuf> = std::fs::read_dir(out)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("manifest.") && n.ends_with(".json")
        })
        .collect();
    let newest = names
        .iter()
        .max_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap())
        .unwrap();
    serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap()
}

#[tokio::test]
async fn reports_progress_and_resumes_after_a_stop() {
    let dir = TempDir::new("resume");
    let out = dir.0.join("out");
    let stop = Shutdown::new();
    let f = fake(Duration::ZERO, Some((stop.clone(), 3))).await;
    let conf = config(&dir.0, &f.server.uri(), "");

    let (code, _, err) =
        sync_with(&["--backfill", "--resource", "Property"], &conf, &out, stop).await;
    assert_eq!(code, EXIT_INTERRUPTED, "{err}");
    assert!(
        err.contains("[mls/Property] backfill: 60 records left, 50% of the rate limits"),
        "{err}"
    );
    assert!(
        err.contains("##progress step=1/1 label=mls/Property sub=10/60 eta="),
        "{err}"
    );
    assert!(err.contains("sub=20/60"), "{err}");
    let first = keys(&out, "Property").len();
    assert!((10..60).contains(&first), "{first}");
    let m = last_manifest(&out);
    assert_eq!(m["resources"][0]["backfill"]["remaining_at_start"], 60);
    assert_eq!(m["resources"][0]["backfill"]["rate_percent"], 50);

    // Resumed: the count is of the records left, and the feed ends complete
    let (code, _, err) = sync(&["--backfill", "--resource", "Property"], &conf, &out).await;
    assert_eq!(code, 0, "{err}");
    let left = PROPERTIES - first;
    assert!(
        err.contains(&format!("backfill: {left} records left")),
        "{err}"
    );
    assert!(err.contains(&format!("sub={left}/{left}")), "{err}");
    let all: BTreeSet<String> = keys(&out, "Property").into_iter().collect();
    assert_eq!(all.len(), PROPERTIES);
    let m = last_manifest(&out);
    assert_eq!(m["resources"][0]["backfill"]["remaining_at_start"], left);

    // The metrics file says nothing is left
    let metrics = dir.0.join("m.prom");
    let (code, _, err) = sync(
        &[
            "--backfill",
            "--resource",
            "Property",
            "--metrics-file",
            metrics.to_str().unwrap(),
        ],
        &conf,
        &out,
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&metrics).unwrap();
    assert!(
        text.contains(
            "reso_ingest_backfill_remaining_records{server=\"mls\",resource=\"Property\"} 0"
        ),
        "{text}"
    );
    assert!(
        text.contains("reso_ingest_backfill_rate_percent{server=\"mls\",resource=\"Property\"} 50")
    );
}

#[tokio::test]
async fn incremental_syncs_run_alongside_with_the_rest_of_the_rate_limits() {
    let dir = TempDir::new("alongside");
    let out = dir.0.join("out");
    // Six slow pages: the backfill runs for over a second
    let f = fake(Duration::from_millis(250), None).await;
    let conf = config(
        &dir.0,
        &f.server.uri(),
        "[server.backfill]\nrate_percent = 70",
    );

    let backfill = sync(&["--backfill", "--resource", "Property"], &conf, &out);
    let marker = out.join("state").join("mls.Property_backfill.lock");
    let alongside = async {
        // Wait for the backfill's marker lease
        for _ in 0..200 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(marker.exists(), "the backfill holds no marker");

        // An incremental sync of another resource runs now, on the rest of the limits
        let (code, out_text, err) = sync(&["--resource", "Member"], &conf, &out).await;
        assert_eq!(code, 0, "{out_text}{err}");
        assert!(
            err.contains("[mls] using 30% of the rate limits (a backfill is running)"),
            "{err}"
        );
        assert_eq!(keys(&out, "Member").len(), MEMBERS);
        // A sync of the feed being backfilled leaves it alone
        let (code, out_text, err) = sync(&["--resource", "Property"], &conf, &out).await;
        assert_eq!(code, 0, "{out_text}{err}");
        assert!(err.contains("[mls/Property] skipped"), "{err}");
        assert!(
            f.property_pages.load(Ordering::SeqCst) < PROPERTIES / PAGE,
            "the backfill finished before the incremental syncs: the test proves nothing"
        );
    };
    let ((code, _, err), ()) = tokio::join!(backfill, alongside);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("70% of the rate limits"), "{err}");
    assert!(!marker.exists(), "the marker is released");
    assert_eq!(keys(&out, "Property").len(), PROPERTIES);

    // With the backfill done, syncs use the whole limits again and carry on from its checkpoint
    let (code, _, err) = sync(&[], &conf, &out).await;
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("rate limits"), "{err}");
    assert_eq!(keys(&out, "Property").len(), PROPERTIES);
}

#[tokio::test]
async fn windows_load_their_range_and_the_last_one_hands_over() {
    let dir = TempDir::new("windows");
    let out = dir.0.join("out");
    let f = fake(Duration::ZERO, None).await;
    let conf = config(&dir.0, &f.server.uri(), "");
    let (mid, late) = (ts(20), ts(40));

    let (code, _, err) = sync(
        &[
            "--backfill",
            "--resource",
            "Property",
            "--from",
            &mid,
            "--to",
            &late,
        ],
        &conf,
        &out,
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("backfill: 20 records left"), "{err}");
    assert_eq!(keys(&out, "Property").len(), 20);
    assert!(
        !out.join("state/mls.Property.json").exists(),
        "a closed window leaves the feed alone"
    );
    let m = last_manifest(&out);
    assert_eq!(m["resources"][0]["backfill"]["from"], mid);
    assert_eq!(m["resources"][0]["backfill"]["to"], late);

    // A sync of the feed while no backfill runs is a full load as before; here, the open-ended window
    let (code, _, err) = sync(
        &["--backfill", "--resource", "Property", "--from", &late],
        &conf,
        &out,
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("incremental syncs continue from its checkpoint"),
        "{err}"
    );
    let cp: Value =
        serde_json::from_slice(&std::fs::read(out.join("state/mls.Property.json")).unwrap())
            .unwrap();
    assert_eq!(cp["timestamp"], ts(PROPERTIES - 1));

    // The earliest window, then an incremental sync that finds nothing new
    let (code, _, err) = sync(
        &["--backfill", "--resource", "Property", "--to", &mid],
        &conf,
        &out,
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let all: Vec<String> = keys(&out, "Property");
    assert_eq!(all.len(), PROPERTIES);
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), PROPERTIES);
    let (code, out_text, err) = sync(&["--resource", "Property"], &conf, &out).await;
    assert_eq!(code, 0, "{err}");
    assert!(out_text.contains("mls/Property: 0 records"), "{out_text}");
}

#[tokio::test]
async fn bad_backfill_options_are_usage_errors() {
    let dir = TempDir::new("usage");
    let conf = config(&dir.0, "http://127.0.0.1:1", "");
    let out = dir.0.join("out");
    for extra in [
        vec!["--from", "2024-01-01T00:00:00Z"],
        vec!["--backfill", "--from", "2024-01-01"],
        vec!["--backfill", "--to", "2024-01-01T00:00:00"],
        vec![
            "--backfill",
            "--from",
            "2025-01-01T00:00:00Z",
            "--to",
            "2024-01-01T00:00:00Z",
        ],
        vec!["--backfill", "--deletes", "--from", "2024-01-01T00:00:00Z"],
    ] {
        let (code, _, err) = sync(&extra, &conf, &out).await;
        assert_eq!(code, 2, "{extra:?}: {err}");
    }
    let mut o = Vec::new();
    assert_eq!(
        run(&args(&["sync", "--help"]), &mut o, &mut Vec::new()).await,
        0
    );
    assert!(String::from_utf8(o).unwrap().contains("--backfill"));
}

#[test]
fn rate_percent_is_checked() {
    let base = r#"
[[server]]
name = "mls"
base_url = "https://x.example"
[server.auth]
type = "bearer"
token_env = "T"
"#;
    for bad in ["0", "100", "250"] {
        let text = format!("{base}[server.backfill]\nrate_percent = {bad}\n");
        assert!(Config::parse(&text).is_err(), "{bad}");
    }
    let ok = Config::parse(&format!("{base}[server.backfill]\nrate_percent = 25\n")).unwrap();
    assert_eq!(ok.servers[0].backfill.rate_percent(), 25);
    assert_eq!(
        Config::parse(base).unwrap().servers[0]
            .backfill
            .rate_percent(),
        50
    );
}
