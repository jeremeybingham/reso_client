//! Media at scale (#89): many records, files stored once by content, unchanged files skipped, and a
//! slow file host that does not hold up the pages (offline, wiremock)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reso_client::{ClientConfig, DownloadConfig, ResoClient};
use reso_ingest::{
    page_pipeline, sha256_hex, FileStateStore, LocalMediaStore, MediaIndex, MediaReport,
    MediaStore, PipelineOptions, Shutdown, StateStore, MEDIA_RESOURCE,
};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "reso-ingest-media-scale-{tag}-{}",
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

/// Files whose body is the `body` query value (`/f/<n>?body=<b>`), after `delay`; `/gone/*` is 403
async fn file_host(delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(move |req: &Request| {
            if req.url.path().starts_with("/gone/") {
                return ResponseTemplate::new(403);
            }
            let body = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "body")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            ResponseTemplate::new(200)
                .set_body_raw(body.into_bytes(), "image/jpeg")
                .set_delay(delay)
        })
        .mount(&server)
        .await;
    server
}

fn record(base: &str, key: usize, body: &str, stamp: &str) -> Value {
    json!({
        "MediaKey": format!("K{key}"),
        "MediaURL": format!("{base}/f/{key}?body={body}&stamp={stamp}"),
        "MediaModificationTimestamp": stamp,
    })
}

struct Setup {
    client: ResoClient,
    state: Arc<dyn StateStore>,
    store: Arc<dyn MediaStore>,
    _dir: TempDir,
}

fn setup(tag: &str, base: &str) -> Setup {
    let dir = TempDir::new(tag);
    Setup {
        client: ResoClient::with_config(ClientConfig::new(base, "t")).unwrap(),
        state: Arc::new(FileStateStore::new(dir.0.join("state"))),
        store: Arc::new(LocalMediaStore::new(dir.0.join("media"))),
        _dir: dir,
    }
}

/// Feed `records` in pages of 50 and run the workers until the queue is empty
async fn run(s: &Setup, records: &[Value], workers: usize) -> MediaReport {
    let options = PipelineOptions {
        state: Arc::clone(&s.state),
        store: Arc::clone(&s.store),
        nested_field: None,
        workers,
        shutdown: Shutdown::new(),
    };
    let (mut media, pool) = page_pipeline(&s.client, "mls", DownloadConfig::default(), options)
        .await
        .unwrap();
    let pages = async {
        for page in records.chunks(50) {
            media.page(MEDIA_RESOURCE, page).await.unwrap();
        }
        drop(media);
    };
    let ((), report) = tokio::join!(pages, pool.run());
    report
}

async fn file_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/f/"))
        .count()
}

