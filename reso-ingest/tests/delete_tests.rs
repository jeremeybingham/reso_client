//! Delete reconciliation: key comparison and the event feed (offline, wiremock)

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use reso_client::{ClientConfig, Metadata, ResoClient};
use reso_ingest::{reconcile_deletes, CheckpointStore, EventFeed, NdjsonWriter};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-del-{tag}-{}", std::process::id()));
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

#[derive(Default)]
struct State {
    keys: Mutex<Vec<String>>,
    events: Mutex<Vec<(i64, String)>>,
    /// Fail every key-listing page after the first
    fail_listing: AtomicBool,
    /// Key-listing requests served (not counting single-key lookups)
    listings: AtomicUsize,
}

fn param(req: &wiremock::Request, name: &str) -> Option<String> {
    req.url
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.to_string())
}

/// `Property` lists keys two per page and answers `ListingKey eq 'X'` lookups; `EntityEvent`
/// lists events after the sequence named in the filter, two per page
async fn server(state: Arc<State>) -> MockServer {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let filter = param(req, "$filter").unwrap_or_default();
            let from: usize = param(req, "from").map_or(0, |v| v.parse().unwrap());
            let path = req.url.path().to_string();
            let (rows, name): (Vec<Value>, &str) = if path == "/EntityEvent" {
                let mut events = state.events.lock().unwrap().clone();
                events.sort();
                if param(req, "$orderby").is_some_and(|o| o.contains("desc")) {
                    events = events.into_iter().rev().take(1).collect();
                }
                let after: i64 = filter
                    .rsplit_once("gt ")
                    .map_or(i64::MIN, |(_, n)| n.trim().parse().unwrap());
                let rows = events
                    .iter()
                    .filter(|(n, _)| *n > after)
                    .map(|(n, k)| json!({"EntityEventSequence": n, "ResourceRecordKey": k}))
                    .collect();
                (rows, "EntityEvent")
            } else if let Some(rest) = filter.strip_prefix("ListingKey eq '") {
                let key = rest.trim_end_matches('\'');
                let rows: Vec<Value> = state
                    .keys
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|k| k.as_str() == key)
                    .map(|k| json!({"ListingKey": k}))
                    .collect();
                return ResponseTemplate::new(200).set_body_json(json!({"value": rows}));
            } else {
                state.listings.fetch_add(1, Ordering::SeqCst);
                if from > 0 && state.fail_listing.load(Ordering::SeqCst) {
                    return ResponseTemplate::new(500).set_body_string("boom");
                }
                let rows = state
                    .keys
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|k| json!({"ListingKey": k}))
                    .collect();
                (rows, "Property")
            };
            let end = (from + 2).min(rows.len());
            let mut body = json!({"value": rows[from.min(end)..end]});
            if end < rows.len() {
                body["@odata.nextLink"] = json!(format!("{base}/{name}?from={end}"));
            }
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    server
}

fn client(server: &MockServer) -> ResoClient {
    ResoClient::with_config(ClientConfig::new(server.uri(), "token")).unwrap()
}

fn set_keys(state: &State, keys: &[&str]) {
    *state.keys.lock().unwrap() = keys.iter().map(|k| k.to_string()).collect();
}

/// Every output line of the directory, parsed
fn lines(dir: &std::path::Path) -> Vec<Value> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n.ends_with(".ndjson"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .iter()
        .flat_map(|n| {
            std::fs::read_to_string(dir.join(n))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<Value>>()
        })
        .collect()
}

async fn run(
    srv: &MockServer,
    store: &CheckpointStore,
    out: &std::path::Path,
    run_id: &str,
    feed: Option<&EventFeed>,
) -> reso_ingest::ResourceEntry {
    let mut w = NdjsonWriter::new(out, "mls", "Property", run_id, 1 << 20).unwrap();
    reconcile_deletes(
        &client(srv),
        "mls",
        "Property",
        "ListingKey",
        feed,
        &mut w,
        store,
    )
    .await
}

#[tokio::test]
async fn removed_record_gives_exactly_one_tagged_delete() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2", "K3", "K4", "K5"]);
    let srv = server(state.clone()).await;
    let dir = TempDir::new("diff");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");

    let e = run(&srv, &store, &out, "r1", None).await;
    assert_eq!(
        (e.error, e.deletes),
        (None, 0),
        "the first run is a baseline"
    );
    assert!(lines(&out).is_empty());

    set_keys(&state, &["K1", "K2", "K4", "K5", "K6"]);
    let e = run(&srv, &store, &out, "r2", None).await;
    assert_eq!((e.error, e.deletes), (None, 1));
    let l = lines(&out);
    assert_eq!(l.len(), 1);
    assert_eq!(
        (l[0]["event"].as_str(), l[0]["key"].as_str()),
        (Some("delete"), Some("K3"))
    );
    assert_eq!(l[0]["source"], "mls");
    assert_eq!(l[0]["resource"], "Property");
    assert_eq!(l[0]["run_id"], "r2");
    assert!(l[0]["fetched_at"].is_string());
    assert!(l[0].get("record").is_none());

    let e = run(&srv, &store, &out, "r3", None).await;
    assert_eq!((e.error, e.deletes), (None, 0), "a delete is reported once");
    assert_eq!(lines(&out).len(), 1);
}

