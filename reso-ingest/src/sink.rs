//! Output destinations: where a finished data file goes
//!
//! The file writers ([`NdjsonWriter`](crate::NdjsonWriter), `ParquetWriter`) always build a file
//! locally. Without a sink that file is the output. With a [`FileSink`] set (`with_sink`), each
//! finished file is handed to the sink and the local copy is removed once the sink confirms. A
//! writer with a sink reports [`RecordWriter::commit`](crate::RecordWriter::commit) as `true` only
//! when no file is open, so a checkpoint is saved only after the sink holds every row it covers
//! (at-least-once delivery). Object storage backends (#85) implement this trait.

use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::IngestError;
use crate::output::output_error;

/// Where finished output files are delivered
///
/// Methods are synchronous because the writers are; a backend with an async client bridges them
/// (for example `tokio::task::block_in_place` with `Handle::block_on`).
pub trait FileSink: Debug + Send + Sync {
    /// Store the local file `local` under `name` and return once the destination has it
    ///
    /// Must not replace an existing `name`.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be stored, or `name` exists.
    fn put_file(&self, name: &str, local: &Path) -> Result<(), IngestError>;

    /// Whether `name` is already stored (the never-overwrite rule: a run id is used once)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the destination cannot be asked.
    fn exists(&self, name: &str) -> Result<bool, IngestError>;
}

/// A directory: today's behaviour when the writer's staging directory is somewhere else
#[derive(Debug, Clone)]
pub struct LocalDirSink {
    dir: PathBuf,
}

impl LocalDirSink {
    /// A sink that copies files into `dir`, created if missing
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when `dir` cannot be created.
    pub fn new(dir: impl AsRef<Path>) -> Result<Self, IngestError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| output_error(&dir, e))?;
        Ok(Self { dir })
    }
}

impl FileSink for LocalDirSink {
    fn put_file(&self, name: &str, local: &Path) -> Result<(), IngestError> {
        let dest = self.dir.join(name);
        if dest.exists() {
            return Err(IngestError::Output {
                path: dest.display().to_string(),
                message: "file already exists; another run may share this run id".to_string(),
            });
        }
        // Copy to a temporary name and rename, so the final name is never a partial file
        let tmp = self.dir.join(format!("{name}.part"));
        fs::copy(local, &tmp).map_err(|e| output_error(&tmp, e))?;
        fs::rename(&tmp, &dest).map_err(|e| output_error(&dest, e))
    }

    fn exists(&self, name: &str) -> Result<bool, IngestError> {
        Ok(self.dir.join(name).exists())
    }
}
