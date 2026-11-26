//! Delete reconciliation: key comparison and the event feed (offline, wiremock)

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use reso_client::{ClientConfig, Metadata, ResoClient};
use reso_ingest::{reconcile_deletes, CheckpointStore, DeleteTarget, EventFeed, NdjsonWriter};
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
            } else if let Some((_, rest)) = filter.split_once("ListingKey eq '") {
                let key = rest.trim_end_matches(')').trim_end_matches('\'');
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
    run_filtered(srv, store, out, run_id, feed, None).await
}

async fn run_filtered(
    srv: &MockServer,
    store: &CheckpointStore,
    out: &std::path::Path,
    run_id: &str,
    feed: Option<&EventFeed>,
    filter: Option<&str>,
) -> reso_ingest::ResourceEntry {
    let mut w = NdjsonWriter::new(out, "mls", "Property", run_id, 1 << 20).unwrap();
    reconcile_deletes(
        &client(srv),
        "mls",
        &DeleteTarget::new("Property", "ListingKey").with_filter(filter),
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
        store
            .load_keys("mls", "Property")
            .await
            .unwrap()
            .unwrap()
            .len(),
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
        store
            .load_keys("mls", "Property")
            .await
            .unwrap()
            .unwrap()
            .len(),
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
        store.load_event_sequence("mls", "Property").await.unwrap(),
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
        store.load_event_sequence("mls", "Property").await.unwrap(),
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

#[tokio::test]
async fn state_files_are_checked_for_owner() {
    let dir = TempDir::new("owner");
    let store = CheckpointStore::new(&dir.0);
    store
        .save_keys("a", "Property", &[json!("K1")])
        .await
        .unwrap();
    assert_eq!(
        store.load_keys("a", "Property").await.unwrap(),
        Some(vec![json!("K1")])
    );
    assert_eq!(store.load_keys("b", "Property").await.unwrap(), None);
    std::fs::write(dir.0.join("c.Property.keys.json"), "{").unwrap();
    assert!(store.load_keys("c", "Property").await.is_err());
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
    assert!(store.load_keys("mls", "Property").await.unwrap().is_none());
}

#[tokio::test]
async fn server_filter_goes_on_key_lookups_but_not_on_the_event_feed() {
    const FILTER: &str = "OriginatingSystemName eq 'mls'";
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2"]);
    *state.events.lock().unwrap() = vec![(1, "K1".into())];
    let srv = server(state.clone()).await;
    let dir = TempDir::new("filtered");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    let feed = EventFeed::entity_event();

    run_filtered(&srv, &store, &out, "r1", Some(&feed), Some(FILTER)).await;
    set_keys(&state, &["K1"]);
    *state.events.lock().unwrap() = vec![(1, "K1".into()), (2, "K2".into()), (3, "K1".into())];
    let e = run_filtered(&srv, &store, &out, "r2", Some(&feed), Some(FILTER)).await;
    assert_eq!((e.error, e.deletes), (None, 1));

    let requests = srv.received_requests().await.unwrap();
    let filters = |path: &str| -> Vec<String> {
        requests
            .iter()
            .filter(|r| r.url.path() == path)
            .map(|r| param(r, "$filter").unwrap_or_default())
            .collect()
    };
    let lookups = filters("/Property");
    assert_eq!(lookups.len(), 2, "{lookups:?}");
    assert!(
        lookups
            .iter()
            .all(|f| f.starts_with("(OriginatingSystemName eq 'mls') and (ListingKey eq '")),
        "{lookups:?}"
    );
    assert!(
        filters("/EntityEvent")
            .iter()
            .all(|f| !f.contains("OriginatingSystemName")),
        "the event resource is queried without the server filter"
    );
}

fn limited(
    limits: reso_ingest::DeleteLimits,
) -> impl Fn(&DeleteTarget<'static>) -> DeleteTarget<'static> {
    move |t| t.with_limits(limits)
}

async fn run_limited(
    srv: &MockServer,
    store: &CheckpointStore,
    out: &std::path::Path,
    run_id: &str,
    feed: Option<&EventFeed>,
    limits: reso_ingest::DeleteLimits,
) -> reso_ingest::ResourceEntry {
    let mut w = NdjsonWriter::new(out, "mls", "Property", run_id, 1 << 20).unwrap();
    let target = limited(limits)(&DeleteTarget::new("Property", "ListingKey"));
    reconcile_deletes(&client(srv), "mls", &target, feed, &mut w, store).await
}

#[tokio::test]
async fn mass_key_diff_deletes_are_held_until_allowed() {
    let state = Arc::new(State::default());
    set_keys(
        &state,
        &["K1", "K2", "K3", "K4", "K5", "K6", "K7", "K8", "K9", "K10"],
    );
    let srv = server(state.clone()).await;
    let dir = TempDir::new("hold-diff");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    let percent = reso_ingest::DeleteLimits {
        max_percent: Some(50.0),
        ..Default::default()
    };

    let e = run_limited(&srv, &store, &out, "r1", None, percent).await;
    assert_eq!((e.error, e.deletes), (None, 0), "baseline");

    // Seven of ten keys vanish (an outage emptying most of the feed): held
    set_keys(&state, &["K1", "K2", "K3"]);
    let e = run_limited(&srv, &store, &out, "r2", None, percent).await;
    assert_eq!(e.error, None);
    assert_eq!((e.deletes, e.deletes_held), (0, 7));
    assert_eq!(e.warnings.len(), 1);
    assert!(
        e.warnings[0].contains("7 of 10 known keys (70.0%)")
            && e.warnings[0].contains("--allow-mass-deletes"),
        "{:?}",
        e.warnings
    );
    assert!(
        lines(&out).iter().all(|l| l["type"] != "delete"),
        "nothing written"
    );

    // The same pass finds the same deletes next time; an absolute limit holds it too
    let absolute = reso_ingest::DeleteLimits {
        max_deletes: Some(5),
        ..Default::default()
    };
    let e = run_limited(&srv, &store, &out, "r3", None, absolute).await;
    assert_eq!((e.deletes, e.deletes_held), (0, 7));

    // Within the limits they go out
    let roomy = reso_ingest::DeleteLimits {
        max_deletes: Some(7),
        max_percent: Some(70.0),
        allow: false,
    };
    let e = run_limited(&srv, &store, &out, "r4", None, roomy).await;
    assert_eq!((e.deletes, e.deletes_held, e.warnings.len()), (7, 0, 0));
}

#[tokio::test]
async fn allow_releases_held_deletes() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2", "K3", "K4"]);
    let srv = server(state.clone()).await;
    let dir = TempDir::new("hold-allow");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    let strict = reso_ingest::DeleteLimits {
        max_deletes: Some(1),
        ..Default::default()
    };
    run_limited(&srv, &store, &out, "r1", None, strict).await;
    set_keys(&state, &["K1"]);
    let e = run_limited(&srv, &store, &out, "r2", None, strict).await;
    assert_eq!((e.deletes, e.deletes_held), (0, 3));
    let allowed = reso_ingest::DeleteLimits {
        allow: true,
        ..strict
    };
    let e = run_limited(&srv, &store, &out, "r3", None, allowed).await;
    assert_eq!((e.deletes, e.deletes_held), (3, 0));
}

#[tokio::test]
async fn mass_event_deletes_are_held_and_found_again() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2", "K3"]);
    *state.events.lock().unwrap() = vec![(1, "K1".into())];
    let srv = server(state.clone()).await;
    let dir = TempDir::new("hold-events");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let out = dir.0.join("out");
    let feed = EventFeed::entity_event();
    let limit = reso_ingest::DeleteLimits {
        max_deletes: Some(1),
        ..Default::default()
    };
    run_limited(&srv, &store, &out, "r1", Some(&feed), limit).await;

    set_keys(&state, &["K1"]);
    *state.events.lock().unwrap() = vec![(1, "K1".into()), (2, "K2".into()), (3, "K3".into())];
    let e = run_limited(&srv, &store, &out, "r2", Some(&feed), limit).await;
    assert_eq!((e.error, e.deletes, e.deletes_held), (None, 0, 2));
    assert_eq!(
        store.load_event_sequence("mls", "Property").await.unwrap(),
        Some(1),
        "the sequence stays, so the next pass sees the events again"
    );
    let allowed = reso_ingest::DeleteLimits {
        allow: true,
        ..limit
    };
    let e = run_limited(&srv, &store, &out, "r3", Some(&feed), allowed).await;
    assert_eq!((e.deletes, e.deletes_held), (2, 0));
}

async fn run_target(
    srv: &MockServer,
    store: &CheckpointStore,
    out: &std::path::Path,
    run_id: &str,
    target: &DeleteTarget<'_>,
) -> reso_ingest::ResourceEntry {
    let mut w = NdjsonWriter::new(out, "mls", "Property", run_id, 1 << 20).unwrap();
    reconcile_deletes(&client(srv), "mls", target, None, &mut w, store).await
}

#[tokio::test]
async fn old_json_key_set_is_converted_once() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1", "K2"]);
    let srv = server(state.clone()).await;
    let dir = TempDir::new("convert");
    let cp = dir.0.join("cp");
    std::fs::create_dir_all(&cp).unwrap();
    let old = json!({"server": "mls", "resource": "Property", "updated_at": "2026-01-01T00:00:00Z",
        "keys": ["K3", "K1", "K1", "K2"]});
    std::fs::write(cp.join("mls.Property.keys.json"), old.to_string()).unwrap();
    let store = CheckpointStore::new(&cp);
    let out = dir.0.join("out");

    let e = run(&srv, &store, &out, "r1", None).await;
    assert_eq!((e.error, e.deletes), (None, 1));
    assert_eq!(lines(&out)[0]["key"], "K3");
    assert!(
        !cp.join("mls.Property.keys.json").exists(),
        "old file removed"
    );
    let text = std::fs::read_to_string(cp.join("mls.Property.keys.ndjson")).unwrap();
    let mut rows = text.lines();
    let header: Value = serde_json::from_str(rows.next().unwrap()).unwrap();
    assert_eq!(
        (header["server"].as_str(), header["resource"].as_str()),
        (Some("mls"), Some("Property"))
    );
    assert_eq!(rows.collect::<Vec<_>>(), ["\"K1\"", "\"K2\""]);
}

