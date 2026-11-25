//! The `reso-ingest` command line: `sync`, `media`, `discover` and `status`
//!
//! [`run`] takes the arguments and two writers (stdout and stderr in the binary) and returns the exit
//! code, so tests drive it in-process. Exit codes: `0` success, `1` a server or resource failed,
//! `2` bad usage or an unreadable configuration, `3` a `sync` cut short by SIGTERM or Ctrl-C. Progress goes to the error writer, results to the
//! output writer. Secrets are read from the environment variables named in the configuration and are
//! never printed. [`run_main`] also installs the log subscriber (`--log-format`, `RUST_LOG`).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{error::ErrorKind, Parser, Subcommand};
use reso_client::{ClientStats, Metadata, ResoClient, StandardReplication};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::Instrument;

use crate::audit::{credential_id, AccessEntry, AccessLog};
use crate::checkpoint::CheckpointStore;
use crate::checks::{count_warning, CheckingWriter, SchemaSnapshot};
use crate::config::{Config, ServerConfig};
use crate::deletes::{reconcile_deletes, DeleteTarget, EventFeed, FlagDeleteWriter};
use crate::error::IngestError;
use crate::lease::{Lease, LeaseOutcome};
use crate::logging::{env_filter, log_subscriber, LogFormat, LOG_FORMAT_ENV};
use crate::manifest::{sync_resource_media, ResourceEntry, ResourceStats, RunManifest};
use crate::media::{sync_media, MediaManifest, MediaOptions, MediaReport, PageMedia};
use crate::metrics::write_metrics_file;
use crate::output::{reserve_run_id, NdjsonWriter, RecordWriter};
#[cfg(feature = "parquet")]
use crate::parquet_out::ParquetWriter;
use crate::runner::run_servers;
use crate::runs;
use crate::secrets::{is_auth_failure, ClientHandle};
use crate::serve::{Daemon, ServeOptions};
use crate::shutdown::Shutdown;
use crate::state::{FileStateStore, StateStore};
use crate::webhook::{post_summary, run_summary};

const TIMESTAMP_FIELD: &str = "ModificationTimestamp";
const DEFAULT_OUT: &str = "reso-out";
const DEFAULT_MAX_FILE_MB: u64 = 256;

