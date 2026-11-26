//! Ingest layer over [`reso_client`]
//!
//! This crate describes many RESO servers in one TOML file ([`Config`]) and runs a task for each of
//! them independently ([`run_servers`]): one server failing does not stop the others, and the number
//! of servers running at once is capped.
//!
//! Secrets are never stored in the file. Authentication settings name the environment variables or
//! files that hold them ([`SecretSource`]), and [`ServerConfig::client_config`] reads them when a
//! server is about to run. A [`ClientHandle`] reads them again before each resource and after an
//! authentication failure, so a rotated secret is used without a restart. A file that contains a
//! literal secret is rejected. A server can carry a `filter` added to every
//! query, and [`ResourceConfig`] adds a filter, an `$expand` list and a page size for one resource.
//!
//! Output goes through [`RecordWriter`]; [`NdjsonWriter`] writes one tagged line per record, rotating
//! files by size (with the `parquet` feature, `ParquetWriter` writes the same rows as typed
//! Parquet), and [`RunManifest`] records counts, files, checkpoints and errors for the run.
//! [`pull_resource`] streams one resource into a writer; [`sync_resource`] does it incrementally, saving
//! a JSON checkpoint ([`CheckpointStore`]) after each batch is written so the next run, or a rerun after
//! a crash, continues from there. [`reserve_run_id`] gives each run an id no other run in the same
//! output directory has, and writers never overwrite an existing file.
//! [`reconcile_deletes`] finds records that left a server's feed, from its event resource ([`EventFeed`])
//! when it has one, otherwise by comparing key sets, and writes them as delete events. For servers
//! that mark removed records with a flag field instead ([`DeletesConfig`]), [`FlagDeleteWriter`]
//! adds a delete event after each flagged record during the sync.
//! [`sync_media`] downloads the files that synced `Media` records point at (NDJSON output only),
//! keeping a [`MediaIndex`] per server so later runs fetch only new and changed files, and writes a
//! [`MediaManifest`] for the run. [`sync_resource_media`] instead downloads each page's files right
//! after the page is written ([`PageMedia`]), including `Media` arrays nested in other records, for
//! servers whose media URLs are single-use or expire quickly. Downloads use their own limits
//! ([`MediaConfig`]).
//!
//! [`render_prometheus`] turns a run's manifest into Prometheus text: per server and resource, the
//! freshness lag (now minus the newest `ModificationTimestamp` written), records, request and
//! response counts, duration and success; `sync --metrics-file` writes it for the node_exporter
//! textfile collector.
//!
//! The crates log through [`tracing`]; [`log_subscriber`] builds the text or JSON subscriber the
//! binary installs, and the `run`, server and resource spans put `run_id`, `server` and `resource` on
//! every event.
//!
//! # Examples
//!
//! ```
//! use reso_ingest::Config;
//!
//! let config = Config::parse(r#"
//! [[server]]
//! name = "demo"
//! base_url = "https://api.mls.example/odata"
//!
//! [server.auth]
//! type = "bearer"
//! token_env = "DEMO_TOKEN"
//! "#)?;
//! assert_eq!(config.servers[0].name, "demo");
//! # Ok::<(), reso_ingest::IngestError>(())
//! ```

mod audit;
mod backfill;
mod checkpoint;
mod checks;
pub mod cli;
mod config;
mod deletes;
mod error;
mod keyset;
mod lease;
mod logging;
mod manifest;
mod media;
mod media_store;
mod metrics;
mod output;
#[cfg(feature = "parquet")]
mod parquet_out;
mod purge;
mod runner;
mod runs;
#[cfg(feature = "s3")]
mod s3;
mod secrets;
mod serve;
mod shutdown;
mod sink;
mod state;
mod webhook;

pub use audit::{credential_id, AccessEntry, AccessLog, PurgeNote, ACCESS_LOG};
pub use backfill::{BackfillEntry, BackfillWindow};
pub use checkpoint::{CheckpointInfo, CheckpointStore};
pub use checks::{count_warning, CheckingWriter, SchemaSnapshot};
pub use config::{
    parse_interval, AuthConfig, BackfillConfig, Config, DeletesConfig, LimitsConfig, MediaConfig,
    NetworkConfig, ResourceConfig, SecretSource, ServerConfig, DEFAULT_USER_AGENT,
};
pub use deletes::{reconcile_deletes, DeleteLimits, DeleteTarget, EventFeed, FlagDeleteWriter};
pub use error::IngestError;
pub use lease::{Lease, LeaseHolder, LeaseOutcome};
pub use logging::{env_filter, log_subscriber, LogFormat, DEFAULT_FILTER, LOG_FORMAT_ENV};
pub use manifest::{
    pull_resource, sync_resource, sync_resource_media, FileEntry, ResourceEntry, ResourceStats,
    RunManifest, ServerFailure,
};
pub use media::{
    encode_name, page_pipeline, sync_media, MediaEntry, MediaFailure, MediaIndex, MediaManifest,
    MediaOptions, MediaReport, MediaWorkers, PageMedia, PipelineOptions, DEFAULT_NESTED_FIELD,
    DEFAULT_WORKERS, MEDIA_RESOURCE,
};
pub use media_store::{LocalMediaStore, MediaRoot, MediaStore, MediaUpload};
pub use metrics::{
    parse_rfc3339, render_media_prometheus, render_prometheus, render_prometheus_at,
    write_metrics_file,
};
pub use output::{
    hive_value, new_run_id, now_rfc3339, partition_prefix, reserve_run_id, NdjsonWriter,
    RecordWriter,
};
#[cfg(feature = "parquet")]
pub use parquet_out::{arrow_type, schema_for, ParquetWriter};
pub use purge::{
    execute_purge, plan_purge, sha256_hex, PurgeOutcome, PurgePlan, PurgeRecord, PurgedFile,
    AUDIT_DIR,
};
pub use runner::{run_servers, ServerReport};
#[cfg(feature = "s3")]
pub use s3::{Provider, S3FileSink, S3MediaStore, S3Options, S3StateStore};
pub use secrets::ClientHandle;
pub use serve::{Daemon, ServeOptions};
pub use shutdown::Shutdown;
pub use sink::{FileSink, LocalDirSink};
pub use state::{FileStateStore, StateStore, StoreFuture, Versioned};
pub use webhook::{post_summary, run_summary};
