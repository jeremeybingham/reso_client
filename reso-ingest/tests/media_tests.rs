//! `reso-ingest media`: sync the Media resource, then download its files (offline, wiremock)

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reso_ingest::cli::run;
use reso_ingest::{sync_media, FileStateStore, MediaIndex, MediaOptions};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("reso-ingest-media-{tag}-{}", std::process::id()));
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

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Media"><Key><PropertyRef Name="MediaKey"/></Key>
<Property Name="MediaKey" Type="Edm.String" Nullable="false"/>
<Property Name="MediaURL" Type="Edm.String"/>
<Property Name="MediaModificationTimestamp" Type="Edm.DateTimeOffset"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Media" EntityType="org.reso.metadata.Media"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// What the fake server holds; tests change it between runs
#[derive(Default)]
struct World {
    /// Media records, in ModificationTimestamp order
    records: Vec<Value>,
    /// Whether `/files/c` works yet
    c_fixed: bool,
}

/// `$metadata`, `/Media` (records newer than the newest timestamp in `$filter`, one page) and
/// files under `/files/`: `a` (ETag "a1", 304 when it matches), `b`, `c` (404 until fixed)
async fn server(world: Arc<Mutex<World>>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(move |req: &Request| {
            let path = req.url.path();
            let w = world.lock().unwrap();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            if path == "/Media" {
                let filter = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "$filter")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                let value: Vec<&Value> = w
                    .records
                    .iter()
                    .filter(|r| {
                        let t = r["ModificationTimestamp"].as_str().unwrap();
                        !w.records.iter().any(|o| {
                            let ot = o["ModificationTimestamp"].as_str().unwrap();
                            filter.contains(ot) && t <= ot
                        })
                    })
                    .collect();
                return ResponseTemplate::new(200).set_body_json(json!({ "value": value }));
            }
            let photo = |body: &str, ctype: &str| {
                ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), ctype)
            };
            match path {
                "/files/a" => {
                    let matches = req
                        .headers
                        .get("if-none-match")
                        .is_some_and(|v| v == "\"a1\"");
                    if matches {
                        ResponseTemplate::new(304)
                    } else {
                        photo("AAAA", "image/jpeg").insert_header("ETag", "\"a1\"")
                    }
                }
                "/files/b" => photo("BB", "image/png"),
                "/files/c" if w.c_fixed => photo("C", "image/jpeg"),
                "/files/n" => photo("N", "application/octet-stream"),
                _ => ResponseTemplate::new(404),
            }
        })
        .mount(&server)
        .await;
    server
}

fn media(key: &str, url: Option<String>, media_ts: &str, ts: &str) -> Value {
    json!({
        "MediaKey": key,
        "MediaURL": url,
        "MediaModificationTimestamp": media_ts,
        "ModificationTimestamp": ts,
    })
}

fn write_config(dir: &TempDir, url: &str, env: &str) -> PathBuf {
    let text = format!(
        "[[server]]\nname = \"srv\"\nbase_url = \"{url}\"\n[server.limits]\nmax_retries = 0\n[server.media]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"{env}\"\n"
    );
    let path = dir.path("servers.toml");
    std::fs::write(&path, text).unwrap();
    path
}

async fn cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err).await;
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Run ids have one-second resolution; two syncs in the same second would share file names
async fn next_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The name a file is stored under: its SHA-256, then the extension of its content type
fn stored(body: &str, ext: &str) -> String {
    let hash = reso_ingest::sha256_hex(body.as_bytes());
    match ext {
        "" => hash,
        ext => format!("{hash}.{ext}"),
    }
}

/// `stored` for several files, in the order `files_in` lists them
fn stored_all(files: &[(&str, &str)]) -> Vec<String> {
    let mut names: Vec<String> = files.iter().map(|(b, e)| stored(b, e)).collect();
    names.sort();
    names
}