/// Pull records from RESO Web API servers into NDJSON (or, with the `parquet` feature, Parquet) files
///
/// Secrets are read from the environment variables the configuration names and are never
/// printed. Exit codes: 0 success, 1 a server or resource failed (named in the summary),
/// 2 bad usage or configuration, 3 interrupted by SIGTERM or Ctrl-C.
#[derive(Debug, Parser)]
#[command(name = "reso-ingest", version)]
struct Cli {
    /// Log line format on stderr (default: $RESO_LOG_FORMAT, else text); level from RUST_LOG
    #[arg(long, value_enum, global = true)]
    log_format: Option<LogFormat>,
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
        /// Write deletes even when they pass the [server.deletes] limits (max_deletes, max_percent)
        ///
        /// Without it, a pass over a limit writes none and keeps its state, so the next run finds
        /// the same deletes; use this once after checking them.
        #[arg(long)]
        allow_mass_deletes: bool,
        /// Download the media files of each page right after it is written
        ///
        /// Takes the Media records (or, for other resources, the `Media` arrays that `expand`
        /// brings) of each page and downloads the new and changed files into --media-dir, so
        /// single-use or short-lived URLs are used while fresh. Uses the [server.media] limits.
        #[arg(long)]
        media: bool,
        /// Where files go with --media (default: <out>/media)
        #[arg(long)]
        media_dir: Option<PathBuf>,
        /// Delete the saved checkpoints of the selected resources first
        #[arg(long)]
        start_over: bool,
        /// Start a new data file after this many MiB
        #[arg(long, default_value_t = DEFAULT_MAX_FILE_MB, value_parser = clap::value_parser!(u64).range(1..))]
        max_file_mb: u64,
        /// Data file format
        #[arg(long, value_enum, default_value_t = Format::Ndjson)]
        format: Format,
        /// Also write the run's metrics (Prometheus text format) to this file
        ///
        /// Replaced atomically at the end of the run, for the node_exporter textfile collector.
        /// The same numbers are in the manifest.
        #[arg(long)]
        metrics_file: Option<PathBuf>,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Run as a daemon: sync each resource on its own interval and serve health and metrics
    ///
    /// Schedules the resources that have an `interval` in the configuration (or are in `include`
    /// under a server `interval`). Each sync is the same as `sync --server S --resource R`. Serves
    /// /healthz, /readyz, /metrics and /status over plain HTTP on --listen (no authentication: use
    /// a private address). Stops after the current pages on SIGTERM or Ctrl-C and exits 0.
    Serve {
        /// Server configuration (TOML; see reso-ingest/servers.example.toml)
        #[arg(long)]
        config: PathBuf,
        /// Only this server; repeat for several (default: all)
        #[arg(long = "server")]
        servers: Vec<String>,
        /// Address for the endpoints
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: std::net::SocketAddr,
        /// Output directory for data files and manifests
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// Checkpoint directory (default: <out>/state)
        #[arg(long)]
        state: Option<PathBuf>,
        /// Also detect deleted records on every sync
        #[arg(long)]
        deletes: bool,
        /// Write deletes even when they pass the [server.deletes] limits
        #[arg(long)]
        allow_mass_deletes: bool,
        /// Download the media files of each page right after it is written
        #[arg(long)]
        media: bool,
        /// Where files go with --media (default: <out>/media)
        #[arg(long)]
        media_dir: Option<PathBuf>,
        /// Start a new data file after this many MiB
        #[arg(long, default_value_t = DEFAULT_MAX_FILE_MB, value_parser = clap::value_parser!(u64).range(1..))]
        max_file_mb: u64,
        /// Data file format
        #[arg(long, value_enum, default_value_t = Format::Ndjson)]
        format: Format,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Download the files of the Media records that `sync` wrote
    ///
    /// Reads the NDJSON Media data files listed in the manifests in --out, and downloads new and
    /// changed files into --media-dir/<server>/, named by MediaKey. Files that failed are retried
    /// on the next run. Downloads use the [server.media] limits, not the API's.
    Media {
        /// Server configuration (TOML; see reso-ingest/servers.example.toml)
        #[arg(long)]
        config: PathBuf,
        /// Only this server; repeat for several (default: all)
        #[arg(long = "server")]
        servers: Vec<String>,
        /// The sync output directory (manifests and data files)
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// State directory (default: <out>/state); the media index goes in <state>/media/
        #[arg(long)]
        state: Option<PathBuf>,
        /// Where files go (default: <out>/media)
        #[arg(long)]
        media_dir: Option<PathBuf>,
        /// Most downloads per server in this run; the rest wait for the next run
        #[arg(long)]
        limit: Option<u64>,
        #[command(flatten)]
        storage: StorageArgs,
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
        #[command(flatten)]
        storage: StorageArgs,
    },
}

/// Where state and output live when not on the local disk (feature `s3`)
///
/// With `--s3-bucket`, checkpoints, leases and media indexes go to `<prefix>/state/` in the bucket
/// and finished data files to `<prefix>/data/`, so several workers share them. Manifests, the
/// metrics file and media files still go to `--out` and `--media-dir`. Credentials come from the
/// standard `AWS_*` environment variables, never from options.
#[derive(Debug, Clone, Default, clap::Args)]
struct StorageArgs {
    /// S3-compatible bucket for state and data files (env RESO_S3_BUCKET)
    #[cfg(feature = "s3")]
    #[arg(long, env = "RESO_S3_BUCKET", conflicts_with = "state")]
    s3_bucket: Option<String>,
    /// Key prefix inside the bucket (env RESO_S3_PREFIX)
    #[cfg(feature = "s3")]
    #[arg(
        long,
        env = "RESO_S3_PREFIX",
        default_value = "",
        requires = "s3_bucket"
    )]
    s3_prefix: String,
    /// Endpoint URL of a non-AWS store: R2, MinIO, GCS interoperability (env RESO_S3_ENDPOINT)
    #[cfg(feature = "s3")]
    #[arg(long, env = "RESO_S3_ENDPOINT", requires = "s3_bucket")]
    s3_endpoint: Option<String>,
    /// Region (env RESO_S3_REGION; default AWS_REGION)
    #[cfg(feature = "s3")]
    #[arg(long, env = "RESO_S3_REGION", requires = "s3_bucket")]
    s3_region: Option<String>,
    /// Allow a plain http:// endpoint (a local MinIO)
    #[cfg(feature = "s3")]
    #[arg(long, requires = "s3_bucket")]
    s3_allow_http: bool,
    /// One DELETE per object instead of multi-delete (GCS interoperability)
    #[cfg(feature = "s3")]
    #[arg(long, requires = "s3_bucket")]
    s3_single_delete: bool,
}

/// File format of the data files
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// One tagged JSON line per record
    #[default]
    Ndjson,
    /// Typed columns from `$metadata`; with --deletes or checkpoints this makes one file per page
    #[cfg(feature = "parquet")]
    Parquet,
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
    allow_mass_deletes: bool,
    media: bool,
    start_over: bool,
    max_file_mb: Option<u64>,
    format: Format,
    media_dir: Option<PathBuf>,
    limit: Option<u64>,
    metrics_file: Option<PathBuf>,
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    storage: StorageArgs,
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

    /// The bucket options when `--s3-bucket` is set, with `suffix` under the prefix
    #[cfg(feature = "s3")]
    fn s3_options(&self, suffix: &str) -> Option<crate::S3Options> {
        let s = &self.storage;
        let bucket = s.s3_bucket.clone()?;
        let prefix = s.s3_prefix.trim_matches('/');
        let prefix = if prefix.is_empty() {
            suffix.to_string()
        } else {
            format!("{prefix}/{suffix}")
        };
        Some(crate::S3Options {
            bucket,
            prefix,
            endpoint: s.s3_endpoint.clone(),
            region: s.s3_region.clone(),
            allow_http: s.s3_allow_http,
            single_delete: s.s3_single_delete,
        })
    }

    /// The state store: the bucket with `--s3-bucket`, else the state directory
    fn state_store(&self) -> Result<Arc<dyn StateStore>, IngestError> {
        #[cfg(feature = "s3")]
        if let Some(options) = self.s3_options("state") {
            return Ok(Arc::new(crate::S3StateStore::new(&options)?));
        }
        Ok(Arc::new(FileStateStore::new(self.state())))
    }

    /// Where finished data files go: the bucket with `--s3-bucket`, else they stay in `--out`
    fn file_sink(&self) -> Result<Option<Arc<dyn crate::sink::FileSink>>, IngestError> {
        #[cfg(feature = "s3")]
        if let Some(options) = self.s3_options("data") {
            return Ok(Some(Arc::new(crate::S3FileSink::new(&options)?)));
        }
        Ok(None)
    }
}