#[tokio::test]
async fn many_records_fetch_changed_files_once_and_store_each_content_once() {
    let host = file_host(Duration::ZERO).await;
    let base = host.uri();
    let s = setup("many", &base);
    let t1 = "2026-01-01T00:00:00Z";
    // 400 records; every fourth one shows the same photo as the record before it
    let body = |i: usize| match i % 4 {
        3 => format!("photo-{}", i - 1),
        _ => format!("photo-{i}"),
    };
    let records: Vec<Value> = (0..400).map(|i| record(&base, i, &body(i), t1)).collect();

    // Run 1: every record fetched once, the 100 repeated photos stored once
    let report = run(&s, &records, 8).await;
    assert_eq!(report.records, 400);
    assert_eq!(report.queued, 400, "{report:?}");
    assert_eq!(report.downloaded + report.shared, 400, "{report:?}");
    assert_eq!(report.downloaded, 300, "{report:?}");
    assert_eq!(report.shared, 100, "{report:?}");
    assert!(
        report.failed.is_empty() && report.pending == 0,
        "{report:?}"
    );
    assert_eq!(file_requests(&host).await, 400);
    let stored = s.store.list().await.unwrap();
    assert_eq!(stored.len(), 300);
    let index = MediaIndex::load(s.state.as_ref(), "mls").await.unwrap();
    assert_eq!(
        index.entries["K3"].file.as_deref(),
        Some(format!("{}.jpg", sha256_hex(b"photo-2")).as_str())
    );
    assert_eq!(index.entries["K3"].file, index.entries["K2"].file);

    // Run 2: the same records again: nothing is queued or fetched
    let report = run(&s, &records, 8).await;
    assert_eq!(report.queued, 0, "{report:?}");
    assert_eq!(file_requests(&host).await, 400);

    // Run 3: 40 records get a new stamp; half keep their photo, half get a new one
    let t2 = "2026-02-01T00:00:00Z";
    let mut changed = records.clone();
    for i in (0..80).step_by(2) {
        let new_body = match i % 4 {
            0 => body(i),
            _ => format!("new-{i}"),
        };
        changed[i] = record(&base, i, &new_body, t2);
    }
    let report = run(&s, &changed, 8).await;
    assert_eq!(report.queued, 40, "{report:?}");
    assert_eq!(
        report.unchanged, 20,
        "same bytes under a new stamp: {report:?}"
    );
    assert_eq!(report.downloaded, 20, "{report:?}");
    assert_eq!(
        file_requests(&host).await,
        440,
        "each changed file fetched once"
    );
    // The replaced photos are still shown by the odd key after each one, so all are kept
    assert_eq!(s.store.list().await.unwrap().len(), 320);
    let index = MediaIndex::load(s.state.as_ref(), "mls").await.unwrap();
    let mut names: Vec<_> = index
        .entries
        .values()
        .filter_map(|e| e.file.clone())
        .collect();
    names.sort();
    names.dedup();
    let stored: Vec<_> = s
        .store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(stored, names, "only files a record points at are kept");
}

#[tokio::test]
async fn a_slow_file_host_does_not_hold_up_the_pages() {
    let host = file_host(Duration::from_millis(400)).await;
    let base = host.uri();
    let s = setup("slow", &base);
    let records: Vec<Value> = (0..200)
        .map(|i| record(&base, i, &format!("p{i}"), "2026-01-01T00:00:00Z"))
        .collect();
    let options = PipelineOptions {
        state: Arc::clone(&s.state),
        store: Arc::clone(&s.store),
        nested_field: None,
        workers: 2,
        shutdown: Shutdown::new(),
    };
    let (mut media, pool) = page_pipeline(&s.client, "mls", DownloadConfig::default(), options)
        .await
        .unwrap();
    let start = Instant::now();
    let pages = async {
        for page in records.chunks(20) {
            media.page(MEDIA_RESOURCE, page).await.unwrap();
        }
        let pages_done = start.elapsed();
        drop(media);
        pages_done
    };
    // Stop the workers after the pages are in, so the test does not wait for 200 slow files
    let shutdown_later = async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
    };
    let pool_run = async {
        tokio::select! {
            report = pool.run() => Some(report),
            () = shutdown_later => None,
        }
    };
    let (pages_done, report) = tokio::join!(pages, pool_run);
    // Ten pages took in while at most two files at a time crawl in at 400 ms each
    assert!(
        pages_done < Duration::from_millis(400),
        "pages waited for downloads: {pages_done:?}"
    );
    assert!(
        report.is_none(),
        "200 files at 400 ms on 2 workers cannot finish in 1.5 s"
    );
    // What was queued and not fetched stays pending in the saved index
    let index = MediaIndex::load(s.state.as_ref(), "mls").await.unwrap();
    assert_eq!(index.entries.len(), 200);
    assert!(index.entries.values().filter(|e| e.pending).count() > 150);
}

#[tokio::test]
async fn refused_urls_count_as_expired_and_stay_pending() {
    let host = file_host(Duration::ZERO).await;
    let base = host.uri();
    let s = setup("expired", &base);
    let records = vec![
        json!({"MediaKey": "G", "MediaURL": format!("{base}/gone/g"), "MediaModificationTimestamp": "t"}),
        record(&base, 1, "ok", "t"),
    ];
    let report = run(&s, &records, 2).await;
    assert_eq!(report.downloaded, 1, "{report:?}");
    assert_eq!(report.expired, 1, "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.pending, 1, "{report:?}");
    let index = MediaIndex::load(s.state.as_ref(), "mls").await.unwrap();
    assert!(index.entries["G"].pending);
}
