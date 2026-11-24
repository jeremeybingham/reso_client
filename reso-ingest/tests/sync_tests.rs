//! Incremental sync with JSON checkpoints (offline, wiremock)

use std::sync::{Arc, Mutex};

use reso_client::{Checkpoint, ClientConfig, ResoClient, StandardReplication};
use reso_ingest::{
    sync_resource, CheckpointStore, FileEntry, IngestError, NdjsonWriter, RecordWriter,
};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-sync-{tag}-{}", std::process::id()));
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

type Data = Arc<Mutex<Vec<(String, String)>>>;

/// A server holding `data` (sorted by timestamp, key), two records per page. A request with a
/// `$filter` starts after the last timestamp named in it, so a rerun gets only newer records.
async fn server(data: Data) -> MockServer {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let data = data.lock().unwrap().clone();
            let pairs: Vec<(String, String)> = req
                .url
                .query_pairs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let from = if let Some((_, v)) = pairs.iter().find(|(k, _)| k == "from") {
                v.parse().unwrap()
            } else if let Some((_, f)) = pairs.iter().find(|(k, _)| k == "$filter") {
                data.iter()
                    .position(|(_, ts)| ts.as_str() > latest_in(f, &data).as_str())
                    .unwrap_or(data.len())
            } else {
                0
            };
            let end = (from + 2).min(data.len());
            let value: Vec<Value> = data[from..end]
                .iter()
                .map(|(k, t)| json!({"ListingKey": k, "ModificationTimestamp": t}))
                .collect();
            let mut body = json!({"value": value});
            if end < data.len() {
                body["@odata.nextLink"] = json!(format!("{base}/Property?from={end}"));
            }
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    server
}

/// The newest known timestamp that appears in the filter text
fn latest_in(filter: &str, data: &[(String, String)]) -> String {
    data.iter()
        .map(|(_, t)| t.clone())
        .filter(|t| filter.contains(t.as_str()))
        .max()
        .unwrap_or_default()
}

fn data(n: usize) -> Data {
    Arc::new(Mutex::new(
        (1..=n)
            .map(|i| (format!("K{i}"), format!("2026-01-01T00:00:{i:02}Z")))
            .collect(),
    ))
}

fn client(server: &MockServer) -> ResoClient {
    ResoClient::with_config(ClientConfig::new(server.uri(), "token")).unwrap()
}

fn spec() -> StandardReplication {
    StandardReplication::new("Property", "ListingKey")
}

