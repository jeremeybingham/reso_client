//! NDJSON writer, manifest and the streaming pull helper (offline, wiremock)

use reso_client::{ClientConfig, ResoClient, StandardReplication};
use reso_ingest::{pull_resource, NdjsonWriter, RecordWriter, RunManifest};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn lines(dir: &std::path::Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(name))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn tags_every_line_and_keeps_the_record() {
    let dir = TempDir::new("tag");
    let records = vec![
        json!({"ListingKey": "A1", "ListPrice": 250000.5, "Nested": {"x": [1, null, "é"]}, "Empty": null}),
        json!({"ListingKey": "A2"}),
    ];
    let mut w = NdjsonWriter::new(dir.path(), "mls-one", "Property", "run1", 1 << 20).unwrap();
    w.write_batch(&records, "2026-11-24T10:00:00Z").unwrap();
    let files = w.finish().unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "mls-one.Property.run1.0001.ndjson");
    assert_eq!(files[0].records, 2);
    let got = lines(dir.path(), &files[0].name);
    assert_eq!(got.len(), 2);
    for (line, original) in got.iter().zip(&records) {
        assert_eq!(line["source"], "mls-one");
        assert_eq!(line["resource"], "Property");
        assert_eq!(line["fetched_at"], "2026-11-24T10:00:00Z");
        assert_eq!(line["run_id"], "run1");
        assert_eq!(&line["record"], original);
    }
    let size = std::fs::metadata(dir.path().join(&files[0].name))
        .unwrap()
        .len();
    assert_eq!(files[0].bytes, size);
}

#[test]
fn rotates_without_splitting_lines() {
    let dir = TempDir::new("rotate");
    let records: Vec<Value> = (0..20)
        .map(|i| json!({"ListingKey": format!("K{i:03}")}))
        .collect();
    // Each line is about 110 bytes; a 350-byte limit holds three
    let mut w = NdjsonWriter::new(dir.path(), "s", "Property", "r", 350).unwrap();
    w.write_batch(&records[..7], "t").unwrap();
    w.write_batch(&records[7..], "t").unwrap();
    let files = w.finish().unwrap();

    assert!(
        files.len() > 3,
        "expected several files, got {}",
        files.len()
    );
    let mut seen = Vec::new();
    for f in &files {
        assert!(f.bytes <= 350, "{} is {} bytes", f.name, f.bytes);
        let got = lines(dir.path(), &f.name); // every line parses, so none is split
        assert_eq!(got.len() as u64, f.records);
        seen.extend(got.into_iter().map(|l| l["record"]["ListingKey"].clone()));
    }
    let expected: Vec<Value> = records.iter().map(|r| r["ListingKey"].clone()).collect();
    assert_eq!(seen, expected);
}

#[test]
fn an_oversize_line_gets_its_own_file() {
    let dir = TempDir::new("big");
    let big = json!({"Remarks": "x".repeat(500)});
    let mut w = NdjsonWriter::new(dir.path(), "s", "P", "r", 100).unwrap();
    w.write_batch(&[big.clone(), big], "t").unwrap();
    let files = w.finish().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.records == 1));
}

fn page(keys: &[(&str, &str)], next: Option<&str>) -> Value {
    let value: Vec<Value> = keys
        .iter()
        .map(|(k, t)| json!({"ListingKey": k, "ModificationTimestamp": t}))
        .collect();
    match next {
        Some(n) => json!({"value": value, "@odata.nextLink": n}),
        None => json!({"value": value}),
    }
}

fn client(server: &MockServer) -> ResoClient {
    ResoClient::with_config(ClientConfig::new(server.uri(), "token")).unwrap()
}

#[tokio::test]
async fn pull_streams_pages_and_the_manifest_lists_them() {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(move |req: &wiremock::Request| {
            let query = req.url.query().unwrap_or("");
            // A request with a $filter is a resume from the checkpoint: nothing newer
            let body = if query.contains("filter") {
                page(&[], None)
            } else if query.contains("page=2") {
                page(&[("C", "2026-01-01T00:00:03Z")], None)
            } else {
                page(
                    &[("A", "2026-01-01T00:00:01Z"), ("B", "2026-01-01T00:00:02Z")],
                    Some(&format!("{base}/Property?page=2")),
                )
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    let dir = TempDir::new("pull");
    let mut w = NdjsonWriter::new(dir.path(), "mls", "Property", "r1", 1 << 20).unwrap();
    let spec = StandardReplication::new("Property", "ListingKey");
    let entry = pull_resource(&client(&server), "mls", "Property", spec, &mut w).await;

    assert_eq!(entry.error, None);
    assert_eq!(entry.records, 3);
    assert_eq!(entry.checkpoint.as_ref().unwrap()["key"], "C");

    let mut manifest = RunManifest::new("r1");
    manifest.add_resource(entry);
    assert!(manifest.is_clean());
    let path = manifest.write(dir.path()).unwrap();
    let written: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(written["run_id"], "r1");
    assert_eq!(written["resources"][0]["records"], 3);
    assert_eq!(written["resources"][0]["files"][0]["records"], 3);
    assert!(written["finished_at"].is_string());
}

#[tokio::test]
async fn a_failing_server_still_gets_a_manifest() {
    let good = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            let resumed = req.url.query().unwrap_or("").contains("filter");
            let records: &[(&str, &str)] = if resumed {
                &[]
            } else {
                &[("A", "2026-01-01T00:00:01Z")]
            };
            ResponseTemplate::new(200).set_body_json(page(records, None))
        })
        .mount(&good)
        .await;
    let bad = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("no"))
        .mount(&bad)
        .await;

    let dir = TempDir::new("fail");
    let mut manifest = RunManifest::new("r2");
    for (name, server) in [("good", &good), ("bad", &bad)] {
        let mut w = NdjsonWriter::new(dir.path(), name, "Property", "r2", 1 << 20).unwrap();
        let spec = StandardReplication::new("Property", "ListingKey");
        manifest.add_resource(pull_resource(&client(server), name, "Property", spec, &mut w).await);
    }
    manifest.add_server_failure("other", "could not build client");
    assert!(!manifest.is_clean());
    let path = manifest.write(dir.path()).unwrap();

    let written: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(written["resources"][0]["records"], 1);
    assert_eq!(written["resources"][0]["error"], Value::Null);
    assert_eq!(written["resources"][1]["records"], 0);
    assert!(written["resources"][1]["error"].is_string());
    assert_eq!(written["server_failures"][0]["server"], "other");
}
