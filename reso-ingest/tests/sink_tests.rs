//! `FileSink`: finished files go to the sink, and a checkpoint waits for the sink (offline)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use reso_ingest::{FileSink, IngestError, LocalDirSink, NdjsonWriter, RecordWriter};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-sink-{tag}-{}", std::process::id()));
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

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Keeps delivered files in memory; can be told to refuse them
#[derive(Debug, Default)]
struct MemorySink {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    refuse: Mutex<bool>,
}

impl FileSink for MemorySink {
    fn put_file(&self, name: &str, local: &Path) -> Result<(), IngestError> {
        if *self.refuse.lock().unwrap() {
            return Err(IngestError::Output {
                path: name.to_string(),
                message: "destination unavailable".to_string(),
            });
        }
        let bytes = std::fs::read(local).unwrap();
        self.files.lock().unwrap().insert(name.to_string(), bytes);
        Ok(())
    }
    fn exists(&self, name: &str) -> Result<bool, IngestError> {
        Ok(self.files.lock().unwrap().contains_key(name))
    }
}

fn writer(dir: &TempDir, sink: &Arc<MemorySink>, max: u64) -> NdjsonWriter {
    NdjsonWriter::new(&dir.0, "mls", "Property", "r1", max)
        .unwrap()
        .with_sink(Arc::clone(sink) as Arc<dyn FileSink>)
}

#[test]
fn commit_waits_until_the_file_is_delivered() {
    let dir = TempDir::new("commit");
    let sink = Arc::new(MemorySink::default());
    let mut w = writer(&dir, &sink, 1 << 20);
    w.write_batch(&[json!({"k": 1})], "2026-11-25T00:00:00Z")
        .unwrap();
    assert!(!w.commit().unwrap(), "rows sit in an open file");
    assert!(sink.files.lock().unwrap().is_empty());
    assert_eq!(names(&dir.0), ["mls.Property.r1.0001.ndjson.part"]);

    w.flush().unwrap();
    assert!(w.commit().unwrap());
    let files = sink.files.lock().unwrap();
    let body = String::from_utf8(files["mls.Property.r1.0001.ndjson"].clone()).unwrap();
    assert_eq!(body.lines().count(), 1);
    assert!(
        names(&dir.0).is_empty(),
        "staged copy removed after delivery"
    );
}

#[test]
fn a_full_file_is_delivered_at_the_page_end_so_the_checkpoint_can_move() {
    let dir = TempDir::new("rotate");
    let sink = Arc::new(MemorySink::default());
    let mut w = writer(&dir, &sink, 150);
    let page = |from: i32| -> Vec<_> { (from..from + 3).map(|i| json!({"k": i})).collect() };
    // A page is never split: the file passes the limit, then commit delivers it
    w.write_batch(&page(0), "2026-11-25T00:00:00Z").unwrap();
    assert!(sink.files.lock().unwrap().is_empty());
    assert!(
        w.commit().unwrap(),
        "a full file is delivered at the page end"
    );
    assert_eq!(sink.files.lock().unwrap().len(), 1);
    w.write_batch(&page(3), "2026-11-25T00:00:00Z").unwrap();
    assert!(w.commit().unwrap());
    // Delete events still rotate at the limit
    let keys: Vec<_> = (0..6).map(|i| json!(i)).collect();
    w.write_deletes(&keys, "2026-11-25T00:00:00Z").unwrap();
    let entries = w.finish().unwrap();
    assert!(entries.len() > 3, "{entries:?}");
    let delivered: Vec<String> = sink.files.lock().unwrap().keys().cloned().collect();
    assert_eq!(
        delivered,
        entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>()
    );
    let files = sink.files.lock().unwrap();
    for (i, name) in delivered.iter().take(2).enumerate() {
        let body = String::from_utf8_lossy(&files[name]).into_owned();
        assert_eq!(body.lines().count(), 3, "file {i} holds one whole page");
    }
    let lines: usize = files
        .values()
        .map(|b| String::from_utf8_lossy(b).lines().count())
        .sum();
    assert_eq!(lines, 12);
}

#[test]
fn a_refusing_sink_fails_the_flush_and_keeps_the_staged_file() {
    let dir = TempDir::new("refuse");
    let sink = Arc::new(MemorySink::default());
    let mut w = writer(&dir, &sink, 1 << 20);
    w.write_batch(&[json!({"k": 1})], "2026-11-25T00:00:00Z")
        .unwrap();
    *sink.refuse.lock().unwrap() = true;
    assert!(matches!(w.flush(), Err(IngestError::Output { .. })));
    assert!(sink.files.lock().unwrap().is_empty());
    assert_eq!(names(&dir.0), ["mls.Property.r1.0001.ndjson.part"]);
}

#[test]
fn an_existing_name_is_never_overwritten() {
    let dir = TempDir::new("exists");
    let sink = Arc::new(MemorySink::default());
    sink.files
        .lock()
        .unwrap()
        .insert("mls.Property.r1.0001.ndjson".to_string(), b"old".to_vec());
    let mut w = writer(&dir, &sink, 1 << 20);
    let err = w.write_batch(&[json!({"k": 1})], "t").unwrap_err();
    assert!(err.to_string().contains("already exists"));
    assert_eq!(
        sink.files.lock().unwrap()["mls.Property.r1.0001.ndjson"],
        b"old"
    );
}

#[test]
fn local_dir_sink_copies_and_refuses_to_overwrite() {
    let staging = TempDir::new("stage");
    let dest = TempDir::new("dest");
    let sink = Arc::new(LocalDirSink::new(&dest.0).unwrap());
    let mut w = NdjsonWriter::new(&staging.0, "mls", "Property", "r1", 1 << 20)
        .unwrap()
        .with_sink(sink.clone());
    w.write_batch(&[json!({"k": 1})], "2026-11-25T00:00:00Z")
        .unwrap();
    w.finish().unwrap();
    assert_eq!(names(&dest.0), ["mls.Property.r1.0001.ndjson"]);
    assert!(names(&staging.0).is_empty());
    assert!(sink.exists("mls.Property.r1.0001.ndjson").unwrap());
    assert!(!sink.exists("other").unwrap());

    let again = staging.0.join("x");
    std::fs::write(&again, b"x").unwrap();
    assert!(sink
        .put_file("mls.Property.r1.0001.ndjson", &again)
        .is_err());
}
