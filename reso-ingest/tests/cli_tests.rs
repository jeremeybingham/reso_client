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

#[tokio::test]
async fn log_format_is_a_global_option() {
    let dir = TempDir::new("logfmt");
    let out = dir.path("out");
    let out_s = out.to_str().unwrap();
    // Before or after the subcommand; `run` itself installs no subscriber
    assert_eq!(
        cli(&["--log-format", "json", "status", "--out", out_s])
            .await
            .code,
        0
    );
    assert_eq!(
        cli(&["status", "--out", out_s, "--log-format", "text"])
            .await
            .code,
        0
    );
    let bad = cli(&["status", "--out", out_s, "--log-format", "xml"]).await;
    assert_eq!(bad.code, 2);
    assert!(bad.err.contains("xml"));
    assert!(cli(&["sync", "--help"]).await.out.contains("--log-format"));
}

#[cfg(feature = "parquet")]
#[tokio::test]
async fn sync_writes_parquet_with_the_format_flag() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    std::env::set_var("CLI_TEST_TOKEN_PQ", SECRET);
    let srv = server().await;
    let dir = TempDir::new("parquet");
    let config = write_config(&dir, &[("mls", srv.uri(), "CLI_TEST_TOKEN_PQ")]);
    let out = dir.path("out");
    let r = cli(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--format",
        "parquet",
    ])
    .await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("5 records"), "{}", r.out);

    let mut rows = 0;
    let mut files = 0;
    for entry in std::fs::read_dir(&out).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "parquet") {
            let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
            rows += reader.metadata().file_metadata().num_rows();
            files += 1;
        }
    }
    assert_eq!(rows, 5);
    assert!(files >= 1);
    assert_eq!(data_keys(&out), Vec::<String>::new(), "no NDJSON files");
}

const RICH_METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EnumType Name="StandardStatus"><Member Name="Active" Value="0"/><Member Name="Closed" Value="1"/></EnumType>
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityType Name="Pair"><Key><PropertyRef Name="A"/><PropertyRef Name="B"/></Key>
<Property Name="A" Type="Edm.String"/><Property Name="B" Type="Edm.String"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>
<EntitySet Name="Pair" EntityType="org.reso.metadata.Pair"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// `$metadata` with a composite-key resource and an enum type; one `Property` record, then
/// nothing once the request resumes after it (has a `$filter`)
async fn rich_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(RICH_METADATA);
            }
            if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
            }
            ResponseTemplate::new(200).set_body_json(json!({"value": [
                {"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"}
            ]}))
        })
        .mount(&server)
        .await;
    server
}

fn write_text_config(dir: &TempDir, text: &str) -> String {
    let path = dir.path("servers.toml");
    std::fs::write(&path, text).unwrap();
    path.to_str().unwrap().to_string()
}

fn bearer_server(name: &str, url: &str, env: &str, extra: &str) -> String {
    format!(
        "[[server]]\nname = \"{name}\"\nbase_url = \"{url}\"\n{extra}\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"{env}\"\n"
    )
}

#[tokio::test]
async fn unreadable_config_is_a_usage_error_for_every_command() {
    let dir = TempDir::new("noconfig");
    let missing = dir.path("missing.toml");
    let missing = missing.to_str().unwrap();
    for command in ["sync", "media", "discover"] {
        let r = cli(&[command, "--config", missing]).await;
        assert_eq!(r.code, 2, "{command}: {}", r.err);
        assert!(r.err.contains("error:"), "{command}: {}", r.err);
    }
    let r = cli(&["media"]).await;
    assert_eq!(r.code, 2);
    let r = cli(&["discover"]).await;
    assert_eq!(r.code, 2);
}

#[tokio::test]
async fn composite_key_resource_is_skipped_unless_asked_for_and_page_size_is_sent() {
    std::env::set_var("CLI_TEST_TOKEN_F", SECRET);
    let srv = rich_server().await;
    let dir = TempDir::new("pair");
    let config = write_text_config(
        &dir,
        &bearer_server("mls", &srv.uri(), "CLI_TEST_TOKEN_F", "page_size = 7"),
    );
    let out = dir.path("out");
    let out_s = out.to_str().unwrap();

    let r = cli(&["sync", "--config", &config, "--out", out_s]).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.err.contains("[mls/Pair] skipped"), "{}", r.err);
    let tops: Vec<String> = srv
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|q| {
            q.url
                .query_pairs()
                .find(|(k, _)| k == "$top")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert!(
        !tops.is_empty() && tops.iter().all(|t| t == "7"),
        "{tops:?}"
    );

    // Asked for by name, it is a failure
    let r = cli(&[
        "sync",
        "--config",
        &config,
        "--out",
        out_s,
        "--resource",
        "Pair",
    ])
    .await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(r.out.contains("FAILED servers: mls"), "{}", r.out);
}

#[tokio::test]
async fn sync_with_nothing_to_pull_fails_the_server() {
    std::env::set_var("CLI_TEST_TOKEN_G", SECRET);
    let srv = rich_server().await;
    let dir = TempDir::new("empty");
    let config = write_text_config(
        &dir,
        &bearer_server(
            "mls",
            &srv.uri(),
            "CLI_TEST_TOKEN_G",
            "include = [\"Nothing\"]",
        ),
    );
    let out = dir.path("out");
    let r = cli(&["sync", "--config", &config, "--out", out.to_str().unwrap()]).await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(r.out.contains("FAILED servers: mls"), "{}", r.out);
}

