//! The S3 state store over HTTP, against a small in-process S3 fake (feature `s3`)
//!
//! The fake implements what `S3StateStore` uses: PUT (with `If-None-Match: *` and `If-Match`),
//! GET, HEAD, DELETE (with `If-Match`, #133) and ListObjectsV2; `fake_s3_ignoring_preconditions`
//! accepts and ignores the precondition headers, as GCS's S3 endpoint does (#131), and
//! `fake_s3_ignoring_delete_preconditions` ignores them on DELETE only. It is a protocol check only; behaviour against a real
//! bucket is unconfirmed (#107).
#![cfg(feature = "s3")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Extension, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use reso_ingest::{Lease, LeaseOutcome, S3Options, S3StateStore, StateStore};

type Objects = Arc<Mutex<BTreeMap<String, (Vec<u8>, u64)>>>;

/// The HEAD and DELETE requests the fake served, as `HEAD key` or `DELETE key If-Match: <etag>`
type Requests = Arc<Mutex<Vec<String>>>;

fn etag(n: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"etag{n}\"")).unwrap()
}

/// Which precondition headers the fake ignores
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ignore {
    /// None: it honours `If-None-Match` and `If-Match` on PUT and `If-Match` on DELETE
    Nothing,
    /// All of them, on PUT and DELETE
    All,
    /// `If-Match` on DELETE only
    OnDelete,
}

