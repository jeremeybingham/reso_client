//! State backends: where checkpoints, key sets, event sequences, schema snapshots, leases and media
//! indexes are kept
//!
//! [`StateStore`] is a small key-value interface with conditional writes. The typed layers
//! ([`CheckpointStore`](crate::CheckpointStore), [`Lease`](crate::Lease),
//! [`MediaIndex`](crate::MediaIndex)) decide what goes in each value and are the same for every
//! backend; a backend only stores bytes under names. [`FileStateStore`] keeps them as files in a
//! directory, which is what `reso-ingest` has always done. Other backends (object storage, a
//! database) implement the same trait (#85).

use std::fmt::Debug;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::IngestError;

/// The future a [`StateStore`] method returns
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, IngestError>> + Send + 'a>>;

/// A stored value and the version it was read at
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versioned {
    /// The bytes stored under the key
    pub bytes: Vec<u8>,
    /// Opaque version of these bytes, for [`StateStore::replace`] and [`StateStore::delete`]
    /// (for example an S3 ETag, a row version or a content hash)
    pub version: String,
}

/// A key-value store for ingest state, with conditional writes
///
/// Keys are relative names such as `mls.Property.json` or `media/mls.json`: letters, digits and
/// `-_.@`, with `/` separating levels. Callers build them; a backend maps them to its own
/// namespace (a directory, a bucket prefix, a table) and stores the bytes unchanged.
///
/// The conditional methods ([`create`](Self::create), [`replace`](Self::replace),
/// [`delete`](Self::delete) with a version) are what leases need to keep one writer per feed. A
/// backend should make each of them atomic (S3 `If-None-Match`/`If-Match`, a database transaction);
/// where it cannot, it says so in its docs. Methods are async because most backends are remote; the
/// futures are boxed so the trait can be used as `dyn StateStore`.
///
/// # Implementing
///
/// ```
/// use std::collections::BTreeMap;
/// use std::sync::Mutex;
/// use reso_ingest::{StateStore, StoreFuture, Versioned};
///
/// /// A store in memory, for tests
/// #[derive(Debug, Default)]
/// struct Memory(Mutex<BTreeMap<String, (Vec<u8>, u64)>>);
///
/// impl StateStore for Memory {
///     fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Versioned>> {
///         let found = self.0.lock().unwrap().get(key).map(|(b, v)| Versioned {
///             bytes: b.clone(),
///             version: v.to_string(),
///         });
///         Box::pin(async move { Ok(found) })
///     }
///     fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, ()> {
///         let mut map = self.0.lock().unwrap();
///         let next = map.get(key).map_or(1, |(_, v)| v + 1);
///         map.insert(key.to_string(), (bytes.to_vec(), next));
///         Box::pin(async { Ok(()) })
///     }
///     fn create<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, bool> {
///         let mut map = self.0.lock().unwrap();
///         let created = !map.contains_key(key);
///         if created {
///             map.insert(key.to_string(), (bytes.to_vec(), 1));
///         }
///         Box::pin(async move { Ok(created) })
///     }
///     fn replace<'a>(&'a self, key: &'a str, bytes: &'a [u8], version: &'a str) -> StoreFuture<'a, bool> {
///         let mut map = self.0.lock().unwrap();
///         let done = match map.get_mut(key) {
///             Some((b, v)) if v.to_string() == version => {
///                 *b = bytes.to_vec();
///                 *v += 1;
///                 true
///             }
///             _ => false,
///         };
///         Box::pin(async move { Ok(done) })
///     }
///     fn delete<'a>(&'a self, key: &'a str, version: Option<&'a str>) -> StoreFuture<'a, bool> {
///         let mut map = self.0.lock().unwrap();
///         let matches = match (map.get(key), version) {
///             (Some(_), None) => true,
///             (Some((_, v)), Some(want)) => v.to_string() == want,
///             (None, _) => false,
///         };
///         if matches {
///             map.remove(key);
///         }
///         Box::pin(async move { Ok(matches) })
///     }
///     fn list(&self) -> StoreFuture<'_, Vec<String>> {
///         let keys = self.0.lock().unwrap().keys().cloned().collect();
///         Box::pin(async move { Ok(keys) })
///     }
///     fn location(&self, key: &str) -> String {
///         format!("memory:{key}")
///     }
/// }
/// ```
pub trait StateStore: Send + Sync + Debug {
    /// The value under `key` and its version, or `None` when there is none
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be read.
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Versioned>>;

