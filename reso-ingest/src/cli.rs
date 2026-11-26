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
use reso_client::{count_requests, Metadata, RateUsage, ResoClient, StandardReplication};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::Instrument;

use crate::audit::{credential_id, AccessEntry, AccessLog};
use crate::backfill::{self, BackfillEntry, BackfillWindow, ProgressWriter};
use crate::checkpoint::CheckpointStore;
use crate::checks::{count_warning, CheckingWriter, SchemaSnapshot};
use crate::config::{Config, ServerConfig};
use crate::deletes::{reconcile_deletes, DeleteTarget, EventFeed, FlagDeleteWriter};
use crate::error::IngestError;
use crate::lease::{live_leases, Lease, LeaseHolder, LeaseOutcome};
use crate::logging::{env_filter, log_subscriber, LogFormat, LOG_FORMAT_ENV};
use crate::manifest::{sync_resource_media, ResourceEntry, ResourceStats, RunManifest};
use crate::media::{
    page_pipeline, sync_media, MediaManifest, MediaOptions, MediaReport, PipelineOptions,
    DEFAULT_WORKERS,
};
use crate::media_store::MediaRoot;
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
        /// Server configuration: a TOML file, or a directory of them (see reso-ingest/servers.example.toml)
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
        /// Download the media files of each page while the sync goes on
        ///
        /// Takes the Media records (or, for other resources, the `Media` arrays that `expand`
        /// brings) of each page and queues the new and changed files for [server.media] workers
        /// (default 4), so single-use or short-lived URLs are used while fresh. Files go to
        /// --media-dir, or the bucket's `media/` folder. Uses the [server.media] limits.
        #[arg(long)]
        media: bool,
        /// Where files go with --media without a bucket (default: <out>/media)
        #[arg(long)]
        media_dir: Option<PathBuf>,
        /// Delete the saved checkpoints of the selected resources first
        #[arg(long)]
        start_over: bool,
        /// Backfill: report progress and time left, and run within the backfill's share of the
        /// rate limits ([server.backfill] rate_percent)
        ///
        /// Asks the server's count of the records left, prints `##progress` lines after every
        /// page, and holds a marker other syncs of the server see: while it runs they use the rest
        /// of the rate limits, and they do not start a full load of a feed it is loading. Stops and
        /// resumes like any sync.
        #[arg(long)]
        backfill: bool,
        /// Backfill only records with ModificationTimestamp at or after this time (RFC 3339)
        ///
        /// A window has its own checkpoint, so windows can run on separate workers. A window with
        /// no --to ends at the present; when it finishes and the feed has no checkpoint yet, its
        /// checkpoint becomes the feed's.
        #[arg(long, requires = "backfill", conflicts_with = "deletes")]
        from: Option<String>,
        /// Backfill only records with ModificationTimestamp before this time (RFC 3339)
        #[arg(long, requires = "backfill", conflicts_with = "deletes")]
        to: Option<String>,
        /// Start a new data file after this many MiB
        #[arg(long, default_value_t = DEFAULT_MAX_FILE_MB, value_parser = clap::value_parser!(u64).range(1..))]
        max_file_mb: u64,
        /// Data file format
        #[arg(long, value_enum, default_value_t = Format::Ndjson)]
        format: Format,
        /// Write Parquet files into Hive-style folders: source=<server>/resource=<name>/date=<day>/
        ///
        /// The date is the UTC day the rows were fetched. Needs --format parquet.
        #[arg(long)]
        partition: bool,
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
    /// /healthz, /readyz, /metrics and /status (and status pages with --ui) over plain HTTP on
    /// --listen (no authentication: use a private address). Stops after the current pages on SIGTERM or Ctrl-C and exits 0.
    Serve {
        /// Server configuration: a TOML file, or a directory of them (see reso-ingest/servers.example.toml)
        #[arg(long)]
        config: PathBuf,
        /// Only this server; repeat for several (default: all)
        #[arg(long = "server")]
        servers: Vec<String>,
        /// Address for the endpoints
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: std::net::SocketAddr,
        /// Re-read --config this often and apply changes without a restart ("30s", "5m")
        ///
        /// Feeds are added, removed, paused (`paused = true`) and resumed; a feed whose settings
        /// changed restarts after its current page. SIGHUP reloads at once, with or without this.
        /// A configuration that does not parse is logged and the running one kept.
        #[arg(long, value_parser = parse_reload_every)]
        reload_every: Option<std::time::Duration>,
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
        /// Download the media files of each page while the sync goes on
        #[arg(long)]
        media: bool,
        /// Where files go with --media without a bucket (default: <out>/media)
        #[arg(long)]
        media_dir: Option<PathBuf>,
        /// Start a new data file after this many MiB
        #[arg(long, default_value_t = DEFAULT_MAX_FILE_MB, value_parser = clap::value_parser!(u64).range(1..))]
        max_file_mb: u64,
        /// Data file format
        #[arg(long, value_enum, default_value_t = Format::Ndjson)]
        format: Format,
        /// Write Parquet files into Hive-style folders: source=<server>/resource=<name>/date=<day>/
        ///
        /// The date is the UTC day the rows were fetched. Needs --format parquet.
        #[arg(long)]
        partition: bool,
        /// Also serve read-only status pages: / (every feed), /feed/<server>/<resource>, /runs
        ///
        /// Plain HTML that reloads every 15 seconds; no authentication, like the other endpoints.
        /// Needs the `ui` feature (on by default).
        #[arg(long)]
        ui: bool,
        /// With --ui, link each feed to Grafana's reso-ingest dashboard filtered to that feed
        ///
        /// Grafana's root URL, e.g. http://127.0.0.1:3000; the dashboard is the one in
        /// deploy/observability (uid reso-ingest-serve).
        #[arg(long, value_name = "URL", requires = "ui")]
        grafana_url: Option<String>,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Download the files of the Media records that `sync` wrote
    ///
    /// Reads the NDJSON Media data files listed in the manifests in --out, and downloads new and
    /// changed files into --media-dir/<server>/, named by MediaKey. Files that failed are retried
    /// on the next run. Downloads use the [server.media] limits, not the API's.
    Media {
        /// Server configuration: a TOML file, or a directory of them (see reso-ingest/servers.example.toml)
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
        /// Server configuration: a TOML file, or a directory of them (see reso-ingest/servers.example.toml)
        #[arg(long)]
        config: PathBuf,
        /// Only this server; repeat for several (default: all)
        #[arg(long = "server")]
        servers: Vec<String>,
        /// Only this resource; repeat for several (default: all the config allows)
        #[arg(long = "resource")]
        resources: Vec<String>,
        /// Onboarding: instead of the model, check that the credentials can read each resource
        /// (one record and its count) and print a starter configuration block in TOML
        #[arg(long)]
        starter: bool,
    },
    /// Delete one server's data files, media files and state when its licence ends
    ///
    /// A dry run by default: lists what would go. With --execute (and --by) it deletes, then writes
    /// a purge record with its SHA-256 to <out>/audit/ and a line to the access log. Data files are
    /// found by their source tag and the run manifests. Refuses while another process holds a lease
    /// on one of the server's feeds. The access log and run manifests are kept. With a bucket, data
    /// files and state are deleted there (media files stay local) and the record is copied to it.
    Purge {
        /// The server to purge (its name in the configuration and the `source` tag)
        #[arg(long)]
        server: String,
        /// Output directory of the data files, manifests and audit log
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// State directory (default: <out>/state)
        #[arg(long)]
        state: Option<PathBuf>,
        /// Media directory (default: <out>/media); the server's files are in <media-dir>/<server>/
        #[arg(long)]
        media_dir: Option<PathBuf>,
        /// Delete; without it nothing changes
        #[arg(long, requires = "by")]
        execute: bool,
        /// Who runs the purge, for the purge record (required with --execute)
        #[arg(long, requires = "execute")]
        by: Option<String>,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Delete one feed's checkpoint, so its next sync is a full load
    ///
    /// Takes the feed's lease while it works, so it refuses while a sync or `serve` is pulling
    /// that feed. Prints the checkpoint it removed; exit code 1 when there was none.
    Reset {
        /// The server's name in the configuration
        #[arg(long)]
        server: String,
        /// The resource (`Property`, `Member`, ...)
        #[arg(long)]
        resource: String,
        /// Output directory (the state is in <out>/state)
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// Checkpoint directory (default: <out>/state)
        #[arg(long)]
        state: Option<PathBuf>,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Move one feed's checkpoint back, so its next sync pulls again every record modified after
    /// a time
    ///
    /// Sets the checkpoint to `--after` with no key: the next run asks for records whose
    /// ModificationTimestamp is strictly later. Refuses a time later than the saved checkpoint
    /// (that would skip records); with no checkpoint it sets the starting point of the first run.
    /// Takes the feed's lease like `reset`. To pull a closed window of time without moving the
    /// checkpoint, use `sync --backfill --from --to`.
    Replay {
        /// The server's name in the configuration
        #[arg(long)]
        server: String,
        /// The resource (`Property`, `Member`, ...)
        #[arg(long)]
        resource: String,
        /// RFC 3339 time, e.g. 2026-11-01T00:00:00Z; records modified after it are pulled again
        #[arg(long)]
        after: String,
        /// Output directory (the state is in <out>/state)
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// Checkpoint directory (default: <out>/state)
        #[arg(long)]
        state: Option<PathBuf>,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// List past runs from their manifests, newest first, one line per feed
    ///
    /// Reads `manifest.*.json` in `--out`, or in the bucket's `<prefix>/data/` with a bucket.
    /// `serve` keeps a manifest only for runs that pulled, deleted or failed something.
    History {
        /// Only this server
        #[arg(long)]
        server: Option<String>,
        /// Only this resource
        #[arg(long)]
        resource: Option<String>,
        /// How many runs to show (of those that match)
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Directory holding the manifests
        #[arg(long, default_value = DEFAULT_OUT)]
        out: PathBuf,
        /// Not used: history reads no state. Present because the bucket options conflict with it
        #[arg(long, hide = true)]
        state: Option<PathBuf>,
        #[command(flatten)]
        storage: StorageArgs,
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
/// With `--s3-bucket` or `--gcs-bucket`, checkpoints, leases and media indexes go to
/// `<prefix>/state/` in the bucket and finished data files to `<prefix>/data/`, so several workers
/// share them; `sync --media` puts media files in `<prefix>/media/`. Run manifests also go to
/// `<prefix>/data/`; the metrics file and the audit log stay in `--out`.
/// Credentials come from the standard `AWS_*` or `GOOGLE_*` environment variables, never from
/// options.
#[derive(Debug, Clone, Default, clap::Args)]
#[cfg_attr(
    feature = "s3",
    command(group = clap::ArgGroup::new("bucket").args(["s3_bucket", "gcs_bucket"]))
)]
struct StorageArgs {
    /// S3-compatible bucket for state and data files (env RESO_S3_BUCKET)
    #[cfg(feature = "s3")]
    #[arg(long, env = "RESO_S3_BUCKET", conflicts_with = "state")]
    s3_bucket: Option<String>,
    /// Google Cloud Storage bucket for state and data files, through GCS's own API (env
    /// RESO_GCS_BUCKET; key file from GOOGLE_SERVICE_ACCOUNT or GOOGLE_APPLICATION_CREDENTIALS)
    #[cfg(feature = "s3")]
    #[arg(long, env = "RESO_GCS_BUCKET", conflicts_with = "state")]
    gcs_bucket: Option<String>,
    /// Key prefix inside the bucket, S3 or GCS (env RESO_S3_PREFIX)
    #[cfg(feature = "s3")]
    #[arg(
        long,
        visible_alias = "gcs-prefix",
        env = "RESO_S3_PREFIX",
        default_value = "",
        requires = "bucket"
    )]
    s3_prefix: String,
    /// Endpoint URL of a non-AWS S3 store: R2, MinIO, RustFS (env RESO_S3_ENDPOINT)
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
    /// One DELETE per object instead of multi-delete (stores without multi-delete)
    #[cfg(feature = "s3")]
    #[arg(long, requires = "s3_bucket")]
    s3_single_delete: bool,
    /// Skip the start-up check that the bucket honours conditional writes. Without them leases
    /// and run ids do nothing, so use this only when one worker alone ever uses the bucket
    #[cfg(feature = "s3")]
    #[arg(long, requires = "bucket")]
    s3_unsafe_no_conditional_writes: bool,
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
    config: PathBuf,
    servers: Vec<String>,
    resources: Vec<String>,
    out: Option<PathBuf>,
    state: Option<PathBuf>,
    deletes: bool,
    allow_mass_deletes: bool,
    media: bool,
    start_over: bool,
    backfill: Option<BackfillWindow>,
    max_file_mb: Option<u64>,
    format: Format,
    partition: bool,
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

    /// The bucket options when `--s3-bucket` or `--gcs-bucket` is set, with `suffix` under the
    /// prefix
    #[cfg(feature = "s3")]
    fn s3_options(&self, suffix: &str) -> Option<crate::S3Options> {
        let s = &self.storage;
        if let Some(bucket) = s.gcs_bucket.clone() {
            return Some(crate::S3Options {
                provider: crate::Provider::Gcs,
                bucket,
                prefix: bucket_prefix(&s.s3_prefix, suffix),
                ..crate::S3Options::default()
            });
        }
        let bucket = s.s3_bucket.clone()?;
        Some(crate::S3Options {
            provider: crate::Provider::S3,
            bucket,
            prefix: bucket_prefix(&s.s3_prefix, suffix),
            endpoint: s.s3_endpoint.clone(),
            region: s.s3_region.clone(),
            allow_http: s.s3_allow_http,
            single_delete: s.s3_single_delete,
        })
    }

    /// The state store: the bucket with `--s3-bucket` or `--gcs-bucket`, else the state directory
    fn state_store(&self) -> Result<Arc<dyn StateStore>, IngestError> {
        #[cfg(feature = "s3")]
        if let Some(options) = self.s3_options("state") {
            return Ok(Arc::new(crate::S3StateStore::new(&options)?));
        }
        Ok(Arc::new(FileStateStore::new(self.state())))
    }

    /// The state store for a command that writes leases and run ids (`sync`, `serve`): a bucket
    /// is first checked to honour conditional writes (#131), unless
    /// `--s3-unsafe-no-conditional-writes`
    async fn writable_state_store(&self) -> Result<Arc<dyn StateStore>, IngestError> {
        #[cfg(feature = "s3")]
        if let Some(options) = self.s3_options("state") {
            let store = crate::S3StateStore::new(&options)?;
            if self.storage.s3_unsafe_no_conditional_writes {
                tracing::warn!(
                    store = %store.location(""),
                    "conditional-write check skipped (--s3-unsafe-no-conditional-writes)"
                );
            } else {
                store.check_conditional_writes().await?;
            }
            return Ok(Arc::new(store));
        }
        self.state_store()
    }

    /// Where `sync --media` and `serve --media` put files: `<prefix>/media/` in the bucket with
    /// `--s3-bucket` or `--gcs-bucket`, else `--media-dir` (default `<out>/media`)
    fn media_root(&self) -> MediaRoot {
        #[cfg(feature = "s3")]
        if let Some(options) = self.s3_options("media") {
            return MediaRoot::Bucket(options);
        }
        MediaRoot::Dir(
            self.media_dir
                .clone()
                .unwrap_or_else(|| self.out().join("media")),
        )
    }

    /// Where finished data files go: the bucket with `--s3-bucket` or `--gcs-bucket`, else they
    /// stay in `--out`
    fn file_sink(&self) -> Result<Option<Arc<dyn crate::sink::FileSink>>, IngestError> {
        #[cfg(feature = "s3")]
        if let Some(options) = self.s3_options("data") {
            return Ok(Some(Arc::new(crate::S3FileSink::new(&options)?)));
        }
        Ok(None)
    }
}

/// `<prefix>/<suffix>` with the prefix's slashes trimmed, or `suffix` alone
#[cfg(feature = "s3")]
fn bucket_prefix(prefix: &str, suffix: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        suffix.to_string()
    } else {
        format!("{prefix}/{suffix}")
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
            backfill,
            from,
            to,
            max_file_mb,
            format,
            partition,
            metrics_file,
            storage,
        } => {
            let backfill = match backfill.then(|| BackfillWindow::new(from, to)).transpose() {
                Ok(b) => b,
                Err(e) => {
                    let _ = writeln!(err, "error: {e}");
                    return 2;
                }
            };
            let args = Args {
                config,
                servers,
                resources,
                out: Some(out_dir),
                state,
                deletes,
                allow_mass_deletes,
                media,
                media_dir,
                start_over,
                backfill,
                max_file_mb: Some(max_file_mb),
                format,
                partition,
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
            reload_every,
            out: out_dir,
            state,
            deletes,
            allow_mass_deletes,
            media,
            media_dir,
            max_file_mb,
            format,
            partition,
            ui,
            grafana_url,
            storage,
        } => {
            let args = Args {
                config,
                servers,
                out: Some(out_dir),
                state,
                deletes,
                allow_mass_deletes,
                media,
                media_dir,
                max_file_mb: Some(max_file_mb),
                format,
                partition,
                storage,
                ..Args::default()
            };
            serve(args, listen, reload_every, ui, grafana_url, err, shutdown).await
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
                config,
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
            starter,
        } => {
            let args = Args {
                config,
                servers,
                resources,
                ..Args::default()
            };
            if starter {
                onboard(args, out, err).await
            } else {
                discover(args, out, err).await
            }
        }
        Command::Purge {
            server,
            out: out_dir,
            state,
            media_dir,
            execute,
            by,
            storage,
        } => {
            let args = Args {
                servers: vec![server],
                out: Some(out_dir),
                state,
                media_dir,
                storage,
                ..Args::default()
            };
            let by = if execute { by } else { None };
            purge(args, by, out, err).await
        }
        Command::Reset {
            server,
            resource,
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
            move_checkpoint(args, &server, &resource, None, out, err).await
        }
        Command::Replay {
            server,
            resource,
            after,
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
            move_checkpoint(args, &server, &resource, Some(&after), out, err).await
        }
        Command::History {
            server,
            resource,
            limit,
            out: out_dir,
            state: _,
            storage,
        } => {
            let args = Args {
                out: Some(out_dir),
                storage,
                ..Args::default()
            };
            history(
                args,
                server.as_deref(),
                resource.as_deref(),
                limit,
                out,
                err,
            )
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
    let config = match Config::from_path(&args.config) {
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
/// `--partition` only applies to Parquet files
fn check_partition(args: &Args, err: &mut dyn Write) -> Result<(), i32> {
    if args.partition && args.format == Format::Ndjson {
        let _ = writeln!(err, "error: --partition needs --format parquet");
        return Err(2);
    }
    Ok(())
}

fn usage_error(err: &mut dyn Write, e: &IngestError) -> i32 {
    let _ = writeln!(err, "error: {e}");
    2
}

// ---------------------------------------------------------------- serve

async fn serve(
    args: Args,
    listen: std::net::SocketAddr,
    reload_every: Option<std::time::Duration>,
    ui: bool,
    grafana_url: Option<String>,
    err: &mut dyn Write,
    shutdown: Shutdown,
) -> i32 {
    if let Err(code) = check_partition(&args, err) {
        return code;
    }
    let (mut config, servers) = match load_servers(&args, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    config.servers = servers;
    let mut options = ServeOptions::new(args.out());
    options.listen = listen;
    options.state = match args.writable_state_store().await {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    options.sink = match args.file_sink() {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    options.deletes = args.deletes;
    options.allow_mass_deletes = args.allow_mass_deletes;
    options.media = args.media.then(|| args.media_root());
    options.max_file_bytes = args.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB) * 1024 * 1024;
    options.format = args.format;
    options.partition = args.partition;
    options.ui = ui;
    options.grafana_url = grafana_url;
    match Daemon::start(config.clone(), options, shutdown.clone()).await {
        Ok(daemon) => {
            let _ = writeln!(err, "listening on http://{}", daemon.local_addr());
            let daemon = Arc::new(daemon);
            let reloads = tokio::spawn(reload_loop(
                Arc::clone(&daemon),
                ReloadSource {
                    path: args.config.clone(),
                    servers: args.servers.clone(),
                },
                config,
                reload_every,
                shutdown,
            ));
            daemon.wait().await;
            reloads.abort();
            0
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            2
        }
    }
}

fn parse_reload_every(text: &str) -> Result<std::time::Duration, String> {
    crate::config::parse_interval(text)
}

/// Where `serve` re-reads its configuration from
struct ReloadSource {
    path: PathBuf,
    /// `--server`: only these (all when empty); a name no longer in the file is not an error
    servers: Vec<String>,
}

impl ReloadSource {
    fn load(&self) -> Result<Config, IngestError> {
        let mut config = Config::from_path(&self.path)?;
        if !self.servers.is_empty() {
            config.servers.retain(|s| self.servers.contains(&s.name));
        }
        Ok(config)
    }
}

/// Reload on SIGHUP and every `every`; a configuration equal to the running one changes nothing
async fn reload_loop(
    daemon: Arc<Daemon>,
    source: ReloadSource,
    mut running: Config,
    every: Option<std::time::Duration>,
    shutdown: Shutdown,
) {
    let mut hangup = Hangup::new();
    loop {
        let poll = async {
            match every {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        let signalled = tokio::select! {
            () = poll => false,
            () = hangup.recv() => true,
            () = shutdown.triggered() => return,
        };
        let config = match source.load() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "reload: configuration not applied, the running one is kept");
                daemon.reload_failed(&e.to_string());
                continue;
            }
        };
        if config == running {
            if signalled {
                tracing::info!("reload: configuration unchanged");
            }
            continue;
        }
        if let Err(e) = daemon.reload(&config) {
            tracing::error!(error = %e, "reload: applied in part");
        }
        running = config;
    }
}

/// SIGHUP, where there is one
struct Hangup {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

impl Hangup {
    fn new() -> Self {
        Self {
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok(),
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some(s) = &mut self.signal {
            if s.recv().await.is_some() {
                return;
            }
        }
        std::future::pending::<()>().await;
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
    pub(crate) media: Option<MediaRoot>,
    pub(crate) start_over: bool,
    /// `sync --backfill` and its window
    pub(crate) backfill: Option<BackfillWindow>,
    pub(crate) max_file_bytes: u64,
    pub(crate) format: Format,
    /// Hive-style partition folders for Parquet files (`--partition`)
    #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
    pub(crate) partition: bool,
    /// Where finished data files go; `None` leaves them in `out`
    pub(crate) sink: Option<Arc<dyn crate::sink::FileSink>>,
    pub(crate) shutdown: Shutdown,
    /// The access log in `out`; one line per resource step (#93)
    pub(crate) access: AccessLog,
}

async fn sync(args: Args, out: &mut dyn Write, err: &mut dyn Write, shutdown: Shutdown) -> i32 {
    if let Err(code) = check_partition(&args, err) {
        return code;
    }
    let (config, mut servers) = match load_servers(&args, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    servers.retain(|s| {
        if s.paused {
            let _ = writeln!(err, "{}: paused in the configuration, skipped", s.name);
        }
        !s.paused
    });
    let (state, sink) = match (args.writable_state_store().await, args.file_sink()) {
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
        media: args.media.then(|| args.media_root()),
        start_over: args.start_over,
        backfill: args.backfill.clone(),
        max_file_bytes: args.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB) * 1024 * 1024,
        format: args.format,
        partition: args.partition,
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
            Ok(outcome) => {
                outcome
                    .entries
                    .into_iter()
                    .for_each(|e| manifest.add_resource(e));
                media_manifest.servers.extend(outcome.media);
                manifest.add_schema_changes(&report.name, outcome.schema_changes);
                manifest.add_rate_usage(&report.name, &outcome.rate_usage);
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
        if let Err(e) = write_metrics_file(&manifest, &media_manifest.servers, path) {
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

/// What one server's sync gives back
pub(crate) struct ServerOutcome {
    /// One entry per resource
    pub(crate) entries: Vec<ResourceEntry>,
    /// The media report (with `--media`)
    pub(crate) media: Option<MediaReport>,
    /// `$metadata` changes since the previous run
    pub(crate) schema_changes: Vec<String>,
    /// How much of its rate limit the server's client had used at the end
    pub(crate) rate_usage: RateUsage,
}

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
    let mut backfills = apply_rate_share(handle, &server, &options, &tx).await;
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
    let (mut resources, missing) = select_resources(&server, &metadata, &options.resources);
    let paused: Vec<String> = resources
        .iter()
        .filter(|r| server.is_paused(r))
        .cloned()
        .collect();
    resources.retain(|r| !server.is_paused(r));
    for r in &paused {
        send(
            &tx,
            format!("{name}/{r}: paused in the configuration, skipped"),
        );
    }
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
    if resources.is_empty() && entries.is_empty() && !paused.is_empty() {
        return Ok(ServerOutcome {
            entries,
            media: None,
            schema_changes,
            rate_usage: handle.rate_usage(),
        });
    }
    if resources.is_empty() && entries.is_empty() {
        return Err("no resources to pull (check include/exclude in the configuration)".into());
    }

    // Media files are downloaded by their own workers next to the record sync (#89)
    let (mut page_media, media_workers) = match &options.media {
        Some(root) => {
            let pipeline = PipelineOptions {
                state: Arc::clone(&options.state),
                store: root.store(&name).map_err(|e| e.to_string())?,
                nested_field: server.media.nested_field.clone(),
                workers: server.media.workers.unwrap_or(DEFAULT_WORKERS),
                shutdown: options.shutdown.clone(),
            };
            let (pages, workers) = page_pipeline(
                &media_client,
                &name,
                server.media.download_config(),
                pipeline,
            )
            .await
            .map_err(|e| e.to_string())?;
            (Some(pages), Some(workers))
        }
        None => (None, None),
    };
    let records = async {
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

            if i > 0 {
                backfills = apply_rate_share(handle, &server, &options, &tx).await;
            }
            // A feed with no checkpoint is left to the backfill loading it
            let store_main = store.clone();
            if options.backfill.is_none()
                && backfills
                    .iter()
                    .any(|b| backfill::is_marker(b, Some(resource)))
                && matches!(store_main.load(&name, resource).await, Ok(None))
            {
                let message = "skipped: a backfill is loading this feed".to_string();
                span.in_scope(|| tracing::warn!("feed skipped: a backfill is loading it"));
                send(&tx, format!("[{label}] {message}"));
                let mut entry = failed_entry(&name, resource, "");
                entry.error = None;
                entry.warnings.push(message);
                entries.push(entry);
                continue;
            }

            // One writer per feed: a second process syncing this server and resource skips it. A
            // backfill also holds its marker; a window holds only its marker (its checkpoint is its own).
            let lease_names: Vec<String> = match &options.backfill {
                None => vec![resource.clone()],
                Some(w) if w.is_whole() => vec![resource.clone(), w.marker(resource)],
                Some(w) => vec![w.marker(resource)],
            };
            let leases = match take_leases(&options.state, &name, &lease_names).await {
                Ok(Ok(leases)) => leases,
                Ok(Err(holder)) => {
                    let message =
                        format!("skipped: another process is syncing this feed ({holder})");
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
                    release(leases).await;
                    continue;
                }
            };
            let started = std::time::Instant::now();
            // Count this resource's own requests: feeds of one server share a client in `serve` (#109)
            let ((mut entry, window_start), requests) = count_requests(async {
                client = handle.client();
                let filter = join_filters(
                    server.filter_for(resource),
                    options
                        .backfill
                        .as_ref()
                        .and_then(|w| w.filter(TIMESTAMP_FIELD)),
                );
                // A backfill window keeps its own checkpoint
                let store = match &options.backfill {
                    Some(w) => store_main.scoped(&w.checkpoint_suffix()),
                    None => store_main.clone(),
                };
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
                let window_start = match &saved {
                    Ok(Some(c)) if !full_load => Some(c.timestamp.clone()),
                    _ => None,
                };
                let backfill_entry = match &options.backfill {
                    Some(w) => {
                        let mut filters: Vec<String> = filter.iter().cloned().collect();
                        if let Ok(Some(cp)) = &saved {
                            if !options.start_over {
                                filters.push(backfill::after_checkpoint(
                                    cp,
                                    TIMESTAMP_FIELD,
                                    &plan.key,
                                ));
                            }
                        }
                        let remaining = backfill::count(&client, resource, &filters)
                            .instrument(span.clone())
                            .await;
                        let rate_percent = handle.rate_percent();
                        span.in_scope(|| {
                            tracing::info!(remaining, rate_percent, from = ?w.from, to = ?w.to, "backfill started")
                        });
                        send(
                            &tx,
                            format!(
                                "[{label}] backfill: {} records left, {rate_percent}% of the rate limits",
                                remaining.map_or("unknown".into(), |n| n.to_string())
                            ),
                        );
                        Some(BackfillEntry {
                            from: w.from.clone(),
                            to: w.to.clone(),
                            remaining_at_start: remaining,
                            rate_percent,
                        })
                    }
                    None => None,
                };
                let mut progress;
                let inner: &mut dyn RecordWriter = match &backfill_entry {
                    Some(b) => {
                        progress = ProgressWriter::new(
                            writer.as_mut(),
                            tx.clone(),
                            format!("##progress step={}/{total} label={label}", i + 1),
                            b.remaining_at_start,
                        );
                        &mut progress
                    }
                    None => writer.as_mut(),
                };
                let mut checked = CheckingWriter::new(inner, &plan.key);
                let mut pull = Pull {
                    handle,
                    client: &mut client,
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
                entry.backfill = backfill_entry;
                if let Some(w) = options.backfill.as_ref().filter(|w| !w.is_whole()) {
                    if w.to.is_none() && entry.error.is_none() && !entry.interrupted {
                        hand_over_checkpoint(&store, &store_main, &name, resource, &span, &tx, &label)
                            .await;
                    }
                }
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
                    // Key-diff temporary files; the image has no /tmp, --out is writable
                    let scratch = options.out.join(".scratch");
                    let deleted = reconcile_deletes(
                        &client,
                        &name,
                        &DeleteTarget::new(resource, &plan.key)
                            .with_filter(filter.as_deref())
                            .with_limits(server.deletes.limits(options.allow_mass_deletes))
                            .with_scratch_dir(&scratch),
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
                (entry, window_start)
            })
            .await;
            entry.stats = ResourceStats::new(started.elapsed(), &requests);
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
            release(leases).await;
        }
        // Dropping the pages closes the queue: the workers finish what is queued and stop
        drop(page_media);
        entries
    };
    let (entries, media) = match media_workers {
        Some(workers) => {
            let (entries, report) = tokio::join!(records, workers.run());
            (entries, Some(report))
        }
        None => (records.await, None),
    };
    Ok(ServerOutcome {
        entries,
        media,
        schema_changes,
        rate_usage: handle.rate_usage(),
    })
}

/// Set the rate share of `handle` for what this sync is about to do, and return the marker leases
/// of the backfills of this server running now (empty for a backfill itself)
///
/// A backfill uses `[server.backfill] rate_percent` of the rate limits; any other sync uses the
/// rest while a backfill of the server runs, else all of them (#88).
async fn apply_rate_share(
    handle: &ClientHandle,
    server: &ServerConfig,
    options: &SyncOptions,
    tx: &UnboundedSender<String>,
) -> Vec<String> {
    let share = server.backfill.rate_percent();
    let (percent, running) = if options.backfill.is_some() {
        (share, Vec::new())
    } else {
        let running: Vec<String> = match live_leases(options.state.as_ref()).await {
            Ok(leases) => leases
                .into_iter()
                .filter(|(s, r)| *s == server.name && backfill::is_marker(r, None))
                .map(|(_, r)| r)
                .collect(),
            Err(e) => {
                tracing::warn!(server = %server.name, error = %e, "cannot list leases to look for a running backfill");
                Vec::new()
            }
        };
        let percent = if running.is_empty() { 100 } else { 100 - share };
        (percent, running)
    };
    if percent != handle.rate_percent() {
        let why = if options.backfill.is_some() {
            "backfill"
        } else if percent < 100 {
            "a backfill is running"
        } else {
            "no backfill running"
        };
        tracing::info!(server = %server.name, rate_percent = percent, "rate share: {why}");
        send(
            tx,
            format!(
                "[{}] using {percent}% of the rate limits ({why})",
                server.name
            ),
        );
        handle.set_rate_percent(percent);
    }
    running
}

/// Take the leases named `names` on `server`, all or none: `Ok(Err(holder))` when another process
/// holds one
async fn take_leases(
    state: &Arc<dyn StateStore>,
    server: &str,
    names: &[String],
) -> Result<Result<Vec<Lease>, LeaseHolder>, IngestError> {
    let mut taken = Vec::new();
    for name in names {
        match Lease::acquire(state, server, name, LEASE_TTL).await {
            Ok(LeaseOutcome::Acquired(lease)) => taken.push(lease),
            Ok(LeaseOutcome::Held(holder)) => {
                release(taken).await;
                return Ok(Err(holder));
            }
            Err(e) => {
                release(taken).await;
                return Err(e);
            }
        }
    }
    Ok(Ok(taken))
}

async fn release(leases: Vec<Lease>) {
    for lease in leases {
        lease.release().await;
    }
}

/// `a and b` when both are set, each in parentheses
fn join_filters(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("({a}) and ({b})")),
        (a, b) => a.or(b),
    }
}

/// A backfill window that runs to the present has finished: when the feed has no checkpoint yet,
/// the window's becomes the feed's, so incremental syncs continue from it
async fn hand_over_checkpoint(
    window: &CheckpointStore,
    feed: &CheckpointStore,
    server: &str,
    resource: &str,
    span: &tracing::Span,
    tx: &UnboundedSender<String>,
    label: &str,
) {
    let result = async {
        if feed.load(server, resource).await?.is_some() {
            return Ok(false);
        }
        match window.load(server, resource).await? {
            Some(cp) => feed.save(server, resource, &cp).await.map(|()| true),
            None => Ok(false),
        }
    }
    .await;
    match result {
        Ok(true) => {
            span.in_scope(|| {
                tracing::info!("backfill window finished: the feed continues from its checkpoint")
            });
            send(
                tx,
                format!(
                    "[{label}] backfill finished: incremental syncs continue from its checkpoint"
                ),
            );
        }
        Ok(false) => {}
        Err(e) => {
            span.in_scope(
                || tracing::warn!(error = %e, "cannot hand the backfill checkpoint to the feed"),
            );
            send(
                tx,
                format!("[{label}] cannot hand the backfill checkpoint to the feed: {e}"),
            );
        }
    }
}

/// One resource's records pull, tried once more with a new client when it failed to authenticate
/// and the server's secrets have changed since (#92)
struct Pull<'a> {
    handle: &'a ClientHandle,
    client: &'a mut Arc<ResoClient>,
    server: &'a str,
    resource: &'a str,
    spec: StandardReplication,
    store: &'a CheckpointStore,
    start_over: bool,
    media: Option<&'a mut crate::media::PageMedia>,
    shutdown: &'a Shutdown,
    span: &'a tracing::Span,
}

impl Pull<'_> {
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
            .with_expanded(expanded)
            .partitioned(options.partition);
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
        backfill: None,
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
                "error: `media` reads the data files in --out, but with a bucket they are in the \
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
                let workers = server.media.workers.unwrap_or(DEFAULT_WORKERS);
                let report =
                    sync_media(&client, &server.name, download, workers, &options, progress).await;
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
    if r.unchanged > 0 {
        line.push_str(&format!(", {} unchanged", r.unchanged));
    }
    if r.shared > 0 {
        line.push_str(&format!(", {} same as a stored file", r.shared));
    }
    if r.expired > 0 {
        line.push_str(&format!(", {} expired URLs", r.expired));
    }
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

/// What `discover --starter` found about one resource
struct Probe {
    name: String,
    key: Vec<String>,
    /// The resource has a `ModificationTimestamp`, so it can be synced incrementally
    incremental: bool,
    /// `Ok(record count, when the server gave one)` or why the read failed
    read: Result<Option<u64>, String>,
}

/// `discover --starter`: read $metadata and one record of each resource with the configured
/// credentials, then print a starter `[[server]]` block per server
async fn onboard(args: Args, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
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
            let mut probes = Vec::new();
            for name in resources {
                let ty = metadata.resource(&name);
                let mut query = reso_client::QueryBuilder::new(&name).top(1).with_count();
                if let Some(filter) = server.filter_for(&name) {
                    query = query.filter(filter);
                }
                let read = match query.build() {
                    Ok(q) => client
                        .execute_page(&q)
                        .await
                        .map(|page| page.count)
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                probes.push(Probe {
                    key: ty.map(|t| t.key.clone()).unwrap_or_default(),
                    incremental: ty
                        .is_some_and(|t| t.properties.iter().any(|p| p.name == TIMESTAMP_FIELD)),
                    read,
                    name,
                });
            }
            Ok::<(ServerConfig, Vec<Probe>), String>((server, probes))
        }
    })
    .await;

    let mut failed = false;
    for report in reports {
        match report.result {
            Ok((server, probes)) => {
                failed |= probes.iter().all(|p| p.read.is_err());
                let _ = write!(out, "{}", starter_block(&server, &probes));
            }
            Err(message) => {
                failed = true;
                let _ = writeln!(err, "[{}] FAILED: {message}", report.name);
            }
        }
    }
    i32::from(failed)
}

/// The starter TOML for one server: the readable, incremental resources in `include`, the others
/// in comments with the reason
fn starter_block(server: &ServerConfig, probes: &[Probe]) -> String {
    let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
    let readable = probes.iter().filter(|p| p.read.is_ok()).count();
    let mut text = format!(
        "# {}: $metadata read; {readable} of {} resources readable with these credentials\n",
        server.name,
        probes.len()
    );
    text.push_str(
        "# Servers do not publish their rate limits in a standard way: copy the vendor's numbers\n\
         # into [server.limits]. Keep the [server.auth] table of your draft (secrets are not printed).\n",
    );
    text.push_str(&format!(
        "[[server]]\nname = {}\nbase_url = {}\n",
        quote(&server.name),
        quote(&server.base_url)
    ));
    if let Some(id) = &server.dataset_id {
        text.push_str(&format!("dataset_id = {}\n", quote(id)));
    }
    if let Some(filter) = &server.filter {
        text.push_str(&format!("filter = {}\n", quote(filter)));
    }
    let include: Vec<String> = probes
        .iter()
        .filter(|p| p.read.is_ok() && p.incremental)
        .map(|p| quote(&p.name))
        .collect();
    if include.is_empty() {
        // An empty include means every resource, so leave it out with the reason
        text.push_str("# include = []: no resource is both readable and incremental\n");
    } else {
        text.push_str(&format!("include = [{}]\n", include.join(", ")));
    }
    for p in probes {
        let key = if p.key.is_empty() {
            "no key".to_string()
        } else {
            format!("key {}", p.key.join(", "))
        };
        let note = match &p.read {
            Err(e) => format!("left out, not readable: {e}"),
            Ok(_) if !p.incremental => {
                format!("left out, no {TIMESTAMP_FIELD} to sync from")
            }
            Ok(Some(n)) => format!("{n} records"),
            Ok(None) => "record count not given".to_string(),
        };
        text.push_str(&format!("# {}: {key}; {note}\n", p.name));
    }
    text.push_str("\n[server.limits]\n# per_second = 2\n# per_hour = 5000\n# per_day = 40000\n\n");
    text
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

/// `purge`: a dry run, or with `by` set, the purge itself
async fn purge(args: Args, by: Option<String>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(server) = args.servers.first().cloned() else {
        return 2;
    };
    let out_dir = args.out();
    // With a bucket, state, data and media files are there; the audit log stays local
    let (state, sink, media) = match (
        args.state_store(),
        args.file_sink(),
        args.media_root().store(&server),
    ) {
        (Ok(state), Ok(sink), Ok(media)) => (state, sink, media),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            let _ = writeln!(err, "error: {e}");
            return 2;
        }
    };
    let files = sink.as_deref();
    let Some(by) = by else {
        let plan = match crate::purge::plan_purge(
            &server,
            &out_dir,
            files,
            media.as_ref(),
            state.as_ref(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                return 1;
            }
        };
        let _ = writeln!(
            out,
            "dry run: nothing deleted; add --execute --by NAME to delete\npurge plan for server {server}"
        );
        print_plan(out, &plan);
        return 0;
    };
    match crate::purge::execute_purge(&server, &out_dir, files, media.as_ref(), &state, &by).await {
        Ok(crate::purge::PurgeOutcome::Held(held)) => {
            for (key, holder) in held {
                let _ = writeln!(err, "error: {} is {holder}", state.location(&key));
            }
            let _ = writeln!(
                err,
                "nothing deleted: a sync of {server} is running; try again after it ends"
            );
            1
        }
        Ok(crate::purge::PurgeOutcome::Done {
            record,
            path,
            sha256,
            delivered,
            delivery_error,
        }) => {
            let _ = writeln!(
                out,
                "{} server {server}: {} data files ({} records), {} media files, {} state keys\nrecord {}\nsha256 {sha256}",
                record.status,
                record.files.len(),
                record.records,
                record.media_files,
                record.state_keys.len(),
                path.display()
            );
            for at in &delivered {
                let _ = writeln!(out, "copy {at}");
            }
            for name in &record.not_purged {
                let _ = writeln!(
                    out,
                    "not purged (no source tag or manifest names it): {name}"
                );
            }
            if let Some(e) = &delivery_error {
                let _ = writeln!(
                    err,
                    "error: copying the purge record to the bucket: {e} (the local record and access line are written)"
                );
            }
            if let Some(e) = &record.error {
                let _ = writeln!(err, "error: {e}");
                return 1;
            }
            i32::from(delivery_error.is_some())
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            1
        }
    }
}

fn print_plan(out: &mut dyn Write, plan: &crate::purge::PurgePlan) {
    let _ = writeln!(
        out,
        "data files: {} ({} records, {} bytes)",
        plan.files.len(),
        plan.records(),
        plan.file_bytes()
    );
    for f in &plan.files {
        let _ = writeln!(
            out,
            "  {} ({} records, {} bytes)",
            f.name, f.records, f.bytes
        );
    }
    let _ = writeln!(
        out,
        "media files: {} ({} bytes) in {}",
        plan.media_files, plan.media_bytes, plan.media
    );
    let _ = writeln!(out, "state keys: {}", plan.state_keys.len());
    for k in &plan.state_keys {
        let _ = writeln!(out, "  {k}");
    }
    for name in &plan.not_purged {
        let _ = writeln!(
            out,
            "not purged (no source tag or manifest names it): {name}"
        );
    }
    for (key, holder) in &plan.held {
        let _ = writeln!(out, "lease {key} is {holder}: --execute would refuse");
    }
    let _ = writeln!(
        out,
        "kept: the access log, run manifests and media manifests"
    );
}

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

// ---------------------------------------------------------------- reset, replay, history

/// `reset` (`after` is `None`) or `replay`: clear or move back one feed's checkpoint under its lease
async fn move_checkpoint(
    args: Args,
    server: &str,
    resource: &str,
    after: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if let Some(after) = after {
        if crate::metrics::parse_rfc3339(after).is_none() {
            let _ = writeln!(
                err,
                "error: --after `{after}` is not an RFC 3339 time (e.g. 2026-11-01T00:00:00Z)"
            );
            return 2;
        }
    }
    let state = match args.writable_state_store().await {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    let lease = match Lease::acquire(&state, server, resource, LEASE_TTL).await {
        Ok(LeaseOutcome::Acquired(lease)) => lease,
        Ok(LeaseOutcome::Held(holder)) => {
            let _ = writeln!(
                err,
                "error: {server}/{resource} is being pulled ({holder}); try again when it is done"
            );
            return 1;
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let store = CheckpointStore::with_store(Arc::clone(&state));
    let code = match change_checkpoint(&store, server, resource, after).await {
        Ok((old, message)) => {
            let old = old.map_or("none".to_string(), |c| describe_checkpoint(&c));
            let _ = writeln!(out, "{server}/{resource}: checkpoint was {old}");
            let _ = writeln!(out, "{server}/{resource}: {message}");
            0
        }
        Err((code, message)) => {
            let _ = writeln!(err, "error: {server}/{resource}: {message}");
            code
        }
    };
    lease.release().await;
    code
}

/// The change itself: `Ok((old checkpoint, what was done))` or `Err((exit code, message))`
async fn change_checkpoint(
    store: &CheckpointStore,
    server: &str,
    resource: &str,
    after: Option<&str>,
) -> Result<(Option<reso_client::Checkpoint>, String), (i32, String)> {
    let old = store
        .load(server, resource)
        .await
        .map_err(|e| (1, e.to_string()))?;
    let Some(after) = after else {
        if old.is_none() {
            return Err((1, "no checkpoint to reset".to_string()));
        }
        store
            .clear(server, resource)
            .await
            .map_err(|e| (1, e.to_string()))?;
        return Ok((
            old,
            "checkpoint removed; the next sync is a full load".to_string(),
        ));
    };
    if let Some(current) = &old {
        let later = crate::metrics::parse_rfc3339(&current.timestamp)
            .zip(crate::metrics::parse_rfc3339(after))
            .is_some_and(|(current, after)| after > current);
        if later {
            return Err((
                1,
                format!(
                    "--after {after} is later than the checkpoint {}; replay only moves back",
                    current.timestamp
                ),
            ));
        }
    }
    let checkpoint = reso_client::Checkpoint::at(after);
    store
        .save(server, resource, &checkpoint)
        .await
        .map_err(|e| (1, e.to_string()))?;
    Ok((
        old,
        format!("checkpoint set to {after}; the next sync pulls records modified after it"),
    ))
}

fn describe_checkpoint(c: &reso_client::Checkpoint) -> String {
    match &c.key {
        Some(key) => format!("{} key {key}", c.timestamp),
        None => c.timestamp.clone(),
    }
}

/// `history`: past runs from the manifests, newest first
fn history(
    args: Args,
    server: Option<&str>,
    resource: Option<&str>,
    limit: usize,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let sink = match args.file_sink() {
        Ok(v) => v,
        Err(e) => return usage_error(err, &e),
    };
    let manifests = match read_manifests(&args.out(), sink.as_deref()) {
        Ok(v) => v,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return 1;
        }
    };
    let mut shown = 0;
    for (name, manifest) in manifests.iter().rev() {
        if shown == limit {
            break;
        }
        let lines = history_lines(manifest, server, resource);
        if lines.is_empty() {
            continue;
        }
        shown += 1;
        let run = manifest
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or(name);
        let started = manifest
            .get("started_at")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let finished = match manifest.get("finished_at").and_then(Value::as_str) {
            Some(f) => format!("finished {f}"),
            None => "not finished".to_string(),
        };
        let _ = writeln!(out, "{run}: started {started}, {finished}");
        for line in lines {
            let _ = writeln!(out, "  {line}");
        }
    }
    if shown == 0 {
        let _ = writeln!(out, "no runs found");
    }
    0
}

/// One line per feed (and per failed server) of `manifest` that matches the filters
fn history_lines(manifest: &Value, server: Option<&str>, resource: Option<&str>) -> Vec<String> {
    let text = |v: &Value, field: &str| {
        v.get(field)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    let mut lines = Vec::new();
    for r in manifest
        .get("resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (s, res) = (text(r, "server"), text(r, "resource"));
        if server.is_some_and(|f| f != s) || resource.is_some_and(|f| f != res) {
            continue;
        }
        let outcome = match r.get("error").and_then(Value::as_str) {
            Some(e) => format!("FAILED: {e}"),
            None if r.get("interrupted").and_then(Value::as_bool) == Some(true) => {
                "interrupted".to_string()
            }
            None => "ok".to_string(),
        };
        let count = |field: &str| r.get(field).and_then(Value::as_u64).unwrap_or(0);
        let backfill = if r.get("backfill").is_some_and(|b| !b.is_null()) {
            " (backfill)"
        } else {
            ""
        };
        lines.push(format!(
            "{s}/{res}{backfill}: {} records, {} deletes, {outcome}",
            count("records"),
            count("deletes")
        ));
    }
    if resource.is_none() {
        for f in manifest
            .get("server_failures")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let s = text(f, "server");
            if server.is_some_and(|filter| filter != s) {
                continue;
            }
            lines.push(format!("{s}: FAILED: {}", text(f, "error")));
        }
    }
    lines
}

/// Every run manifest, oldest first (run ids sort by time): from the sink when there is one, else
/// from `dir`. Each is `(file name, parsed JSON)`
fn read_manifests(
    dir: &Path,
    sink: Option<&dyn crate::sink::FileSink>,
) -> Result<Vec<(String, Value)>, String> {
    let is_manifest = |n: &str| manifest_run_id(n).is_some();
    let mut found = Vec::new();
    if let Some(sink) = sink {
        let names: Vec<String> = sink
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(name, _)| name)
            .filter(|n| is_manifest(n))
            .collect();
        let scratch =
            std::env::temp_dir().join(format!("reso-ingest-history-{}", std::process::id()));
        for name in names {
            let read = sink
                .get_file(&name, &scratch)
                .map_err(|e| e.to_string())
                .and_then(|got| {
                    if got {
                        std::fs::read_to_string(&scratch)
                            .map(Some)
                            .map_err(|e| format!("cannot read {name}: {}", e.kind()))
                    } else {
                        Ok(None)
                    }
                });
            let _ = std::fs::remove_file(&scratch);
            if let Some(text) = read? {
                found.push((name, text));
            }
        }
    } else {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read {}: {}", dir.display(), e.kind())),
        };
        for path in entries.filter_map(Result::ok).map(|e| e.path()) {
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if !is_manifest(&name) {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?;
            found.push((name, text));
        }
    }
    // By run id, not file name: `manifest.X-02.json` sorts before `manifest.X.json`
    found.sort_by(|a, b| manifest_run_id(&a.0).cmp(&manifest_run_id(&b.0)));
    found
        .into_iter()
        .map(|(name, text)| {
            serde_json::from_str(&text)
                .map(|value| (name.clone(), value))
                .map_err(|e| format!("{name} is not valid JSON: {e}"))
        })
        .collect()
}

/// The run id of a manifest file name `manifest.<run id>.json`; `None` for other files
fn manifest_run_id(name: &str) -> Option<&str> {
    name.strip_prefix("manifest.")?.strip_suffix(".json")
}

/// The newest `manifest.*.json` in `dir` (run ids sort by time), parsed
fn last_manifest(dir: &Path) -> Result<Option<(String, Value)>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {}", dir.display(), e.kind())),
    };
    // By run id, not file name: `manifest.X-02.json` sorts before `manifest.X.json`
    let newest = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter_map(|p| {
            let id = p
                .file_name()?
                .to_str()
                .and_then(manifest_run_id)?
                .to_string();
            Some((id, p))
        })
        .max();
    let Some((_, path)) = newest else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?;
    let value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    Ok(Some((path.display().to_string(), value)))
}