/// Run the command line and return the exit code
///
/// `args` excludes the program name. Output that is the result of a command (summary, JSON, status,
/// `--help`) goes to `out`; progress lines and errors go to `err`.
///
/// It installs no log subscriber (tests and library users choose their own); the binary calls
/// [`run_main`].
pub async fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    run_with_shutdown(args, out, err, Shutdown::new()).await
}

/// Like [`run`], with a [`Shutdown`] the caller can trigger
///
/// A `sync` that sees the request stops after the last page written, saves what is written, marks the
/// manifest `interrupted` and returns [`EXIT_INTERRUPTED`]. The binary triggers it on SIGTERM or
/// Ctrl-C; tests trigger it directly.
pub async fn run_with_shutdown(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    shutdown: Shutdown,
) -> i32 {
    match parse(args, out, err) {
        Ok(cli) => execute(cli, out, err, shutdown).await,
        Err(code) => code,
    }
}

/// Like [`run`], but first installs the global log subscriber, writing to stderr
///
/// The format is `--log-format`, else the `RESO_LOG_FORMAT` variable (`text` or `json`), else
/// text; the level filter is `RUST_LOG`, else `info`. An invalid `RESO_LOG_FORMAT` is bad usage
/// (exit code 2).
pub async fn run_main(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let cli = match parse(args, out, err) {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    let format = match cli.log_format {
        Some(f) => f,
        None => match std::env::var(LOG_FORMAT_ENV) {
            Ok(v) if !v.is_empty() => match v.parse() {
                Ok(f) => f,
                Err(e) => {
                    let _ = writeln!(err, "error: {LOG_FORMAT_ENV}: {e}");
                    return 2;
                }
            },
            _ => LogFormat::default(),
        },
    };
    let subscriber = log_subscriber(format, env_filter(), std::io::stderr);
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        let _ = writeln!(err, "warning: a log subscriber is already installed");
    }
    let shutdown = Shutdown::new();
    watch_signals(shutdown.clone());
    execute(cli, out, err, shutdown).await
}

/// Exit code of a `sync` that a shutdown request (SIGTERM, Ctrl-C) ended early
pub const EXIT_INTERRUPTED: i32 = 3;

/// Exit code when a second shutdown signal ends the process at once (as after Ctrl-C in a shell)
const EXIT_FORCED: i32 = 130;

/// Trigger `shutdown` on the first SIGTERM or Ctrl-C; a second one exits the process at once
fn watch_signals(shutdown: Shutdown) {
    tokio::spawn(async move {
        let mut count = 0;
        loop {
            if wait_for_signal().await.is_err() {
                return; // no signal handling here; the run goes on without it
            }
            count += 1;
            if count > 1 {
                eprintln!("second signal: exiting now");
                std::process::exit(EXIT_FORCED);
            }
            tracing::warn!("shutdown requested: stopping after the last page written");
            shutdown.trigger();
        }
    });
}

#[cfg(unix)]
async fn wait_for_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    // Re-created each time: cheap, and keeps the handler installed between calls
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

/// Parse the arguments; `Err` is the exit code (help and version are printed to `out`)
fn parse(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Result<Cli, i32> {
    let argv = std::iter::once("reso-ingest".to_string()).chain(args.iter().cloned());
    match Cli::try_parse_from(argv) {
        Ok(cli) => Ok(cli),
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            let _ = write!(out, "{e}");
            Err(0)
        }
        Err(e) => {
            let _ = write!(err, "{e}");
            Err(2)
        }
    }
}