    /// Store `bytes` under `key`, replacing any value; a reader sees the old or the new value, never
    /// part of one, and the value is durable when the future completes
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be written.
    fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, ()>;

    /// Store `bytes` under `key` only if there is no value yet; `false` when there was one
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be written.
    fn create<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, bool>;

    /// Replace the value under `key` only if it is still at `version`; `false` when it changed or
    /// is gone
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be written.
    fn replace<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        version: &'a str,
    ) -> StoreFuture<'a, bool>;

    /// Remove `key`, only if it is still at `version` when one is given; `false` when there was
    /// nothing to remove or the version did not match
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be written.
    fn delete<'a>(&'a self, key: &'a str, version: Option<&'a str>) -> StoreFuture<'a, bool>;

    /// Every key in the store, at all levels, in no particular order
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be listed.
    fn list(&self) -> StoreFuture<'_, Vec<String>>;

    /// Where `key` lives, for messages (a file path, a URL); never contains a secret
    fn location(&self, key: &str) -> String;

    /// [`create`](Self::create) that also reports the version the value was stored at, so a
    /// caller that later replaces or deletes it does not have to read it back first
    ///
    /// The provided version calls [`create`](Self::create) and reports no version; backends
    /// that learn it from the write (an ETag, a generation, a content hash) override it.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be written.
    fn create_versioned<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, Written> {
        Box::pin(async move {
            Ok(if self.create(key, bytes).await? {
                Written::Stored(None)
            } else {
                Written::Refused
            })
        })
    }

    /// [`replace`](Self::replace) that also reports the new version, as
    /// [`create_versioned`](Self::create_versioned) does
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be written.
    fn replace_versioned<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        version: &'a str,
    ) -> StoreFuture<'a, Written> {
        Box::pin(async move {
            Ok(if self.replace(key, bytes, version).await? {
                Written::Stored(None)
            } else {
                Written::Refused
            })
        })
    }

    /// The keys directly under `dir` (`""` for the top level, else a name such as `runs`), not
    /// those at deeper levels, as full keys (`runs/a.reserved`)
    ///
    /// Run id reservations and leases are looked up this way, so a store holding many feeds' key
    /// sets and media indexes is not listed in full on every run. The provided version filters
    /// [`list`](Self::list); backends override it to list one level.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be listed.
    fn list_dir<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, Vec<String>> {
        Box::pin(async move {
            let prefix = if dir.is_empty() {
                String::new()
            } else {
                format!("{}/", dir.trim_end_matches('/'))
            };
            Ok(self
                .list()
                .await?
                .into_iter()
                .filter(|k| {
                    k.strip_prefix(&prefix)
                        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
                })
                .collect())
        })
    }

    /// Copy the value under `key` into the local file `dest` (created or replaced); `false` when
    /// there is no value
    ///
    /// For large values such as key sets (#87). The provided version reads the value into memory
    /// with [`get`](Self::get); backends override it to stream.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the backend cannot be read or `dest` cannot be written.
    fn get_to_file<'a>(&'a self, key: &'a str, dest: &'a Path) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let Some(found) = self.get(key).await? else {
                return Ok(false);
            };
            fs::write(dest, &found.bytes).map_err(|e| io_error(dest, e))?;
            Ok(true)
        })
    }

    /// Store the contents of the local file `src` under `key`, as [`put`](Self::put) does
    ///
    /// The provided version reads the file into memory; backends override it to stream.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when `src` cannot be read or the backend cannot be written.
    fn put_from_file<'a>(&'a self, key: &'a str, src: &'a Path) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let bytes = fs::read(src).map_err(|e| io_error(src, e))?;
            self.put(key, &bytes).await
        })
    }
}

/// What a conditional write did ([`StateStore::create_versioned`], [`StateStore::replace_versioned`])
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// The value was stored; its new version when the backend reports it
    Stored(Option<String>),
    /// Nothing was stored: there was a value already (create) or it had changed (replace)
    Refused,
}