fn keys(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".ndjson"))
        .collect();
    names.sort();
    names
        .iter()
        .flat_map(|n| {
            std::fs::read_to_string(dir.join(n))
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

#[tokio::test]
async fn second_run_pulls_only_newer_records() {
    let d = data(5);
    let srv = server(d.clone()).await;
    let dir = TempDir::new("incr");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");

    let mut w = NdjsonWriter::new(&out, "mls", "Property", "r1", 1 << 20).unwrap();
    let e1 = sync_resource(
        &client(&srv),
        "mls",
        "Property",
        spec(),
        &mut w,
        &store,
        false,
    )
    .await;
    assert_eq!(e1.error, None);
    assert_eq!(e1.records, 5);
    assert_eq!(
        store.load("mls", "Property").unwrap().unwrap().key,
        Some(json!("K5"))
    );

    d.lock()
        .unwrap()
        .push(("K6".into(), "2026-01-01T00:00:06Z".into()));
    d.lock()
        .unwrap()
        .push(("K7".into(), "2026-01-01T00:00:07Z".into()));
    let mut w = NdjsonWriter::new(&out, "mls", "Property", "r2", 1 << 20).unwrap();
    let e2 = sync_resource(
        &client(&srv),
        "mls",
        "Property",
        spec(),
        &mut w,
        &store,
        false,
    )
    .await;
    assert_eq!(e2.error, None);
    assert_eq!(e2.records, 2);
    assert_eq!(keys(&out), ["K1", "K2", "K3", "K4", "K5", "K6", "K7"]);
}

/// Fails the Nth `write_batch`; counts flushes
struct FailingWriter {
    inner: NdjsonWriter,
    fail_on: usize,
    calls: usize,
}

impl RecordWriter for FailingWriter {
    fn write_batch(&mut self, records: &[Value], at: &str) -> Result<(), IngestError> {
        self.calls += 1;
        if self.calls == self.fail_on {
            return Err(IngestError::Output {
                path: "x".into(),
                message: "disk full".into(),
            });
        }
        self.inner.write_batch(records, at)
    }
    fn flush(&mut self) -> Result<(), IngestError> {
        self.inner.flush()
    }
    fn finish(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.inner.finish()
    }
}

#[tokio::test]
async fn failed_write_never_advances_the_checkpoint_and_rerun_loses_nothing() {
    let d = data(5);
    let srv = server(d).await;
    let dir = TempDir::new("fail");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");

    // Pages are K1-K2, K3-K4, K5; the second write fails
    let mut w = FailingWriter {
        inner: NdjsonWriter::new(&out, "mls", "Property", "r1", 1 << 20).unwrap(),
        fail_on: 2,
        calls: 0,
    };
    let e = sync_resource(
        &client(&srv),
        "mls",
        "Property",
        spec(),
        &mut w,
        &store,
        false,
    )
    .await;
    assert!(e.error.unwrap().contains("disk full"));
    let cp = store.load("mls", "Property").unwrap().unwrap();
    assert_eq!(
        cp.key,
        Some(json!("K2")),
        "checkpoint stays at the last written page"
    );

    let mut w = NdjsonWriter::new(&out, "mls", "Property", "r2", 1 << 20).unwrap();
    let e = sync_resource(
        &client(&srv),
        "mls",
        "Property",
        spec(),
        &mut w,
        &store,
        false,
    )
    .await;
    assert_eq!(e.error, None);
    assert_eq!(e.records, 3);
    assert_eq!(keys(&out), ["K1", "K2", "K3", "K4", "K5"]);
}

#[tokio::test]
async fn start_over_clears_the_checkpoint_and_reloads() {
    let srv = server(data(3)).await;
    let dir = TempDir::new("over");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");

    let mut w = NdjsonWriter::new(&out, "mls", "Property", "r1", 1 << 20).unwrap();
    sync_resource(
        &client(&srv),
        "mls",
        "Property",
        spec(),
        &mut w,
        &store,
        false,
    )
    .await;
    let mut w = NdjsonWriter::new(&out, "mls", "Property", "r2", 1 << 20).unwrap();
    let e = sync_resource(
        &client(&srv),
        "mls",
        "Property",
        spec(),
        &mut w,
        &store,
        true,
    )
    .await;
    assert_eq!(e.records, 3);
    assert_eq!(e.error, None);
}

#[tokio::test]
async fn damaged_checkpoint_is_an_error_not_a_full_reload() {
    let srv = server(data(3)).await;
    let dir = TempDir::new("bad");
    let store = CheckpointStore::new(dir.0.join("cp"));
    std::fs::create_dir_all(dir.0.join("cp")).unwrap();
    std::fs::write(dir.0.join("cp/mls.Property.json"), "{ not json").unwrap();

    let mut w = NdjsonWriter::new(dir.0.join("out"), "mls", "Property", "r1", 1 << 20).unwrap();
    let e = sync_resource(
        &client(&srv),
        "mls",
        "Property",
        spec(),
        &mut w,
        &store,
        false,
    )
    .await;
    assert!(e.error.unwrap().contains("invalid checkpoint"));
    assert_eq!(e.records, 0);
}

#[test]
fn store_is_per_server_and_resource_and_leaves_no_temp_file() {
    let dir = TempDir::new("store");
    let store = CheckpointStore::new(&dir.0);
    store
        .save("a", "Property", &Checkpoint::new("t1", json!("K1")))
        .unwrap();
    store.save("b", "Property", &Checkpoint::at("t2")).unwrap();
    assert_eq!(
        store.load("a", "Property").unwrap().unwrap().timestamp,
        "t1"
    );
    assert_eq!(store.load("b", "Property").unwrap().unwrap().key, None);
    assert!(store.load("a", "Member").unwrap().is_none());
    store
        .save("a", "Property", &Checkpoint::new("t3", json!(7)))
        .unwrap();
    assert_eq!(
        store.load("a", "Property").unwrap().unwrap().key,
        Some(json!(7))
    );
    let tmp = std::fs::read_dir(&dir.0)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        })
        .count();
    assert_eq!(tmp, 0);
    store.clear("a", "Property").unwrap();
    store.clear("a", "Property").unwrap();
    assert!(store.load("a", "Property").unwrap().is_none());
}
