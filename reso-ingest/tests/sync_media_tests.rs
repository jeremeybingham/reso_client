//! `reso-ingest sync --media`: files downloaded page by page while the URLs are fresh (offline, wiremock)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use reso_ingest::cli::run;
use reso_ingest::{FileStateStore, MediaIndex};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "reso-ingest-syncmedia-{tag}-{}",
            std::process::id()
        ));
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
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

fn property(key: &str, ts: &str, media: Value) -> Value {
    json!({"ListingKey": key, "ModificationTimestamp": ts, "Media": media})
}

fn media(key: &str, url: &str, stamp: &str) -> Value {
    json!({"MediaKey": key, "MediaURL": url, "MediaModificationTimestamp": stamp})
}

/// `$metadata`, `/Property` (all records, then none once the filter holds the newest timestamp)
/// and single-use files: the second request for the same path answers 429
async fn server(records: Arc<Mutex<Vec<Value>>>) -> MockServer {
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    Mock::given(method("GET"))
        .respond_with(move |req: &Request| {
            let path = req.url.path().to_string();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            if path == "/Property" {
                let filter = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "$filter")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                let value: Vec<Value> = records
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| !filter.contains(r["ModificationTimestamp"].as_str().unwrap()))
                    .cloned()
                    .collect();
                let value = if filter.is_empty() {
                    value
                } else {
                    newer(&value, &filter)
                };
                return ResponseTemplate::new(200).set_body_json(json!({ "value": value }));
            }
            if path.starts_with("/files/") {
                let mut seen = seen.lock().unwrap();
                let uri = req.url.as_str().to_string();
                if seen.contains(&uri) {
                    return ResponseTemplate::new(429);
                }
                seen.push(uri);
                // The body changes with the signature, so a changed record brings a new file
                let body = format!("{path}?{}", req.url.query().unwrap_or(""));
                return ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "image/jpeg");
            }
            ResponseTemplate::new(404)
        })
        .mount(&server)
        .await;
    server
}

/// Records whose timestamp is after every timestamp that appears in `filter`
fn newer(records: &[Value], filter: &str) -> Vec<Value> {
    records
        .iter()
        .filter(|r| {
            let t = r["ModificationTimestamp"].as_str().unwrap();
            !records.iter().any(|o| {
                filter.contains(o["ModificationTimestamp"].as_str().unwrap())
                    && t <= o["ModificationTimestamp"].as_str().unwrap()
            })
        })
        .cloned()
        .collect()
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

fn file_requests(requests: &[Request], prefix: &str) -> usize {
    requests
        .iter()
        .filter(|r| r.url.path().starts_with(prefix))
        .count()
}

#[tokio::test]
async fn nested_media_are_downloaded_during_sync_and_single_use_urls_are_not_retried() {
    let records = Arc::new(Mutex::new(vec![property(
        "L1",
        "2026-01-01T00:00:01Z",
        json!([
            media("M1", "PLACEHOLDER/files/m1?sig=1", "2026-01-01T00:00:00Z"),
            media("M2", "PLACEHOLDER/files/m2?sig=1", "2026-01-01T00:00:00Z"),
        ]),
    )]));
    let server = server(Arc::clone(&records)).await;
    let base = server.uri();
    let fix = |recs: &mut Vec<Value>, sig: &str, stamp1: &str, ts: &str| {
        recs[0] = property(
            "L1",
            ts,
            json!([
                media("M1", &format!("{base}/files/m1?sig={sig}"), stamp1),
                media("M2", &format!("{base}/files/m2?sig={sig}"), "2026-01-01T00:00:00Z"),
                {"MediaKey": null, "MediaURL": "x"},
            ]),
        );
    };
    fix(
        &mut records.lock().unwrap(),
        "1",
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:01Z",
    );

    let dir = TempDir::new("nested");
    std::env::set_var("SYNC_MEDIA_TOKEN", "token");
    let config = dir.0.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"srv\"\nbase_url = \"{base}\"\n[server.limits]\nmax_retries = 0\n[server.media]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"SYNC_MEDIA_TOKEN\"\n[server.resources.Property]\nexpand = [\"Media\"]\n"
        ),
    )
    .unwrap();
    let out = dir.0.join("out");
    let (config, out_s) = (config.to_str().unwrap(), out.to_str().unwrap());

    // Run 1: both files downloaded as the page arrives
    let (code, stdout, err) = cli(&["sync", "--media", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 0, "{stdout}{err}");
    assert!(stdout.contains("2 downloaded"), "{stdout}");
    let files = out.join("media/srv");
    let index = MediaIndex::load(&FileStateStore::new(out.join("state")), "srv")
        .await
        .unwrap();
    let name = |key: &str| index.entries[key].file.clone().unwrap();
    assert_eq!(
        name("M1"),
        format!("{}.jpg", reso_ingest::sha256_hex(b"/files/m1?sig=1"))
    );
    assert_eq!(
        std::fs::read(files.join(name("M1"))).unwrap(),
        b"/files/m1?sig=1"
    );
    assert!(files.join(name("M2")).exists());
    assert!(out.read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("media.")));

    // Run 2: nothing new on the server, so no file request
    let (code, _, err) = cli(&["sync", "--media", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        file_requests(&server.received_requests().await.unwrap(), "/files/"),
        2
    );

    // Run 3: M1 changed (new stamp, new signed URL), M2 did not
    fix(
        &mut records.lock().unwrap(),
        "2",
        "2026-02-01T00:00:00Z",
        "2026-02-01T00:00:01Z",
    );
    let (code, stdout, err) = cli(&["sync", "--media", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 0, "{stdout}{err}");
    assert!(stdout.contains("1 downloaded"), "{stdout}");
    assert_eq!(
        file_requests(&server.received_requests().await.unwrap(), "/files/"),
        3
    );
    // The old file of M1 is gone, the new one and M2's remain
    assert_eq!(std::fs::read_dir(&files).unwrap().count(), 2);

    // Run 4: M1 changes again, but the host refuses its URL (single use already spent)
    {
        let mut recs = records.lock().unwrap();
        fix(
            &mut recs,
            "2",
            "2026-03-01T00:00:00Z",
            "2026-03-01T00:00:01Z",
        );
    }
    let (code, stdout, _) = cli(&["sync", "--media", "--config", config, "--out", out_s]).await;
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("1 failed"), "{stdout}");
    assert!(stdout.contains("1 pending"), "{stdout}");
    let index = MediaIndex::load(&FileStateStore::new(out.join("state")), "srv")
        .await
        .unwrap();
    assert!(index.entries["M1"].pending);
    assert!(!index.entries["M2"].pending);
}

