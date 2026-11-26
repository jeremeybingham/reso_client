//! Checks of the S3 backends against a real S3-compatible server or AWS S3
//!
//! Skipped unless `RESO_S3_TEST_BUCKET` is set together with `RESO_S3_TEST_ENDPOINT` (a non-AWS
//! server such as RustFS, MinIO or R2) or `RESO_S3_TEST_REGION` (AWS S3, no
//! endpoint). Both may be set; the region defaults to `us-east-1` with an endpoint. Set
//! `RESO_S3_TEST_SINGLE_DELETE=1` for a store without multi-object delete. Credentials come from
//! `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`); the bucket must exist.
//!
//! With `RESO_GCS_TEST_BUCKET` set instead, the same checks run against Google Cloud Storage through
//! its own API ([`Provider::Gcs`]), with the service-account key file named by
//! `GOOGLE_SERVICE_ACCOUNT`. Run with `--features s3`. Written against MinIO-style servers; developed against RustFS because
//! MinIO no longer publishes binaries or images.
#![cfg(feature = "s3")]

use std::sync::Arc;

use reso_ingest::{FileSink, Provider, S3FileSink, S3Options, S3StateStore, StateStore};

fn options(tag: &str) -> Option<S3Options> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    if let Some(bucket) = var("RESO_GCS_TEST_BUCKET") {
        return Some(S3Options {
            provider: Provider::Gcs,
            ..S3Options::new(bucket, format!("live/{tag}-{nanos}"))
        });
    }
    let bucket = var("RESO_S3_TEST_BUCKET")?;
    let endpoint = var("RESO_S3_TEST_ENDPOINT");
    let region = match (&endpoint, var("RESO_S3_TEST_REGION")) {
        (_, Some(region)) => region,
        (Some(_), None) => "us-east-1".into(),
        (None, None) => return None,
    };
    Some(S3Options {
        provider: Provider::S3,
        bucket,
        prefix: format!("live/{tag}-{nanos}"),
        allow_http: endpoint
            .as_deref()
            .is_some_and(|e| e.starts_with("http://")),
        endpoint,
        region: Some(region),
        single_delete: var("RESO_S3_TEST_SINGLE_DELETE").is_some_and(|v| v != "0"),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn state_store_conditional_writes_on_a_real_server() {
    let Some(o) = options("state") else { return };
    let s = Arc::new(S3StateStore::new(&o).unwrap());
    s.check_conditional_writes().await.unwrap();
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

    // List, read and delete, as `purge` does (#139); deeper keys are not listed
    let store = S3StateStore::new(&o).unwrap();
    assert!(store.create("sub/deeper.json", b"{}").await.unwrap());
    assert_eq!(
        sink.list().unwrap(),
        vec![
            ("big.ndjson".to_string(), data.len() as u64),
            ("small.ndjson".to_string(), 8)
        ]
    );
    let copy = dir.join("copy");
    assert!(sink.get_file("small.ndjson", &copy).unwrap());
    assert_eq!(std::fs::read(&copy).unwrap(), b"{\"a\":1}\n");
    assert!(!sink.get_file("missing.ndjson", &copy).unwrap());
    for name in ["small.ndjson", "big.ndjson"] {
        assert!(sink.delete(name).unwrap());
        assert!(!sink.exists(name).unwrap());
    }
    assert!(sink.list().unwrap().is_empty());
    store.delete("sub/deeper.json", None).await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_more_than_one_page_of_keys_on_a_real_server() {
    let Some(o) = options("many") else { return };
    let s = Arc::new(S3StateStore::new(&o).unwrap());
    // S3 returns at most 1000 keys per ListObjectsV2 page, so 1100 need a second page
    let keys: Vec<String> = (0..1100).map(|i| format!("k/{i:05}.json")).collect();
    for chunk in keys.chunks(100) {
        let tasks: Vec<_> = chunk
            .iter()
            .map(|k| {
                let (s, k) = (Arc::clone(&s), k.clone());
                tokio::spawn(async move { s.put(&k, b"x").await })
            })
            .collect();
        for t in tasks {
            t.await.unwrap().unwrap();
        }
    }
    let mut listed = s.list().await.unwrap();
    listed.sort();
    assert_eq!(listed, keys);
    for chunk in keys.chunks(100) {
        let tasks: Vec<_> = chunk
            .iter()
            .map(|k| {
                let (s, k) = (Arc::clone(&s), k.clone());
                tokio::spawn(async move { s.delete(&k, None).await })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap().unwrap());
        }
    }
    assert!(s.list().await.unwrap().is_empty());
}

/// The bucket options of the `reso-ingest` command for the bucket and prefix in `o`
#[cfg(feature = "parquet")]
fn bucket_args(o: &S3Options) -> Vec<String> {
    let mut args = match o.provider {
        Provider::Gcs => vec!["--gcs-bucket".to_string(), o.bucket.clone()],
        _ => vec!["--s3-bucket".to_string(), o.bucket.clone()],
    };
    args.extend(["--s3-prefix".to_string(), o.prefix.clone()]);
    if let Some(endpoint) = &o.endpoint {
        args.extend(["--s3-endpoint".to_string(), endpoint.clone()]);
    }
    if o.provider != Provider::Gcs {
        if let Some(region) = &o.region {
            args.extend(["--s3-region".to_string(), region.clone()]);
        }
    }
    if o.allow_http {
        args.push("--s3-allow-http".into());
    }
    if o.single_delete {
        args.push("--s3-single-delete".into());
    }
    args
}

/// `sync --format parquet --partition` into the real bucket, then `purge` of one server's tree
/// (#86; until #177 only the in-process fake covered the partitioned keys)
#[cfg(feature = "parquet")]
#[tokio::test(flavor = "multi_thread")]
async fn partitioned_parquet_and_purge_on_a_real_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let Some(o) = options("hive") else { return };
    std::env::set_var("S3_LIVE_HIVE_TOKEN", "t");
    let reso = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/$metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#,
        ))
        .mount(&reso)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .respond_with(|req: &wiremock::Request| {
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
        .mount(&reso)
        .await;

    let dir = std::env::temp_dir().join(format!("reso-s3-live-hive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("servers.toml");
    let server = |name: &str| {
        format!(
            "[[server]]\nname = \"{name}\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n\
             [server.auth]\ntype = \"bearer\"\ntoken_env = \"S3_LIVE_HIVE_TOKEN\"\n",
            reso.uri()
        )
    };
    // A name the partition folder has to escape
    std::fs::write(&config, server("mls one") + &server("other")).unwrap();
    let out = dir.join("out");
    let cli = async |cmd: &[&str]| {
        let args: Vec<String> = cmd
            .iter()
            .map(|s| s.to_string())
            .chain(["--out".to_string(), out.to_str().unwrap().to_string()])
            .chain(bucket_args(&o))
            .collect();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = reso_ingest::cli::run(&args, &mut stdout, &mut stderr).await;
        let text = String::from_utf8(stdout).unwrap() + &String::from_utf8(stderr).unwrap();
        (code, text)
    };
    let store = S3StateStore::new(&o).unwrap();
    let under = async |prefix: &str| -> Vec<String> {
        let mut keys: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|k| k.starts_with(prefix))
            .collect();
        keys.sort();
        keys
    };

    let config = config.to_str().unwrap();
    let (code, text) = cli(&[
        "sync",
        "--config",
        config,
        "--format",
        "parquet",
        "--partition",
    ])
    .await;
    assert_eq!(code, 0, "{text}");
    let mls = under("data/source=mls%20one/resource=Property/date=").await;
    assert_eq!(mls.len(), 1, "{:?}", store.list().await.unwrap());
    assert!(mls[0].contains("/mls_one.Property."), "{mls:?}");
    let other = under("data/source=other/resource=Property/date=").await;
    assert_eq!(other.len(), 1, "{:?}", store.list().await.unwrap());

    let (code, text) = cli(&["purge", "--server", "mls one", "--execute", "--by", "ops"]).await;
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("purged server mls one: 1 data files (2 records)"),
        "{text}"
    );
    assert!(under("data/source=mls").await.is_empty());
    assert_eq!(under("data/source=other/").await, other);

    // Leave the bucket clean
    for key in store.list().await.unwrap() {
        store.delete(&key, None).await.unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