/// A [`StateStore`] in a local directory: one file per key
///
/// Key `a/b.json` is the file `<dir>/a/b.json`. [`put`](StateStore::put) writes a temporary file
/// (`*.tmp`, unique per process and write), syncs it and renames it over the old one, so a crash
/// leaves the old or the new value. The version of a value is a hash of its bytes.
/// [`create`](StateStore::create) is atomic (the file is created exclusively).
/// [`replace`](StateStore::replace) and [`delete`](StateStore::delete) with a version compare and
/// then act, which leaves a window of a few microseconds in which another process could change the
/// file; leases tolerate that (the loser notices at its next renewal). Share the directory between
/// processes on one host or on a filesystem with exclusive create (not every network filesystem
/// has it).
///
/// # Examples
///
/// ```
/// # async fn demo() -> Result<(), reso_ingest::IngestError> {
/// use reso_ingest::{FileStateStore, StateStore};
///
/// let dir = std::env::temp_dir().join(format!("reso-ingest-state-doc-{}", std::process::id()));
/// let store = FileStateStore::new(&dir);
/// assert!(store.create("feed.lock", b"mine").await?);
/// assert!(!store.create("feed.lock", b"theirs").await?);
/// let held = store.get("feed.lock").await?.unwrap();
/// assert!(store.replace("feed.lock", b"renewed", &held.version).await?);
/// assert!(!store.replace("feed.lock", b"stale", &held.version).await?);
/// # let _ = std::fs::remove_dir_all(&dir);
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct FileStateStore {
    dir: PathBuf,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl FileStateStore {
    /// A store in `dir`; the directory is created on the first write
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    /// The directory
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: &str) -> PathBuf {
        key.split('/')
            .fold(self.dir.clone(), |p, part| p.join(part))
    }

    fn read(&self, key: &str) -> Result<Option<Versioned>, IngestError> {
        let path = self.path(key);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(Versioned {
                version: content_version(&bytes),
                bytes,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error(&path, e)),
        }
    }

    fn write(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        self.write_with(key, |tmp| {
            File::create(tmp).and_then(|mut f| f.write_all(bytes))
        })
    }

    /// Fill a temporary file with `fill`, sync it and rename it over the value
    fn write_with(
        &self,
        key: &str,
        fill: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<(), IngestError> {
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp = path.clone().into_os_string();
        tmp.push(format!(".{}-{n}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let written = fill(&tmp)
            .and_then(|()| OpenOptions::new().write(true).open(&tmp))
            .and_then(|f| f.sync_all())
            .and_then(|()| fs::rename(&tmp, &path));
        written.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            io_error(&path, e)
        })
    }

    fn create_new(&self, key: &str, bytes: &[u8]) -> Result<bool, IngestError> {
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(bytes)
                    .and_then(|()| f.sync_all())
                    .map_err(|e| io_error(&path, e))?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(io_error(&path, e)),
        }
    }

    fn remove(&self, key: &str, version: Option<&str>) -> Result<bool, IngestError> {
        if let Some(want) = version {
            match self.read(key)? {
                Some(v) if v.version == want => {}
                _ => return Ok(false),
            }
        }
        let path = self.path(key);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error(&path, e)),
        }
    }

    fn walk(&self) -> Result<Vec<String>, IngestError> {
        let mut keys = Vec::new();
        let mut pending = vec![(self.dir.clone(), String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_error(&dir, e)),
            };
            for entry in entries {
                let entry = entry.map_err(|e| io_error(&dir, e))?;
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let file_type = entry.file_type().map_err(|e| io_error(&entry.path(), e))?;
                if file_type.is_dir() {
                    pending.push((entry.path(), format!("{prefix}{name}/")));
                } else if !name.ends_with(".tmp") {
                    keys.push(format!("{prefix}{name}"));
                }
            }
        }
        Ok(keys)
    }
}

impl StateStore for FileStateStore {
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Versioned>> {
        Box::pin(async move { self.read(key) })
    }

    fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, ()> {
        Box::pin(async move { self.write(key, bytes) })
    }

    fn create<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, bool> {
        Box::pin(async move { self.create_new(key, bytes) })
    }

