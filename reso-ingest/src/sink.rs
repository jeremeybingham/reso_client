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

    /// Where `name` is stored, for messages (a path, a URL); never contains a secret
    fn location(&self, name: &str) -> String {
        name.to_string()
    }

    /// The stored names with their sizes in bytes, in name order (`purge`, #139)
    ///
    /// Names at the top level, plus the files of the partition tree (`source=…/…`, from
    /// `--partition`) with their folders; nothing else in deeper folders. The default refuses, for
    /// sinks that only take files.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the destination cannot be listed, or the sink cannot list.
    fn list(&self) -> Result<Vec<(String, u64)>, IngestError> {
        Err(unsupported(self, "list"))
    }

    /// Copy `name` into the local file `dest` (replaced if it exists); `false` when `name` is not
    /// stored
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be read or written, or the sink cannot read.
    fn get_file(&self, name: &str, dest: &Path) -> Result<bool, IngestError> {
        let _ = dest;
        Err(unsupported(self, &format!("read {name}")))
    }

    /// Delete `name`; `false` when it was not stored (an S3-compatible store does not say, so it
    /// reports `true`)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be deleted, or the sink cannot delete.
    fn delete(&self, name: &str) -> Result<bool, IngestError> {
        Err(unsupported(self, &format!("delete {name}")))
    }

    /// The local path of `name` when the sink is a directory on this machine, so a reader can open
    /// it in place; `None` for remote storage
    fn local_path(&self, name: &str) -> Option<PathBuf> {
        let _ = name;
        None
    }
}

fn unsupported<S: FileSink + ?Sized>(sink: &S, what: &str) -> IngestError {
    IngestError::Output {
        path: sink.location(""),
        message: format!("this sink cannot {what}"),
    }
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

    /// A sink over `dir` that does not create it (a dry-run `purge` changes nothing); listing a
    /// missing directory finds no files
    pub fn existing(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }
}

impl FileSink for LocalDirSink {
    fn put_file(&self, name: &str, local: &Path) -> Result<(), IngestError> {
        let dest = self.dir.join(name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| output_error(parent, e))?;
        }
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

    fn location(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    /// Regular files in the directory (including `.part` files a killed run left), and in the
    /// partition tree (`source=…` folders) at all levels
    fn list(&self) -> Result<Vec<(String, u64)>, IngestError> {
        let mut files = Vec::new();
        list_into(&self.dir, "", &mut files)?;
        files.sort();
        Ok(files)
    }

    fn get_file(&self, name: &str, dest: &Path) -> Result<bool, IngestError> {
        let src = self.dir.join(name);
        match fs::copy(&src, dest) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !src.exists() => Ok(false),
            Err(e) => Err(output_error(&src, e)),
        }
    }

    /// Also removes the partition folders the deletion left empty
    fn delete(&self, name: &str) -> Result<bool, IngestError> {
        let path = self.dir.join(name);
        let deleted = match fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(output_error(&path, e)),
        };
        let mut folder = path.parent();
        while let Some(dir) = folder.filter(|d| *d != self.dir && d.starts_with(&self.dir)) {
            // Fails, and stops, at the first folder that still holds something
            if fs::remove_dir(dir).is_err() {
                break;
            }
            folder = dir.parent();
        }
        Ok(deleted)
    }

    fn local_path(&self, name: &str) -> Option<PathBuf> {
        Some(self.dir.join(name))
    }
}

/// Files of `dir` into `files` as `prefix` + name; descends into `source=…` folders at the top
/// and into every folder below them
fn list_into(dir: &Path, prefix: &str, files: &mut Vec<(String, u64)>) -> Result<(), IngestError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(output_error(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| output_error(dir, e))?;
        let Ok(meta) = entry.metadata() else { continue };
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        if meta.is_file() {
            files.push((format!("{prefix}{name}"), meta.len()));
        } else if meta.is_dir() && (!prefix.is_empty() || name.starts_with("source=")) {
            list_into(&entry.path(), &format!("{prefix}{name}/"), files)?;
        }
    }
    Ok(())
}
