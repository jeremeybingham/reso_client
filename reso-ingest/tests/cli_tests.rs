//! The command line, driven in-process against a wiremock server (offline)

use std::path::{Path, PathBuf};

use reso_ingest::cli::run;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "s3cr3t-token-value";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityType Name="Lookup"><Key><PropertyRef Name="LookupKey"/></Key>
<Property Name="LookupKey" Type="Edm.String"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>
<EntitySet Name="Lookup" EntityType="org.reso.metadata.Lookup"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Serves `$metadata` and five `Property` records, two per page; a `$filter` skips records up to
/// the newest timestamp it names
async fn server() -> MockServer {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let path = req.url.path();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            if path != "/Property" {
                return ResponseTemplate::new(404);
            }
            let rows: Vec<(String, String)> = (1..=5)
                .map(|i| (format!("K{i}"), format!("2026-01-01T00:00:0{i}Z")))
                .collect();
            let query: Vec<(String, String)> = req
                .url
                .query_pairs()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let from = if let Some((_, v)) = query.iter().find(|(k, _)| k == "from") {
                v.parse().unwrap()
            } else if let Some((_, f)) = query.iter().find(|(k, _)| k == "$filter") {
                let newest = rows
                    .iter()
                    .filter(|(_, t)| f.contains(t.as_str()))
                    .map(|(_, t)| t.clone())
                    .max();
                rows.iter()
                    .filter(|(_, t)| Some(t) <= newest.as_ref())
                    .count()
            } else {
                0
            };
            let end = (from + 2).min(rows.len());
            let value: Vec<Value> = rows[from..end]
                .iter()
                .map(|(k, t)| json!({"ListingKey": k, "ModificationTimestamp": t}))
                .collect();
            let mut body = json!({"value": value});
            if end < rows.len() {
                body["@odata.nextLink"] = json!(format!("{base}/Property?from={end}"));
            }
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    server
}

fn write_config(dir: &TempDir, servers: &[(&str, String, &str)]) -> PathBuf {
    let mut text = String::from("max_concurrent_servers = 2\n");
    for (name, url, env) in servers {
        text.push_str(&format!(
            "[[server]]\nname = \"{name}\"\nbase_url = \"{url}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"{env}\"\n"
        ));
    }
    let path = dir.path("servers.toml");
    std::fs::write(&path, text).unwrap();
    path
}

struct Outcome {
    code: i32,
    out: String,
    err: String,
}

async fn cli(args: &[&str]) -> Outcome {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err).await;
    Outcome {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn data_keys(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".ndjson"))
        .collect();
    names.sort();
    names
        .iter()
        .flat_map(|n| {
            std::fs::read_to_string(dir.join(n))
                .unwrap()
                .lines()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .map(|l| {
            serde_json::from_str::<Value>(&l).unwrap()["record"]["ListingKey"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn sync_pulls_incrementally_and_status_reports_it() {
    std::env::set_var("CLI_TEST_TOKEN_A", SECRET);
    let srv = server().await;
    let dir = TempDir::new("sync");
    let config = write_config(&dir, &[("mls", srv.uri(), "CLI_TEST_TOKEN_A")]);
    let (config, out) = (config.to_str().unwrap(), dir.path("out"));
    let out_s = out.to_str().unwrap();

    let r = cli(&["sync", "--config", config, "--out", out_s]).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("5 records"), "{}", r.out);
    assert!(r.out.contains("all servers ok"));
    assert!(r.err.contains("##progress"), "{}", r.err);
    // Lookup has no ModificationTimestamp: skipped, not a failure
    assert!(r.err.contains("skipped"), "{}", r.err);
    assert_eq!(data_keys(&out), ["K1", "K2", "K3", "K4", "K5"]);

    // A second run continues from the checkpoint: nothing new
    let r = cli(&["sync", "--config", config, "--out", out_s]).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("0 records"), "{}", r.out);

    let s = cli(&["status", "--out", out_s]).await;
    assert_eq!(s.code, 0, "{}", s.err);
    assert!(
        s.out.contains("mls/Property: up to 2026-01-01T00:00:05Z"),
        "{}",
        s.out
    );
    assert!(s.out.contains("last run:"), "{}", s.out);

    // --start-over reloads everything
    let r = cli(&["sync", "--config", config, "--out", out_s, "--start-over"]).await;
    assert!(r.out.contains("5 records"), "{}", r.out);
}

#[tokio::test]
async fn server_and_resource_filters_restrict_the_run() {
    std::env::set_var("CLI_TEST_TOKEN_B", SECRET);
    let (a, b) = (server().await, server().await);
    let dir = TempDir::new("filters");
    let config = write_config(
        &dir,
        &[
            ("one", a.uri(), "CLI_TEST_TOKEN_B"),
            ("two", b.uri(), "CLI_TEST_TOKEN_B"),
        ],
    );
    let config = config.to_str().unwrap();
    let out = dir.path("out");
    let out_s = out.to_str().unwrap();

    let r = cli(&[
        "sync",
        "--config",
        config,
        "--out",
        out_s,
        "--server",
        "two",
        "--resource",
        "Property",
    ])
    .await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("two/Property"));
    assert!(!r.out.contains("one/"));
    let names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.starts_with("one.")), "{names:?}");

    // A resource the server does not have fails the run
    let r = cli(&[
        "sync",
        "--config",
        config,
        "--out",
        out_s,
        "--server",
        "one",
        "--resource",
        "Nope",
    ])
    .await;
    assert_eq!(r.code, 1);
    assert!(r.out.contains("FAILED servers: one"), "{}", r.out);

    // An unknown server is a usage error
    let r = cli(&["sync", "--config", config, "--server", "zzz"]).await;
    assert_eq!(r.code, 2);
    assert!(r.err.contains("zzz"));
}

