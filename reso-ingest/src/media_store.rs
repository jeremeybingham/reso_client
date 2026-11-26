//! Where media files go: a directory on this machine or a bucket (#89)
//!
//! A [`MediaStore`] holds the files of one server under flat names. Media downloads stream into
//! a [`MediaUpload`] chunk by chunk and name the file only when it is complete (by its content
//! hash, see [`crate::media`]), so the store never needs a local copy and a file is stored once
//! however many `MediaKey`s point at it. [`LocalMediaStore`] is a directory; the bucket version
//! is `S3MediaStore` (feature `s3`).

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use crate::error::IngestError;
use crate::state::StoreFuture;

/// Prefix of the temporary names uploads use before they are complete
pub const PART_PREFIX: &str = ".part-";

/// The media files of one server
pub trait MediaStore: Debug + Send + Sync {
    /// Start an upload; nothing is visible under a final name until [`MediaUpload::finish`]
    fn upload(&self) -> Box<dyn MediaUpload>;

    /// Whether `name` is stored
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the store cannot be asked.
    fn exists<'a>(&'a self, name: &'a str) -> StoreFuture<'a, bool>;

    /// Delete `name` (no error when it is not stored)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be deleted.
    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()>;

    /// Every stored name with its size in bytes, in name order (temporary upload names included)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the store cannot be listed.
    fn list(&self) -> StoreFuture<'_, Vec<(String, u64)>>;

    /// Where `name` is stored, for messages; never contains a secret (`""` gives the store itself)
    fn location(&self, name: &str) -> String;

    /// Delete every stored file, temporary upload names included; returns the files and bytes
    /// deleted (used by `purge`)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the store cannot be listed or a file cannot be deleted; the
    /// files before it are deleted.
    fn clear(&self) -> StoreFuture<'_, (u64, u64)> {
        Box::pin(async move {
            let (mut files, mut bytes) = (0, 0);
            for (name, size) in self.list().await? {
                self.delete(&name).await?;
                files += 1;
                bytes += size;
            }
            Ok((files, bytes))
        })
    }
}

/// One file being streamed into a [`MediaStore`]
pub trait MediaUpload: Send {
    /// Append `chunk`
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the bytes cannot be written.
    fn write<'a>(&'a mut self, chunk: &'a [u8]) -> StoreFuture<'a, ()>;

    /// Store what was written under `name`; `false` when `name` was already stored (the new
    /// bytes are dropped: names are content hashes, so they are the same file)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be stored.
    fn finish<'a>(&'a mut self, name: &'a str) -> StoreFuture<'a, bool>;

    /// Drop what was written
    fn abort(&mut self) -> StoreFuture<'_, ()>;
}

/// Where the media files of every server go: `<dir>/<server>/`, or `<prefix>/<server>/` in a
/// bucket (the command uses `<prefix>/media/`)
#[derive(Debug, Clone)]
pub enum MediaRoot {
    /// A directory on this machine
    Dir(PathBuf),
    /// A bucket (feature `s3`); the options' prefix is the media root
    #[cfg(feature = "s3")]
    Bucket(crate::S3Options),
}

impl MediaRoot {
    /// The store of `server`'s files
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when no bucket client can be built.
    pub fn store(&self, server: &str) -> Result<Arc<dyn MediaStore>, IngestError> {
        let part = crate::media::encode_name(server);
        match self {
            Self::Dir(dir) => Ok(Arc::new(LocalMediaStore::new(dir.join(part)))),
            #[cfg(feature = "s3")]
            Self::Bucket(options) => {
                let mut options = options.clone();
                options.prefix = match options.prefix.trim_matches('/') {
                    "" => part,
                    prefix => format!("{prefix}/{part}"),
                };
                Ok(Arc::new(crate::S3MediaStore::new(&options)?))
            }
        }
    }
}

static UPLOADS: AtomicU64 = AtomicU64::new(0);

/// A temporary name no other upload of this or another process uses
pub fn part_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = UPLOADS.fetch_add(1, Ordering::Relaxed);
    format!("{PART_PREFIX}{}-{nanos}-{n}", std::process::id())
}

/// A directory: `<media_dir>/<server>/`
#[derive(Debug, Clone)]
pub struct LocalMediaStore {
    dir: PathBuf,
}

impl LocalMediaStore {
    /// A store over `dir`, created on the first upload
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }
}

fn io(path: &Path, e: std::io::Error) -> IngestError {
    IngestError::Output {
        path: path.display().to_string(),
        message: e.kind().to_string(),
    }
}

impl MediaStore for LocalMediaStore {
    fn upload(&self) -> Box<dyn MediaUpload> {
        Box::new(LocalUpload {
            dir: self.dir.clone(),
            part: self.dir.join(part_name()),
            file: None,
        })
    }

