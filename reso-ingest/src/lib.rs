//! Ingest layer over [`reso_client`]
//!
//! This crate describes many RESO servers in one TOML file ([`Config`]) and runs a task for each of
//! them independently ([`run_servers`]): one server failing does not stop the others, and the number
//! of servers running at once is capped.
//!
//! Secrets are never stored in the file. Authentication settings name the environment variables that
//! hold them, and [`ServerConfig::client_config`] reads those variables when a server is about to run.
//! A file that contains a literal secret is rejected.
//!
//! Output goes through [`RecordWriter`]; [`NdjsonWriter`] writes one tagged line per record, rotating
//! files by size (with the `parquet` feature, `ParquetWriter` writes the same rows as typed
//! Parquet), and [`RunManifest`] records counts, files, checkpoints and errors for the run.
//! [`pull_resource`] streams one resource into a writer; [`sync_resource`] does it incrementally, saving
//! a JSON checkpoint ([`CheckpointStore`]) after each batch is written so the next run, or a rerun after
//! a crash, continues from there.
//! [`reconcile_deletes`] finds records that left a server's feed, from its event resource ([`EventFeed`])
//! when it has one, otherwise by comparing key sets, and writes them as delete events.
//! [`sync_media`] downloads the files that synced `Media` records point at (NDJSON output only),
//! keeping a [`MediaIndex`] per server so later runs fetch only new and changed files, and writes a
//! [`MediaManifest`] for the run. Downloads use their own limits ([`MediaConfig`]).
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
mod manifest;
mod media;
mod output;
#[cfg(feature = "parquet")]
mod parquet_out;
mod runner;

pub use checkpoint::{CheckpointInfo, CheckpointStore};
pub use config::{AuthConfig, Config, LimitsConfig, MediaConfig, ResourceConfig, ServerConfig};
pub use deletes::{reconcile_deletes, DeleteTarget, EventFeed};
pub use error::IngestError;
pub use manifest::{
    pull_resource, sync_resource, FileEntry, ResourceEntry, RunManifest, ServerFailure,
};
pub use media::{
    encode_name, sync_media, MediaEntry, MediaFailure, MediaIndex, MediaManifest, MediaOptions,
    MediaReport, MEDIA_RESOURCE,
};
pub use output::{new_run_id, now_rfc3339, NdjsonWriter, RecordWriter};
#[cfg(feature = "parquet")]
pub use parquet_out::{arrow_type, schema_for, ParquetWriter};
pub use runner::{run_servers, ServerReport};
