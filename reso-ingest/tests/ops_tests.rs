//! Operating many feeds (#94): config directories, paused feeds, admin commands (offline, wiremock)

use std::path::PathBuf;

use reso_ingest::cli::run;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const METADATA: &str = r#"<?xml version="1.0"?>
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

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Property and Member, one record each, three Property records after a `$filter` on time
async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            let path = req.url.path();
            if path.ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            let key = if path.ends_with("/Member") {
                "MemberKey"
            } else {
                "ListingKey"
            };
            let filter = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "$filter")
                .map(|(_, v)| v.to_string());
            let rows: Vec<_> = (1..=3)
                .map(|i| json!({key: format!("K{i}"), "ModificationTimestamp": format!("2026-01-0{i}T00:00:00Z")}))
                .filter(|r| match &filter {
                    // "ModificationTimestamp gt <t>" or a keyset filter: keep later rows
                    Some(f) => {
                        let t = r["ModificationTimestamp"].as_str().unwrap();
                        f.split(['(', ')', ' '])
                            .filter(|w| w.starts_with("20"))
                            .all(|w| t > w)
                    }
                    None => true,
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"value": rows}))
        })
        .mount(&server)
        .await;
    server
}

pub fn server_toml(name: &str, url: &str, extra: &str, resources: &str) -> String {
    format!(
        "[[server]]\nname = \"{name}\"\nbase_url = \"{url}\"\n{extra}\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"OPS_TEST_TOKEN\"\n{resources}\n"
    )
}

pub struct Outcome {
    pub code: i32,
    pub out: String,
    pub err: String,
}

pub async fn cli(args: &[&str]) -> Outcome {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err).await;
    Outcome {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

#[tokio::test]
async fn sync_reads_a_config_directory_and_skips_paused_servers_and_resources() {
    std::env::set_var("OPS_TEST_TOKEN", "t");
    let (a, b) = (server().await, server().await);
    let dir = TempDir::new("paused");
    let conf = dir.path("servers.d");
    std::fs::create_dir(&conf).unwrap();
    std::fs::write(
        conf.join("one.toml"),
        server_toml(
            "one",
            &a.uri(),
            "",
            "[server.resources.Member]\npaused = true",
        ),
    )
    .unwrap();
    std::fs::write(
        conf.join("two.toml"),
        server_toml("two", &b.uri(), "paused = true", ""),
    )
    .unwrap();
    let out = dir.path("out");
    let (conf_s, out_s) = (conf.to_str().unwrap(), out.to_str().unwrap());

    let r = cli(&["sync", "--config", conf_s, "--out", out_s]).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("one/Property"), "{}", r.out);
    assert!(!r.out.contains("one/Member"), "{}", r.out);
    assert!(!r.out.contains("two/"), "{}", r.out);
    assert!(r.err.contains("one/Member: paused"), "{}", r.err);
    assert!(r.err.contains("two: paused"), "{}", r.err);
    assert!(
        b.received_requests().await.unwrap().is_empty(),
        "a paused server is not contacted"
    );
    let member_calls = a
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/Member"))
        .count();
    assert_eq!(member_calls, 0);

    // Every resource of a server paused: the server is fine, not "nothing to pull"
    std::fs::write(
        conf.join("one.toml"),
        server_toml(
            "one",
            &a.uri(),
            "",
            "[server.resources.Member]\npaused = true\n[server.resources.Property]\npaused = true",
        ),
    )
    .unwrap();
    let r = cli(&["sync", "--config", conf_s, "--out", out_s]).await;
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
}
