//! The `reso-ingest` command line: `sync`, `discover` and `status`
//!
//! [`run`] takes the arguments and two writers (stdout and stderr in the binary) and returns the exit
//! code, so tests drive it in-process. Exit codes: `0` success, `1` a server or resource failed,
//! `2` bad usage or an unreadable configuration. Progress goes to the error writer, results to the
//! output writer. Secrets are read from the environment variables named in the configuration and are
//! never printed.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{error::ErrorKind, Parser, Subcommand};
use reso_client::{Metadata, ResoClient, StandardReplication};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::checkpoint::CheckpointStore;
use crate::config::{Config, ServerConfig};
use crate::deletes::{reconcile_deletes, EventFeed};
use crate::manifest::{sync_resource, ResourceEntry, RunManifest};
use crate::output::{new_run_id, NdjsonWriter};
use crate::runner::run_servers;

const TIMESTAMP_FIELD: &str = "ModificationTimestamp";
const DEFAULT_OUT: &str = "reso-out";
const DEFAULT_MAX_FILE_MB: u64 = 256;

/// Pull records from RESO Web API servers into NDJSON files
///
/// Secrets are read from the environment variables the configuration names and are never
/// printed. Exit codes: 0 success, 1 a server or resource failed (named in the summary),
/// 2 bad usage or configuration.
#[derive(Debug, Parser)]
#[command(name = "reso-ingest", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Pull every resource of the configured servers, incrementally
    ///
    /// Each run continues from the saved checkpoint (the first run is the full load). Writes
    /// tagged NDJSON files and a manifest into --out, and checkpoints into --state.
    Sync {
        /// Server configuration (TOML; see reso-ingest/servers.example.toml)
        #[arg(long)]
        config: PathBuf,
        /// Only this server; repeat for several (default: all)
        #[arg(long = "server")]
        servers: Vec<String>,
        /// Only this resource; repeat for several (default: all the config allows)
        #[arg(long = "resource")]
        resources: Vec<String>,
        /// Output directory for data files and manifests
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// Checkpoint directory (default: <out>/state)
        #[arg(long)]
        state: Option<PathBuf>,
        /// Also detect deleted records and write delete events
        ///
        /// Without an EntityEvent feed this pulls every key of each resource on each run.
        #[arg(long)]
        deletes: bool,
        /// Delete the saved checkpoints of the selected resources first
        #[arg(long)]
        start_over: bool,
        /// Start a new data file after this many MiB
        #[arg(long, default_value_t = DEFAULT_MAX_FILE_MB, value_parser = clap::value_parser!(u64).range(1..))]
        max_file_mb: u64,
    },
    /// Print each server's $metadata model as JSON (resources, keys, properties, enums)
    Discover {
        /// Server configuration (TOML; see reso-ingest/servers.example.toml)
        #[arg(long)]
        config: PathBuf,
        /// Only this server; repeat for several (default: all)
        #[arg(long = "server")]
        servers: Vec<String>,
        /// Only this resource; repeat for several (default: all the config allows)
        #[arg(long = "resource")]
        resources: Vec<String>,
    },
    /// Show the saved checkpoints and the last run's manifest
    Status {
        /// Directory holding the manifests
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// Checkpoint directory (default: <out>/state)
        #[arg(long)]
        state: Option<PathBuf>,
    },
}

/// The options the commands share, after parsing
#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    servers: Vec<String>,
    resources: Vec<String>,
    out: Option<PathBuf>,
    state: Option<PathBuf>,
    deletes: bool,
    start_over: bool,
    max_file_mb: Option<u64>,
}

impl Args {
    fn out(&self) -> PathBuf {
        self.out.clone().unwrap_or_else(|| DEFAULT_OUT.into())
    }

    fn state(&self) -> PathBuf {
        self.state
            .clone()
            .unwrap_or_else(|| self.out().join("state"))
    }
}

/// Run the command line and return the exit code
///
/// `args` excludes the program name. Output that is the result of a command (summary, JSON, status,
/// `--help`) goes to `out`; progress lines and errors go to `err`.
pub async fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let argv = std::iter::once("reso-ingest".to_string()).chain(args.iter().cloned());
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            let _ = write!(out, "{e}");
            return 0;
        }
        Err(e) => {
            let _ = write!(err, "{e}");
            return 2;
        }
    };
    match cli.command {
        Command::Sync {
            config,
            servers,
            resources,
            out: out_dir,
            state,
            deletes,
            start_over,
            max_file_mb,
        } => {
            let args = Args {
                config: Some(config),
                servers,
                resources,
                out: Some(out_dir),
                state,
                deletes,
                start_over,
                max_file_mb: Some(max_file_mb),
            };
            sync(args, out, err).await
        }
        Command::Discover {
            config,
            servers,
            resources,
        } => {
            let args = Args {
                config: Some(config),
                servers,
                resources,
                ..Args::default()
            };
            discover(args, out, err).await
        }
        Command::Status {
            out: out_dir,
            state,
        } => {
            let args = Args {
                out: Some(out_dir),
                state,
                ..Args::default()
            };
            status(args, out, err)
        }
    }
}