fn file_requests(requests: &[Request], path: &str) -> usize {
    requests.iter().filter(|r| r.url.path() == path).count()
}

#[tokio::test]
async fn sync_then_media_downloads_retries_and_skips_unchanged() {
    let world = Arc::new(Mutex::new(World::default()));
    let server = server(Arc::clone(&world)).await;
    let base = server.uri();
    let dir = TempDir::new("flow");
    std::env::set_var("MEDIA_TEST_TOKEN_A", "token");
    let config = write_config(&dir, &base, "MEDIA_TEST_TOKEN_A");
    let out = dir.path("out");
    let (config, out_s) = (config.to_str().unwrap(), out.to_str().unwrap());

    world.lock().unwrap().records = vec![
        media(
            "A",
            Some(format!("{base}/files/a")),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
        ),
        media(
            "B",
            Some(format!("{base}/files/b?sig=1")),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:02Z",
        ),
        media(
            "C",
            Some(format!("{base}/files/c")),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:03Z",
        ),
        media("D", None, "2026-01-01T00:00:00Z", "2026-01-01T00:00:04Z"),
    ];

    // Run 1: A and B saved, C fails (404), D has no URL
    let (code, _, err) = cli(&["sync", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 0, "{err}");
    let (code, stdout, _) = cli(&["media", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 1, "a failed file makes exit code 1: {stdout}");
    assert!(stdout.contains("2 downloaded"), "{stdout}");
    assert!(stdout.contains("1 failed"), "{stdout}");
    assert!(stdout.contains("1 without MediaURL"), "{stdout}");
    assert!(stdout.contains("C: "), "{stdout}");
    let media_dir = out.join("media").join("srv");
    assert_eq!(
        files_in(&media_dir),
        stored_all(&[("AAAA", "jpg"), ("BB", "png")])
    );
    assert_eq!(
        std::fs::read_to_string(media_dir.join(stored("AAAA", "jpg"))).unwrap(),
        "AAAA"
    );

    // Run 2: B gets a new signed URL only, A gets a new media timestamp, C's file now works
    next_second().await;
    {
        let mut w = world.lock().unwrap();
        w.c_fixed = true;
        w.records.push(media(
            "B",
            Some(format!("{base}/files/b?sig=2")),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:05Z",
        ));
        w.records.push(media(
            "A",
            Some(format!("{base}/files/a")),
            "2026-02-01T00:00:00Z",
            "2026-01-01T00:00:06Z",
        ));
    }
    let (code, _, err) = cli(&["sync", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 0, "{err}");
    let (code, stdout, _) = cli(&["media", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("1 downloaded, 1 not modified"), "{stdout}");
    assert!(stdout.contains("0 failed, 0 pending"), "{stdout}");
    assert_eq!(
        files_in(&media_dir),
        stored_all(&[("AAAA", "jpg"), ("BB", "png"), ("C", "jpg")])
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        file_requests(&requests, "/files/b"),
        1,
        "B not fetched again for a new URL"
    );
    assert_eq!(file_requests(&requests, "/files/a"), 2);
    assert_eq!(file_requests(&requests, "/files/c"), 2);

    // The index keeps the newest URL
    let index = MediaIndex::load(&FileStateStore::new(out.join("state")), "srv")
        .await
        .unwrap();
    assert!(index.entries["B"]
        .url
        .as_deref()
        .unwrap()
        .ends_with("sig=2"));

    // Run 3: nothing new, nothing fetched
    let (code, stdout, _) = cli(&["media", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("0 downloaded, 0 not modified"), "{stdout}");
    let after = server.received_requests().await.unwrap();
    assert_eq!(
        after
            .iter()
            .filter(|r| r.url.path().starts_with("/files/"))
            .count(),
        5
    );
}

#[tokio::test]
async fn limit_leaves_the_rest_for_the_next_run() {
    let world = Arc::new(Mutex::new(World::default()));
    let server = server(Arc::clone(&world)).await;
    let base = server.uri();
    let dir = TempDir::new("limit");
    std::env::set_var("MEDIA_TEST_TOKEN_B", "token");
    let config = write_config(&dir, &base, "MEDIA_TEST_TOKEN_B");
    let out = dir.path("out");
    let (config, out_s) = (config.to_str().unwrap(), out.to_str().unwrap());
    world.lock().unwrap().records = vec![
        media(
            "A",
            Some(format!("{base}/files/a")),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
        ),
        media(
            "B",
            Some(format!("{base}/files/b")),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:02Z",
        ),
        media(
            "N",
            Some(format!("{base}/files/n")),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:03Z",
        ),
    ];
    assert_eq!(
        cli(&["sync", "--config", config, "--out", out_s]).await.0,
        0
    );

    let media_dir = dir.path("files");
    let media_s = media_dir.to_str().unwrap();
    let (code, stdout, _) = cli(&[
        "media",
        "--config",
        config,
        "--out",
        out_s,
        "--media-dir",
        media_s,
        "--limit",
        "1",
    ])
    .await;
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("1 downloaded"), "{stdout}");
    assert!(stdout.contains("2 pending"), "{stdout}");

    let (code, stdout, _) = cli(&[
        "media",
        "--config",
        config,
        "--out",
        out_s,
        "--media-dir",
        media_s,
    ])
    .await;
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("2 downloaded"), "{stdout}");
    // An unknown content type gets no extension
    assert_eq!(
        files_in(&media_dir.join("srv")),
        stored_all(&[("AAAA", "jpg"), ("BB", "png"), ("N", "")])
    );
}

/// Writes a manifest and its NDJSON data file the way `sync` does
fn write_run(out: &Path, run_id: &str, lines: &[Value]) {
    std::fs::create_dir_all(out).unwrap();
    let name = format!("srv.Media.{run_id}.0001.ndjson");
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(out.join(&name), &text).unwrap();
    let manifest = json!({
        "run_id": run_id,
        "resources": [{"server": "srv", "resource": "Media", "files": [{"name": name}]}],
    });
    std::fs::write(
        out.join(format!("manifest.{run_id}.json")),
        manifest.to_string(),
    )
    .unwrap();
}

fn line(run_id: &str, record: Value) -> Value {
    json!({"source": "srv", "resource": "Media", "fetched_at": "x", "run_id": run_id, "record": record})
}

#[tokio::test]
async fn delete_event_removes_file_and_entry() {
    let world = Arc::new(Mutex::new(World::default()));
    let server = server(world).await;
    let base = server.uri();
    let dir = TempDir::new("delete");
    let options = MediaOptions {
        out: dir.path("out"),
        state: Arc::new(FileStateStore::new(dir.path("state"))),
        media_dir: dir.path("media"),
        limit: None,
    };
    let client =
        reso_client::ResoClient::with_config(reso_client::ClientConfig::new(&base, "t")).unwrap();

    write_run(
        &options.out,
        "20260101T000000Z",
        &[
            line("r1", media("A", Some(format!("{base}/files/a")), "t", "t1")),
            line("r1", media("B", Some(format!("{base}/files/b")), "t", "t2")),
        ],
    );
    let report = sync_media(&client, "srv", Default::default(), 2, &options, |_| {}).await;
    assert_eq!(report.downloaded, 2, "{report:?}");

    write_run(
        &options.out,
        "20260102T000000Z",
        &[json!({
            "source": "srv", "resource": "Media", "fetched_at": "x", "run_id": "r2",
            "event": "delete", "key": "A"
        })],
    );
    let report = sync_media(&client, "srv", Default::default(), 2, &options, |_| {}).await;
    assert_eq!(report.deleted, 1, "{report:?}");
    assert_eq!(
        report.files_read, 1,
        "the first data file is not read again"
    );
    assert_eq!(
        files_in(&options.media_dir.join("srv")),
        [stored("BB", "png")]
    );
    let index = MediaIndex::load(options.state.as_ref(), "srv")
        .await
        .unwrap();
    assert!(!index.entries.contains_key("A"));
}

#[tokio::test]
async fn damaged_index_stops_the_server_without_downloading() {
    let dir = TempDir::new("damaged");
    let options = MediaOptions {
        out: dir.path("out"),
        state: Arc::new(FileStateStore::new(dir.path("state"))),
        media_dir: dir.path("media"),
        limit: None,
    };
    let path = dir.path("state").join("media").join("srv.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{not json").unwrap();
    let client = reso_client::ResoClient::with_config(reso_client::ClientConfig::new(
        "http://127.0.0.1:1",
        "t",
    ))
    .unwrap();
    let report = sync_media(&client, "srv", Default::default(), 2, &options, |_| {}).await;
    assert!(report.error.is_some());
    assert!(!report.is_clean());
}

#[tokio::test]
async fn media_command_reports_failures_skipped_files_and_records_it_cannot_use() {
    std::env::set_var("MEDIA_TEST_TOKEN_REPORT", "t");
    let world = Arc::new(Mutex::new(World::default()));
    let server = server(world).await;
    let base = server.uri();
    let dir = TempDir::new("report");
    let config = write_config(&dir, &base, "MEDIA_TEST_TOKEN_REPORT");
    let out = dir.path("out");

    // Seven records whose files are missing (404), one without MediaURL, one without MediaKey
    let mut lines: Vec<Value> = (1..=7)
        .map(|i| {
            line(
                "r1",
                media(
                    &format!("M{i}"),
                    Some(format!("{base}/files/gone{i}")),
                    "t",
                    "t",
                ),
            )
        })
        .collect();
    lines.push(line("r1", media("NOURL", None, "t", "t")));
    lines.push(line("r1", json!({"MediaURL": format!("{base}/files/b")})));
    write_run(&out, "20260101T000000Z", &lines);
    // A Parquet data file in a later manifest is listed but not read
    std::fs::write(out.join("srv.Media.20260102T000000Z.0001.parquet"), b"PAR1").unwrap();
    std::fs::write(
        out.join("manifest.20260102T000000Z.json"),
        json!({
            "run_id": "20260102T000000Z",
            "resources": [{"server": "srv", "resource": "Media",
                           "files": [{"name": "srv.Media.20260102T000000Z.0001.parquet"}]}],
        })
        .to_string(),
    )
    .unwrap();

    let (code, stdout, stderr) = cli(&[
        "media",
        "--config",
        config.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ])
    .await;
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("7 failed"), "{stdout}");
    assert!(stdout.contains("... 2 more in the manifest"), "{stdout}");
    assert!(stdout.contains("1 without MediaURL"), "{stdout}");
    assert!(
        stdout.contains("1 non-NDJSON data files not read"),
        "{stdout}"
    );
    let manifest = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            let n = p.file_name().unwrap().to_str().unwrap();
            n.starts_with("media.") && n.ends_with(".json")
        })
        .expect("a media manifest");
    let m: Value = serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
    assert_eq!(m["servers"][0]["records_without_key"], 1, "{m}");
}

#[tokio::test]
async fn media_command_names_a_server_that_failed() {
    std::env::set_var("MEDIA_TEST_TOKEN_FAIL", "t");
    let dir = TempDir::new("cmdfail");
    let config = write_config(&dir, "http://127.0.0.1:1", "MEDIA_TEST_TOKEN_FAIL");
    let state = dir.path("state");
    let path = state.join("media").join("srv.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{not json").unwrap();
    let (code, stdout, _) = cli(&[
        "media",
        "--config",
        config.to_str().unwrap(),
        "--out",
        dir.path("out").to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
    ])
    .await;
    assert_eq!(code, 1);
    assert!(stdout.contains("[srv/media] FAILED"), "{stdout}");
}
