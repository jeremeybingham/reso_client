//! The per-run manifest and a helper that streams one resource into a writer

use std::fs;
use std::path::{Path, PathBuf};

use reso_client::{Checkpoint, ResoClient, StandardReplication};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::IngestError;
use crate::output::{now_rfc3339, RecordWriter};

/// One output file in the manifest
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileEntry {
    /// File name (relative to the output directory)
    pub name: String,
    /// Lines in the file
    pub records: u64,
    /// Size in bytes
    pub bytes: u64,
}

/// What happened to one resource of one server
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceEntry {
    /// Server name
    pub server: String,
    /// Resource name
    pub resource: String,
    /// Records written
    pub records: u64,
    /// Files written
    pub files: Vec<FileEntry>,
    /// Position after the last record written, when the pull used replication
    pub checkpoint: Option<Value>,
    /// The error that stopped the pull, if any (records before it are kept)
    pub error: Option<String>,
}

/// A server that failed as a whole (for example a task error from `run_servers`)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerFailure {
    /// Server name
    pub server: String,
    /// Error text
    pub error: String,
}

/// The record of one run: counts, files, checkpoints and errors
///
/// Build it while the run goes, then call [`write`](Self::write), also when servers failed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunManifest {
    /// Run id, the same as in every output line
    pub run_id: String,
    /// When the run started (RFC 3339, UTC)
    pub started_at: String,
    /// Set by [`write`](Self::write)
    pub finished_at: Option<String>,
    /// One entry per resource pulled
    pub resources: Vec<ResourceEntry>,
    /// Servers that failed outright
    pub server_failures: Vec<ServerFailure>,
}

impl RunManifest {
    /// Start a manifest for `run_id`
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            started_at: now_rfc3339(),
            finished_at: None,
            resources: Vec::new(),
            server_failures: Vec::new(),
        }
    }

    /// Add a resource's outcome
    pub fn add_resource(&mut self, entry: ResourceEntry) {
        self.resources.push(entry);
    }

    /// Record that a whole server failed
    pub fn add_server_failure(&mut self, server: impl Into<String>, error: impl Into<String>) {
        self.server_failures.push(ServerFailure {
            server: server.into(),
            error: error.into(),
        });
    }

    /// True when no resource or server reported an error
    pub fn is_clean(&self) -> bool {
        self.server_failures.is_empty() && self.resources.iter().all(|r| r.error.is_none())
    }

    /// Write `manifest.{run_id}.json` into `dir` and return its path
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be written.
    pub fn write(&mut self, dir: impl AsRef<Path>) -> Result<PathBuf, IngestError> {
        let dir = dir.as_ref();
        self.finished_at = Some(now_rfc3339());
        let name: String = self
            .run_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("manifest.{name}.json"));
        let err = |e: &dyn std::fmt::Display| IngestError::Output {
            path: path.display().to_string(),
            message: e.to_string(),
        };
        fs::create_dir_all(dir).map_err(|e| err(&e))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| err(&e))?;
        fs::write(&path, text + "\n").map_err(|e| err(&e))?;
        Ok(path)
    }
}

/// Stream `resource` of `server` through RESO-standard replication into `writer`
///
/// Batches are written as they arrive, so memory use stays at one page. A fetch or write error
/// stops this resource and is recorded in the returned entry (it is not returned as an `Err`),
/// so the caller can add the entry to the manifest and go on with the next resource. The
/// checkpoint in the entry covers every batch successfully written.
pub async fn pull_resource(
    client: &ResoClient,
    server: &str,
    resource: &str,
    spec: StandardReplication,
    writer: &mut dyn RecordWriter,
) -> ResourceEntry {
    let mut run = client.replicate(spec);
    let mut records = 0u64;
    let mut checkpoint = checkpoint_json(run.checkpoint());
    let mut error = None;

    loop {
        match run.next_batch().await {
            Ok(Some(batch)) => {
                if let Err(e) = writer.write_batch(&batch, &now_rfc3339()) {
                    error = Some(e.to_string());
                    break;
                }
                records += batch.len() as u64;
                checkpoint = checkpoint_json(run.checkpoint());
            }
            Ok(None) => break,
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        }
    }

    let files = match writer.finish() {
        Ok(files) => files,
        Err(e) => {
            error.get_or_insert_with(|| e.to_string());
            Vec::new()
        }
    };
    ResourceEntry {
        server: server.to_string(),
        resource: resource.to_string(),
        records,
        files,
        checkpoint,
        error,
    }
}

fn checkpoint_json(c: Option<&Checkpoint>) -> Option<Value> {
    c.map(|c| json!({ "timestamp": c.timestamp, "key": c.key }))
}
