//! Ingest layer over [`reso_client`]
//!
//! This crate describes many RESO servers in one TOML file ([`Config`]) and runs a task for each of
//! them independently ([`run_servers`]): one server failing does not stop the others, and the number
//! of servers running at once is capped.
//!
//! Secrets are never stored in the file. Authentication settings name the environment variables that
//! hold them, and [`ServerConfig::client_config`] reads those variables when a server is about to run.
//! A file that contains a literal secret is rejected. A server can carry a `filter` added to every
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

mod checkpoint;
pub mod cli;
mod config;
mod deletes;
mod error;
mod logging;
mod manifest;
mod media;
mod output;
#[cfg(feature = "parquet")]
mod parquet_out;
mod runner;

pub use checkpoint::{CheckpointInfo, CheckpointStore};
pub use config::{
    AuthConfig, Config, DeletesConfig, LimitsConfig, MediaConfig, ResourceConfig, ServerConfig,
};
pub use deletes::{reconcile_deletes, DeleteTarget, EventFeed, FlagDeleteWriter};
pub use error::IngestError;
pub use logging::{env_filter, log_subscriber, LogFormat, DEFAULT_FILTER, LOG_FORMAT_ENV};
pub use manifest::{
    pull_resource, sync_resource, sync_resource_media, FileEntry, ResourceEntry, RunManifest,
    ServerFailure,
};
pub use media::{
    encode_name, sync_media, MediaEntry, MediaFailure, MediaIndex, MediaManifest, MediaOptions,
    MediaReport, PageMedia, DEFAULT_NESTED_FIELD, MEDIA_RESOURCE,
};
pub use output::{new_run_id, now_rfc3339, reserve_run_id, NdjsonWriter, RecordWriter};
#[cfg(feature = "parquet")]
pub use parquet_out::{arrow_type, schema_for, ParquetWriter};
pub use runner::{run_servers, ServerReport};