#[tokio::test]
async fn chunked_diff_matches_an_in_memory_diff() {
    use std::collections::BTreeSet;
    let before: Vec<String> = (0..120)
        .map(|i| format!("K{:03}", (i * 37) % 120))
        .collect();
    let after: Vec<String> = before
        .iter()
        .filter(|k| k[1..].parse::<u32>().unwrap() % 7 != 3)
        .cloned()
        .chain((200..215).map(|i| format!("K{i}")))
        .collect();
    let state = Arc::new(State::default());
    *state.keys.lock().unwrap() = before.clone();
    let srv = server(state.clone()).await;
    let dir = TempDir::new("chunked");
    let store = CheckpointStore::new(dir.0.join("cp"));
    let (out, scratch) = (dir.0.join("out"), dir.0.join("scratch"));
    let target = DeleteTarget::new("Property", "ListingKey")
        .with_scratch_dir(&scratch)
        .with_chunk_keys(7);
    // What a run killed two days ago left behind: the pass removes it (#134)
    let killed = scratch.join("keys-999999-0");
    std::fs::create_dir_all(&killed).unwrap();
    std::fs::write(killed.join("run-0"), "\"K1\"\n").unwrap();
    let two_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
    for p in [killed.join("run-0"), killed.clone()] {
        std::fs::File::open(&p)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
    }

    let e = run_target(&srv, &store, &out, "r1", &target).await;
    assert_eq!((e.error, e.deletes), (None, 0));
    assert!(
        !killed.exists(),
        "a killed run's scratch directory is swept"
    );
    *state.keys.lock().unwrap() = after.clone();
    let e = run_target(&srv, &store, &out, "r2", &target).await;

    let gone: BTreeSet<&String> = before.iter().filter(|k| !after.contains(k)).collect();
    let found: Vec<String> = lines(&out)
        .iter()
        .map(|l| l["key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!((e.error, e.deletes), (None, gone.len() as u64));
    assert_eq!(
        found,
        gone.into_iter().cloned().collect::<Vec<_>>(),
        "sorted, once each"
    );
    let saved = store.load_keys("mls", "Property").await.unwrap().unwrap();
    let expected: BTreeSet<&String> = after.iter().collect();
    assert_eq!(
        saved,
        expected.into_iter().map(|k| json!(k)).collect::<Vec<_>>()
    );
    assert_eq!(
        std::fs::read_dir(&scratch).unwrap().count(),
        0,
        "scratch cleaned up"
    );
}

#[tokio::test]
async fn damaged_line_key_set_is_an_error() {
    let state = Arc::new(State::default());
    set_keys(&state, &["K1"]);
    let srv = server(state.clone()).await;
    let dir = TempDir::new("damaged");
    let cp = dir.0.join("cp");
    std::fs::create_dir_all(&cp).unwrap();
    let store = CheckpointStore::new(&cp);
    let out = dir.0.join("out");
    let file = cp.join("mls.Property.keys.ndjson");
    let header =
        |server: &str| json!({"server": server, "resource": "Property", "updated_at": "x"});

    std::fs::write(&file, format!("{}\n\"K1\"\n", header("other"))).unwrap();
    let e = run(&srv, &store, &out, "r1", None).await;
    assert!(e.error.unwrap().contains("belongs to"));
    assert!(store.load_keys("mls", "Property").await.is_err());

    std::fs::write(&file, format!("{}\n\"K2\"\n\"K1\"\n", header("mls"))).unwrap();
    let e = run(&srv, &store, &out, "r2", None).await;
    assert!(e.error.unwrap().contains("not sorted"));

    std::fs::write(&file, "{").unwrap();
    let e = run(&srv, &store, &out, "r3", None).await;
    assert!(e.error.unwrap().contains("header"));
    assert!(lines(&out).is_empty());
}