/// Load the config and apply `--server`; `Err` is the exit code (message already printed)
fn load_servers(args: &Args, err: &mut dyn Write) -> Result<(Config, Vec<ServerConfig>), i32> {
    let Some(path) = &args.config else {
        let _ = writeln!(err, "error: --config FILE is required");
        return Err(2);
    };
    let config = match Config::from_file(path) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return Err(2);
        }
    };
    for name in &args.servers {
        if !config.servers.iter().any(|s| &s.name == name) {
            let _ = writeln!(err, "error: no server named `{name}` in the configuration");
            return Err(2);
        }
    }
    let servers = config
        .servers
        .iter()
        .filter(|s| args.servers.is_empty() || args.servers.contains(&s.name))
        .cloned()
        .collect();
    Ok((config, servers))
}

/// The resources of a server to work on, from its metadata, the config filters and `--resource`
///
/// Returns the names and the requested names that are not available.
fn select_resources(
    server: &ServerConfig,
    metadata: &Metadata,
    requested: &[String],
) -> (Vec<String>, Vec<String>) {
    let available: Vec<String> = metadata
        .resource_names()
        .into_iter()
        .filter(|r| server.includes(r))
        .map(str::to_owned)
        .collect();
    if requested.is_empty() {
        return (available, Vec::new());
    }
    let missing = requested
        .iter()
        .filter(|r| !available.contains(r))
        .cloned()
        .collect();
    let chosen = available
        .into_iter()
        .filter(|r| requested.contains(r))
        .collect();
    (chosen, missing)
}

