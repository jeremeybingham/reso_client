//! Ingest layer over [`reso_client`]
//!
//! This crate is the library behind the `reso-ingest` command: it pulls every resource from many
//! RESO Web API servers into tagged NDJSON or Parquet files, keeps them current with incremental
//! runs, records deletes and downloads the files `Media` records point at. Records are written
//! raw, inside source tags. The command is documented in the crate's
//! [README](https://github.com/jeremeybingham/reso-rs/blob/main/reso-ingest/README.md).
//!
//! **Status.** Tested against mock servers and RESO's reference server, not yet against a vendor
//! server, so options, configuration and output may change in 0.5.0. The library API exists for
//! the binary: before 1.0 it may change in any minor release without a deprecation (see
//! `docs/STABILITY.md` in the repository).
//!
//! # Configuration and running
//!
//! [`Config`] reads one TOML file, or a directory of them ([`Config::from_path`]), describing many
//! servers, and
//! [`run_servers`] runs a task for each of them independently: one server failing does not stop
//! the others, and the number of servers running at once is capped. Secrets are never stored in
//! the file: authentication settings name the environment variables or files that hold them
//! ([`SecretSource`]), and a file that contains a literal secret is rejected. A [`ClientHandle`]
//! reads them again before each resource and after an authentication failure, so a rotated secret
//! is used without a restart. A server can carry a `filter` added to every query, and
//! [`ResourceConfig`] adds a filter, an `$expand` list and a page size for one resource;
//! [`NetworkConfig`] sets the proxy, CA certificates and User-Agent.
//!
//! # Output
//!
//! Output goes through [`RecordWriter`]: [`NdjsonWriter`] writes one tagged line per record,
//! rotating files by size, and with the `parquet` feature `ParquetWriter` writes the same rows as
//! typed Parquet, optionally in Hive-style partition folders. [`RunManifest`] records counts,
//! files, checkpoints and errors for the run. [`pull_resource`] streams one resource into a
//! writer; [`sync_resource`] does it incrementally, saving a checkpoint ([`CheckpointStore`])
//! after each batch is delivered, so the next run, or a rerun after a crash, continues from there
//! (at-least-once). [`reserve_run_id`] gives each run an id no other run has, and writers never
//! overwrite an existing file.
//!
//! # State and storage
//!
//! Checkpoints, key sets, leases and the media index go through a [`StateStore`] (an async
//! key-value trait with conditional writes); [`FileStateStore`] keeps them in a directory.
//! Finished data files go to a [`FileSink`] ([`LocalDirSink`]). With the `s3` feature,
//! `S3StateStore`, `S3FileSink` and `S3MediaStore` keep state, data and media files in an
//! S3-compatible bucket or in Google Cloud Storage. A [`Lease`] per server and resource keeps two
//! processes sharing the state from syncing the same feed at once.
//!
//! # Deletes
//!
//! [`reconcile_deletes`] finds records that left a server's feed, from its event resource
//! ([`EventFeed`]) when it has one, otherwise by comparing sorted key sets (memory stays flat at
//! millions of keys), and writes them as delete events; [`DeleteLimits`] holds back a mass
//! delete. For servers that mark removed records with a flag field instead ([`DeletesConfig`]),
//! [`FlagDeleteWriter`] adds a delete event after each flagged record during the sync.
//!
//! # Media files
//!
//! [`sync_media`] downloads the files that synced `Media` records point at (NDJSON output only).
//! [`page_pipeline`] instead queues each page's files ([`PageMedia`]) for a pool of download
//! workers ([`MediaWorkers`]) while the sync goes on, including `Media` arrays nested in other
//! records, for servers whose media URLs are single-use or expire quickly. A [`MediaIndex`] per
//! server means later runs fetch only new and changed files; files are stored once, named by the
//! SHA-256 of their content, in a [`MediaStore`] ([`LocalMediaStore`] or, with `s3`, a bucket).
//! Downloads use their own limits ([`MediaConfig`]).
//!
//! # Running as a service
//!
//! [`Daemon`] is `reso-ingest serve`: each resource syncs on its own interval, with health,
//! readiness, Prometheus metrics and status endpoints, and [`Daemon::reload`] applies an edited
//! configuration. [`Shutdown`] stops a sync cleanly between pages. [`BackfillWindow`] covers the
//! first load of a large feed on its own share of the rate limits.
//!
//! # Checks, metrics and logs
//!
//! [`SchemaSnapshot`] reports `$metadata` changes between runs and [`CheckingWriter`] repeated
//! keys; [`post_summary`] sends the run summary to a webhook. [`render_prometheus`] turns a run's
//! manifest into Prometheus text (freshness lag, records, requests, responses, duration,
//! success); `sync --metrics-file` writes it for the node_exporter textfile collector. The crates
//! log through [`tracing`]; [`log_subscriber`] builds the text or JSON subscriber the binary
//! installs, and the `run`, server and resource spans put `run_id`, `server` and `resource` on
//! every event.
//!
//! # Licence terms
//!
//! [`AccessLog`] appends one line per resource pull (what, when, which credential) to an
//! append-only file, and [`plan_purge`] and [`execute_purge`] delete one server's data files,
//! media and state when its licence ends, leaving a [`PurgeRecord`] with its SHA-256.
//!
//! # Features
//!
//! - `parquet`: Parquet output (`ParquetWriter`, `sync --format parquet`).
//! - `s3`: state, data and media files in an S3-compatible bucket or GCS (`object_store`).
//! - `mimalloc` (default): mimalloc as the binary's global allocator.
//! - `ui` (default): `serve --ui` status pages; adds no dependency.
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
pub use serve::{Daemon, ReloadSummary, ServeOptions};
pub use shutdown::Shutdown;
pub use sink::{FileSink, LocalDirSink};
pub use state::{FileStateStore, StateStore, StoreFuture, Versioned, Written};
pub use webhook::{post_summary, run_summary};
