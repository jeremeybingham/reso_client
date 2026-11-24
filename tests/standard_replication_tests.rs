// Integration tests for RESO-standard replication (#20). All data is invented.
//
// `FakeServer` answers like a RESO server over an in-memory resource: it understands the
// filter shapes this crate emits, orders by (timestamp, key), caps `$top`, and can drop
// `@odata.nextLink`, ignore `$orderby` or ignore `$filter`.

use std::sync::{Arc, Mutex};

use reso_client::{Checkpoint, ClientConfig, ResoClient, ResoError, StandardReplication};
use serde_json::{json, Value};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Clone)]
struct FakeServer {
    /// (timestamp, key)
    data: Vec<(String, String)>,
    cap: usize,
    next_links: bool,
    ignore_orderby: bool,
    ignore_filter: bool,
    urls: Arc<Mutex<Vec<String>>>,
}

fn param(req: &Request, name: &str) -> Option<String> {
    req.url
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// The text between the first `prefix` and the next space or `)`.
fn token_after(s: &str, prefix: &str) -> Option<String> {
    let rest = &s[s.find(prefix)? + prefix.len()..];
    if let Some(quoted) = rest.strip_prefix('\'') {
        let end = quoted.find('\'')?;
        return Some(quoted[..end].to_string());
    }
    Some(rest.split([' ', ')']).next()?.to_string())
}

impl Respond for FakeServer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.urls.lock().unwrap().push(req.url.to_string());
        let mut rows = self.data.clone();
        if !self.ignore_orderby {
            rows.sort();
        }
        if !self.ignore_filter {
            if let Some(f) = param(req, "$filter") {
                let ts = token_after(&f, "ModificationTimestamp gt ").unwrap();
                match token_after(&f, "ListingKey gt ") {
                    Some(key) => rows.retain(|(t, k)| *t > ts || (*t == ts && *k > key)),
                    None => rows.retain(|(t, _)| *t > ts),
                }
            }
        }
        let skip: usize = param(req, "$skip").map_or(0, |s| s.parse().unwrap());
        let top: usize = param(req, "$top").map_or(self.cap, |s| s.parse().unwrap());
        let size = top.min(self.cap);
        let page: Vec<Value> = rows
            .iter()
            .skip(skip)
            .take(size)
            .map(|(t, k)| json!({"ListingKey": k, "ModificationTimestamp": t}))
            .collect();
        let mut body = json!({ "value": page });
        if self.next_links && skip + size < rows.len() {
            let mut url = req.url.clone();
            let host = req.headers.get("host").unwrap().to_str().unwrap();
            let (h, p) = host.split_once(':').unwrap();
            url.set_host(Some(h)).unwrap();
            url.set_port(Some(p.parse().unwrap())).unwrap();
            let pairs: Vec<_> = req
                .url
                .query_pairs()
                .filter(|(k, _)| k != "$skip")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            url.query_pairs_mut()
                .clear()
                .extend_pairs(pairs)
                .append_pair("$skip", &(skip + size).to_string());
            body["@odata.nextLink"] = json!(url.to_string());
        }
        ResponseTemplate::new(200).set_body_json(body)
    }
}

fn server_data(spec: &[(&str, usize)]) -> Vec<(String, String)> {
    // (timestamp, how many records share it); keys are zero-padded so they sort.
    let mut out = Vec::new();
    let mut n = 0;
    for (ts, count) in spec {
        for _ in 0..*count {
            n += 1;
            out.push((ts.to_string(), format!("L{n:03}")));
        }
    }
    out
}

fn fake(data: Vec<(String, String)>, cap: usize) -> FakeServer {
    FakeServer {
        data,
        cap,
        next_links: true,
        ignore_orderby: false,
        ignore_filter: false,
        urls: Arc::default(),
    }
}

async fn start(fake: FakeServer) -> (MockServer, ResoClient) {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(fake)
        .mount(&server)
        .await;
    let client = ResoClient::with_config(ClientConfig::new(server.uri(), "t")).unwrap();
    (server, client)
}

async fn pull(client: &ResoClient, spec: StandardReplication) -> Result<Vec<String>, ResoError> {
    let mut run = client.replicate(spec);
    let mut keys = Vec::new();
    while let Some(batch) = run.next_batch().await? {
        keys.extend(
            batch
                .iter()
                .map(|r| r["ListingKey"].as_str().unwrap().to_string()),
        );
    }
    Ok(keys)
}

fn spec() -> StandardReplication {
    StandardReplication::new("Property", "ListingKey")
}

#[tokio::test]
async fn pulls_all_pages_in_timestamp_and_key_order() {
    let data = server_data(&[
        ("2026-01-01T00:00:03.000Z", 2),
        ("2026-01-01T00:00:01.000Z", 3),
        ("2026-01-01T00:00:02.000Z", 2),
    ]);
    let mut expected = data.clone();
    expected.sort();
    let (_s, client) = start(fake(data, 100)).await;
    let keys = pull(&client, spec().page_size(3)).await.unwrap();
    let want: Vec<_> = expected.into_iter().map(|(_, k)| k).collect();
    assert_eq!(keys, want);
}

