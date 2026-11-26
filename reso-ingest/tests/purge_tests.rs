//! `purge`: dry run, execute, other servers untouched, lease check, record hash (offline; #93)

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reso_ingest::cli::run;
use reso_ingest::{
    sha256_hex, AccessEntry, FileStateStore, Lease, LeaseOutcome, PurgeRecord, StateStore,
    ACCESS_LOG,
};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("reso-purge-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.path().ends_with("$metadata") {
                return ResponseTemplate::new(200).set_body_string(METADATA);
            }
            if req.url.query_pairs().any(|(k, _)| k == "$filter") {
                return ResponseTemplate::new(200).set_body_json(json!({"value": []}));
            }
            ResponseTemplate::new(200).set_body_json(json!({"value": [
                {"ListingKey": "K1", "ModificationTimestamp": "2026-01-01T00:00:01Z"},
                {"ListingKey": "K2", "ModificationTimestamp": "2026-01-01T00:00:02Z"}
            ]}))
        })
        .mount(&server)
        .await;
    server
}

async fn cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run(&args, &mut o, &mut e).await;
    (
        code,
        String::from_utf8(o).unwrap(),
        String::from_utf8(e).unwrap(),
    )
}

/// Two servers synced into `dir/out`, a media file each, a media index for alpha
async fn setup(dir: &Path) -> PathBuf {
    std::env::set_var("PURGE_TEST_TOKEN", "purge-secret");
    let (a, b) = (server().await, server().await);
    let mut config = String::new();
    for (name, s) in [("alpha", &a), ("beta", &b)] {
        config.push_str(&format!(
            "[[server]]\nname = \"{name}\"\nbase_url = \"{}\"\n[server.limits]\nmax_retries = 0\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"PURGE_TEST_TOKEN\"\n",
            s.uri()
        ));
    }
    let config_path = dir.join("servers.toml");
    fs::write(&config_path, config).unwrap();
    let out = dir.join("out");
    let (code, _, err) = cli(&[
        "sync",
        "--config",
        config_path.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    for name in ["alpha", "beta"] {
        let m = out.join("media").join(name);
        fs::create_dir_all(&m).unwrap();
        fs::write(m.join("K1.jpg"), b"jpeg").unwrap();
    }
    FileStateStore::new(out.join("state"))
        .put("media/alpha.json", b"{}")
        .await
        .unwrap();
    out
}

fn data_files(out: &Path, server: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(out)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(&format!("{server}.")) && n.ends_with(".ndjson"))
        .collect();
    names.sort();
    names
}

fn state_keys(out: &Path) -> Vec<String> {
    let mut keys: Vec<String> = walk(&out.join("state"), "");
    keys.sort();
    keys
}

fn walk(dir: &Path, prefix: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().into_string().unwrap();
        if e.file_type().unwrap().is_dir() {
            keys.extend(walk(&e.path(), &format!("{prefix}{name}/")));
        } else {
            keys.push(format!("{prefix}{name}"));
        }
    }
    keys
}

fn access_lines(out: &Path) -> Vec<AccessEntry> {
    fs::read_to_string(out.join(ACCESS_LOG))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn a_dry_run_lists_and_deletes_nothing() {
    let dir = TempDir::new("dry");
    let out = setup(&dir.0).await;
    let before = (data_files(&out, "alpha"), state_keys(&out));
    let (code, stdout, err) =
        cli(&["purge", "--server", "alpha", "--out", out.to_str().unwrap()]).await;
    assert_eq!(code, 0, "{err}");
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(stdout.contains("data files: 1 (2 records"), "{stdout}");
    assert!(stdout.contains("media files: 1 (4 bytes)"), "{stdout}");
    assert!(stdout.contains("alpha.Property.json"), "{stdout}");
    assert!(stdout.contains("media/alpha.json"), "{stdout}");
    assert!(!stdout.contains("beta"), "{stdout}");
    assert_eq!((data_files(&out, "alpha"), state_keys(&out)), before);
    assert!(out.join("media/alpha/K1.jpg").exists());
    assert!(!out.join("audit").read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("purge.")));
}