#[tokio::test]
async fn sync_without_media_flag_downloads_nothing() {
    let records = Arc::new(Mutex::new(vec![property(
        "L1",
        "2026-01-01T00:00:01Z",
        json!([]),
    )]));
    let server = server(records).await;
    let dir = TempDir::new("off");
    std::env::set_var("SYNC_MEDIA_TOKEN_B", "token");
    let config = dir.0.join("servers.toml");
    std::fs::write(
        &config,
        format!("[[server]]\nname = \"srv\"\nbase_url = \"{}\"\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"SYNC_MEDIA_TOKEN_B\"\n", server.uri()),
    )
    .unwrap();
    let out = dir.0.join("out");
    let (code, stdout, err) = cli(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ])
    .await;
    assert_eq!(code, 0, "{stdout}{err}");
    assert!(!out.join("media").exists());
    assert!(!stdout.contains("media manifest"));
}

/// Top-level `Media` pages: records without a key or a URL are counted, and a file already
/// fetched is not fetched again when its unchanged record comes back
#[tokio::test]
async fn top_level_media_pages_count_records_without_key_or_url() {
    use reso_client::{ClientConfig, DownloadConfig, ResoClient};
    use reso_ingest::{page_pipeline, LocalMediaStore, PipelineOptions, Shutdown, MEDIA_RESOURCE};

    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"jpeg".to_vec(), "image/jpeg"))
        .mount(&srv)
        .await;
    let dir = std::env::temp_dir().join(format!("reso-page-media-top-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let state: Arc<dyn reso_ingest::StateStore> = Arc::new(FileStateStore::new(dir.join("state")));
    let client = ResoClient::with_config(ClientConfig::new(srv.uri(), "t")).unwrap();
    let url = format!("{}/a.jpg", srv.uri());
    let batch = [
        json!({"MediaKey": "A", "MediaURL": url, "MediaModificationTimestamp": "2026-01-01T00:00:00Z"}),
        json!({"MediaURL": url}),
        json!({"MediaKey": "B"}),
    ];
    let options = PipelineOptions {
        state: Arc::clone(&state),
        store: Arc::new(LocalMediaStore::new(dir.join("media").join("mls"))),
        nested_field: None,
        workers: 2,
        shutdown: Shutdown::new(),
    };
    let (mut media, workers) = page_pipeline(&client, "mls", DownloadConfig::default(), options)
        .await
        .unwrap();
    media.page(MEDIA_RESOURCE, &batch).await.unwrap();
    media.page(MEDIA_RESOURCE, &batch[..1]).await.unwrap();
    drop(media);
    let report = workers.run().await;
    assert_eq!(report.queued, 1, "{report:?}");
    assert_eq!(report.records, 4);
    assert_eq!(report.records_without_key, 1);
    assert_eq!(report.without_url, 1);
    assert_eq!(report.downloaded, 1, "{report:?}");
    assert_eq!(srv.received_requests().await.unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