#[tokio::test]
async fn many_records_with_one_timestamp_are_not_skipped_or_repeated() {
    // 9 records share one timestamp; pages of 4 split them twice, with and without next links.
    let data = server_data(&[
        ("2026-01-01T00:00:00.000Z", 9),
        ("2026-01-01T00:00:01.000Z", 2),
    ]);
    let want: Vec<_> = {
        let mut d = data.clone();
        d.sort();
        d.into_iter().map(|(_, k)| k).collect()
    };
    for next_links in [true, false] {
        let mut f = fake(data.clone(), 4);
        f.next_links = next_links;
        let (_s, client) = start(f).await;
        let keys = pull(&client, spec().page_size(4)).await.unwrap();
        assert_eq!(keys, want, "next_links={next_links}");
    }
}

#[tokio::test]
async fn server_cap_below_requested_page_size_still_completes() {
    let data = server_data(&[
        ("2026-01-01T00:00:00.000Z", 5),
        ("2026-01-01T00:00:01.000Z", 5),
    ]);
    let mut f = fake(data, 2);
    f.next_links = false; // silent truncation: the run must re-query from its checkpoint
    let (_s, client) = start(f).await;
    let keys = pull(&client, spec().page_size(1000)).await.unwrap();
    assert_eq!(keys.len(), 10);
    let mut sorted = keys.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), 10);
}

#[tokio::test]
async fn server_ignoring_orderby_is_an_error() {
    let data = vec![
        ("2026-01-01T00:00:05.000Z".to_string(), "L001".to_string()),
        ("2026-01-01T00:00:01.000Z".to_string(), "L002".to_string()),
    ];
    let mut f = fake(data, 100);
    f.ignore_orderby = true;
    let (_s, client) = start(f).await;
    let err = pull(&client, spec()).await.unwrap_err();
    assert!(matches!(err, ResoError::OutOfOrder(_)), "{err}");
}

#[tokio::test]
async fn server_ignoring_filter_is_an_error_on_resume() {
    let data = server_data(&[
        ("2026-01-01T00:00:00.000Z", 2),
        ("2026-01-01T00:00:09.000Z", 2),
    ]);
    let mut f = fake(data, 100);
    f.ignore_filter = true;
    let (_s, client) = start(f).await;
    let err = pull(
        &client,
        spec().from_checkpoint(Checkpoint::new("2026-01-01T00:00:05.000Z", json!("L002"))),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ResoError::OutOfOrder(_)), "{err}");
}

#[tokio::test]
async fn resumes_from_a_stored_checkpoint() {
    let data = server_data(&[
        ("2026-01-01T00:00:00.000Z", 6),
        ("2026-01-01T00:00:01.000Z", 2),
    ]);
    let (_s, client) = start(fake(data, 100)).await;

    let mut run = client.replicate(spec().page_size(4));
    let first = run.next_batch().await.unwrap().unwrap();
    assert_eq!(first.len(), 4);
    let saved = run.checkpoint().cloned().unwrap();
    assert_eq!(saved.key, Some(json!("L004")));

    let rest = pull(&client, spec().from_checkpoint(saved)).await.unwrap();
    assert_eq!(rest, ["L005", "L006", "L007", "L008"]);
}

#[tokio::test]
async fn empty_resource_yields_no_batches_and_no_checkpoint() {
    let (_s, client) = start(fake(vec![], 100)).await;
    let mut run = client.replicate(spec());
    assert!(run.next_batch().await.unwrap().is_none());
    assert!(run.checkpoint().is_none());
}

#[tokio::test]
async fn record_without_timestamp_is_a_parse_error() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"value": [{"ListingKey": "A"}]})),
        )
        .mount(&server)
        .await;
    let client = ResoClient::with_config(ClientConfig::new(server.uri(), "t")).unwrap();
    let err = pull(&client, spec()).await.unwrap_err();
    assert!(matches!(err, ResoError::Parse(_)), "{err}");
}

#[tokio::test]
async fn repeating_its_own_next_link_is_an_error() {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(wiremock::matchers::any())
        .respond_with(move |req: &Request| {
            ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"ListingKey": "A", "ModificationTimestamp": "2026-01-01T00:00:00Z"}],
                "@odata.nextLink": format!("{base}{}?{}", req.url.path(), req.url.query().unwrap_or("")),
            }))
        })
        .mount(&server)
        .await;
    let client = ResoClient::with_config(ClientConfig::new(server.uri(), "t")).unwrap();
    let mut run = client.replicate(spec());
    let err = run.next_batch().await.unwrap_err();
    assert!(matches!(err, ResoError::Parse(_)), "{err}");
}