    fn exists<'a>(&'a self, name: &'a str) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let path = self.dir.join(name);
            tokio::fs::try_exists(&path).await.map_err(|e| io(&path, e))
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.dir.join(name);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io(&path, e)),
            }
        })
    }

    fn list(&self) -> StoreFuture<'_, Vec<(String, u64)>> {
        Box::pin(async move {
            let mut entries = match tokio::fs::read_dir(&self.dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(io(&self.dir, e)),
            };
            let mut files = Vec::new();
            while let Some(entry) = entries.next_entry().await.map_err(|e| io(&self.dir, e))? {
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if let (true, Some(name)) = (meta.is_file(), entry.file_name().to_str()) {
                    files.push((name.to_string(), meta.len()));
                }
            }
            files.sort();
            Ok(files)
        })
    }

    fn location(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    fn clear(&self) -> StoreFuture<'_, (u64, u64)> {
        Box::pin(async move {
            let listed = self.list().await?;
            match tokio::fs::remove_dir_all(&self.dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(&self.dir, e)),
            }
            Ok((listed.len() as u64, listed.iter().map(|(_, n)| n).sum()))
        })
    }
}

/// A file written to a `.part-*` name in the directory and renamed when complete
struct LocalUpload {
    dir: PathBuf,
    part: PathBuf,
    file: Option<tokio::fs::File>,
}

impl LocalUpload {
    async fn open(&mut self) -> Result<&mut tokio::fs::File, IngestError> {
        if self.file.is_none() {
            tokio::fs::create_dir_all(&self.dir)
                .await
                .map_err(|e| io(&self.dir, e))?;
            let file = tokio::fs::File::create(&self.part)
                .await
                .map_err(|e| io(&self.part, e))?;
            self.file = Some(file);
        }
        // Set just above
        self.file.as_mut().ok_or_else(|| IngestError::Output {
            path: self.part.display().to_string(),
            message: "not open".into(),
        })
    }
}

impl MediaUpload for LocalUpload {
    fn write<'a>(&'a mut self, chunk: &'a [u8]) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let part = self.part.clone();
            self.open()
                .await?
                .write_all(chunk)
                .await
                .map_err(|e| io(&part, e))
        })
    }

    fn finish<'a>(&'a mut self, name: &'a str) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let part = self.part.clone();
            let file = self.open().await?;
            file.flush().await.map_err(|e| io(&part, e))?;
            self.file = None;
            let dest = self.dir.join(name);
            // A hard link fails when the name exists, so two uploads of the same content cannot
            // both store it
            let stored = match tokio::fs::hard_link(&part, &dest).await {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                // No hard links on this file system: check, then rename
                Err(_) if tokio::fs::try_exists(&dest).await.unwrap_or(false) => false,
                Err(_) => {
                    tokio::fs::rename(&part, &dest)
                        .await
                        .map_err(|e| io(&dest, e))?;
                    return Ok(true);
                }
            };
            let _ = tokio::fs::remove_file(&part).await;
            Ok(stored)
        })
    }

    fn abort(&mut self) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            if self.file.take().is_some() {
                let _ = tokio::fs::remove_file(&self.part).await;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reso-media-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn uploads_appear_only_when_finished_and_never_replace() {
        let dir = temp_dir("finish");
        let store = LocalMediaStore::new(&dir);
        let mut up = store.upload();
        up.write(b"ab").await.unwrap();
        up.write(b"cd").await.unwrap();
        assert!(!store.exists("x.jpg").await.unwrap());
        assert!(up.finish("x.jpg").await.unwrap());
        assert_eq!(std::fs::read(dir.join("x.jpg")).unwrap(), b"abcd");

        let mut again = store.upload();
        again.write(b"zz").await.unwrap();
        assert!(!again.finish("x.jpg").await.unwrap());
        assert_eq!(std::fs::read(dir.join("x.jpg")).unwrap(), b"abcd");
        // No temporary file is left behind
        assert_eq!(store.list().await.unwrap(), vec![("x.jpg".to_string(), 4)]);

        let mut dropped = store.upload();
        dropped.write(b"half").await.unwrap();
        dropped.abort().await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 1);

        store.delete("x.jpg").await.unwrap();
        store.delete("x.jpg").await.unwrap();
        assert!(store.list().await.unwrap().is_empty());

        let mut up = store.upload();
        up.write(b"abc").await.unwrap();
        assert!(up.finish("y.jpg").await.unwrap());
        assert_eq!(store.clear().await.unwrap(), (1, 3));
        assert!(!dir.exists());
        assert_eq!(store.clear().await.unwrap(), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_empty_file_can_be_stored_and_a_missing_dir_lists_nothing() {
        let dir = temp_dir("empty");
        let store = LocalMediaStore::new(&dir);
        assert!(store.list().await.unwrap().is_empty());
        let mut up = store.upload();
        assert!(up.finish("empty").await.unwrap());
        assert_eq!(store.list().await.unwrap(), vec![("empty".to_string(), 0)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn part_names_are_distinct() {
        assert_ne!(part_name(), part_name());
        assert!(part_name().starts_with(PART_PREFIX));
    }
}
