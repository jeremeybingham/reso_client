//! StateStore: checkpoints, leases and media indexes work on a backend other than files

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reso_client::Checkpoint;
use reso_ingest::{
    CheckpointStore, Lease, LeaseOutcome, MediaIndex, StateStore, StoreFuture, Versioned,
};
use serde_json::json;

/// Values in memory with a counter as the version, the way a database row version works
#[derive(Debug, Default)]
struct Memory(Mutex<BTreeMap<String, (Vec<u8>, u64)>>);

impl Memory {
    fn keys(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

impl StateStore for Memory {
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Versioned>> {
        let found = self.0.lock().unwrap().get(key).map(|(b, v)| Versioned {
            bytes: b.clone(),
            version: v.to_string(),
        });
        Box::pin(async move { Ok(found) })
    }

    fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, ()> {
        let mut map = self.0.lock().unwrap();
        let next = map.get(key).map_or(1, |(_, v)| v + 1);
        map.insert(key.to_string(), (bytes.to_vec(), next));
        Box::pin(async { Ok(()) })
    }

    fn create<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, bool> {
        let mut map = self.0.lock().unwrap();
        let created = !map.contains_key(key);
        if created {
            map.insert(key.to_string(), (bytes.to_vec(), 1));
        }
        Box::pin(async move { Ok(created) })
    }

    fn replace<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        version: &'a str,
    ) -> StoreFuture<'a, bool> {
        let mut map = self.0.lock().unwrap();
        let done = match map.get_mut(key) {
            Some((b, v)) if v.to_string() == version => {
                *b = bytes.to_vec();
                *v += 1;
                true
            }
            _ => false,
        };
        Box::pin(async move { Ok(done) })
    }

    fn delete<'a>(&'a self, key: &'a str, version: Option<&'a str>) -> StoreFuture<'a, bool> {
        let mut map = self.0.lock().unwrap();
        let matches = match (map.get(key), version) {
            (Some(_), None) => true,
            (Some((_, v)), Some(want)) => v.to_string() == want,
            (None, _) => false,
        };
        if matches {
            map.remove(key);
        }
        Box::pin(async move { Ok(matches) })
    }

    fn list(&self) -> StoreFuture<'_, Vec<String>> {
        let keys = self.keys();
        Box::pin(async move { Ok(keys) })
    }

    fn location(&self, key: &str) -> String {
        format!("memory:{key}")
    }
}

#[tokio::test]
async fn checkpoint_state_lives_in_any_backend() {
    let memory = Arc::new(Memory::default());
    let store = CheckpointStore::with_store(memory.clone());
    store
        .save("mls", "Property", &Checkpoint::at("2026-01-01T00:00:00Z"))
        .await
        .unwrap();
    store
        .save_keys("mls", "Property", &[json!("K1")])
        .await
        .unwrap();
    store
        .save_event_sequence("mls", "Property", 9)
        .await
        .unwrap();
    assert_eq!(
        memory.keys(),
        [
            "mls.Property.events.json",
            "mls.Property.json",
            "mls.Property.keys.ndjson"
        ]
    );
    let listed = store.list().await.unwrap();
    assert_eq!(listed.len(), 1, "only checkpoints are listed");
    assert_eq!(listed[0].timestamp, "2026-01-01T00:00:00Z");
    assert_eq!(
        store.load_event_sequence("mls", "Property").await.unwrap(),
        Some(9)
    );

    // Errors name the backend's location of the bad value
    memory.put("mls.Member.json", b"{bad").await.unwrap();
    let err = store.load("mls", "Member").await.unwrap_err().to_string();
    assert!(err.contains("memory:mls.Member.json"), "{err}");

    store.clear("mls", "Property").await.unwrap();
    assert_eq!(store.load("mls", "Property").await.unwrap(), None);
}

#[tokio::test]
async fn leases_use_the_backends_conditional_writes() {
    let memory: Arc<dyn StateStore> = Arc::new(Memory::default());
    let ttl = Duration::from_secs(60);
    let LeaseOutcome::Acquired(lease) = Lease::acquire(&memory, "mls", "Property", ttl)
        .await
        .unwrap()
    else {
        panic!("first must acquire");
    };
    assert!(matches!(
        Lease::acquire(&memory, "mls", "Property", ttl)
            .await
            .unwrap(),
        LeaseOutcome::Held(_)
    ));
    lease.release().await;
    assert!(memory.get("mls.Property.lock").await.unwrap().is_none());

    // An expired lease from a crashed holder is taken over
    let stale = json!({"server": "mls", "resource": "Member", "pid": 1, "host": "gone",
        "token": "dead", "expires_at": 1});
    memory
        .put("mls.Member.lock", stale.to_string().as_bytes())
        .await
        .unwrap();
    assert!(matches!(
        Lease::acquire(&memory, "mls", "Member", ttl).await.unwrap(),
        LeaseOutcome::Acquired(_)
    ));
}

#[tokio::test]
async fn media_index_round_trips() {
    let memory = Memory::default();
    let mut index = MediaIndex::default();
    index.read_files.insert("a.ndjson".into());
    index.save(&memory, "mls/one").await.unwrap();
    assert_eq!(memory.keys(), ["media/mls%2Fone.json"]);
    assert_eq!(MediaIndex::load(&memory, "mls/one").await.unwrap(), index);
    assert_eq!(
        MediaIndex::load(&memory, "other").await.unwrap(),
        MediaIndex::default()
    );
}

#[tokio::test]
async fn values_copy_to_and_from_files_on_every_backend() {
    let dir = std::env::temp_dir().join(format!("reso-ingest-state-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src");
    std::fs::write(&src, b"line one\nline two\n").unwrap();
    let file_store = reso_ingest::FileStateStore::new(dir.join("state"));
    let stores: [&dyn StateStore; 2] = [&Memory::default(), &file_store];
    for store in stores {
        let dest = dir.join("dest");
        assert!(!store.get_to_file("k.keys.ndjson", &dest).await.unwrap());
        store.put_from_file("k.keys.ndjson", &src).await.unwrap();
        assert!(store.get_to_file("k.keys.ndjson", &dest).await.unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"line one\nline two\n");
        assert_eq!(
            store.get("k.keys.ndjson").await.unwrap().unwrap().bytes,
            b"line one\nline two\n"
        );
        assert_eq!(store.list().await.unwrap(), ["k.keys.ndjson"]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