#[tokio::test]
async fn execute_removes_only_that_servers_data_and_keeps_the_log() {
    let dir = TempDir::new("exec");
    let out = setup(&dir.0).await;
    // A Parquet-like file only a manifest names, and an untagged file with alpha's name
    let manifest = fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("manifest.")
        })
        .unwrap();
    let mut m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    m["resources"].as_array_mut().unwrap().push(json!({
        "server": "alpha", "resource": "Member",
        "files": [{"name": "alpha.Member.r.0001.parquet", "records": 7, "bytes": 3},
                  {"name": "../escape.parquet", "records": 1, "bytes": 1}]
    }));
    fs::write(&manifest, serde_json::to_string(&m).unwrap()).unwrap();
    fs::write(out.join("alpha.Member.r.0001.parquet"), b"PAR").unwrap();
    fs::write(dir.0.join("escape.parquet"), b"x").unwrap();
    fs::write(out.join("alpha.notes.txt"), b"mine?").unwrap();
    let beta_before = (data_files(&out, "beta"), out.join("media/beta/K1.jpg"));
    let lines_before = access_lines(&out);

    let (code, stdout, err) = cli(&[
        "purge",
        "--server",
        "alpha",
        "--out",
        out.to_str().unwrap(),
        "--execute",
        "--by",
        "Compliance Desk",
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        stdout.starts_with("purged server alpha: 2 data files (9 records), 1 media files"),
        "{stdout}"
    );

    // alpha is gone, beta is not
    assert!(data_files(&out, "alpha").is_empty());
    assert!(!out.join("alpha.Member.r.0001.parquet").exists());
    assert!(!out.join("media/alpha").exists());
    assert!(
        state_keys(&out)
            .iter()
            .all(|k| !k.starts_with("alpha.") && k != "media/alpha.json"),
        "{:?}",
        state_keys(&out)
    );
    assert_eq!(data_files(&out, "beta"), beta_before.0);
    assert!(beta_before.1.exists());
    assert!(state_keys(&out).contains(&"beta.Property.json".to_string()));
    // Left alone: a file nothing claims, a name outside the output directory, the manifests
    assert!(out.join("alpha.notes.txt").exists());
    assert!(
        stdout.contains("not purged (no source tag or manifest names it): alpha.notes.txt"),
        "{stdout}"
    );
    assert!(dir.0.join("escape.parquet").exists());
    assert!(manifest.exists());

    // The access log kept every line and gained the purge line
    let lines = access_lines(&out);
    assert_eq!(lines[..lines_before.len()], lines_before[..]);
    let purge_line = lines.last().unwrap();
    assert_eq!(lines.len(), lines_before.len() + 1);
    assert_eq!(
        (purge_line.server.as_str(), purge_line.status.as_str()),
        ("alpha", "purged")
    );
    let note = purge_line.purge.as_ref().unwrap();
    assert_eq!(note.by, "Compliance Desk");
    assert_eq!((note.files, note.media_files), (2, 1));

    // The record, and its hash in the sidecar and the log
    let path = out.join("audit").join(&note.record);
    let bytes = fs::read(&path).unwrap();
    assert_eq!(sha256_hex(&bytes), note.sha256);
    let sidecar =
        fs::read_to_string(path.with_file_name(format!("{}.sha256", note.record))).unwrap();
    assert_eq!(sidecar, format!("{}  {}\n", note.sha256, note.record));
    let record: PurgeRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(record.server, "alpha");
    assert_eq!(record.by, "Compliance Desk");
    assert_eq!(record.records, 9);
    assert!(record.state_keys.contains(&"media/alpha.json".to_string()));
    assert!(record
        .state_keys
        .contains(&"alpha.Property.json".to_string()));
    // purge's own leases are released, not left behind
    assert!(!state_keys(&out).iter().any(|k| k.ends_with(".lock")));
}

