//! Checkpoints: where each server's resource was last pulled up to

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use reso_client::Checkpoint;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::IngestError;
use crate::output::now_rfc3339;

#[derive(Serialize, Deserialize)]
struct CheckpointFile {
    server: String,
    resource: String,
    timestamp: String,
    key: Option<Value>,
    updated_at: String,
}

/// A directory of JSON checkpoint files, one per server and resource
///
/// Files are named `{server}.{resource}.json` (characters other than letters, digits, `-` and `_`
/// become `_`) and also record the real server and resource names, which are checked on load.
/// [`save`](Self::save) writes a temporary file, syncs it and renames it over the old one, so a
/// crash leaves either the old or the new checkpoint, never a torn file.
///
/// # Examples
///
/// ```
/// use reso_client::Checkpoint;
/// use reso_ingest::CheckpointStore;
///
/// let dir = std::env::temp_dir().join(format!("reso-ingest-doc-{}", std::process::id()));
/// let store = CheckpointStore::new(&dir);
/// store.save("mls", "Property", &Checkpoint::new("2026-01-01T00:00:00Z", "A1".into()))?;
/// assert_eq!(store.load("mls", "Property")?.unwrap().timestamp, "2026-01-01T00:00:00Z");
/// store.clear("mls", "Property")?;
/// assert!(store.load("mls", "Property")?.is_none());
/// # let _ = std::fs::remove_dir_all(&dir);
/// # Ok::<(), reso_ingest::IngestError>(())
/// ```
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    dir: PathBuf,
}

impl CheckpointStore {
    /// A store in `dir`; the directory is created on the first save
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    fn path(&self, server: &str, resource: &str) -> PathBuf {
        self.dir
            .join(format!("{}.{}.json", sanitize(server), sanitize(resource)))
    }

    /// The saved checkpoint, or `None` when there is none yet
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file is unreadable, is not valid, or belongs to another
    /// server or resource. A bad file is an error rather than "no checkpoint", so a damaged file
    /// never silently turns an incremental sync into a full reload.
    pub fn load(&self, server: &str, resource: &str) -> Result<Option<Checkpoint>, IngestError> {
        let path = self.path(server, resource);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(&path, e)),
        };
        let file: CheckpointFile =
            serde_json::from_str(&text).map_err(|e| IngestError::Output {
                path: path.display().to_string(),
                message: format!("invalid checkpoint: {e}"),
            })?;
        if file.server != server || file.resource != resource {
            return Err(IngestError::Output {
                path: path.display().to_string(),
                message: format!(
                    "checkpoint belongs to `{}`/`{}`, not `{server}`/`{resource}`",
                    file.server, file.resource
                ),
            });
        }
        Ok(Some(Checkpoint {
            timestamp: file.timestamp,
            key: file.key,
        }))
    }

    /// Save `checkpoint` atomically
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be written.
    pub fn save(
        &self,
        server: &str,
        resource: &str,
        checkpoint: &Checkpoint,
    ) -> Result<(), IngestError> {
        let path = self.path(server, resource);
        let tmp = path.with_extension("json.tmp");
        let file = CheckpointFile {
            server: server.to_string(),
            resource: resource.to_string(),
            timestamp: checkpoint.timestamp.clone(),
            key: checkpoint.key.clone(),
            updated_at: now_rfc3339(),
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| IngestError::Output {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        fs::create_dir_all(&self.dir).map_err(|e| io_error(&self.dir, e))?;
        let mut f = File::create(&tmp).map_err(|e| io_error(&tmp, e))?;
        f.write_all(text.as_bytes())
            .and_then(|()| f.write_all(b"\n"))
            .and_then(|()| f.sync_all())
            .map_err(|e| io_error(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| io_error(&path, e))
    }

    /// Delete the checkpoint ("start over"); no error when there is none
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file exists but cannot be removed.
    pub fn clear(&self, server: &str, resource: &str) -> Result<(), IngestError> {
        let path = self.path(server, resource);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&path, e)),
        }
    }
}

fn io_error(path: &Path, e: std::io::Error) -> IngestError {
    IngestError::Output {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