    fn replace<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        version: &'a str,
    ) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            match self.read(key)? {
                Some(v) if v.version == version => self.write(key, bytes).map(|()| true),
                _ => Ok(false),
            }
        })
    }

    fn delete<'a>(&'a self, key: &'a str, version: Option<&'a str>) -> StoreFuture<'a, bool> {
        Box::pin(async move { self.remove(key, version) })
    }

    fn list(&self) -> StoreFuture<'_, Vec<String>> {
        Box::pin(async move { self.walk() })
    }

    fn create_versioned<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, Written> {
        Box::pin(async move {
            Ok(if self.create_new(key, bytes)? {
                Written::Stored(Some(content_version(bytes)))
            } else {
                Written::Refused
            })
        })
    }

    fn replace_versioned<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        version: &'a str,
    ) -> StoreFuture<'a, Written> {
        Box::pin(async move {
            Ok(if self.replace(key, bytes, version).await? {
                Written::Stored(Some(content_version(bytes)))
            } else {
                Written::Refused
            })
        })
    }

    fn list_dir<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, Vec<String>> {
        Box::pin(async move {
            let path = if dir.is_empty() {
                self.dir.clone()
            } else {
                self.path(dir.trim_end_matches('/'))
            };
            let entries = match fs::read_dir(&path) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(io_error(&path, e)),
            };
            let prefix = if dir.is_empty() {
                String::new()
            } else {
                format!("{}/", dir.trim_end_matches('/'))
            };
            let mut keys = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| io_error(&path, e))?;
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let file_type = entry.file_type().map_err(|e| io_error(&entry.path(), e))?;
                if !file_type.is_dir() && !name.ends_with(".tmp") {
                    keys.push(format!("{prefix}{name}"));
                }
            }
            Ok(keys)
        })
    }

    fn location(&self, key: &str) -> String {
        self.path(key).display().to_string()
    }

    fn get_to_file<'a>(&'a self, key: &'a str, dest: &'a Path) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let path = self.path(key);
            match fs::copy(&path, dest) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !path.exists() => Ok(false),
                Err(e) => Err(io_error(&path, e)),
            }
        })
    }

    fn put_from_file<'a>(&'a self, key: &'a str, src: &'a Path) -> StoreFuture<'a, ()> {
        Box::pin(async move { self.write_with(key, |tmp| fs::copy(src, tmp).map(|_| ())) })
    }
}

