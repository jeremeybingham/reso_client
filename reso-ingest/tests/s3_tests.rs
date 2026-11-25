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
