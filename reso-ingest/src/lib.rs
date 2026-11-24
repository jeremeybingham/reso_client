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
//! files by size, and [`RunManifest`] records counts, files, checkpoints and errors for the run.
//! [`pull_resource`] streams one resource into a writer.
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

mod config;
mod error;
mod manifest;
mod output;
mod runner;

pub use config::{AuthConfig, Config, LimitsConfig, ServerConfig};
pub use error::IngestError;
pub use manifest::{pull_resource, FileEntry, ResourceEntry, RunManifest, ServerFailure};
pub use output::{new_run_id, now_rfc3339, NdjsonWriter, RecordWriter};
pub use runner::{run_servers, ServerReport};