#[tokio::test]
async fn failing_server_gives_nonzero_exit_and_is_named_while_the_other_completes() {
    std::env::set_var("CLI_TEST_TOKEN_C", SECRET);
    std::env::remove_var("CLI_TEST_TOKEN_UNSET");
    let srv = server().await;
    let dir = TempDir::new("fail");
    let config = write_config(
        &dir,
        &[
            ("good", srv.uri(), "CLI_TEST_TOKEN_C"),
            (
                "bad",
                "http://127.0.0.1:1".to_string(),
                "CLI_TEST_TOKEN_UNSET",
            ),
        ],
    );
    let out = dir.path("out");
    let r = cli(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ])
    .await;
    assert_eq!(r.code, 1);
    assert!(r.out.contains("FAILED servers: bad"), "{}", r.out);
    assert!(r.out.contains("good/Property: 5 records"), "{}", r.out);
    assert_eq!(data_keys(&out).len(), 5);
}

#[tokio::test]
async fn discover_prints_the_metadata_model() {
    std::env::set_var("CLI_TEST_TOKEN_D", SECRET);
    let srv = server().await;
    let dir = TempDir::new("discover");
    let config = write_config(&dir, &[("mls", srv.uri(), "CLI_TEST_TOKEN_D")]);
    let r = cli(&[
        "discover",
        "--config",
        config.to_str().unwrap(),
        "--resource",
        "Property",
    ])
    .await;
    assert_eq!(r.code, 0, "{}", r.err);
    let docs: Value = serde_json::from_str(&r.out).unwrap();
    let resources = &docs[0]["metadata"]["resources"];
    assert_eq!(resources.as_array().unwrap().len(), 1);
    assert_eq!(resources[0]["key"], json!(["ListingKey"]));
    assert_eq!(
        resources[0]["properties"][1]["name"],
        "ModificationTimestamp"
    );
}

#[tokio::test]
async fn no_secret_reaches_any_output() {
    std::env::set_var("CLI_TEST_TOKEN_E", SECRET);
    let srv = server().await;
    let dir = TempDir::new("secret");
    let config = write_config(
        &dir,
        &[
            ("mls", srv.uri(), "CLI_TEST_TOKEN_E"),
            ("bad", "http://127.0.0.1:1".to_string(), "CLI_TEST_TOKEN_E"),
        ],
    );
    let (config, out) = (config.to_str().unwrap().to_string(), dir.path("out"));
    let out_s = out.to_str().unwrap();
    let mut all = String::new();
    for args in [
        vec!["sync", "--config", &config, "--out", out_s, "--deletes"],
        vec!["discover", "--config", &config],
        vec!["status", "--out", out_s],
        vec!["--help"],
    ] {
        let r = cli(&args).await;
        all.push_str(&r.out);
        all.push_str(&r.err);
    }
    for entry in std::fs::read_dir(&out).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            all.push_str(&std::fs::read_to_string(path).unwrap());
        }
    }
    assert!(!all.contains(SECRET));
}

#[tokio::test]
async fn usage_errors_exit_2_and_help_exits_0() {
    assert_eq!(cli(&[]).await.code, 2);
    assert_eq!(cli(&["bogus"]).await.code, 2);
    assert_eq!(cli(&["sync"]).await.code, 2); // no --config
    assert_eq!(cli(&["sync", "--wat"]).await.code, 2);
    let h = cli(&["sync", "--help"]).await;
    assert_eq!(h.code, 0);
    assert!(h.out.contains("Usage"));
}
