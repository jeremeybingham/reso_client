//! CheckpointStore: damaged, foreign or unreadable state files are errors, never "no checkpoint"

use std::path::PathBuf;

use reso_client::Checkpoint;
use reso_ingest::{CheckpointStore, IngestError};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-cp-{tag}-{}", std::process::id()));
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

fn is_output_error(e: &IngestError) -> bool {
    matches!(e, IngestError::Output { .. })
}

#[test]
fn names_that_sanitize_alike_do_not_read_each_others_state() {
    let dir = TempDir::new("owner");
    let store = CheckpointStore::new(&dir.0);
    // `a/b` and `a_b` share file names; the owner check keeps them apart
    store
        .save("a/b", "Property", &Checkpoint::at("2026-01-01T00:00:00Z"))
        .unwrap();
    store.save_keys("a/b", "Property", &[json!("K1")]).unwrap();
    store.save_event_sequence("a/b", "Property", 7).unwrap();

    assert_eq!(
        store.load("a/b", "Property").unwrap(),
        Some(Checkpoint::at("2026-01-01T00:00:00Z"))
    );
    assert!(is_output_error(&store.load("a_b", "Property").unwrap_err()));
    assert!(is_output_error(
        &store.load_keys("a_b", "Property").unwrap_err()
    ));
    let err = store.load_event_sequence("a_b", "Property").unwrap_err();
    assert!(err.to_string().contains("belongs to"), "{err}");
}

#[test]
fn damaged_files_are_errors() {
    let dir = TempDir::new("damaged");
    let store = CheckpointStore::new(&dir.0);
    std::fs::write(dir.0.join("s.Property.json"), "{not json").unwrap();
    std::fs::write(dir.0.join("s.Property.keys.json"), "[").unwrap();
    assert!(is_output_error(&store.load("s", "Property").unwrap_err()));
    assert!(is_output_error(
        &store.load_keys("s", "Property").unwrap_err()
    ));
    assert!(is_output_error(&store.list().unwrap_err()));
}

#[test]
fn a_directory_where_a_file_belongs_is_an_error() {
    let dir = TempDir::new("isdir");
    let store = CheckpointStore::new(&dir.0);
    std::fs::create_dir_all(dir.0.join("s.Property.json")).unwrap();
    std::fs::create_dir_all(dir.0.join("s.Property.keys.json")).unwrap();
    assert!(is_output_error(&store.load("s", "Property").unwrap_err()));
    assert!(is_output_error(
        &store.load_keys("s", "Property").unwrap_err()
    ));
    assert!(is_output_error(&store.clear("s", "Property").unwrap_err()));
    assert!(is_output_error(
        &store
            .save("s", "Property", &Checkpoint::at("2026-01-01T00:00:00Z"))
            .unwrap_err()
    ));
}

#[test]
fn a_file_where_the_state_directory_belongs_is_an_error() {
    let dir = TempDir::new("isfile");
    let path = dir.0.join("state");
    std::fs::write(&path, "").unwrap();
    let store = CheckpointStore::new(&path);
    assert!(is_output_error(&store.list().unwrap_err()));
}

#[test]
fn clear_without_a_checkpoint_is_fine_and_list_skips_other_files() {
    let dir = TempDir::new("clear");
    let store = CheckpointStore::new(&dir.0);
    store.clear("s", "Property").unwrap();
    store
        .save("s", "Property", &Checkpoint::at("2026-01-01T00:00:00Z"))
        .unwrap();
    store.save_keys("s", "Property", &[json!(1)]).unwrap();
    store.save_event_sequence("s", "Property", 1).unwrap();
    std::fs::write(dir.0.join("notes.txt"), "x").unwrap();
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].server, "s");
    store.clear("s", "Property").unwrap();
    assert_eq!(store.load("s", "Property").unwrap(), None);
}
