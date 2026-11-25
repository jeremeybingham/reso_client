//! The S3 state store over HTTP, against a small in-process S3 fake (feature `s3`)
//!
//! The fake implements what `S3StateStore` uses: PUT (with `If-None-Match: *` and `If-Match`),
//! GET, HEAD, DELETE and ListObjectsV2. It is a protocol check only; behaviour against a real
//! bucket is unconfirmed (#107).
#![cfg(feature = "s3")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use reso_ingest::{Lease, LeaseOutcome, S3Options, S3StateStore, StateStore};

type Objects = Arc<Mutex<BTreeMap<String, (Vec<u8>, u64)>>>;

fn etag(n: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"etag{n}\"")).unwrap()
}

async fn put_object(
    State(objects): State<Objects>,
    Path((_bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut map = objects.lock().unwrap();
    let current = map.get(&key).map(|(_, n)| *n);
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
    Path((_bucket, key)): Path<(String, String)>,
    method: axum::http::Method,
) -> Response {
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
    Path((_bucket, key)): Path<(String, String)>,
) -> StatusCode {
    objects.lock().unwrap().remove(&key);
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
    let objects = Objects::default();
    let app = Router::new()
        .route("/{bucket}", get(list_objects))
        .route(
            "/{bucket}/{*key}",
            get(get_object)
                .head(get_object)
                .put(put_object)
                .delete(delete_object),
        )
        .with_state(objects.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), objects)
}

fn options(endpoint: &str, prefix: &str) -> S3Options {
    S3Options {
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
