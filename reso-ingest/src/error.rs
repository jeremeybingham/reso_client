//! Errors for the ingest layer

use thiserror::Error;

/// Errors from reading or resolving a server configuration
///
/// Messages name the server (and key) at fault.
#[derive(Debug, Error)]
pub enum IngestError {
    /// The configuration file could not be read
    #[error("cannot read config file {path}: {message}")]
    Read {
        /// File path
        path: String,
        /// Underlying I/O error text
        message: String,
    },

    /// The file is not valid TOML, or a top-level key is wrong
    #[error("config: {0}")]
    Parse(String),

    /// A server entry is invalid (unknown key, missing field, bad value, literal secret)
    #[error("server `{server}`: {message}")]
    Server {
        /// The server's `name` (or `#N` when it has none)
        server: String,
        /// What is wrong
        message: String,
    },

    /// An environment variable named in the config is not set
    #[error("server `{server}`: environment variable `{variable}` is not set")]
    MissingEnv {
        /// The server's name
        server: String,
        /// The variable that was looked up
        variable: String,
    },

    /// An output file or the manifest could not be written
    #[error("output {path}: {message}")]
    Output {
        /// File or directory involved
        path: String,
        /// Underlying I/O or serialization error text
        message: String,
    },
}