async fn execute(cli: Cli, out: &mut dyn Write, err: &mut dyn Write, shutdown: Shutdown) -> i32 {
    match cli.command {
        Command::Sync {
            config,
            servers,
            resources,
            out: out_dir,
            state,
            deletes,
            allow_mass_deletes,
            media,
            media_dir,
            start_over,
            max_file_mb,
            format,
            metrics_file,
            storage,
        } => {
            let args = Args {
                config: Some(config),
                servers,
                resources,
                out: Some(out_dir),
                state,
                deletes,
                allow_mass_deletes,
                media,
                media_dir,
                start_over,
                max_file_mb: Some(max_file_mb),
                format,
                metrics_file,
                storage,
                ..Args::default()
            };
            sync(args, out, err, shutdown).await
        }
        Command::Serve {
            config,
            servers,
            listen,
            out: out_dir,
            state,
            deletes,
            allow_mass_deletes,
            media,
            media_dir,
            max_file_mb,
            format,
            storage,
        } => {
            let args = Args {
                config: Some(config),
                servers,
                out: Some(out_dir),
                state,
                deletes,
                allow_mass_deletes,
                media,
                media_dir,
                max_file_mb: Some(max_file_mb),
                format,
                storage,
                ..Args::default()
            };
            serve(args, listen, err, shutdown).await
        }
        Command::Media {
            config,
            servers,
            out: out_dir,
            state,
            media_dir,
            limit,
            storage,
        } => {
            let args = Args {
                config: Some(config),
                servers,
                out: Some(out_dir),
                state,
                media_dir,
                limit,
                storage,
                ..Args::default()
            };
            media(args, out, err).await
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
            storage,
        } => {
            let args = Args {
                out: Some(out_dir),
                state,
                storage,
                ..Args::default()
            };
            status(args, out, err).await
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

pub(crate) fn client_for(server: &ServerConfig) -> Result<ResoClient, String> {
    let config = server.client_config().map_err(|e| e.to_string())?;
    ResoClient::with_config(config).map_err(|e| e.to_string())
}

/// Print a storage setup error (a bucket or credentials that cannot be used) and return exit code 2
fn usage_error(err: &mut dyn Write, e: &IngestError) -> i32 {
    let _ = writeln!(err, "error: {e}");
    2
}

// ---------------------------------------------------------------- serve

async fn serve(
    args: Args,
    listen: std::net::SocketAddr,
    err: &mut dyn Write,
    shutdown: Shutdown,
) -> i32 {
    let (mut config, servers) = match load_servers(&args, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    config.servers = servers;
    let mut options = ServeOptions::new(args.out());
    options.listen = listen;
    options.state = match args.state_store() {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    options.sink = match args.file_sink() {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    options.deletes = args.deletes;
    options.allow_mass_deletes = args.allow_mass_deletes;
    options.media_dir = args.media.then(|| {
        args.media_dir
            .clone()
            .unwrap_or_else(|| args.out().join("media"))
    });
    options.max_file_bytes = args.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB) * 1024 * 1024;
    options.format = args.format;
    match Daemon::start(config, options, shutdown).await {
        Ok(daemon) => {
            let _ = writeln!(err, "listening on http://{}", daemon.local_addr());
            daemon.wait().await;
            0
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            2
        }
    }
}

// ---------------------------------------------------------------- sync

/// How long a feed's lease lasts without renewal (a crashed run blocks its feeds for at most this long)
const LEASE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

pub(crate) struct SyncOptions {
    pub(crate) out: PathBuf,
    pub(crate) state: Arc<dyn StateStore>,
    pub(crate) run_id: String,
    pub(crate) resources: Vec<String>,
    pub(crate) deletes: bool,
    pub(crate) allow_mass_deletes: bool,
    pub(crate) media: Option<PathBuf>,
    pub(crate) start_over: bool,
    pub(crate) max_file_bytes: u64,
    pub(crate) format: Format,
    /// Where finished data files go; `None` leaves them in `out`
    pub(crate) sink: Option<Arc<dyn crate::sink::FileSink>>,
    pub(crate) shutdown: Shutdown,
    /// The access log in `out`; one line per resource step (#93)
    pub(crate) access: AccessLog,
}

async fn sync(args: Args, out: &mut dyn Write, err: &mut dyn Write, shutdown: Shutdown) -> i32 {
    let (config, servers) = match load_servers(&args, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let (state, sink) = match (args.state_store(), args.file_sink()) {
        (Ok(state), Ok(sink)) => (state, sink),
        (Err(e), _) | (_, Err(e)) => return usage_error(err, &e),
    };
    let run_id = match runs::reserve(&args.out(), state.as_ref(), sink.as_deref()).await {
        Ok(id) => id,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let options = Arc::new(SyncOptions {
        out: args.out(),
        state,
        run_id,
        resources: args.resources.clone(),
        deletes: args.deletes,
        allow_mass_deletes: args.allow_mass_deletes,
        media: args.media.then(|| {
            args.media_dir
                .clone()
                .unwrap_or_else(|| args.out().join("media"))
        }),
        start_over: args.start_over,
        max_file_bytes: args.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB) * 1024 * 1024,
        format: args.format,
        access: AccessLog::new(&args.out()),
        sink,
        shutdown,
    });

    let (tx, mut rx) = unbounded_channel::<String>();
    let task_options = Arc::clone(&options);
    let reports = {
        let task = move |server: ServerConfig| {
            let options = Arc::clone(&task_options);
            let tx = tx.clone();
            async move { sync_server(server, options, tx).await }
        };
        tracing::info!(run_id = %options.run_id, servers = servers.len(), "sync started");
        let run = run_servers(servers, config.max_concurrent_servers, task)
            .instrument(tracing::info_span!("run", run_id = %options.run_id));
        let progress = async {
            while let Some(line) = rx.recv().await {
                let _ = writeln!(err, "{line}");
            }
        };
        let (reports, ()) = tokio::join!(run, progress);
        reports
    };

    let mut manifest = RunManifest::new(&options.run_id);
    manifest.interrupted = options.shutdown.is_triggered();
    let mut media_manifest = MediaManifest::new(&options.run_id);
    for report in reports {
        match report.result {
            Ok((entries, media, schema_changes)) => {
                entries.into_iter().for_each(|e| manifest.add_resource(e));
                media_manifest.servers.extend(media);
                manifest.add_schema_changes(&report.name, schema_changes);
            }
            Err(message) => manifest.add_server_failure(report.name, message),
        }
    }
    let sink = options.sink.as_deref();
    let manifest_path = match manifest.write(&options.out) {
        Ok(p) => runs::deliver_manifest(&p, options.state.as_ref(), sink).await,
        Err(e) => Err(e),
    };
    let mut access_failed = false;
    if let Err(e) = options.access.deliver(&options.out, &options.run_id, sink) {
        let _ = writeln!(err, "error: access log: {e}");
        access_failed = true;
    }
    let mut metrics_failed = false;
    if let Some(path) = &args.metrics_file {
        if let Err(e) = write_metrics_file(&manifest, path) {
            let _ = writeln!(err, "error: {e}");
            metrics_failed = true;
        }
    }
    let mut webhook_failed = false;
    if let Some(var) = &config.webhook_url_env {
        match std::env::var(var) {
            Ok(url) if !url.trim().is_empty() => {
                let path = manifest_path.as_deref().ok();
                let summary = run_summary(&manifest, path);
                if let Err(e) = post_summary(url.trim(), &summary).await {
                    let _ = writeln!(err, "error: {e}");
                    webhook_failed = true;
                }
            }
            _ => {
                let _ = writeln!(err, "error: webhook_url_env: {var} is not set or empty");
                webhook_failed = true;
            }
        }
    }
    print_summary(&manifest, manifest_path.as_deref().ok(), out);
    let mut clean = manifest.is_clean() && !metrics_failed && !webhook_failed && !access_failed;
    if options.media.is_some() {
        for r in &media_manifest.servers {
            let _ = writeln!(out, "  {}", media_line(r));
        }
        match media_manifest
            .write(&options.out)
            .and_then(|p| runs::deliver(&p, sink))
        {
            Ok(p) => {
                let _ = writeln!(out, "media manifest: {p}");
            }
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                clean = false;
            }
        }
        clean &= media_manifest.is_clean();
    }
    if let Err(e) = &manifest_path {
        let _ = writeln!(err, "error: {e}");
        return 1;
    }
    runs::release(options.state.as_ref(), sink, &options.run_id).await;
    if manifest.interrupted {
        return EXIT_INTERRUPTED;
    }
    i32::from(!clean)
}

fn send(tx: &UnboundedSender<String>, line: String) {
    let _ = tx.send(line);
}

/// What one server's sync gives back: resource entries, the media report and schema changes
pub(crate) type ServerOutcome = (Vec<ResourceEntry>, Option<MediaReport>, Vec<String>);

async fn sync_server(
    server: ServerConfig,
    options: Arc<SyncOptions>,
    tx: UnboundedSender<String>,
) -> Result<ServerOutcome, String> {
    let handle = ClientHandle::new(&server).map_err(|e| e.to_string())?;
    sync_server_with(&handle, server, options, tx).await
}

/// Sync the selected resources of one server with a client the caller owns (the daemon shares one
/// client per server, so its rate limit and token cover every feed)
///
/// The handle reads the server's secrets again before each resource; after an authentication
/// failure, a resource is tried once more when the secrets changed (#92).
pub(crate) async fn sync_server_with(
    handle: &ClientHandle,
    server: ServerConfig,
    options: Arc<SyncOptions>,
    tx: UnboundedSender<String>,
) -> Result<ServerOutcome, String> {
    let name = server.name.clone();
    let credential = credential_id(&server.auth);
    // One access-log line per resource step; a line that cannot be written fails the resource
    let audit = |entry: &mut ResourceEntry, window_start: Option<String>| {
        let line = AccessEntry::new(&options.run_id, &credential, window_start, entry);
        if let Err(e) = options.access.append(&line) {
            tracing::error!(server = %entry.server, resource = %entry.resource, error = %e, "cannot write the access log");
            entry
                .error
                .get_or_insert_with(|| format!("access log: {e}"));
        }
    };
    let mut client = handle.client();
    let metadata = match client.fetch_metadata_model().await {
        Ok(m) => Ok(m),
        Err(e) => match is_auth_failure(&e.to_string())
            .then(|| handle.renew(&client))
            .flatten()
        {
            Some(renewed) => {
                client = renewed;
                client.fetch_metadata_model().await
            }
            None => Err(e),
        },
    }
    .map_err(|e| format!("cannot read $metadata: {e}"))?;
    // Media downloads borrow the client the server's sync started with
    let media_client = Arc::clone(&client);
    let (resources, missing) = select_resources(&server, &metadata, &options.resources);
    let store = CheckpointStore::with_store(Arc::clone(&options.state));
    let schema_changes = schema_changes(&server, &metadata, &store).await?;
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
    entries.iter_mut().for_each(|e| audit(e, None));
    if resources.is_empty() && entries.is_empty() {
        return Err("no resources to pull (check include/exclude in the configuration)".into());
    }

    let mut page_media = match &options.media {
        Some(dir) => Some(
            PageMedia::new(
                &media_client,
                &name,
                server.media.download_config(),
                Arc::clone(&options.state),
                dir,
                server.media.nested_field.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    let feed = EventFeed::detect(&metadata);
    let total = resources.len();
    for (i, resource) in resources.iter().enumerate() {
        if options.shutdown.is_triggered() {
            tracing::warn!("shutdown requested: not starting the remaining resources");
            break;
        }
        let explicit = !options.resources.is_empty();
        let label = format!("{name}/{resource}");
        // Events inside carry `resource`; the server and run spans add `server` and `run_id`
        let span = tracing::info_span!("resource", resource = %resource);
        send(
            &tx,
            format!("##progress step={}/{total} label={label}", i + 1),
        );

        let Some(plan) = plan_resource(&metadata, resource) else {
            span.in_scope(|| {
                tracing::info!("skipped: no single key or no {TIMESTAMP_FIELD}");
            });
            send(
                &tx,
                format!("[{label}] skipped: no single key or no {TIMESTAMP_FIELD}"),
            );
            if explicit {
                let mut entry = failed_entry(
                    &name,
                    resource,
                    &format!(
                        "cannot sync: needs a single-field key and a {TIMESTAMP_FIELD} property"
                    ),
                );
                audit(&mut entry, None);
                entries.push(entry);
            }
            continue;
        };

        // One writer per feed: a second process syncing this server and resource skips it
        let lease = match Lease::acquire(&options.state, &name, resource, LEASE_TTL).await {
            Ok(LeaseOutcome::Acquired(lease)) => lease,
            Ok(LeaseOutcome::Held(holder)) => {
                let message = format!("skipped: another process is syncing this feed ({holder})");
                span.in_scope(
                    || tracing::warn!(%holder, "feed skipped: leased by another process"),
                );
                send(&tx, format!("[{label}] {message}"));
                let mut entry = failed_entry(&name, resource, "");
                entry.error = None;
                entry.warnings.push(message);
                entries.push(entry);
                continue;
            }
            Err(e) => {
                span.in_scope(|| tracing::warn!(error = %e, "cannot take the feed's lease"));
                let mut entry = failed_entry(&name, resource, &e.to_string());
                audit(&mut entry, None);
                entries.push(entry);
                continue;
            }
        };

        let mut writer = match make_writer(
            &options,
            &metadata,
            &name,
            resource,
            server.expand_for(resource),
        ) {
            Ok(w) => w,
            Err(e) => {
                span.in_scope(|| tracing::warn!(error = %e, "cannot open the output file"));
                let mut entry = failed_entry(&name, resource, &e.to_string());
                audit(&mut entry, None);
                entries.push(entry);
                lease.release().await;
                continue;
            }
        };
        let started = std::time::Instant::now();
        client = handle.client();
        let mut stats_before = client.stats();
        let filter = server.filter_for(resource);
        let mut spec = StandardReplication::new(resource, &plan.key);
        if let Some(n) = server.page_size_for(resource) {
            spec = spec.page_size(n);
        }
        if let Some(f) = &filter {
            spec = spec.filter(f.as_str());
        }
        let expand: Vec<&str> = server
            .expand_for(resource)
            .iter()
            .map(String::as_str)
            .collect();
        if !expand.is_empty() {
            spec = spec.expand(&expand);
        }
        // A full load when there is no checkpoint to start from
        let saved = store.load(&name, resource).await;
        let full_load = options.start_over || matches!(saved, Ok(None) | Err(_));
        let window_start = match saved {
            Ok(Some(c)) if !full_load => Some(c.timestamp),
            _ => None,
        };
        let mut checked = CheckingWriter::new(writer.as_mut(), &plan.key);
        let mut pull = Pull {
            handle,
            client: &mut client,
            stats_before: &mut stats_before,
            server: &name,
            resource,
            spec,
            store: &store,
            start_over: options.start_over,
            media: page_media.as_mut(),
            shutdown: &options.shutdown,
            span: &span,
        };
        let mut entry = if let Some(flag) = &server.deletes.flag_field {
            let mut flagged = FlagDeleteWriter::new(&mut checked, flag, &plan.key);
            let mut entry = pull.run(&mut flagged).await;
            entry.deletes = flagged.deletes();
            entry
        } else {
            pull.run(&mut checked).await
        };
        entry.warnings = checked.warnings();
        if full_load && entry.error.is_none() && !entry.interrupted {
            // A full load should hold what the server counts (servers without $count are skipped)
            let written = entry.records;
            entry.warnings.extend(
                count_warning(&client, resource, filter.as_deref(), written)
                    .instrument(span.clone())
                    .await,
            );
        }
        if options.deletes
            && entry.error.is_none()
            && !entry.interrupted
            && server.deletes.flag_field.is_none()
        {
            let deleted = reconcile_deletes(
                &client,
                &name,
                &DeleteTarget::new(resource, &plan.key)
                    .with_filter(filter.as_deref())
                    .with_limits(server.deletes.limits(options.allow_mass_deletes)),
                feed.as_ref(),
                writer.as_mut(),
                &store,
            )
            .instrument(span.clone())
            .await;
            entry.deletes = deleted.deletes;
            entry.deletes_held = deleted.deletes_held;
            entry.warnings.extend(deleted.warnings);
            entry.error = deleted.error;
            if !deleted.files.is_empty() {
                entry.files = deleted.files; // the writer's list is cumulative
            }
        }
        entry.stats = ResourceStats::new(started.elapsed(), &client.stats().since(&stats_before));
        span.in_scope(|| {
            for w in &entry.warnings {
                tracing::warn!(warning = %w, "check finding");
            }
        });
        span.in_scope(|| match &entry.error {
            None if entry.interrupted => {
                tracing::warn!(records = entry.records, "resource interrupted")
            }
            None => tracing::info!(
                records = entry.records,
                deletes = entry.deletes,
                "resource synced"
            ),
            Some(e) => tracing::warn!(
                records = entry.records,
                deletes = entry.deletes,
                error = %e,
                "resource failed"
            ),
        });
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
        audit(&mut entry, window_start);
        entries.push(entry);
        lease.release().await;
    }
    Ok((entries, page_media.map(PageMedia::finish), schema_changes))
}

/// One resource's records pull, tried once more with a new client when it failed to authenticate
/// and the server's secrets have changed since (#92)
struct Pull<'a, 'm> {
    handle: &'a ClientHandle,
    client: &'a mut Arc<ResoClient>,
    stats_before: &'a mut ClientStats,
    server: &'a str,
    resource: &'a str,
    spec: StandardReplication,
    store: &'a CheckpointStore,
    start_over: bool,
    media: Option<&'a mut PageMedia<'m>>,
    shutdown: &'a Shutdown,
    span: &'a tracing::Span,
}

impl Pull<'_, '_> {
    async fn run(&mut self, writer: &mut dyn RecordWriter) -> ResourceEntry {
        let mut entry = self.once(writer, self.start_over).await;
        let auth_failed = entry.error.as_deref().is_some_and(is_auth_failure);
        if let Some(renewed) = auth_failed
            .then(|| self.handle.renew(self.client))
            .flatten()
        {
            self.span
                .in_scope(|| tracing::info!("retrying with the changed secrets"));
            *self.client = renewed;
            *self.stats_before = self.client.stats();
            let earlier = entry.records;
            // From the checkpoint the first try saved; the writer carries on with its files
            entry = self.once(writer, false).await;
            entry.records += earlier;
        }
        entry
    }

    async fn once(&mut self, writer: &mut dyn RecordWriter, start_over: bool) -> ResourceEntry {
        sync_resource_media(
            self.client,
            self.server,
            self.resource,
            self.spec.clone(),
            writer,
            self.store,
            start_over,
            self.media.as_deref_mut(),
            self.shutdown,
        )
        .instrument(self.span.clone())
        .await
    }
}

/// Compare the server's `$metadata` with the snapshot of the previous run and save the new one
///
/// The first run has nothing to compare with and only saves. Resources are the ones the
/// configuration allows, not only the ones this run was asked for, so `--resource` runs do not
/// make the next full run look like a change.
async fn schema_changes(
    server: &ServerConfig,
    metadata: &Metadata,
    store: &CheckpointStore,
) -> Result<Vec<String>, String> {
    let (all, _) = select_resources(server, metadata, &[]);
    let now = SchemaSnapshot::from_metadata(metadata, &all);
    let before = store
        .load_schema(&server.name)
        .await
        .map_err(|e| e.to_string())?;
    store
        .save_schema(&server.name, &now)
        .await
        .map_err(|e| e.to_string())?;
    let changes = before.map(|b| b.changes_to(&now)).unwrap_or_default();
    for change in &changes {
        tracing::warn!(server = %server.name, %change, "schema changed");
    }
    Ok(changes)
}

fn make_writer(
    options: &SyncOptions,
    #[cfg_attr(not(feature = "parquet"), allow(unused_variables))] metadata: &Metadata,
    server: &str,
    resource: &str,
    #[cfg_attr(not(feature = "parquet"), allow(unused_variables))] expanded: &[String],
) -> Result<Box<dyn RecordWriter>, crate::error::IngestError> {
    match options.format {
        Format::Ndjson => {
            let mut writer = NdjsonWriter::new(
                &options.out,
                server,
                resource,
                &options.run_id,
                options.max_file_bytes,
            )?;
            if let Some(sink) = &options.sink {
                writer = writer.with_sink(Arc::clone(sink));
            }
            Ok(Box::new(writer))
        }
        #[cfg(feature = "parquet")]
        Format::Parquet => {
            let entity =
                metadata
                    .resource(resource)
                    .ok_or_else(|| crate::error::IngestError::Output {
                        path: options.out.display().to_string(),
                        message: format!("no entity type for {resource} in $metadata"),
                    })?;
            let mut writer = ParquetWriter::new(
                &options.out,
                server,
                resource,
                &options.run_id,
                options.max_file_bytes,
                entity,
            )?
            .with_expanded(expanded);
            if let Some(sink) = &options.sink {
                writer = writer.with_sink(Arc::clone(sink));
            }
            Ok(Box::new(writer))
        }
    }
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
        stats: ResourceStats::default(),
        warnings: Vec::new(),
        deletes_held: 0,
        interrupted: false,
    }
}

fn print_summary(manifest: &RunManifest, path: Option<&str>, out: &mut dyn Write) {
    let records: u64 = manifest.resources.iter().map(|r| r.records).sum();
    let deletes: u64 = manifest.resources.iter().map(|r| r.deletes).sum();
    let _ = writeln!(
        out,
        "run {}: {} resources, {records} records, {deletes} deletes",
        manifest.run_id,
        manifest.resources.len()
    );
    for r in &manifest.resources {
        let outcome = r.error.as_ref().map_or(
            if r.interrupted { "INTERRUPTED" } else { "ok" }.to_string(),
            |e| format!("FAILED: {e}"),
        );
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
    if manifest.interrupted {
        let _ = writeln!(
            out,
            "INTERRUPTED by a shutdown request: output and checkpoints are consistent, the next run continues"
        );
    }
    if failed.is_empty() {
        let _ = writeln!(out, "all servers ok");
    } else {
        let names: Vec<&str> = failed.into_iter().collect();
        let _ = writeln!(out, "FAILED servers: {}", names.join(", "));
    }
    if let Some(path) = path {
        let _ = writeln!(out, "manifest: {path}");
    }
}

// ---------------------------------------------------------------- media

async fn media(args: Args, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match args.file_sink() {
        Ok(None) => {}
        Ok(Some(_)) => {
            let _ = writeln!(
                err,
                "error: `media` reads the data files in --out, but with --s3-bucket they are in the \
                 bucket; use `sync --media` to download Media files while syncing"
            );
            return 2;
        }
        Err(e) => return usage_error(err, &e),
    }
    let (config, servers) = match load_servers(&args, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let state = match args.state_store() {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    let options = Arc::new(MediaOptions {
        out: args.out(),
        state,
        media_dir: args
            .media_dir
            .clone()
            .unwrap_or_else(|| args.out().join("media")),
        limit: args.limit,
    });
    let run_id = match reserve_run_id(args.out()) {
        Ok(id) => id,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let mut manifest = MediaManifest::new(run_id);

    let (tx, mut rx) = unbounded_channel::<String>();
    let task_options = Arc::clone(&options);
    let reports = {
        let task = move |server: ServerConfig| {
            let options = Arc::clone(&task_options);
            let tx = tx.clone();
            async move {
                let client = client_for(&server)?;
                let download = server.media.download_config();
                let progress = |line: String| send(&tx, line);
                let report = sync_media(&client, &server.name, download, &options, progress).await;
                let line = media_line(&report);
                if report.error.is_some() {
                    tracing::warn!("{line}");
                } else {
                    tracing::info!("{line}");
                }
                send(&tx, line);
                Ok::<MediaReport, String>(report)
            }
        };
        tracing::info!(run_id = %manifest.run_id, servers = servers.len(), "media run started");
        let run = run_servers(servers, config.max_concurrent_servers, task)
            .instrument(tracing::info_span!("run", run_id = %manifest.run_id));
        let progress = async {
            while let Some(line) = rx.recv().await {
                let _ = writeln!(err, "{line}");
            }
        };
        let (reports, ()) = tokio::join!(run, progress);
        reports
    };
    for report in reports {
        manifest
            .servers
            .push(report.result.unwrap_or_else(|message| MediaReport {
                server: report.name,
                error: Some(message),
                ..MediaReport::default()
            }));
    }

    let path = manifest.write(&options.out);
    let _ = writeln!(out, "media run {}:", manifest.run_id);
    for r in &manifest.servers {
        let _ = writeln!(out, "  {}", media_line(r));
        for f in r.failed.iter().take(5) {
            let _ = writeln!(out, "    {}: {}", f.key, f.error);
        }
        if r.failed.len() > 5 {
            let _ = writeln!(out, "    ... {} more in the manifest", r.failed.len() - 5);
        }
    }
    match &path {
        Ok(p) => {
            let _ = writeln!(out, "manifest: {}", p.display());
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    }
    i32::from(!manifest.is_clean())
}

fn media_line(r: &MediaReport) -> String {
    if let Some(e) = &r.error {
        return format!("[{}/media] FAILED: {e}", r.server);
    }
    let mut line = format!(
        "[{}/media] {} downloaded, {} not modified, {} deleted, {} failed, {} pending, {} bytes",
        r.server,
        r.downloaded,
        r.not_modified,
        r.deleted,
        r.failed.len(),
        r.pending,
        r.bytes
    );
    if r.without_url > 0 {
        line.push_str(&format!(", {} without MediaURL", r.without_url));
    }
    if !r.files_skipped.is_empty() {
        line.push_str(&format!(
            ", {} non-NDJSON data files not read",
            r.files_skipped.len()
        ));
    }
    line
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

async fn status(args: Args, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let state = match args.state_store() {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    let location = state.location("");
    // With a bucket the manifests are there; the state keeps a copy of the newest one
    let last_run = match state.get(runs::LAST_RUN_KEY).await {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let last_run = match last_run {
        Some(v) => match serde_json::from_slice::<Value>(&v.bytes) {
            Ok(value) => Ok(Some((state.location(runs::LAST_RUN_KEY), value))),
            Err(e) => Err(format!(
                "{} is not valid JSON: {e}",
                state.location(runs::LAST_RUN_KEY)
            )),
        },
        None => last_manifest(&args.out()),
    };
    let store = CheckpointStore::with_store(state);
    let checkpoints = match store.list().await {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let _ = writeln!(out, "checkpoints in {location}:");
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

    match last_run {
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
            let _ = writeln!(out, "  {failures} failures; manifest {path}");
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
fn last_manifest(dir: &Path) -> Result<Option<(String, Value)>, String> {
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
    Ok(Some((path.display().to_string(), value)))
}