fn client_for(server: &ServerConfig) -> Result<ResoClient, String> {
    let config = server.client_config().map_err(|e| e.to_string())?;
    ResoClient::with_config(config).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- sync

struct SyncOptions {
    out: PathBuf,
    state: PathBuf,
    run_id: String,
    resources: Vec<String>,
    deletes: bool,
    start_over: bool,
    max_file_bytes: u64,
}

async fn sync(args: Args, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (config, servers) = match load_servers(&args, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let options = Arc::new(SyncOptions {
        out: args.out(),
        state: args.state(),
        run_id: new_run_id(),
        resources: args.resources.clone(),
        deletes: args.deletes,
        start_over: args.start_over,
        max_file_bytes: args.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB) * 1024 * 1024,
    });

    let (tx, mut rx) = unbounded_channel::<String>();
    let task_options = Arc::clone(&options);
    let reports = {
        let task = move |server: ServerConfig| {
            let options = Arc::clone(&task_options);
            let tx = tx.clone();
            async move { sync_server(server, options, tx).await }
        };
        let run = run_servers(servers, config.max_concurrent_servers, task);
        let progress = async {
            while let Some(line) = rx.recv().await {
                let _ = writeln!(err, "{line}");
            }
        };
        let (reports, ()) = tokio::join!(run, progress);
        reports
    };

    let mut manifest = RunManifest::new(&options.run_id);
    for report in reports {
        match report.result {
            Ok(entries) => entries.into_iter().for_each(|e| manifest.add_resource(e)),
            Err(message) => manifest.add_server_failure(report.name, message),
        }
    }
    let manifest_path = manifest.write(&options.out);
    print_summary(&manifest, manifest_path.as_deref().ok(), out);
    if let Err(e) = &manifest_path {
        let _ = writeln!(err, "error: {e}");
        return 1;
    }
    i32::from(!manifest.is_clean())
}

fn send(tx: &UnboundedSender<String>, line: String) {
    let _ = tx.send(line);
}

async fn sync_server(
    server: ServerConfig,
    options: Arc<SyncOptions>,
    tx: UnboundedSender<String>,
) -> Result<Vec<ResourceEntry>, String> {
    let name = server.name.clone();
    let client = client_for(&server)?;
    let metadata = client
        .fetch_metadata_model()
        .await
        .map_err(|e| format!("cannot read $metadata: {e}"))?;
    let (resources, missing) = select_resources(&server, &metadata, &options.resources);
    let mut entries: Vec<ResourceEntry> = missing
        .into_iter()
        .map(|r| {
            failed_entry(
                &name,
                &r,
                "not available: not on the server, or excluded by the configuration",
            )
        })
        .collect();
    if resources.is_empty() && entries.is_empty() {
        return Err("no resources to pull (check include/exclude in the configuration)".into());
    }

    let feed = EventFeed::detect(&metadata);
    let store = CheckpointStore::new(&options.state);
    let total = resources.len();
    for (i, resource) in resources.iter().enumerate() {
        let explicit = !options.resources.is_empty();
        let label = format!("{name}/{resource}");
        send(
            &tx,
            format!("##progress step={}/{total} label={label}", i + 1),
        );

        let Some(plan) = plan_resource(&metadata, resource) else {
            send(
                &tx,
                format!("[{label}] skipped: no single key or no {TIMESTAMP_FIELD}"),
            );
            if explicit {
                entries.push(failed_entry(
                    &name,
                    resource,
                    &format!(
                        "cannot sync: needs a single-field key and a {TIMESTAMP_FIELD} property"
                    ),
                ));
            }
            continue;
        };

        let mut writer = match NdjsonWriter::new(
            &options.out,
            &name,
            resource,
            &options.run_id,
            options.max_file_bytes,
        ) {
            Ok(w) => w,
            Err(e) => {
                entries.push(failed_entry(&name, resource, &e.to_string()));
                continue;
            }
        };
        let mut spec = StandardReplication::new(resource, &plan.key);
        if let Some(n) = server.page_size {
            spec = spec.page_size(n);
        }
        let mut entry = sync_resource(
            &client,
            &name,
            resource,
            spec,
            &mut writer,
            &store,
            options.start_over,
        )
        .await;
        if options.deletes && entry.error.is_none() {
            let deleted = reconcile_deletes(
                &client,
                &name,
                resource,
                &plan.key,
                feed.as_ref(),
                &mut writer,
                &store,
            )
            .await;
            entry.deletes = deleted.deletes;
            entry.error = deleted.error;
            if !deleted.files.is_empty() {
                entry.files = deleted.files; // the writer's list is cumulative
            }
        }
        send(
            &tx,
            format!(
                "[{label}] {} records, {} deletes{}",
                entry.records,
                entry.deletes,
                entry
                    .error
                    .as_ref()
                    .map_or(String::new(), |e| format!(", FAILED: {e}"))
            ),
        );
        entries.push(entry);
    }
    Ok(entries)
}

struct Plan {
    key: String,
}

/// The key to order ties by, when the resource can be synced incrementally
fn plan_resource(metadata: &Metadata, resource: &str) -> Option<Plan> {
    let ty = metadata.resource(resource)?;
    let [key] = ty.key.as_slice() else {
        return None;
    };
    ty.property(TIMESTAMP_FIELD)?;
    Some(Plan { key: key.clone() })
}

fn failed_entry(server: &str, resource: &str, error: &str) -> ResourceEntry {
    ResourceEntry {
        server: server.to_string(),
        resource: resource.to_string(),
        records: 0,
        deletes: 0,
        files: Vec::new(),
        checkpoint: None,
        error: Some(error.to_string()),
    }
}

fn print_summary(manifest: &RunManifest, path: Option<&Path>, out: &mut dyn Write) {
    let records: u64 = manifest.resources.iter().map(|r| r.records).sum();
    let deletes: u64 = manifest.resources.iter().map(|r| r.deletes).sum();
    let _ = writeln!(
        out,
        "run {}: {} resources, {records} records, {deletes} deletes",
        manifest.run_id,
        manifest.resources.len()
    );
    for r in &manifest.resources {
        let outcome = r
            .error
            .as_ref()
            .map_or("ok".to_string(), |e| format!("FAILED: {e}"));
        let _ = writeln!(
            out,
            "  {}/{}: {} records, {} deletes, {} files, {outcome}",
            r.server,
            r.resource,
            r.records,
            r.deletes,
            r.files.len()
        );
    }
    for f in &manifest.server_failures {
        let _ = writeln!(out, "  {}: FAILED: {}", f.server, f.error);
    }
    let failed: BTreeSet<&str> = manifest
        .server_failures
        .iter()
        .map(|f| f.server.as_str())
        .chain(
            manifest
                .resources
                .iter()
                .filter(|r| r.error.is_some())
                .map(|r| r.server.as_str()),
        )
        .collect();
    if failed.is_empty() {
        let _ = writeln!(out, "all servers ok");
    } else {
        let names: Vec<&str> = failed.into_iter().collect();
        let _ = writeln!(out, "FAILED servers: {}", names.join(", "));
    }
    if let Some(path) = path {
        let _ = writeln!(out, "manifest: {}", path.display());
    }
}

// ---------------------------------------------------------------- discover

async fn discover(args: Args, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (config, servers) = match load_servers(&args, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let requested = args.resources.clone();
    let reports = run_servers(servers, config.max_concurrent_servers, move |server| {
        let requested = requested.clone();
        async move {
            let client = client_for(&server)?;
            let metadata = client
                .fetch_metadata_model()
                .await
                .map_err(|e| format!("cannot read $metadata: {e}"))?;
            let (resources, missing) = select_resources(&server, &metadata, &requested);
            if let Some(name) = missing.first() {
                return Err(format!("resource `{name}` not available on this server"));
            }
            Ok::<Value, String>(describe(&metadata, &resources))
        }
    })
    .await;

    let mut failed = false;
    let docs: Vec<Value> = reports
        .into_iter()
        .map(|r| match r.result {
            Ok(model) => json!({"server": r.name, "metadata": model}),
            Err(message) => {
                failed = true;
                let _ = writeln!(err, "[{}] FAILED: {message}", r.name);
                json!({"server": r.name, "error": message})
            }
        })
        .collect();
    let text = serde_json::to_string_pretty(&docs).unwrap_or_default();
    let _ = writeln!(out, "{text}");
    i32::from(failed)
}

fn describe(metadata: &Metadata, resources: &[String]) -> Value {
    let resources: Vec<Value> = resources
        .iter()
        .map(|name| {
            let ty = metadata.resource(name);
            let properties: Vec<Value> = ty
                .map(|t| {
                    t.properties
                        .iter()
                        .map(|p| {
                            json!({
                                "name": p.name,
                                "type": p.type_name,
                                "nullable": p.nullable,
                                "max_length": p.max_length,
                                "lookup_name": p.lookup_name,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "name": name,
                "key": ty.map(|t| t.key.clone()).unwrap_or_default(),
                "properties": properties,
                "navigation": ty
                    .map(|t| t.navigation.iter().map(|n| json!({"name": n.name, "type": n.type_name})).collect::<Vec<_>>())
                    .unwrap_or_default(),
            })
        })
        .collect();
    let enums: Vec<Value> = metadata
        .enum_types
        .iter()
        .map(|e| {
            json!({
                "name": e.name,
                "namespace": e.namespace,
                "members": e.members.iter().map(|m| json!({"name": m.name, "value": m.value})).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({"resources": resources, "enum_types": enums})
}

// ---------------------------------------------------------------- status

fn status(args: Args, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let store = CheckpointStore::new(args.state());
    let checkpoints = match store.list() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let _ = writeln!(out, "checkpoints in {}:", args.state().display());
    if checkpoints.is_empty() {
        let _ = writeln!(out, "  none");
    }
    for c in &checkpoints {
        let key = c
            .key
            .as_ref()
            .map_or(String::new(), |k| format!(" key {k}"));
        let _ = writeln!(
            out,
            "  {}/{}: up to {}{key} (saved {})",
            c.server, c.resource, c.timestamp, c.updated_at
        );
    }

    match last_manifest(&args.out()) {
        Ok(Some((path, value))) => {
            let _ = writeln!(out, "last run: {}", value["run_id"].as_str().unwrap_or("?"));
            let _ = writeln!(
                out,
                "  started {}, finished {}",
                value["started_at"].as_str().unwrap_or("?"),
                value["finished_at"].as_str().unwrap_or("not finished")
            );
            let mut failures = 0;
            for r in value["resources"].as_array().into_iter().flatten() {
                let error = r["error"].as_str();
                failures += usize::from(error.is_some());
                let _ = writeln!(
                    out,
                    "  {}/{}: {} records, {} deletes, {}",
                    r["server"].as_str().unwrap_or("?"),
                    r["resource"].as_str().unwrap_or("?"),
                    r["records"],
                    r["deletes"],
                    error.map_or("ok".to_string(), |e| format!("FAILED: {e}"))
                );
            }
            for f in value["server_failures"].as_array().into_iter().flatten() {
                failures += 1;
                let _ = writeln!(
                    out,
                    "  {}: FAILED: {}",
                    f["server"].as_str().unwrap_or("?"),
                    f["error"].as_str().unwrap_or("?")
                );
            }
            let _ = writeln!(out, "  {failures} failures; manifest {}", path.display());
        }
        Ok(None) => {
            let _ = writeln!(
                out,
                "last run: none (no manifest in {})",
                args.out().display()
            );
        }
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return 1;
        }
    }
    0
}

/// The newest `manifest.*.json` in `dir` (run ids sort by time), parsed
fn last_manifest(dir: &Path) -> Result<Option<(PathBuf, Value)>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {}", dir.display(), e.kind())),
    };
    let newest = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("manifest.") && n.ends_with(".json"))
        })
        .max();
    let Some(path) = newest else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?;
    let value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    Ok(Some((path, value)))
}