/// FNV-1a over the bytes, as 16 hex digits: stable across processes and builds
fn content_version(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn io_error(path: &Path, e: std::io::Error) -> IngestError {
    IngestError::Output {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

/// An [`IngestError::Output`] for a value that is in the store but not valid
pub(crate) fn invalid(store: &dyn StateStore, key: &str, message: String) -> IngestError {
    IngestError::Output {
        path: store.location(key),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[tokio::test]
    async fn versioned_writes_report_the_version_a_read_sees() {
        let d = tmp("versioned");
        let s = FileStateStore::new(&d);
        let Written::Stored(Some(v1)) = s.create_versioned("a.lock", b"one").await.unwrap() else {
            panic!("create must store and report a version");
        };
        assert_eq!(s.get("a.lock").await.unwrap().unwrap().version, v1);
        assert_eq!(
            s.create_versioned("a.lock", b"two").await.unwrap(),
            Written::Refused
        );
        let Written::Stored(Some(v2)) = s.replace_versioned("a.lock", b"two", &v1).await.unwrap()
        else {
            panic!("replace at the current version must store");
        };
        assert_eq!(s.get("a.lock").await.unwrap().unwrap().version, v2);
        assert_eq!(
            s.replace_versioned("a.lock", b"three", &v1).await.unwrap(),
            Written::Refused
        );
        assert!(s.delete("a.lock", Some(&v2)).await.unwrap());
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn list_dir_lists_one_level() {
        let d = tmp("listdir");
        let s = FileStateStore::new(&d);
        assert!(s.list_dir("").await.unwrap().is_empty(), "no directory yet");
        for k in [
            "top.json",
            "f.R.lock",
            "runs/a.reserved",
            "runs/deep/b.json",
            "media/m.json",
        ] {
            s.put(k, b"{}").await.unwrap();
        }
        let mut top = s.list_dir("").await.unwrap();
        top.sort();
        assert_eq!(top, ["f.R.lock", "top.json"]);
        assert_eq!(s.list_dir("runs").await.unwrap(), ["runs/a.reserved"]);
        assert_eq!(s.list_dir("runs/").await.unwrap(), ["runs/a.reserved"]);
        assert!(s.list_dir("none").await.unwrap().is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    /// The provided methods, for backends that do not override them
    #[derive(Debug)]
    struct Plain(FileStateStore);

    impl StateStore for Plain {
        fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Versioned>> {
            self.0.get(key)
        }
        fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, ()> {
            self.0.put(key, bytes)
        }
        fn create<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, bool> {
            self.0.create(key, bytes)
        }
        fn replace<'a>(
            &'a self,
            key: &'a str,
            bytes: &'a [u8],
            version: &'a str,
        ) -> StoreFuture<'a, bool> {
            self.0.replace(key, bytes, version)
        }
        fn delete<'a>(&'a self, key: &'a str, version: Option<&'a str>) -> StoreFuture<'a, bool> {
            self.0.delete(key, version)
        }
        fn list(&self) -> StoreFuture<'_, Vec<String>> {
            self.0.list()
        }
        fn location(&self, key: &str) -> String {
            self.0.location(key)
        }
    }

    #[tokio::test]
    async fn provided_methods_work_without_overrides() {
        let d = tmp("provided");
        let s = Plain(FileStateStore::new(&d));
        assert_eq!(
            s.create_versioned("x", b"1").await.unwrap(),
            Written::Stored(None)
        );
        assert_eq!(
            s.create_versioned("x", b"2").await.unwrap(),
            Written::Refused
        );
        let v = s.get("x").await.unwrap().unwrap().version;
        assert_eq!(
            s.replace_versioned("x", b"2", &v).await.unwrap(),
            Written::Stored(None)
        );
        assert_eq!(
            s.replace_versioned("x", b"3", &v).await.unwrap(),
            Written::Refused
        );
        for k in ["runs/a.reserved", "runs/deep/b.json", "top.lock"] {
            s.put(k, b"{}").await.unwrap();
        }
        let mut top = s.list_dir("").await.unwrap();
        top.sort();
        assert_eq!(top, ["top.lock", "x"]);
        assert_eq!(s.list_dir("runs").await.unwrap(), ["runs/a.reserved"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn conditional_writes_follow_the_version() {
        let dir = tmp("cond");
        let s = FileStateStore::new(&dir);
        assert_eq!(s.get("a.json").await.unwrap(), None);
        assert!(s.create("a.json", b"one").await.unwrap());
        assert!(!s.create("a.json", b"two").await.unwrap());
        let v1 = s.get("a.json").await.unwrap().unwrap();
        assert_eq!(v1.bytes, b"one");
        assert!(s.replace("a.json", b"two", &v1.version).await.unwrap());
        assert!(!s.replace("a.json", b"three", &v1.version).await.unwrap());
        assert!(!s.delete("a.json", Some(&v1.version)).await.unwrap());
        let v2 = s.get("a.json").await.unwrap().unwrap();
        assert!(s.delete("a.json", Some(&v2.version)).await.unwrap());
        assert!(!s.delete("a.json", None).await.unwrap());
        assert!(!s.replace("a.json", b"x", &v2.version).await.unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lists_nested_keys_and_skips_temporary_files() {
        let dir = tmp("list");
        let s = FileStateStore::new(&dir);
        s.put("x.json", b"1").await.unwrap();
        s.put("media/m.json", b"2").await.unwrap();
        fs::write(dir.join("y.json.1-1.tmp"), b"partial").unwrap();
        let mut keys = s.list().await.unwrap();
        keys.sort();
        assert_eq!(keys, ["media/m.json", "x.json"]);
        assert_eq!(s.get("media/m.json").await.unwrap().unwrap().bytes, b"2");
        assert_eq!(
            s.location("media/m.json"),
            dir.join("media").join("m.json").display().to_string()
        );
        // A missing directory lists as empty
        assert!(FileStateStore::new(dir.join("none"))
            .list()
            .await
            .unwrap()
            .is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn versions_are_stable() {
        assert_eq!(content_version(b""), "cbf29ce484222325");
        assert_ne!(content_version(b"a"), content_version(b"b"));
    }
}