async fn put_object(
    State(objects): State<Objects>,
    Extension(ignore): Extension<Ignore>,
    Path((_bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut map = objects.lock().unwrap();
    let current = map.get(&key).map(|(_, n)| *n);
    if ignore == Ignore::All {
        let n = current.unwrap_or(0) + 1;
        map.insert(key, (body.to_vec(), n));
        return ([("etag", etag(n))], StatusCode::OK).into_response();
    }
    if headers.get("if-none-match").is_some_and(|v| v == "*") && current.is_some() {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }
    if let Some(want) = headers.get("if-match") {
        if current.map(etag).as_ref() != Some(want) {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
    }
    let n = current.unwrap_or(0) + 1;
    map.insert(key, (body.to_vec(), n));
    ([("etag", etag(n))], StatusCode::OK).into_response()
}

async fn get_object(
    State(objects): State<Objects>,
    Extension(requests): Extension<Requests>,
    Path((_bucket, key)): Path<(String, String)>,
    method: axum::http::Method,
) -> Response {
    if method == axum::http::Method::HEAD {
        requests.lock().unwrap().push(format!("HEAD {key}"));
    }
    match objects.lock().unwrap().get(&key) {
        Some((bytes, n)) => {
            let body = if method == axum::http::Method::HEAD {
                Vec::new()
            } else {
                bytes.clone()
            };
            (
                [
                    ("etag", etag(*n)),
                    (
                        "last-modified",
                        HeaderValue::from_static("Wed, 25 Nov 2026 10:00:00 GMT"),
                    ),
                    ("content-length", HeaderValue::from(bytes.len())),
                ],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchKey</Code></Error>",
        )
            .into_response(),
    }
}

async fn delete_object(
    State(objects): State<Objects>,
    Extension(ignore): Extension<Ignore>,
    Extension(requests): Extension<Requests>,
    Path((_bucket, key)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> StatusCode {
    let if_match = headers.get("if-match").and_then(|v| v.to_str().ok());
    requests.lock().unwrap().push(match if_match {
        Some(v) => format!("DELETE {key} If-Match: {v}"),
        None => format!("DELETE {key}"),
    });
    let mut map = objects.lock().unwrap();
    if let Some(want) = headers.get("if-match") {
        // A conditional delete is a presigned URL with If-Match in its signature (#133)
        let signed = query.unwrap_or_default().split('&').any(|kv| {
            kv.strip_prefix("X-Amz-SignedHeaders=")
                .is_some_and(|h| h.split("%3B").any(|h| h == "if-match"))
        });
        if !signed {
            return StatusCode::FORBIDDEN;
        }
        if ignore == Ignore::Nothing {
            match map.get(&key) {
                None => return StatusCode::NOT_FOUND,
                Some((_, n)) if etag(*n) != want => return StatusCode::PRECONDITION_FAILED,
                Some(_) => {}
            }
        }
    }
    map.remove(&key);
    StatusCode::NO_CONTENT
}

async fn list_objects(State(objects): State<Objects>, RawQuery(query): RawQuery) -> Response {
    // The client sends `prefix=reso%2Fstate%2F`; the keys here contain no characters that need more
    let prefix = query
        .unwrap_or_default()
        .split('&')
        .find_map(|kv| kv.strip_prefix("prefix="))
        .unwrap_or_default()
        .replace("%2F", "/");
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><IsTruncated>false</IsTruncated>",
    );
    for (key, (bytes, n)) in objects.lock().unwrap().iter() {
        if key.starts_with(&prefix) {
            xml.push_str(&format!(
                "<Contents><Key>{key}</Key><LastModified>2026-11-25T10:00:00.000Z</LastModified>\
                 <ETag>\"etag{n}\"</ETag><Size>{}</Size></Contents>",
                bytes.len()
            ));
        }
    }
    xml.push_str("</ListBucketResult>");
    ([("content-type", "application/xml")], xml).into_response()
}

/// Start the fake on a free port; returns its endpoint and the shared objects
async fn fake_s3() -> (String, Objects) {
    fake_s3_with(Ignore::Nothing).await
}

/// A fake that accepts the precondition headers and ignores them
async fn fake_s3_ignoring_preconditions() -> (String, Objects) {
    fake_s3_with(Ignore::All).await
}

/// A fake that honours the precondition headers on PUT and ignores `If-Match` on DELETE
async fn fake_s3_ignoring_delete_preconditions() -> (String, Objects) {
    fake_s3_with(Ignore::OnDelete).await
}

async fn fake_s3_with(ignore: Ignore) -> (String, Objects) {
    let (endpoint, objects, _) = fake_s3_logged(ignore).await;
    (endpoint, objects)
}

/// A fake that also records its HEAD and DELETE requests
async fn fake_s3_logged(ignore: Ignore) -> (String, Objects, Requests) {
    let objects = Objects::default();
    let requests = Requests::default();
    let app = Router::new()
        .route("/{bucket}", get(list_objects))
        .route(
            "/{bucket}/{*key}",
            get(get_object)
                .head(get_object)
                .put(put_object)
                .delete(delete_object),
        )
        .layer(Extension(ignore))
        .layer(Extension(requests.clone()))
        .with_state(objects.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), objects, requests)
}

fn options(endpoint: &str, prefix: &str) -> S3Options {
    S3Options {
        provider: reso_ingest::Provider::S3,
        bucket: "bkt".to_string(),
        prefix: prefix.to_string(),
        endpoint: Some(endpoint.to_string()),
        region: Some("us-east-1".to_string()),
        allow_http: true,
        single_delete: true,
    }
}

/// Credentials are read from the environment when the client is built; the fake ignores them
fn set_credentials() {
    std::env::set_var("AWS_ACCESS_KEY_ID", "test");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
}

#[tokio::test]
async fn state_round_trips_over_http() {
    set_credentials();
    let (endpoint, objects) = fake_s3().await;
    let s = S3StateStore::new(&options(&endpoint, "reso/state")).unwrap();
    assert_eq!(s.get("a.json").await.unwrap(), None);
    assert!(s.create("a.json", b"one").await.unwrap());
    assert!(!s.create("a.json", b"two").await.unwrap());
    let v1 = s.get("a.json").await.unwrap().unwrap();
    assert_eq!(v1.bytes, b"one");
    assert!(s.replace("a.json", b"two", &v1.version).await.unwrap());
    assert!(!s.replace("a.json", b"three", &v1.version).await.unwrap());
    s.put("media/m.json", b"m").await.unwrap();
    let mut keys = s.list().await.unwrap();
    keys.sort();
    assert_eq!(keys, ["a.json", "media/m.json"]);
    assert!(objects.lock().unwrap().contains_key("reso/state/a.json"));
    let v2 = s.get("a.json").await.unwrap().unwrap();
    assert!(!s.delete("a.json", Some(&v1.version)).await.unwrap());
    assert!(s.delete("a.json", Some(&v2.version)).await.unwrap());
    assert_eq!(s.get("a.json").await.unwrap(), None);

    // Key sets go through local files (#87)
    let dir = std::env::temp_dir().join(format!("reso-s3-http-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("src"), "h\n\"K1\"\n").unwrap();
    s.put_from_file("m.P.keys.ndjson", &dir.join("src"))
        .await
        .unwrap();
    assert!(s
        .get_to_file("m.P.keys.ndjson", &dir.join("dest"))
        .await
        .unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join("dest")).unwrap(),
        "h\n\"K1\"\n"
    );
    assert!(!s.get_to_file("none", &dir.join("dest2")).await.unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_workers_hold_different_feeds_and_one_holds_a_feed() {
    set_credentials();
    let (endpoint, _objects) = fake_s3().await;
    let one: Arc<dyn StateStore> = Arc::new(S3StateStore::new(&options(&endpoint, "p")).unwrap());
    let two: Arc<dyn StateStore> = Arc::new(S3StateStore::new(&options(&endpoint, "p")).unwrap());
    let ttl = std::time::Duration::from_secs(60);
    let a = Lease::acquire(&one, "mls", "Property", ttl).await.unwrap();
    let b = Lease::acquire(&two, "mls", "Property", ttl).await.unwrap();
    let c = Lease::acquire(&two, "mls", "Member", ttl).await.unwrap();
    assert!(matches!(a, LeaseOutcome::Acquired(_)));
    assert!(
        matches!(b, LeaseOutcome::Held(_)),
        "second worker must not take a held feed"
    );
    assert!(matches!(c, LeaseOutcome::Acquired(_)));
}

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A RESO server with `$metadata` and two `Property` records on one page
async fn reso_server() -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/$metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_string(METADATA))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(|req: &wiremock::Request| {
            // A filter from the newest record on (the follow-up query) finds nothing new
            let after_newest = req
                .url
                .query_pairs()
                .any(|(k, v)| k == "$filter" && v.contains("2026-01-01T00:00:02Z"));
            let value = if after_newest {
                serde_json::json!([])
            } else {
                serde_json::json!([
                    {"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"},
                    {"ListingKey": "K2", "ModificationTimestamp": "2026-01-01T00:00:02Z"}
                ])
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "value": value }))
        })
        .mount(&server)
        .await;
    server
}

async fn cli(args: &[String]) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = reso_ingest::cli::run(args, &mut out, &mut err).await;
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn local_names(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_keeps_manifests_and_last_run_in_the_bucket() {
    set_credentials();
    std::env::set_var("S3_CLI_TEST_TOKEN", "t");
    let (endpoint, objects) = fake_s3().await;
    let reso = reso_server().await;
    let dir = std::env::temp_dir().join(format!("reso-s3-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n\
             [server.auth]\ntype = \"bearer\"\ntoken_env = \"S3_CLI_TEST_TOKEN\"\n",
            reso.uri()
        ),
    )
    .unwrap();
    let out = dir.join("out");
    let bucket = [
        "--out",
        out.to_str().unwrap(),
        "--s3-bucket",
        "bkt",
        "--s3-prefix",
        "p",
        "--s3-endpoint",
        &endpoint,
        "--s3-region",
        "us-east-1",
        "--s3-allow-http",
        "--s3-single-delete",
    ]
    .map(String::from);
    let args = |cmd: &[&str]| -> Vec<String> {
        cmd.iter()
            .map(|s| s.to_string())
            .chain(bucket.iter().cloned())
            .collect()
    };

    let (code, o, e) = cli(&args(&["sync", "--config", config.to_str().unwrap()])).await;
    assert_eq!(code, 0, "{o}{e}");
    assert!(o.contains("manifest: s3://bkt/p/data/manifest."), "{o}");

    let keys: Vec<String> = objects.lock().unwrap().keys().cloned().collect();
    let manifests: Vec<&String> = keys
        .iter()
        .filter(|k| k.starts_with("p/data/manifest."))
        .collect();
    assert_eq!(manifests.len(), 1, "{keys:?}");
    assert!(keys
        .iter()
        .any(|k| k.starts_with("p/data/") && k.ends_with(".ndjson")));
    assert!(keys.contains(&"p/state/runs/last.json".to_string()));
    assert!(
        !keys.iter().any(|k| k.ends_with(".reserved")),
        "reservation left behind: {keys:?}"
    );
    // Nothing a later run needs stays on the local disk
    let local = local_names(&out);
    assert!(
        !local
            .iter()
            .any(|n| n.starts_with("manifest.") || n.ends_with(".ndjson")),
        "{local:?}"
    );

    // A fresh worker (empty --out) sees the last run and the checkpoint in the bucket
    std::fs::remove_dir_all(&out).unwrap();
    let (code, o, e) = cli(&args(&["status"])).await;
    assert_eq!(code, 0, "{e}");
    assert!(
        o.contains("mls/Property: up to 2026-01-01T00:00:02Z"),
        "{o}"
    );
    assert!(o.contains("mls/Property: 2 records"), "{o}");
    assert!(o.contains("s3://bkt/p/state/runs/last.json"), "{o}");

    // `media` cannot read data files from the bucket and says what to use instead
    let (code, _, e) = cli(&args(&["media", "--config", config.to_str().unwrap()])).await;
    assert_eq!(code, 2);
    assert!(e.contains("sync --media"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The servers whose lines an object holds, by its `source` tags
fn sources(bytes: &[u8]) -> std::collections::BTreeSet<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["source"].as_str().map(String::from))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_deletes_one_servers_objects_and_copies_its_record_to_the_bucket() {
    set_credentials();
    std::env::set_var("S3_PURGE_TEST_TOKEN", "t");
    let (endpoint, objects) = fake_s3().await;
    let reso = reso_server().await;
    let dir = std::env::temp_dir().join(format!("reso-s3-purge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("servers.toml");
    let server = |name: &str| {
        format!(
            "[[server]]\nname = \"{name}\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n\
             [server.auth]\ntype = \"bearer\"\ntoken_env = \"S3_PURGE_TEST_TOKEN\"\n",
            reso.uri()
        )
    };
    std::fs::write(&config, server("mls") + &server("other")).unwrap();
    let out = dir.join("out");
    let bucket = [
        "--out",
        out.to_str().unwrap(),
        "--s3-bucket",
        "bkt",
        "--s3-prefix",
        "p",
        "--s3-endpoint",
        &endpoint,
        "--s3-region",
        "us-east-1",
        "--s3-allow-http",
        "--s3-single-delete",
    ]
    .map(String::from);
    let args = |cmd: &[&str]| -> Vec<String> {
        cmd.iter()
            .map(|s| s.to_string())
            .chain(bucket.iter().cloned())
            .collect()
    };
    let (code, o, e) = cli(&args(&["sync", "--config", config.to_str().unwrap()])).await;
    assert_eq!(code, 0, "{o}{e}");

    // A file a killed run left without a manifest, and one named like mls's that is not
    {
        let mut map = objects.lock().unwrap();
        map.insert(
            "p/data/mls.Property.killed.0001.ndjson".into(),
            (b"{\"source\":\"mls\",\"record\":{}}\n".to_vec(), 1),
        );
        map.insert(
            "p/data/mls.Lookalike.x.ndjson".into(),
            (b"{\"source\":\"other\",\"record\":{}}\n".to_vec(), 1),
        );
    }
    let mls_data = |objects: &Objects| -> Vec<String> {
        objects
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, (b, _))| k.starts_with("p/data/") && sources(b).contains("mls"))
            .map(|(k, _)| k.clone())
            .collect()
    };
    let keys =
        |objects: &Objects| -> Vec<String> { objects.lock().unwrap().keys().cloned().collect() };
    assert_eq!(mls_data(&objects).len(), 2, "{:?}", keys(&objects));
    assert!(keys(&objects).iter().any(|k| k.starts_with("p/state/mls.")));

    // A dry run lists the synced file and the orphan, and changes nothing
    let before = keys(&objects);
    let (code, o, e) = cli(&args(&["purge", "--server", "mls"])).await;
    assert_eq!(code, 0, "{o}{e}");
    assert!(o.contains("data files: 2 (3 records"), "{o}");
    assert!(o.contains("mls.Property.killed.0001.ndjson"), "{o}");
    assert!(
        o.contains("not purged (no source tag or manifest names it): mls.Lookalike.x.ndjson"),
        "{o}"
    );
    assert_eq!(keys(&objects), before);

    let (code, o, e) = cli(&args(&[
        "purge",
        "--server",
        "mls",
        "--execute",
        "--by",
        "tester",
    ]))
    .await;
    assert_eq!(code, 0, "{o}{e}");
    assert!(
        o.contains("purged server mls: 2 data files (3 records)"),
        "{o}"
    );
    assert!(o.contains("copy s3://bkt/p/data/purge."), "{o}");
    let after = keys(&objects);
    assert!(mls_data(&objects).is_empty(), "{after:?}");
    assert!(
        !after.iter().any(|k| k.starts_with("p/state/mls.")),
        "{after:?}"
    );
    // The other server's files and state, the lookalike and the run manifest stay
    let map = objects.lock().unwrap();
    assert!(map
        .iter()
        .any(|(k, (b, _))| k.starts_with("p/data/other.") && sources(b).contains("other")));
    assert!(after.iter().any(|k| k.starts_with("p/state/other.")));
    assert!(after.contains(&"p/data/mls.Lookalike.x.ndjson".to_string()));
    assert!(after.iter().any(|k| k.starts_with("p/data/manifest.")));

    // The record in the bucket is the local one, its checksum matches, and the access line names it
    let audit = out.join("audit");
    let local: Vec<String> = local_names(&audit)
        .into_iter()
        .filter(|n| n.starts_with("purge.") && n.ends_with(".json"))
        .collect();
    assert_eq!(local.len(), 1, "{local:?}");
    let record = std::fs::read(audit.join(&local[0])).unwrap();
    assert_eq!(map[&format!("p/data/{}", local[0])].0, record);
    let sum = String::from_utf8(map[&format!("p/data/{}.sha256", local[0])].0.clone()).unwrap();
    assert!(sum.starts_with(&reso_ingest::sha256_hex(&record)), "{sum}");
    let id = local[0]
        .trim_start_matches("purge.")
        .trim_end_matches(".json");
    let access = map
        .iter()
        .find(|(k, _)| k.starts_with("p/data/access.") && k.contains(&id.replace('.', "_")))
        .map(|(_, (b, _))| String::from_utf8(b.clone()).unwrap())
        .unwrap_or_else(|| panic!("no access copy for {id}: {after:?}"));
    let line: serde_json::Value = serde_json::from_str(access.trim()).unwrap();
    assert_eq!(line["status"], "purged");
    assert_eq!(line["purge"]["record"], local[0]);
    drop(map);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Data files in the bucket under `p/data/`, in name order (run id, then file number)
fn bucket_lines(objects: &Objects) -> Vec<serde_json::Value> {
    let map = objects.lock().unwrap();
    map.iter()
        // Data files only: the access-log copies (`access.<run>.ndjson`, #93) are not records
        .filter(|(k, _)| {
            k.starts_with("p/data/") && k.ends_with(".ndjson") && !k.starts_with("p/data/access.")
        })
        .flat_map(|(_, (bytes, _))| {
            String::from_utf8(bytes.clone())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<serde_json::Value>>()
        })
        .collect()
}

/// Eight `Property` records of about 300 KiB each, two per page, so a 1 MiB file is full after
/// two pages. Resumes from a `$filter` on the newest timestamp it mentions. While `hang` is set,
/// the `hang_at`th data request never gets an answer. Returns the server and the data requests
/// (their `$filter`, empty for none).
async fn big_records_server(
    hang: Arc<std::sync::atomic::AtomicBool>,
    hang_at: usize,
) -> (wiremock::MockServer, Arc<Mutex<Vec<String>>>) {
    use std::sync::atomic::Ordering;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let base = server.uri();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let padding = "x".repeat(300 * 1024);
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            let query: Vec<(String, String)> = req
                .url
                .query_pairs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let filter = query
                .iter()
                .find(|(k, _)| k == "$filter")
                .map(|(_, v)| v.clone());
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(filter.clone().unwrap_or_default());
                seen.len()
            };
            if hang.load(Ordering::SeqCst) && n == hang_at {
                return ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(600));
            }
            let rows: Vec<(String, String)> = (1..=8)
                .map(|i| (format!("K{i}"), format!("2026-01-01T00:00:0{i}Z")))
                .collect();
            let from = if let Some((_, v)) = query.iter().find(|(k, _)| k == "from") {
                v.parse().unwrap()
            } else if let Some(f) = &filter {
                let newest = rows.iter().filter(|(_, t)| f.contains(t.as_str())).max();
                newest.map_or(0, |n| rows.iter().filter(|(_, t)| *t <= n.1).count())
            } else {
                0
            };
            let end = (from + 2).min(rows.len());
            let value: Vec<serde_json::Value> = rows[from..end]
                .iter()
                .map(|(k, t)| {
                    serde_json::json!({"ListingKey": k, "ModificationTimestamp": t, "Pad": padding})
                })
                .collect();
            let mut body = serde_json::json!({ "value": value });
            if end < rows.len() {
                body["@odata.nextLink"] = serde_json::json!(format!("{base}/Property?from={end}"));
            }
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    (server, requests)
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !ready() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_sync_resumes_from_the_bucket_on_a_fresh_worker() {
    use std::sync::atomic::{AtomicBool, Ordering};
    set_credentials();
    std::env::set_var("S3_CLI_TEST_TOKEN", "t");
    let (endpoint, objects) = fake_s3().await;
    let hang = Arc::new(AtomicBool::new(true));
    // Pages 1-2 fill the first file, page 3 sits in the open file, page 4 never comes
    let (reso, requests) = big_records_server(Arc::clone(&hang), 4).await;
    let dir = std::env::temp_dir().join(format!("reso-s3-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n\
             [server.auth]\ntype = \"bearer\"\ntoken_env = \"S3_CLI_TEST_TOKEN\"\n",
            reso.uri()
        ),
    )
    .unwrap();
    let args = |out: &std::path::Path| -> Vec<String> {
        [
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--max-file-mb",
            "1",
            "--out",
            out.to_str().unwrap(),
            "--s3-bucket",
            "bkt",
            "--s3-prefix",
            "p",
            "--s3-endpoint",
            &endpoint,
            "--s3-region",
            "us-east-1",
            "--s3-allow-http",
            "--s3-single-delete",
        ]
        .map(String::from)
        .to_vec()
    };

    // Worker one is killed while it waits for page 4
    // Dropping the command's future stops it where it stands. The server task it spawned stays
    // parked on the request for page 4 and writes nothing more; in a killed process it would be
    // gone, and its lease would be left behind until it runs out.
    let first = args(&dir.join("one"));
    tokio::select! {
        r = cli(&first) => panic!("the run ended: {r:?}"),
        () = until("page 4 requested", || requests.lock().unwrap().len() >= 4) => {}
    }
    // The lease of the killed process, run out
    let stale = serde_json::json!({"server": "mls", "resource": "Property", "pid": 999_999,
        "host": "gone", "token": "dead", "expires_at": 1});
    objects.lock().unwrap().insert(
        "p/state/mls.Property.lock".to_string(),
        (serde_json::to_vec(&stale).unwrap(), 1),
    );
    let delivered: Vec<String> = bucket_lines(&objects)
        .iter()
        .map(|l| l["record"]["ListingKey"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(delivered, ["K1", "K2", "K3", "K4"], "the first file only");

    // Worker two starts on an empty disk and continues from the checkpoint in the bucket
    hang.store(false, Ordering::SeqCst);
    let (code, o, e) = cli(&args(&dir.join("two"))).await;
    assert_eq!(code, 0, "{o}{e}");
    let resumed = &requests.lock().unwrap()[4];
    assert!(
        resumed.contains("2026-01-01T00:00:04Z"),
        "worker two did not start from the checkpoint: {resumed}"
    );
    let mut keys: Vec<String> = bucket_lines(&objects)
        .iter()
        .map(|l| l["record"]["ListingKey"].as_str().unwrap().to_string())
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["K1", "K2", "K3", "K4", "K5", "K6", "K7", "K8"],
        "no record lost or repeated"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_serve_workers_share_a_bucket_through_leases() {
    use reso_ingest::{Config, Daemon, S3FileSink, ServeOptions, Shutdown};
    use std::collections::HashMap;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    set_credentials();
    std::env::set_var("S3_SERVE_TEST_TOKEN", "t");
    let (endpoint, objects) = fake_s3().await;

    // Each data request takes a while, so the two workers' syncs overlap. The server records how
    // many requests per resource are in flight at once.
    #[derive(Default)]
    struct Load {
        now: HashMap<String, usize>,
        most: HashMap<String, usize>,
        total: HashMap<String, usize>,
    }
    let load = Arc::new(Mutex::new(Load::default()));
    let reso = MockServer::start().await;
    let l = Arc::clone(&load);
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let path = req.url.path();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(SERVE_METADATA);
            }
            let (resource, key) = if path.ends_with("/Member") {
                ("Member", "MemberKey")
            } else {
                ("Property", "ListingKey")
            };
            {
                let mut l = l.lock().unwrap();
                let now = *l
                    .now
                    .entry(resource.into())
                    .and_modify(|n| *n += 1)
                    .or_insert(1);
                let most = l.most.entry(resource.into()).or_default();
                *most = (*most).max(now);
                *l.total.entry(resource.into()).or_default() += 1;
            }
            // The in-flight count drops 100 ms before the 300 ms response goes out, so a late
            // timer thread cannot count one worker's next request as an overlap (it did about
            // once in 20 loaded runs); two workers on one feed still overlap by far more
            let l2 = Arc::clone(&l);
            let r = resource.to_string();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                *l2.lock().unwrap().now.get_mut(&r).unwrap() -= 1;
            });
            let value = if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                serde_json::json!([])
            } else {
                serde_json::json!([{key: "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"}])
            };
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(300))
                .set_body_json(serde_json::json!({ "value": value }))
        })
        .mount(&reso)
        .await;

    let config = || {
        Config::parse(&format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n\
             [server.auth]\ntype = \"bearer\"\ntoken_env = \"S3_SERVE_TEST_TOKEN\"\n\
             [server.resources.Property]\ninterval = \"100ms\"\n\
             [server.resources.Member]\ninterval = \"100ms\"\n",
            reso.uri()
        ))
        .unwrap()
    };
    let dir = std::env::temp_dir().join(format!("reso-s3-serve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let worker = |name: &str| {
        let mut o = ServeOptions::new(dir.join(name));
        o.listen = "127.0.0.1:0".parse().unwrap();
        o.state = Arc::new(S3StateStore::new(&options(&endpoint, "p/state")).unwrap());
        o.sink = Some(Arc::new(
            S3FileSink::new(&options(&endpoint, "p/data")).unwrap(),
        ));
        o
    };
    let shutdown = Shutdown::new();
    let one = Daemon::start(config(), worker("one"), shutdown.clone())
        .await
        .unwrap();
    let two = Daemon::start(config(), worker("two"), shutdown.clone())
        .await
        .unwrap();

    until("each feed polled six times", || {
        let l = load.lock().unwrap();
        l.total.get("Property") >= Some(&6) && l.total.get("Member") >= Some(&6)
    })
    .await;
    shutdown.trigger();
    for d in [one, two] {
        tokio::time::timeout(std::time::Duration::from_secs(10), d.wait())
            .await
            .expect("the daemon stops after the shutdown request");
    }

    let most = load.lock().unwrap().most.clone();
    assert_eq!(
        most.get("Property"),
        Some(&1),
        "a feed ran in both workers at once"
    );
    assert_eq!(
        most.get("Member"),
        Some(&1),
        "a feed ran in both workers at once"
    );
    // Each record was written once: the worker that came second continued from the checkpoint
    let mut written: Vec<String> = bucket_lines(&objects)
        .iter()
        .map(|l| l["resource"].as_str().unwrap().to_string())
        .collect();
    written.sort();
    assert_eq!(written, ["Member", "Property"]);
    let keys: Vec<String> = objects.lock().unwrap().keys().cloned().collect();
    assert!(
        keys.iter().any(|k| k.starts_with("p/data/access.")),
        "the access-log lines go to the bucket: {keys:?}"
    );
    assert!(
        keys.iter().any(|k| k.starts_with("p/data/manifest.")),
        "{keys:?}"
    );
    assert!(
        keys.iter().any(|k| k.starts_with("p/state/mls.Property")),
        "{keys:?}"
    );
    assert!(
        !keys.iter().any(|k| k.ends_with(".lock")),
        "leases left: {keys:?}"
    );
    for name in ["one", "two"] {
        let local = local_names(&dir.join(name));
        assert!(
            !local
                .iter()
                .any(|n| n.starts_with("manifest.") || n.ends_with(".ndjson")),
            "{name}: {local:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

const SERVE_METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityType Name="Member"><Key><PropertyRef Name="MemberKey"/></Key>
<Property Name="MemberKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>
<EntitySet Name="Member" EntityType="org.reso.metadata.Member"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A bucket that refuses every request with `403 Forbidden`; `HEAD` answers `head_status` (with
/// object headers, so a 200 reads as a found object)
async fn forbidden_s3(head_status: Arc<std::sync::atomic::AtomicU16>) -> String {
    let app = Router::new().fallback(move |method: axum::http::Method| {
        let head = head_status.load(std::sync::atomic::Ordering::SeqCst);
        async move {
            if method != axum::http::Method::HEAD {
                let body = "<Error><Code>AccessDenied</Code></Error>";
                return (StatusCode::FORBIDDEN, body).into_response();
            }
            let mut headers = HeaderMap::new();
            headers.insert("etag", etag(1));
            headers.insert(
                "last-modified",
                HeaderValue::from_static("Wed, 25 Nov 2026 12:00:00 GMT"),
            );
            headers.insert("content-length", HeaderValue::from_static("0"));
            (StatusCode::from_u16(head).unwrap(), headers).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Every store error that is not a failed condition comes back as an error naming the object
/// (403 is not retried, so each call fails at once)
#[tokio::test(flavor = "multi_thread")]
async fn bucket_errors_name_the_object() {
    use reso_ingest::{FileSink, S3FileSink};
    use std::sync::atomic::{AtomicU16, Ordering};

    set_credentials();
    let head = Arc::new(AtomicU16::new(403));
    let endpoint = forbidden_s3(Arc::clone(&head)).await;
    let s = S3StateStore::new(&options(&endpoint, "p/state")).unwrap();
    let check = |what: &str, r: Result<String, reso_ingest::IngestError>| {
        let message = r.expect_err(what).to_string();
        assert!(message.contains("s3://bkt/p/"), "{what}: {message}");
    };
    check("get", s.get("a.json").await.map(|_| String::new()));
    check("put", s.put("a.json", b"x").await.map(|_| String::new()));
    check(
        "create",
        s.create("a.json", b"x").await.map(|_| String::new()),
    );
    check(
        "replace",
        s.replace("a.json", b"x", "v").await.map(|_| String::new()),
    );
    check(
        "delete with a version",
        s.delete("a.json", Some("v")).await.map(|_| String::new()),
    );
    check(
        "delete",
        s.delete("a.json", None).await.map(|_| String::new()),
    );
    check("list", s.list().await.map(|_| String::new()));
    check(
        "a key that is not a path",
        s.get("a//b").await.map(|_| String::new()),
    );

    let sink = S3FileSink::new(&options(&endpoint, "p/data")).unwrap();
    let dir = std::env::temp_dir().join(format!("reso-s3-forbidden-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let small = dir.join("small.ndjson");
    std::fs::write(&small, b"{}\n").unwrap();
    check("exists", sink.exists("small.ndjson").map(|_| String::new()));
    check(
        "put_file",
        sink.put_file("small.ndjson", &small).map(|_| String::new()),
    );
    check(
        "a name that is not a path",
        sink.exists("a//b").map(|_| String::new()),
    );
    check("sink list", sink.list().map(|_| String::new()));
    check(
        "get_file",
        sink.get_file("small.ndjson", &dir.join("got"))
            .map(|_| String::new()),
    );
    check(
        "sink delete",
        sink.delete("small.ndjson").map(|_| String::new()),
    );

    // Past the single-PUT size the upload is multipart: an absent object, then a refused start
    head.store(404, Ordering::SeqCst);
    let big = dir.join("big.ndjson");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(33 * 1024 * 1024)
        .unwrap();
    check(
        "multipart",
        sink.put_file("big.ndjson", &big).map(|_| String::new()),
    );
    let _ = std::fs::remove_dir_all(&dir);

    // An object that is there but cannot be deleted
    head.store(200, Ordering::SeqCst);
    check(
        "delete a found object",
        s.delete("a.json", None).await.map(|_| String::new()),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gcs_interop_endpoint_is_refused_and_bucket_flags_conflict() {
    let args = |extra: &[&str]| -> Vec<String> {
        ["status", "--out", "/nonexistent-reso-out"]
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    };
    let (code, _, err) = cli(&args(&[
        "--s3-bucket",
        "b",
        "--s3-region",
        "auto",
        "--s3-endpoint",
        "https://storage.googleapis.com",
    ]))
    .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--gcs-bucket"), "{err}");

    let (code, _, err) = cli(&args(&["--s3-bucket", "a", "--gcs-bucket", "b"])).await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("cannot be used with"), "{err}");

    let (code, _, err) = cli(&args(&["--gcs-prefix", "p"])).await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("required"), "{err}");
}

#[tokio::test]
async fn the_probe_passes_a_store_with_conditional_writes_and_leaves_nothing() {
    set_credentials();
    let (endpoint, objects) = fake_s3().await;
    let s = S3StateStore::new(&options(&endpoint, "p/state")).unwrap();
    s.check_conditional_writes().await.unwrap();
    assert!(objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_probe_refuses_a_store_that_ignores_conditional_writes() {
    set_credentials();
    let (endpoint, objects) = fake_s3_ignoring_preconditions().await;
    let s = S3StateStore::new(&options(&endpoint, "p/state")).unwrap();
    let err = s.check_conditional_writes().await.unwrap_err().to_string();
    assert!(err.contains("s3://bkt/p/state"), "{err}");
    assert!(err.contains("does not honour conditional writes"), "{err}");
    assert!(
        objects.lock().unwrap().is_empty(),
        "the probe object is deleted"
    );
}

#[tokio::test]
async fn the_probe_refuses_a_store_that_ignores_if_match_on_delete() {
    set_credentials();
    let (endpoint, objects) = fake_s3_ignoring_delete_preconditions().await;
    let s = S3StateStore::new(&options(&endpoint, "p/state")).unwrap();
    let err = s.check_conditional_writes().await.unwrap_err().to_string();
    assert!(
        err.contains("a delete with a stale version (If-Match) succeeded"),
        "{err}"
    );
    assert!(objects.lock().unwrap().is_empty());
}

/// Another worker replaces the object between our read and our delete: the delete is one
/// conditional request with no HEAD before it, so there is no window in which the newer object
/// could be deleted, and the newer object stays (#133)
#[tokio::test]
async fn a_delete_after_another_worker_replaced_the_object_leaves_it() {
    set_credentials();
    let (endpoint, objects, requests) = fake_s3_logged(Ignore::Nothing).await;
    let s = S3StateStore::new(&options(&endpoint, "p/state")).unwrap();
    let other = S3StateStore::new(&options(&endpoint, "p/state")).unwrap();
    assert!(s.create("lease.json", b"mine").await.unwrap());
    let read = s.get("lease.json").await.unwrap().unwrap();
    assert!(other
        .replace("lease.json", b"theirs", &read.version)
        .await
        .unwrap());
    assert!(!s.delete("lease.json", Some(&read.version)).await.unwrap());
    assert_eq!(
        *requests.lock().unwrap(),
        [format!(
            "DELETE p/state/lease.json If-Match: {}",
            read.version
        )]
    );
    let kept = objects.lock().unwrap().get("p/state/lease.json").cloned();
    assert_eq!(kept.map(|(bytes, _)| bytes), Some(b"theirs".to_vec()));
    // A missing object is not deleted either
    let now = s.get("lease.json").await.unwrap().unwrap();
    assert!(s.delete("lease.json", Some(&now.version)).await.unwrap());
    assert!(!s.delete("lease.json", Some(&now.version)).await.unwrap());
}

/// `sync` and `serve` stop before touching a server when the bucket ignores conditional writes;
/// `--s3-unsafe-no-conditional-writes` lets `sync` run anyway (#131)
#[tokio::test(flavor = "multi_thread")]
async fn sync_and_serve_refuse_a_bucket_without_conditional_writes() {
    set_credentials();
    std::env::set_var("S3_PROBE_TEST_TOKEN", "t");
    let (endpoint, objects) = fake_s3_ignoring_preconditions().await;
    let reso = reso_server().await;
    let dir = std::env::temp_dir().join(format!("reso-s3-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("servers.toml");
    std::fs::write(
        &config,
        format!(
            "[[server]]\nname = \"mls\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n\
             [server.auth]\ntype = \"bearer\"\ntoken_env = \"S3_PROBE_TEST_TOKEN\"\n",
            reso.uri()
        ),
    )
    .unwrap();
    let out = dir.join("out");
    let args = |command: &str, extra: &[&str]| -> Vec<String> {
        [
            command,
            "--config",
            config.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--s3-bucket",
            "bkt",
            "--s3-prefix",
            "p",
            "--s3-endpoint",
            &endpoint,
            "--s3-region",
            "us-east-1",
            "--s3-allow-http",
            "--s3-single-delete",
        ]
        .iter()
        .chain(extra)
        .map(|s| s.to_string())
        .collect()
    };
    for command in ["sync", "serve"] {
        let (code, _, err) = cli(&args(command, &[])).await;
        assert_eq!(code, 2, "{command}: {err}");
        assert!(err.contains("does not honour conditional writes"), "{err}");
        assert!(err.contains("--s3-unsafe-no-conditional-writes"), "{err}");
    }
    assert!(objects.lock().unwrap().is_empty());
    assert!(reso.received_requests().await.unwrap().is_empty());

    let (code, _, err) = cli(&args("sync", &["--s3-unsafe-no-conditional-writes"])).await;
    assert_eq!(code, 0, "{err}");
    assert!(!objects.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