#[tokio::test]
async fn a_live_lease_blocks_the_purge() {
    let dir = TempDir::new("lease");
    let out = setup(&dir.0).await;
    let store: Arc<dyn StateStore> = Arc::new(FileStateStore::new(out.join("state")));
    let LeaseOutcome::Acquired(lease) =
        Lease::acquire(&store, "alpha", "Property", Duration::from_secs(60))
            .await
            .unwrap()
    else {
        panic!("lease not taken");
    };
    let files = data_files(&out, "alpha");
    let (code, stdout, _) =
        cli(&["purge", "--server", "alpha", "--out", out.to_str().unwrap()]).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("--execute would refuse"), "{stdout}");
    let (code, _, err) = cli(&[
        "purge",
        "--server",
        "alpha",
        "--out",
        out.to_str().unwrap(),
        "--execute",
        "--by",
        "ops",
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("nothing deleted"), "{err}");
    assert_eq!(data_files(&out, "alpha"), files);
    assert!(out.join("media/alpha/K1.jpg").exists());
    assert!(access_lines(&out).iter().all(|l| l.purge.is_none()));
    lease.release().await;
}

#[tokio::test]
async fn execute_needs_by() {
    let (code, _, err) = cli(&["purge", "--server", "alpha", "--execute"]).await;
    assert_eq!(code, 2);
    assert!(err.contains("--by"), "{err}");
    let (code, _, _) = cli(&["purge", "--server", "alpha", "--by", "x"]).await;
    assert_eq!(code, 2);
}

/// A directory that acts like a bucket (no local paths, so objects are downloaded to be read) and
/// refuses new files, as a bucket might after the deletes
#[derive(Debug)]
struct RemoteRefusingPuts(reso_ingest::LocalDirSink);

impl reso_ingest::FileSink for RemoteRefusingPuts {
    fn put_file(&self, name: &str, _local: &Path) -> Result<(), reso_ingest::IngestError> {
        Err(reso_ingest::IngestError::Output {
            path: name.to_string(),
            message: "access denied".into(),
        })
    }
    fn exists(&self, name: &str) -> Result<bool, reso_ingest::IngestError> {
        self.0.exists(name)
    }
    fn list(&self) -> Result<Vec<(String, u64)>, reso_ingest::IngestError> {
        self.0.list()
    }
    fn get_file(&self, name: &str, dest: &Path) -> Result<bool, reso_ingest::IngestError> {
        self.0.get_file(name, dest)
    }
    fn delete(&self, name: &str) -> Result<bool, reso_ingest::IngestError> {
        self.0.delete(name)
    }
}

#[tokio::test]
async fn a_failed_copy_to_the_bucket_keeps_the_local_record() {
    let dir = TempDir::new("remote");
    let out = setup(&dir.0).await;
    // Move the data files and manifests into the "bucket"
    let bucket = dir.0.join("bucket");
    fs::create_dir_all(&bucket).unwrap();
    for entry in fs::read_dir(&out).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::rename(entry.path(), bucket.join(entry.file_name())).unwrap();
        }
    }
    let sink = RemoteRefusingPuts(reso_ingest::LocalDirSink::new(&bucket).unwrap());
    let state: Arc<dyn StateStore> = Arc::new(FileStateStore::new(out.join("state")));
    let outcome =
        reso_ingest::execute_purge("alpha", &out, Some(&sink), &out.join("media"), &state, "t")
            .await
            .unwrap();
    let reso_ingest::PurgeOutcome::Done {
        record,
        path,
        delivered,
        delivery_error,
        ..
    } = outcome
    else {
        panic!("held")
    };
    assert_eq!(record.status, "purged");
    assert_eq!(record.files.len(), 1, "{record:?}");
    assert!(delivered.is_empty());
    assert!(delivery_error.unwrap().contains("access denied"));
    assert!(path.is_file());
    assert!(data_files(&bucket, "alpha").is_empty());
    assert_eq!(data_files(&bucket, "beta").len(), 1);
    // No downloaded copies are left in the local directory
    assert!(!fs::read_dir(&out).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".purge-")));
}