#[tokio::test]
async fn failed_key_pull_never_emits_deletes() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2", "K3", "K4", "K5"]);
    let srv = server(state.clone()).await;
    let dir = TempDir::new("partial");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    run(&srv, &store, &out, "r1", None).await;

    // K1 is gone, and the pull dies after its first page (which would make K3..K5 look gone too)
    set_keys(&state, &["K2", "K3", "K4", "K5"]);
    state.fail_listing.store(true, Ordering::SeqCst);
    let e = run(&srv, &store, &out, "r2", None).await;
    assert!(e.error.is_some());
    assert_eq!(e.deletes, 0);
    assert!(lines(&out).is_empty());
    assert_eq!(
        store.load_keys("mls", "Property").unwrap().unwrap().len(),
        5
    );

    state.fail_listing.store(false, Ordering::SeqCst);
    let e = run(&srv, &store, &out, "r3", None).await;
    assert_eq!((e.error, e.deletes), (None, 1));
    assert_eq!(lines(&out)[0]["key"], "K1");
}

#[tokio::test]
async fn empty_key_pull_is_refused() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2"]);
    let srv = server(state.clone()).await;
    let dir = TempDir::new("empty");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    run(&srv, &store, &out, "r1", None).await;

    set_keys(&state, &[]);
    let e = run(&srv, &store, &out, "r2", None).await;
    assert!(e.error.unwrap().contains("refusing"));
    assert!(lines(&out).is_empty());
    assert_eq!(
        store.load_keys("mls", "Property").unwrap().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn event_feed_is_used_instead_of_the_key_diff() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2", "K3", "K4"]);
    *state.events.lock().unwrap() = vec![(1, "K1".into()), (2, "K2".into())];
    let srv = server(state.clone()).await;
    let dir = TempDir::new("events");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    let feed = EventFeed::entity_event();

    let e = run(&srv, &store, &out, "r1", Some(&feed)).await;
    assert_eq!(
        (e.error, e.deletes),
        (None, 0),
        "the first run is a baseline"
    );
    assert_eq!(
        store.load_event_sequence("mls", "Property").unwrap(),
        Some(2)
    );

    // K3 deleted (after an update), K4 updated, K5 created and deleted within the window
    set_keys(&state, &["K1", "K2", "K4"]);
    *state.events.lock().unwrap() = vec![
        (1, "K1".into()),
        (2, "K2".into()),
        (3, "K3".into()),
        (4, "K4".into()),
        (5, "K3".into()),
        (6, "K5".into()),
        (7, "K4".into()),
    ];
    let e = run(&srv, &store, &out, "r2", Some(&feed)).await;
    assert_eq!(e.error, None);
    let mut keys: Vec<_> = lines(&out)
        .iter()
        .map(|l| l["key"].as_str().unwrap().to_string())
        .collect();
    keys.sort();
    assert_eq!(keys, ["K3", "K5"], "each gone record once; K4 still exists");
    assert_eq!(
        store.load_event_sequence("mls", "Property").unwrap(),
        Some(7)
    );
    assert_eq!(
        state.listings.load(Ordering::SeqCst),
        0,
        "no key pull was made"
    );

    let e = run(&srv, &store, &out, "r3", Some(&feed)).await;
    assert_eq!((e.error, e.deletes), (None, 0));
}

#[test]
fn event_feed_is_detected_from_metadata() {
    let xml = |extra: &str| {
        format!(
            r#"<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
            <Schema Namespace="org.example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
                <Property Name="ListingKey" Type="Edm.String"/></EntityType>
              {extra}
            </Schema></edmx:DataServices></edmx:Edmx>"#
        )
    };
    let with = xml(
        r#"<EntityType Name="EntityEvent"><Key><PropertyRef Name="EntityEventSequence"/></Key>
           <Property Name="EntityEventSequence" Type="Edm.Int64"/></EntityType>"#,
    );
    assert_eq!(
        EventFeed::detect(&Metadata::parse(&with).unwrap()),
        Some(EventFeed::entity_event())
    );
    assert_eq!(EventFeed::detect(&Metadata::parse(&xml("")).unwrap()), None);
    let wrong_field = xml(
        r#"<EntityType Name="EntityEvent"><Key><PropertyRef Name="Sequence"/></Key>
           <Property Name="Sequence" Type="Edm.Int64"/></EntityType>"#,
    );
    assert_eq!(
        EventFeed::detect(&Metadata::parse(&wrong_field).unwrap()),
        None
    );
}

#[test]
fn state_files_are_checked_for_owner() {
    let dir = TempDir::new("owner");
    let store = CheckpointStore::new(&dir.0);
    store.save_keys("a", "Property", &[json!("K1")]).unwrap();
    assert_eq!(
        store.load_keys("a", "Property").unwrap(),
        Some(vec![json!("K1")])
    );
    assert_eq!(store.load_keys("b", "Property").unwrap(), None);
    std::fs::write(dir.0.join("c.Property.keys.json"), "{").unwrap();
    assert!(store.load_keys("c", "Property").is_err());
}

#[tokio::test]
async fn key_pull_with_a_keyless_record_is_not_trusted() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"value": [{"ListingKey": "K1"}, {"ListingKey": null}]})),
        )
        .mount(&srv)
        .await;
    let dir = TempDir::new("keyless");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    let e = run(&srv, &store, &out, "r1", None).await;
    assert!(e.error.unwrap().contains("not trusted"));
    assert_eq!(e.deletes, 0);
    assert!(store.load_keys("mls", "Property").unwrap().is_none());
}