#[tokio::test]
async fn discover_lists_enum_types_and_fails_on_a_missing_resource() {
    std::env::set_var("CLI_TEST_TOKEN_H", SECRET);
    let srv = rich_server().await;
    let dir = TempDir::new("discover-enum");
    let config = write_text_config(
        &dir,
        &bearer_server("mls", &srv.uri(), "CLI_TEST_TOKEN_H", ""),
    );

    let r = cli(&["discover", "--config", &config]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let docs: Value = serde_json::from_str(&r.out).unwrap();
    let enums = &docs[0]["metadata"]["enum_types"];
    assert_eq!(enums[0]["name"], "StandardStatus");
    assert_eq!(enums[0]["members"][1]["name"], "Closed");

    let r = cli(&["discover", "--config", &config, "--resource", "Nope"]).await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("[mls] FAILED"), "{}", r.err);
    assert!(r.err.contains("Nope"), "{}", r.err);
}

#[tokio::test]
async fn status_of_an_empty_directory_says_so() {
    let dir = TempDir::new("status-empty");
    let out = dir.path("out");
    let r = cli(&["status", "--out", out.to_str().unwrap()]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("  none"), "{}", r.out);
    assert!(r.out.contains("no"), "{}", r.out);
}

#[tokio::test]
async fn status_with_a_broken_state_or_out_directory_fails() {
    let dir = TempDir::new("status-broken");
    let file = dir.path("a-file");
    std::fs::write(&file, "x").unwrap();
    let file = file.to_str().unwrap();
    // The state directory is a file: checkpoints cannot be listed
    let r = cli(&["status", "--out", dir.0.to_str().unwrap(), "--state", file]).await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    // The out directory is a file: manifests cannot be listed
    let state = dir.path("state");
    let r = cli(&["status", "--out", file, "--state", state.to_str().unwrap()]).await;
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(r.err.contains("error:"), "{}", r.err);
}

#[tokio::test]
async fn server_filter_and_resource_expand_reach_every_query() {
    std::env::set_var("CLI_TEST_TOKEN_X", SECRET);
    let srv = server().await;
    let dir = TempDir::new("expand");
    let extra = "filter = \"OriginatingSystemName eq 'mls'\"\n\
                 [server.resources.Property]\nexpand = [\"Media\"]\npage_size = 3";
    let config = write_text_config(
        &dir,
        &bearer_server("mls", &srv.uri(), "CLI_TEST_TOKEN_X", extra),
    );
    let out = dir.path("out");
    let out_s = out.to_str().unwrap();
    for _ in 0..2 {
        let r = cli(&["sync", "--config", &config, "--out", out_s, "--deletes"]).await;
        assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    }

    let mut syncs = 0;
    let mut key_pulls = 0;
    for request in srv.received_requests().await.unwrap() {
        let q: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let get = |name: &str| q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        if request.url.path().ends_with("$metadata") || get("from").is_some() {
            continue; // metadata, and next links the server wrote
        }
        let filter = get("$filter").unwrap_or_default();
        assert!(filter.contains("OriginatingSystemName eq 'mls'"), "{q:?}");
        if get("$orderby").is_some() {
            syncs += 1;
            assert_eq!(get("$expand"), Some("Media"), "{q:?}");
            assert_eq!(get("$top"), Some("3"), "{q:?}");
        } else {
            key_pulls += 1;
            assert_eq!(get("$select"), Some("ListingKey"), "{q:?}");
            assert_eq!(get("$expand"), None, "{q:?}");
        }
    }
    assert!(
        syncs >= 2 && key_pulls == 2,
        "{syncs} syncs, {key_pulls} key pulls"
    );
}

#[tokio::test]
async fn flag_field_deletes_are_events_without_a_key_comparison() {
    std::env::set_var("CLI_TEST_TOKEN_FLAG", SECRET);
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(RICH_METADATA);
            }
            if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
            }
            ResponseTemplate::new(200).set_body_json(json!({"value": [
                {"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z", "MlgCanView": true},
                {"ListingKey": "K2", "ModificationTimestamp": "2026-01-01T00:00:02Z", "MlgCanView": false},
                {"ListingKey": "K3", "ModificationTimestamp": "2026-01-01T00:00:03Z"}
            ]}))
        })
        .mount(&srv)
        .await;
    let dir = TempDir::new("flag");
    let config = write_text_config(
        &dir,
        &bearer_server(
            "mls",
            &srv.uri(),
            "CLI_TEST_TOKEN_FLAG",
            "deletes = { flag_field = \"MlgCanView\" }",
        ),
    );
    let out = dir.path("out");
    let args = [
        "sync",
        "--config",
        &config,
        "--out",
        out.to_str().unwrap(),
        "--resource",
        "Property",
        "--deletes",
    ];

    let r = cli(&args).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(
        r.err.contains("[mls/Property] 3 records, 1 deletes"),
        "{}",
        r.err
    );
    let lines: Vec<Value> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ndjson"))
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .map(|l| serde_json::from_str(&l).unwrap())
        .collect();
    // All three records stay raw; K2 also has a delete event, and nobody else does
    assert_eq!(
        lines.iter().filter(|l| l.get("record").is_some()).count(),
        3
    );
    let events: Vec<&Value> = lines.iter().filter(|l| l.get("event").is_some()).collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "delete");
    assert_eq!(events[0]["key"], "K2");
    let flagged = lines
        .iter()
        .find(|l| l["record"]["ListingKey"] == "K2")
        .unwrap();
    assert_eq!(flagged["record"]["MlgCanView"], false);

    // No key pull (`$select`) and no event-resource query, even with --deletes
    let requests = srv.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|q| !q.url.query_pairs().any(|(k, _)| k == "$select")),
        "key pull sent"
    );

    // The checkpoint is unaffected: a rerun fetches nothing new and writes no second event
    let r = cli(&args).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(
        r.err.contains("[mls/Property] 0 records, 0 deletes"),
        "{}",
        r.err
    );
}
