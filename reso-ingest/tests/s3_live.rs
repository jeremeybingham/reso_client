//! Checks of the S3 backends against a real S3-compatible server
//!
//! Skipped unless `RESO_S3_TEST_ENDPOINT` and `RESO_S3_TEST_BUCKET` are set (credentials from
//! `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`; the bucket must exist). Run with
//! `--features s3`. Written against MinIO-style servers; developed against RustFS because MinIO
//! no longer publishes binaries or images.
#![cfg(feature = "s3")]

use std::sync::Arc;

use reso_ingest::{FileSink, S3FileSink, S3Options, S3StateStore, StateStore};

fn options(tag: &str) -> Option<S3Options> {
    let endpoint = std::env::var("RESO_S3_TEST_ENDPOINT").ok()?;
    let bucket = std::env::var("RESO_S3_TEST_BUCKET").ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Some(S3Options {
        bucket,
        prefix: format!("live/{tag}-{nanos}"),
        endpoint: Some(endpoint),
        region: Some("us-east-1".into()),
        allow_http: true,
        single_delete: false,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn state_store_conditional_writes_on_a_real_server() {
    let Some(o) = options("state") else { return };
    let s = Arc::new(S3StateStore::new(&o).unwrap());
    assert_eq!(s.get("a.json").await.unwrap(), None);
    assert!(s.create("a.json", b"one").await.unwrap());
    assert!(!s.create("a.json", b"two").await.unwrap());
    let v1 = s.get("a.json").await.unwrap().unwrap();
    assert_eq!(v1.bytes, b"one");
    assert!(s.replace("a.json", b"two", &v1.version).await.unwrap());
    assert!(!s.replace("a.json", b"three", &v1.version).await.unwrap());
    assert!(!s.delete("a.json", Some(&v1.version)).await.unwrap());
    s.put("media/x@1.json", b"m").await.unwrap();
    let mut keys = s.list().await.unwrap();
    keys.sort();
    assert_eq!(keys, ["a.json", "media/x@1.json"]);
    let v2 = s.get("a.json").await.unwrap().unwrap();
    assert!(s.delete("a.json", Some(&v2.version)).await.unwrap());
    assert!(s.delete("media/x@1.json", None).await.unwrap());
    assert!(s.list().await.unwrap().is_empty());

    // Eight racing creators: exactly one wins
    let tasks: Vec<_> = (0..8)
        .map(|i| {
            let s = Arc::clone(&s);
            tokio::spawn(async move { s.create("lock", format!("w{i}").as_bytes()).await })
        })
        .collect();
    let mut won = 0;
    for t in tasks {
        won += usize::from(t.await.unwrap().unwrap());
    }
    assert_eq!(won, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn sink_uploads_small_and_multipart_files_on_a_real_server() {
    let Some(o) = options("sink") else { return };
    let sink = S3FileSink::new(&o).unwrap();
    let dir = std::env::temp_dir().join(format!("reso-s3-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let small = dir.join("small.ndjson");
    std::fs::write(&small, b"{\"a\":1}\n").unwrap();
    assert!(!sink.exists("small.ndjson").unwrap());
    sink.put_file("small.ndjson", &small).unwrap();
    assert!(sink.exists("small.ndjson").unwrap());
    assert!(sink.put_file("small.ndjson", &small).is_err());

    let big = dir.join("big.ndjson");
    let data: Vec<u8> = (0..40_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&big, &data).unwrap();
    sink.put_file("big.ndjson", &big).unwrap();
    assert!(sink.exists("big.ndjson").unwrap());
    assert!(sink.put_file("big.ndjson", &big).is_err());

    // Read it back through a state store on the same prefix
    let back = S3StateStore::new(&o)
        .unwrap()
        .get("big.ndjson")
        .await
        .unwrap()
        .unwrap();
    assert!(back.bytes == data);
    std::fs::remove_dir_all(&dir).unwrap();
}
