//! Media file download: the `Media` records `sync` wrote, turned into stored files
//!
//! Files go to a [`MediaStore`]: a directory (`<media_dir>/<server>/`) or, with a bucket, the
//! bucket (`<prefix>/media/<server>/`), streamed there without a local copy (#89). Each file is
//! named by the SHA-256 of its content plus an extension from `Content-Type`
//! (`<sha256>.jpg`), so a photo that several `MediaKey`s point at, or that comes back unchanged
//! under a new timestamp, is stored once. A per-server [`MediaIndex`] (state store key
//! `media/<server>.json`, plus small deltas in `media/<server>/`) maps each `MediaKey` to its file
//! and records which data files were read, so a run only downloads what changed and picks up
//! failed files again. A save writes only the entries that changed (#149).
//!
//! Downloads run in their own worker pool ([`MediaWorkers`], `[server.media] workers`) with the
//! `[server.media]` rate limits, apart from the API's: `sync --media` hands each page's new and
//! changed files to the pool and goes on with the next page at once, so a slow media server never
//! holds up record syncs. [`sync_media`] (the `media` command) feeds the same pool from the data
//! files of earlier runs.
//!
//! A file is downloaded again when the record's `MediaModificationTimestamp` (or, when absent,
//! `ModificationTimestamp`) changes. A changed URL alone does not count, because signed CDN URLs
//! change on every pull; the index keeps the newest URL for the next download. A record with no
//! timestamp at all is downloaded again when its URL changes. Re-downloads send the saved
//! `ETag`/`Last-Modified`, so an unchanged file costs a 304, and a body whose hash is the one
//! already stored is not written again. A delete event for a `MediaKey` removes its index entry,
//! and its file once no other key points at it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use reso_client::{
    DownloadConfig, DownloadOutcome, DownloadTarget, Downloader, ResoClient, ResoError, SavedFile,
    Validators,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::error::IngestError;
use crate::media_store::{MediaStore, MediaUpload};
use crate::output::now_rfc3339;
use crate::shutdown::Shutdown;
use crate::state::{invalid, StateStore};

/// The resource whose records point at files
pub const MEDIA_RESOURCE: &str = "Media";
const KEY_FIELD: &str = "MediaKey";
const URL_FIELD: &str = "MediaURL";
const STAMP_FIELDS: [&str; 2] = ["MediaModificationTimestamp", "ModificationTimestamp"];
/// Save the index after this many finished downloads, so a crash loses little
const SAVE_EVERY: u64 = 50;
/// Download workers per server when `[server.media] workers` is not set
pub const DEFAULT_WORKERS: usize = 4;

/// What is known about one `MediaKey`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaEntry {
    /// Newest `MediaURL` seen
    pub url: Option<String>,
    /// Timestamp of the newest record (`MediaModificationTimestamp`, else `ModificationTimestamp`)
    pub stamp: Option<String>,
    /// The file still has to be downloaded (new, changed, or the last attempt failed)
    pub pending: bool,
    /// Name of the stored file (`<sha256>.<ext>`), once saved; other keys may share it
    pub file: Option<String>,
    /// SHA-256 (hex) of the stored file's content
    #[serde(default)]
    pub sha256: Option<String>,
    /// `ETag` of the saved file
    pub etag: Option<String>,
    /// `Last-Modified` of the saved file
    pub last_modified: Option<String>,
    /// `Content-Type` of the saved file
    pub content_type: Option<String>,
    /// Size of the saved file
    pub bytes: Option<u64>,
    /// Error of the last attempt, if it failed
    pub error: Option<String>,
}

/// The per-server record of data files read and files saved
///
/// Stored in two parts so that a save costs what changed, not the size of the index (#149): a
/// snapshot under [`key`](Self::key) (`media/<server>.json`) and a run of small deltas under
/// [`delta_prefix`](Self::delta_prefix) (`media/<server>/<seq>.json`), each holding the entries
/// changed or removed since the save before it. [`load`](Self::load) reads the snapshot and
/// applies the deltas newer than it, in order. [`save_changes`](Self::save_changes) writes one
/// delta, or, once the deltas hold more entries than the index itself, a new snapshot (and then
/// deletes the deltas it folded in), so the full rewrites cost a constant share of the writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaIndex {
    /// Data files (names in the output directory) already read
    pub read_files: BTreeSet<String>,
    /// One entry per `MediaKey`
    pub entries: BTreeMap<String, MediaEntry>,
    /// Number of the newest delta folded into this index
    #[serde(default)]
    seq: u64,
    /// How many entries point at each stored file (rebuilt on load)
    #[serde(skip)]
    refs: HashMap<String, u64>,
    /// What changed since the last save
    #[serde(skip)]
    journal: Journal,
}

impl PartialEq for MediaIndex {
    fn eq(&self, other: &Self) -> bool {
        self.read_files == other.read_files && self.entries == other.entries
    }
}

impl Eq for MediaIndex {}

/// Changes not saved yet, and the deltas the next snapshot replaces
#[derive(Debug, Clone, Default)]
struct Journal {
    /// Keys changed or removed since the last save
    dirty: BTreeSet<String>,
    /// Data files read since the last save
    read: BTreeSet<String>,
    /// Delta keys a new snapshot makes obsolete
    deltas: Vec<String>,
    /// Entries written in deltas since the snapshot
    written: u64,
    /// The last save failed: write a whole snapshot next time
    full: bool,
}

/// One delta of a [`MediaIndex`]
#[derive(Debug, Default, Serialize, Deserialize)]
struct Delta {
    #[serde(default)]
    read_files: Vec<String>,
    #[serde(default)]
    entries: BTreeMap<String, MediaEntry>,
    #[serde(default)]
    removed: Vec<String>,
}

/// Deltas may hold this many entries before a snapshot is due, however small the index
const MIN_COMPACT: u64 = 10_000;

/// A planned write of the index (built under the lock, written outside it)
struct IndexWrite {
    key: String,
    bytes: Vec<u8>,
    /// A snapshot: these deltas are deleted once it is written
    replaces: Option<Vec<String>>,
}

impl MediaIndex {
    /// The state store key of `server`'s index snapshot: `media/<server>.json` (with a
    /// [`FileStateStore`](crate::FileStateStore), the file `<state>/media/<server>.json`)
    pub fn key(server: &str) -> String {
        format!("media/{}.json", encode_name(server))
    }

    /// The prefix of `server`'s delta keys: `media/<server>/`
    pub fn delta_prefix(server: &str) -> String {
        format!("media/{}/", encode_name(server))
    }

    /// True when the state store key `key` is part of `server`'s index (its snapshot or a delta)
    pub fn is_index_key(server: &str, key: &str) -> bool {
        key == Self::key(server) || delta_seq(key, &Self::delta_prefix(server)).is_some()
    }

    /// Load the index of `server`, or an empty one when there is none yet
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when a value is unreadable or not a valid index (a damaged index
    /// is an error, not a fresh start, so it never triggers a full re-download silently).
    pub async fn load(store: &dyn StateStore, server: &str) -> Result<Self, IngestError> {
        // A delta that vanishes between the listing and the read was folded into a newer
        // snapshot by another process: read again
        for _ in 0..3 {
            if let Some(index) = Self::try_load(store, server).await? {
                return Ok(index);
            }
        }
        Err(invalid(
            store,
            &Self::key(server),
            "the index kept changing while it was read".into(),
        ))
    }

    async fn try_load(store: &dyn StateStore, server: &str) -> Result<Option<Self>, IngestError> {
        let key = Self::key(server);
        let mut index: Self = match store.get(&key).await? {
            Some(v) => {
                serde_json::from_slice(&v.bytes).map_err(|e| invalid(store, &key, e.to_string()))?
            }
            None => Self::default(),
        };
        index.journal = Journal::default();
        let prefix = Self::delta_prefix(server);
        let mut deltas: Vec<(u64, String)> = store
            .list()
            .await?
            .into_iter()
            .filter_map(|k| delta_seq(&k, &prefix).map(|n| (n, k)))
            .collect();
        deltas.sort();
        for (seq, delta_key) in deltas {
            if seq > index.seq {
                let Some(v) = store.get(&delta_key).await? else {
                    return Ok(None);
                };
                let delta: Delta = serde_json::from_slice(&v.bytes)
                    .map_err(|e| invalid(store, &delta_key, e.to_string()))?;
                index.journal.written += (delta.entries.len() + delta.removed.len()) as u64;
                for k in &delta.removed {
                    index.entries.remove(k);
                }
                index.entries.extend(delta.entries);
                index.read_files.extend(delta.read_files);
                index.seq = seq;
            }
            index.journal.deltas.push(delta_key);
        }
        index.refs.clear();
        let names: Vec<String> = index
            .entries
            .values()
            .filter_map(|e| e.file.clone())
            .collect();
        for name in names {
            *index.refs.entry(name).or_default() += 1;
        }
        Ok(Some(index))
    }

    /// Save the index of `server` whole, as a new snapshot
    ///
    /// The deltas known to this index (read by [`load`](Self::load)) are deleted afterwards.
    /// [`save_changes`](Self::save_changes) is the cheap save for an index that changes often.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value cannot be written.
    pub async fn save(&self, store: &dyn StateStore, server: &str) -> Result<(), IngestError> {
        let key = Self::key(server);
        let text = serde_json::to_vec(self).map_err(|e| invalid(store, &key, e.to_string()))?;
        store.put(&key, &text).await?;
        delete_deltas(store, &self.journal.deltas).await;
        Ok(())
    }

    /// Save what changed since the last save: one delta with the changed entries, or a new
    /// snapshot once the deltas hold more entries than the index (see [`MediaIndex`])
    ///
    /// Nothing is written when nothing changed.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value cannot be written; the next save then writes a
    /// whole snapshot, so nothing is lost.
    pub async fn save_changes(
        &mut self,
        store: &dyn StateStore,
        server: &str,
    ) -> Result<(), IngestError> {
        let Some(write) = self.plan_save(store, server)? else {
            return Ok(());
        };
        match store.put(&write.key, &write.bytes).await {
            Err(e) => {
                self.save_failed(write.replaces);
                Err(e)
            }
            Ok(()) => {
                delete_deltas(store, write.replaces.as_deref().unwrap_or_default()).await;
                Ok(())
            }
        }
    }

    /// Build the next write and clear the journal
    fn plan_save(
        &mut self,
        store: &dyn StateStore,
        server: &str,
    ) -> Result<Option<IndexWrite>, IngestError> {
        let j = &mut self.journal;
        if !j.full && j.dirty.is_empty() && j.read.is_empty() {
            return Ok(None);
        }
        let snapshot = j.full
            || j.written + j.dirty.len() as u64 > (self.entries.len() as u64).max(MIN_COMPACT);
        self.seq += 1;
        let dirty = std::mem::take(&mut self.journal.dirty);
        let read = std::mem::take(&mut self.journal.read);
        if snapshot {
            let key = Self::key(server);
            let bytes =
                serde_json::to_vec(&*self).map_err(|e| invalid(store, &key, e.to_string()))?;
            let j = &mut self.journal;
            j.full = false;
            j.written = 0;
            return Ok(Some(IndexWrite {
                key,
                bytes,
                replaces: Some(std::mem::take(&mut j.deltas)),
            }));
        }
        let mut delta = Delta {
            read_files: read.into_iter().collect(),
            ..Delta::default()
        };
        for k in dirty {
            match self.entries.get(&k) {
                Some(e) => {
                    delta.entries.insert(k, e.clone());
                }
                None => delta.removed.push(k),
            }
        }
        let key = format!("{}{:012}.json", Self::delta_prefix(server), self.seq);
        let bytes = serde_json::to_vec(&delta).map_err(|e| invalid(store, &key, e.to_string()))?;
        let j = &mut self.journal;
        j.written += (delta.entries.len() + delta.removed.len()) as u64;
        j.deltas.push(key.clone());
        Ok(Some(IndexWrite {
            key,
            bytes,
            replaces: None,
        }))
    }

    /// A planned write failed: the next save writes a whole snapshot, which also replaces the
    /// deltas this one would have
    fn save_failed(&mut self, replaces: Option<Vec<String>>) {
        self.journal.full = true;
        self.journal.deltas.extend(replaces.into_iter().flatten());
    }

    /// Mark the data file `name` as read
    pub fn mark_read(&mut self, name: String) {
        if self.read_files.insert(name.clone()) {
            self.journal.read.insert(name);
        }
    }

    /// The entry of `key`, to change it (marks it changed for the next save)
    pub fn entry_mut(&mut self, key: &str) -> Option<&mut MediaEntry> {
        let entry = self.entries.get_mut(key)?;
        self.journal.dirty.insert(key.to_string());
        Some(entry)
    }

    /// How many entries point at the stored file `name`
    pub fn references(&self, name: &str) -> u64 {
        self.refs.get(name).copied().unwrap_or(0)
    }

    /// Point `key`'s entry at the stored file `name`; returns the file it pointed at before when
    /// no entry points at that one any more (the caller deletes it)
    fn link(&mut self, key: &str, name: &str) -> Option<String> {
        let entry = self.entry_mut(key)?;
        let old = entry.file.replace(name.to_string());
        *self.refs.entry(name.to_string()).or_default() += 1;
        match old {
            Some(old) => self.unlink(&old).then_some(old),
            None => None,
        }
    }

    /// One reference to `name` fewer; `true` when none is left
    fn unlink(&mut self, name: &str) -> bool {
        match self.refs.get_mut(name) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            _ => {
                self.refs.remove(name);
                true
            }
        }
    }

    /// Remove `key`'s entry; returns its file when no other entry points at it
    pub fn remove(&mut self, key: &str) -> Option<String> {
        let entry = self.entries.remove(key)?;
        self.journal.dirty.insert(key.to_string());
        let file = entry.file?;
        self.unlink(&file).then_some(file)
    }

    /// Take in one `Media` record; returns `false` when it has no `MediaKey`
    pub fn add_record(&mut self, record: &Value) -> bool {
        let Some(key) = key_string(&record[KEY_FIELD]) else {
            return false;
        };
        let url = record[URL_FIELD]
            .as_str()
            .filter(|u| !u.is_empty())
            .map(str::to_owned);
        let stamp = STAMP_FIELDS
            .iter()
            .find_map(|f| record[*f].as_str())
            .map(str::to_owned);
        let entry = self.entries.entry(key.clone()).or_default();
        let fresh = entry.url.is_none() && entry.stamp.is_none() && entry.file.is_none();
        let changed = fresh || entry.stamp != stamp || (stamp.is_none() && entry.url != url);
        let touched = changed || entry.url != url;
        entry.url = url;
        if changed {
            entry.stamp = stamp;
            entry.pending = true;
        }
        if touched {
            self.journal.dirty.insert(key);
        }
        true
    }

    /// Take in the media records nested in `parent` under `field` (an array, as `$expand=Media`
    /// returns it); the keys of the entries touched are pushed onto `keys`
    ///
    /// Returns how many nested records had no `MediaKey`.
    pub fn add_nested(&mut self, parent: &Value, field: &str, keys: &mut Vec<String>) -> u64 {
        let mut without_key = 0;
        for media in parent[field].as_array().into_iter().flatten() {
            match key_string(&media[KEY_FIELD]) {
                Some(key) if self.add_record(media) => keys.push(key),
                _ => without_key += 1,
            }
        }
        without_key
    }

    /// Entries still to download (pending, with a URL)
    fn pending(&self) -> u64 {
        self.entries
            .values()
            .filter(|e| e.pending && e.url.is_some())
            .count() as u64
    }
}

/// The nested-array field that holds media records when none is configured
pub const DEFAULT_NESTED_FIELD: &str = "Media";

/// What the producer and the workers of one server share
struct Shared {
    server: String,
    state: Arc<dyn StateStore>,
    store: Arc<dyn MediaStore>,
    index: Mutex<MediaIndex>,
    report: Mutex<MediaReport>,
    /// Keys in the queue and not yet taken by a worker, so a key seen on two pages is queued once
    queued: Mutex<HashSet<String>>,
    /// Saves run one at a time, so an older snapshot never lands after a newer one
    save_lock: tokio::sync::Mutex<()>,
    finished: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves plain data behind: carry on with it
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    /// Save what changed in the index since the last save (see [`MediaIndex::save_changes`])
    async fn save(&self) -> Result<(), IngestError> {
        let _serial = self.save_lock.lock().await;
        let state = self.state.as_ref();
        let Some(write) = lock(&self.index).plan_save(state, &self.server)? else {
            return Ok(());
        };
        match state.put(&write.key, &write.bytes).await {
            Err(e) => {
                lock(&self.index).save_failed(write.replaces);
                Err(e)
            }
            Ok(()) => {
                delete_deltas(state, write.replaces.as_deref().unwrap_or_default()).await;
                Ok(())
            }
        }
    }

    async fn delete_files(&self, names: Vec<String>) {
        for name in names {
            if let Err(e) = self.store.delete(&name).await {
                tracing::warn!(file = %self.store.location(&name), error = %e, "cannot delete a media file");
            }
        }
    }
}

/// Set up the media pipeline of one server: a [`PageMedia`] that takes in pages and queues their
/// files, and the [`MediaWorkers`] that download them
///
/// Run the workers next to the record sync (`tokio::join!`); they end once the `PageMedia` is
/// dropped and the queue is empty, or when `shutdown` is triggered (what is left stays pending).
///
/// # Errors
///
/// [`IngestError::Output`] when the index is unreadable or the HTTP client cannot be built.
pub async fn page_pipeline<'a>(
    client: &'a ResoClient,
    server: &str,
    download: DownloadConfig,
    options: PipelineOptions,
) -> Result<(PageMedia, MediaWorkers<'a>), IngestError> {
    let index = MediaIndex::load(options.state.as_ref(), server).await?;
    let (tx, workers) = workers(client, server, download, &options, index)?;
    Ok((
        PageMedia {
            nested_field: options
                .nested_field
                .unwrap_or_else(|| DEFAULT_NESTED_FIELD.to_string()),
            shared: Arc::clone(&workers.shared),
            tx,
        },
        workers,
    ))
}

/// Where the media pipeline of one server keeps its index and files, and how hard it works
#[derive(Debug, Clone)]
pub struct PipelineOptions {
    /// The state store holding the index
    pub state: Arc<dyn StateStore>,
    /// Where the files go
    pub store: Arc<dyn MediaStore>,
    /// Field of a record that holds its nested media records (default [`DEFAULT_NESTED_FIELD`])
    pub nested_field: Option<String>,
    /// Download workers (at least 1)
    pub workers: usize,
    /// Stops the workers between files
    pub shutdown: Shutdown,
}

fn workers<'a>(
    client: &'a ResoClient,
    server: &str,
    download: DownloadConfig,
    options: &PipelineOptions,
    index: MediaIndex,
) -> Result<(UnboundedSender<String>, MediaWorkers<'a>), IngestError> {
    let downloader = client
        .downloader(download)
        .map_err(|e| output_error(options.store.location(""), e))?;
    let (tx, rx) = unbounded_channel();
    let shared = Arc::new(Shared {
        server: server.to_string(),
        state: Arc::clone(&options.state),
        store: Arc::clone(&options.store),
        index: Mutex::new(index),
        report: Mutex::new(MediaReport {
            server: server.to_string(),
            ..MediaReport::default()
        }),
        queued: Mutex::new(HashSet::new()),
        save_lock: tokio::sync::Mutex::new(()),
        finished: AtomicU64::new(0),
    });
    Ok((
        tx,
        MediaWorkers {
            shared,
            rx: tokio::sync::Mutex::new(rx),
            downloader,
            workers: options.workers.max(1),
            shutdown: options.shutdown.clone(),
        },
    ))
}

/// Takes in the pages `sync --media` writes and queues their new and changed files for the
/// [`MediaWorkers`], while the page's URLs are fresh
///
/// For the top-level [`MEDIA_RESOURCE`] each record is a media record; for any other resource the
/// media records nested in each record (`$expand=Media`) are taken. Only files that are new or
/// changed (or failed earlier) *and* whose record is in this page are queued. [`page`](Self::page)
/// never waits for a download. A file that fails stays pending and is fetched again when a later
/// sync sees its record again.
pub struct PageMedia {
    nested_field: String,
    shared: Arc<Shared>,
    tx: UnboundedSender<String>,
}

impl PageMedia {
    /// Take in one written page of `resource`, queue its pending files and save the index
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the index cannot be saved.
    pub async fn page(&mut self, resource: &str, batch: &[Value]) -> Result<(), IngestError> {
        let mut keys = Vec::new();
        {
            let mut index = lock(&self.shared.index);
            let mut report = lock(&self.shared.report);
            for record in batch {
                if resource == MEDIA_RESOURCE {
                    match key_string(&record[KEY_FIELD]) {
                        Some(key) if index.add_record(record) => keys.push(key),
                        _ => report.records_without_key += 1,
                    }
                    report.records += 1;
                } else {
                    report.records_without_key +=
                        index.add_nested(record, &self.nested_field, &mut keys);
                }
            }
            keys.sort();
            keys.dedup();
            keys.retain(|key| match index.entries.get(key) {
                Some(e) if e.pending && e.url.is_none() => {
                    report.without_url += 1;
                    false
                }
                Some(e) => e.pending,
                None => false,
            });
            let mut queued = lock(&self.shared.queued);
            keys.retain(|key| queued.insert(key.clone()));
            report.queued += keys.len() as u64;
        }
        for key in keys {
            // The workers are gone only after a shutdown: the file stays pending
            let _ = self.tx.send(key);
        }
        self.shared.save().await
    }
}

/// The download workers of one server; see [`page_pipeline`] and [`sync_media`]
pub struct MediaWorkers<'a> {
    shared: Arc<Shared>,
    rx: tokio::sync::Mutex<UnboundedReceiver<String>>,
    downloader: Downloader<'a>,
    workers: usize,
    shutdown: Shutdown,
}

impl MediaWorkers<'_> {
    /// Download queued files until the queue is closed and empty (or a shutdown), then save the
    /// index and return the report
    pub async fn run(self) -> MediaReport {
        self.run_with(None).await
    }

    async fn run_with(self, progress: Option<(&(dyn Fn(String) + Sync), u64)>) -> MediaReport {
        let workers: Vec<_> = (0..self.workers)
            .map(|_| Box::pin(self.worker(progress)))
            .collect();
        join_all(workers).await;
        let save = self.shared.save().await;
        let mut report = lock(&self.shared.report).clone();
        report.bytes = self.downloader.stats().bytes;
        report.pending = lock(&self.shared.index).pending();
        if let Err(e) = save {
            report.error.get_or_insert(e.to_string());
        }
        report
    }

    async fn worker(&self, progress: Option<(&(dyn Fn(String) + Sync), u64)>) {
        loop {
            if self.shutdown.is_triggered() {
                return;
            }
            let Some(key) = self.rx.lock().await.recv().await else {
                return;
            };
            lock(&self.shared.queued).remove(&key);
            if self.shutdown.is_triggered() {
                return;
            }
            self.fetch(&key).await;
            let n = self.shared.finished.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some((progress, total)) = progress {
                if n == 1 || n.is_multiple_of(10) || n == total {
                    progress(format!(
                        "##progress step={n}/{total} label={}/media",
                        self.shared.server
                    ));
                }
            }
            if n.is_multiple_of(SAVE_EVERY) {
                if let Err(e) = self.shared.save().await {
                    tracing::warn!(error = %e, "cannot save the media index");
                }
            }
        }
    }

    /// Download one key's file and record the result
    async fn fetch(&self, key: &str) {
        let shared = &self.shared;
        let job = {
            let index = lock(&shared.index);
            match index.entries.get(key) {
                Some(e) if e.pending && e.url.is_some() => e.clone(),
                _ => return,
            }
        };
        let Some(url) = job.url.clone() else {
            return;
        };
        // Validators only make sense while the stored copy is still there
        let stored = match &job.file {
            Some(f) => shared.store.exists(f).await.unwrap_or(false),
            None => false,
        };
        let validators = match stored {
            true => Validators {
                etag: job.etag.clone(),
                last_modified: job.last_modified.clone(),
            },
            false => Validators::default(),
        };
        let mut target = HashingTarget {
            store: shared.store.as_ref(),
            upload: None,
            hasher: Sha256::new(),
            content_type: None,
            name: None,
            stored_new: false,
            error: None,
        };
        let outcome = self
            .downloader
            .download_into(&url, &validators, &mut target)
            .await;

        let mut orphans = Vec::new();
        {
            let mut index = lock(&shared.index);
            let mut report = lock(&shared.report);
            // The record changed (or was deleted) while this file was on its way: the newer job
            // decides, this one only cleans up after itself
            let current = index
                .entries
                .get(key)
                .is_some_and(|e| e.stamp == job.stamp && (job.stamp.is_some() || e.url == job.url));
            match outcome {
                Ok(DownloadOutcome::NotModified) => {
                    report.not_modified += 1;
                    if let Some(e) = current.then(|| index.entry_mut(key)).flatten() {
                        e.pending = false;
                        e.error = None;
                    }
                }
                Ok(DownloadOutcome::Saved(saved)) => {
                    let name = target.name.clone().unwrap_or_default();
                    if job.file.as_deref() == Some(name.as_str()) {
                        report.unchanged += 1;
                    } else if target.stored_new {
                        report.downloaded += 1;
                    } else {
                        report.shared += 1;
                    }
                    if current {
                        orphans.extend(index.link(key, &name));
                        if let Some(e) = index.entry_mut(key) {
                            e.sha256 = target.name.as_deref().map(hash_of).map(str::to_owned);
                            e.etag = saved.etag;
                            e.last_modified = saved.last_modified;
                            e.content_type = saved.content_type;
                            e.bytes = Some(saved.bytes);
                            e.pending = false;
                            e.error = None;
                        }
                    } else if target.stored_new && index.references(&name) == 0 {
                        orphans.push(name);
                    }
                }
                Err(e) => {
                    let error = target.error.take().unwrap_or_else(|| e.to_string());
                    if is_expired(&e) {
                        report.expired += 1;
                    }
                    report.failed.push(MediaFailure {
                        key: key.to_string(),
                        error: error.clone(),
                    });
                    if let Some(entry) = current.then(|| index.entry_mut(key)).flatten() {
                        entry.error = Some(error);
                    }
                }
            }
        }
        shared.delete_files(orphans).await;
    }
}

/// A signed or single-use URL that is no longer valid: 401, 403 or 410 from the file host
fn is_expired(e: &ResoError) -> bool {
    matches!(
        e,
        ResoError::Unauthorized { .. }
            | ResoError::Forbidden { .. }
            | ResoError::ODataError {
                status_code: 410,
                ..
            }
    )
}

/// Wait for every future (a minimal `join_all`, without the `futures` crate)
async fn join_all<F: std::future::Future<Output = ()> + Unpin>(mut futures: Vec<F>) {
    std::future::poll_fn(|cx| {
        futures.retain_mut(|f| std::pin::Pin::new(f).poll(cx).is_pending());
        if futures.is_empty() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

/// Streams a body into a [`MediaUpload`] while hashing it; names the file `<sha256>.<ext>`
struct HashingTarget<'s> {
    store: &'s dyn MediaStore,
    upload: Option<Box<dyn MediaUpload>>,
    hasher: Sha256,
    content_type: Option<String>,
    /// Set by `finish`
    name: Option<String>,
    /// `finish` stored a new file (not one already there)
    stored_new: bool,
    /// The store's error text, kept whole for the report
    error: Option<String>,
}

impl HashingTarget<'_> {
    fn fail(&mut self, e: IngestError) -> ResoError {
        let text = e.to_string();
        self.error = Some(text.clone());
        ResoError::Io(text)
    }
}

impl DownloadTarget for HashingTarget<'_> {
    async fn start(&mut self, file: &SavedFile) -> reso_client::Result<()> {
        if let Some(mut old) = self.upload.take() {
            let _ = old.abort().await;
        }
        self.hasher = Sha256::new();
        self.content_type = file.content_type.clone();
        self.upload = Some(self.store.upload());
        Ok(())
    }

    async fn write(&mut self, chunk: &[u8]) -> reso_client::Result<()> {
        self.hasher.update(chunk);
        let result = match self.upload.as_mut() {
            Some(up) => up.write(chunk).await,
            None => return Err(ResoError::Io("upload not started".into())),
        };
        result.map_err(|e| self.fail(e))
    }

    async fn finish(&mut self) -> reso_client::Result<()> {
        let hash = hex(&std::mem::take(&mut self.hasher).finalize());
        let name = match self.content_type.as_deref().and_then(extension_for) {
            Some(ext) => format!("{hash}.{ext}"),
            None => hash,
        };
        let result = match self.upload.as_mut() {
            Some(up) => up.finish(&name).await,
            None => return Err(ResoError::Io("upload not started".into())),
        };
        self.upload = None;
        let stored = result.map_err(|e| self.fail(e))?;
        self.stored_new = stored;
        self.name = Some(name);
        Ok(())
    }

    async fn abort(&mut self) {
        if let Some(mut up) = self.upload.take() {
            let _ = up.abort().await;
        }
    }
}

/// The hash part of a stored name (`<sha256>.<ext>`)
fn hash_of(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where `sync_media` reads and writes
#[derive(Debug, Clone)]
pub struct MediaOptions {
    /// The `sync` output directory: manifests and data files
    pub out: PathBuf,
    /// The state store; the index of each server is under `media/` (see [`MediaIndex::key`])
    pub state: Arc<dyn StateStore>,
    /// Files go in `<media_dir>/<server>/`
    pub media_dir: PathBuf,
    /// Most downloads per server in this run (`None` = all)
    pub limit: Option<u64>,
}

/// One file that failed
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaFailure {
    /// `MediaKey`
    pub key: String,
    /// Error text
    pub error: String,
}

/// What a media run did for one server
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MediaReport {
    /// Server name
    pub server: String,
    /// Data files read in this run
    pub files_read: u64,
    /// `Media` records read
    pub records: u64,
    /// Records without a `MediaKey` (ignored)
    pub records_without_key: u64,
    /// Data files listed in manifests but not read (not NDJSON)
    pub files_skipped: Vec<String>,
    /// Files handed to the download workers
    pub queued: u64,
    /// Files downloaded and stored as new files
    pub downloaded: u64,
    /// Files downloaded whose content was the one already stored for the key (nothing written)
    pub unchanged: u64,
    /// Files downloaded whose content another key's file already holds (stored once, shared)
    pub shared: u64,
    /// Files the server said were unchanged (304)
    pub not_modified: u64,
    /// Entries removed after a delete event
    pub deleted: u64,
    /// Entries with no `MediaURL` (nothing to download)
    pub without_url: u64,
    /// Files that failed; they stay pending for the next run
    pub failed: Vec<MediaFailure>,
    /// Failures whose URL was refused as expired (401, 403 or 410); also in `failed`
    pub expired: u64,
    /// Files still pending after this run (failures and those over `--limit`)
    pub pending: u64,
    /// Bytes downloaded
    pub bytes: u64,
    /// The error that stopped this server, if any
    pub error: Option<String>,
}

impl MediaReport {
    /// True when nothing failed
    pub fn is_clean(&self) -> bool {
        self.error.is_none() && self.failed.is_empty()
    }
}

/// Download the new and changed media files of `server` from earlier `sync` output
///
/// Reads the NDJSON data files of the `Media` resource that the run manifests (`manifest.*.json`)
/// in `options.out` list and that the [`MediaIndex`] has not seen, then downloads each pending
/// `MediaURL` with `workers` download workers (at least 1) into `<media_dir>/<server>/`, each file
/// named by the SHA-256 of its content (`<sha256>.<ext>`) and stored once however many keys show it.
///
/// Errors on single files are recorded in the report's `failed` list and the run goes on;
/// an error that stops the whole server (an unreadable manifest or index) is the report's
/// `error`. `progress` receives `##progress` lines.
pub async fn sync_media(
    client: &ResoClient,
    server: &str,
    download: DownloadConfig,
    workers: usize,
    options: &MediaOptions,
    progress: impl Fn(String) + Sync,
) -> MediaReport {
    let mut report = MediaReport {
        server: server.to_string(),
        ..MediaReport::default()
    };
    let job = Job {
        client,
        server,
        download,
        workers,
    };
    match run(job, options, &progress, &mut report).await {
        Ok(Some(done)) => done,
        Ok(None) => report,
        Err(e) => {
            report.error = Some(e.to_string());
            report
        }
    }
}

/// One server's `sync_media`
struct Job<'a> {
    client: &'a ResoClient,
    server: &'a str,
    download: DownloadConfig,
    workers: usize,
}

async fn run(
    job: Job<'_>,
    options: &MediaOptions,
    progress: &(dyn Fn(String) + Sync),
    report: &mut MediaReport,
) -> Result<Option<MediaReport>, IngestError> {
    let Job {
        client,
        server,
        download,
        workers: worker_count,
    } = job;
    let state = options.state.as_ref();
    let mut index = MediaIndex::load(state, server).await?;
    let store: Arc<dyn MediaStore> = Arc::new(crate::media_store::LocalMediaStore::new(
        options.media_dir.join(encode_name(server)),
    ));

    // 1. Read the Media data files not read before, oldest run first
    let mut orphans = Vec::new();
    for name in media_files(&options.out, server)? {
        if index.read_files.contains(&name) {
            continue;
        }
        if !name.ends_with(".ndjson") {
            report.files_skipped.push(name);
            continue;
        }
        read_data_file(
            &options.out.join(&name),
            server,
            &mut index,
            &mut orphans,
            report,
        )?;
        index.mark_read(name);
        report.files_read += 1;
    }
    index.save_changes(state, server).await?;
    for name in orphans {
        store.delete(&name).await?;
    }

    // 2. Download what is pending, with the worker pool
    let mut todo: Vec<String> = Vec::new();
    for (key, entry) in &index.entries {
        if !entry.pending {
            continue;
        }
        if entry.url.is_some() {
            todo.push(key.clone());
        } else {
            report.without_url += 1;
        }
    }
    let total = options
        .limit
        .map_or(todo.len(), |n| todo.len().min(n as usize));
    let pipeline = PipelineOptions {
        state: Arc::clone(&options.state),
        store,
        nested_field: None,
        workers: worker_count,
        shutdown: Shutdown::new(),
    };
    let (tx, workers) = workers(client, server, download, &pipeline, index)?;
    *lock(&workers.shared.report) = MediaReport {
        queued: total as u64,
        ..report.clone()
    };
    for key in todo.into_iter().take(total) {
        let _ = tx.send(key);
    }
    drop(tx);
    let done = workers.run_with(Some((progress, total as u64))).await;
    Ok(Some(done))
}

/// Apply one NDJSON data file to the index: records update entries, delete events remove
/// entries (files no entry points at any more go to `orphans`)
fn read_data_file(
    path: &Path,
    server: &str,
    index: &mut MediaIndex,
    orphans: &mut Vec<String>,
    report: &mut MediaReport,
) -> Result<(), IngestError> {
    let file = fs::File::open(path).map_err(|e| output_error(path, e.kind()))?;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| output_error(path, e.kind()))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|e| output_error(path, e))?;
        if value["source"].as_str() != Some(server)
            || value["resource"].as_str() != Some(MEDIA_RESOURCE)
        {
            continue;
        }
        if value["event"].as_str() == Some("delete") {
            if let Some(key) = key_string(&value["key"]) {
                if index.entries.contains_key(&key) {
                    orphans.extend(index.remove(&key));
                    report.deleted += 1;
                }
            }
            continue;
        }
        report.records += 1;
        if !index.add_record(&value["record"]) {
            report.records_without_key += 1;
        }
    }
    Ok(())
}

/// The `Media` data files of `server`, from every `manifest.*.json` in `out`, oldest run first
fn media_files(out: &Path, server: &str) -> Result<Vec<String>, IngestError> {
    let entries = match fs::read_dir(out) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(output_error(out, e.kind())),
    };
    let mut manifests: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("manifest.") && n.ends_with(".json"))
        })
        .collect();
    // Run ids start with the start time, so names sort by time
    manifests.sort();
    let mut files = Vec::new();
    for path in manifests {
        let text = fs::read_to_string(&path).map_err(|e| output_error(&path, e.kind()))?;
        let manifest: Value = serde_json::from_str(&text).map_err(|e| output_error(&path, e))?;
        for r in manifest["resources"].as_array().into_iter().flatten() {
            if r["server"].as_str() != Some(server)
                || r["resource"].as_str() != Some(MEDIA_RESOURCE)
            {
                continue;
            }
            for f in r["files"].as_array().into_iter().flatten() {
                if let Some(name) = f["name"].as_str() {
                    files.push(name.to_string());
                }
            }
        }
    }
    Ok(files)
}

/// The number of a delta key under `prefix` (`<prefix><seq>.json`)
fn delta_seq(key: &str, prefix: &str) -> Option<u64> {
    key.strip_prefix(prefix)?
        .strip_suffix(".json")
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))?
        .parse()
        .ok()
}

/// Delete index deltas a snapshot replaced; one left behind is ignored by the next load (it is not
/// newer than the snapshot) and deleted by the next snapshot
async fn delete_deltas(store: &dyn StateStore, keys: &[String]) {
    for key in keys {
        if let Err(e) = store.delete(key, None).await {
            tracing::warn!(key = %store.location(key), error = %e, "cannot delete a media index delta");
        }
    }
}

fn key_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A file name for `s`: letters, digits, `-` and `_` kept, every other byte as `%XX`
///
/// Distinct keys give distinct names, and no name can climb out of the directory.
pub fn encode_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// File extension for a `Content-Type`, when it is a common media type
fn extension_for(content_type: &str) -> Option<&'static str> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match mime.as_str() {
        "image/jpeg" | "image/jpg" | "image/pjpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/tiff" => "tif",
        "image/bmp" => "bmp",
        "image/heic" => "heic",
        "image/avif" => "avif",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "text/plain" => "txt",
        "application/rtf" | "text/rtf" => "rtf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        _ => return None,
    })
}

fn output_error(path: impl AsRef<Path>, e: impl std::fmt::Display) -> IngestError {
    IngestError::Output {
        path: path.as_ref().display().to_string(),
        message: e.to_string(),
    }
}

/// The record of one `reso-ingest media` run, written as `media.{run_id}.json`
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaManifest {
    /// Run id
    pub run_id: String,
    /// When the run started (RFC 3339, UTC)
    pub started_at: String,
    /// Set by [`write`](Self::write)
    pub finished_at: Option<String>,
    /// One report per server
    pub servers: Vec<MediaReport>,
}

impl MediaManifest {
    /// Start a manifest for `run_id`
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            started_at: now_rfc3339(),
            finished_at: None,
            servers: Vec::new(),
        }
    }

    /// True when no server or file failed
    pub fn is_clean(&self) -> bool {
        self.servers.iter().all(MediaReport::is_clean)
    }

    /// Write `media.{run_id}.json` into `dir` and return its path
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be written.
    pub fn write(&mut self, dir: &Path) -> Result<PathBuf, IngestError> {
        self.finished_at = Some(now_rfc3339());
        let path = dir.join(format!("media.{}.json", encode_name(&self.run_id)));
        fs::create_dir_all(dir).map_err(|e| output_error(dir, e.kind()))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| output_error(&path, e))?;
        fs::write(&path, text + "\n").map_err(|e| output_error(&path, e.kind()))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encode_name_is_safe_and_distinct() {
        assert_eq!(encode_name("abc-1_2"), "abc-1_2");
        assert_eq!(encode_name("../x"), "%2E%2E%2Fx");
        assert_ne!(encode_name("a.jpg"), encode_name("a_jpg"));
        assert_eq!(encode_name("é"), "%C3%A9");
    }

    #[test]
    fn extension_from_content_type() {
        assert_eq!(extension_for("image/JPEG; charset=binary"), Some("jpg"));
        assert_eq!(extension_for("application/octet-stream"), None);
        for (mime, ext) in [
            ("image/pjpeg", "jpg"),
            ("image/png", "png"),
            ("image/gif", "gif"),
            ("image/webp", "webp"),
            ("image/tiff", "tif"),
            ("image/bmp", "bmp"),
            ("image/heic", "heic"),
            ("image/avif", "avif"),
            ("image/svg+xml", "svg"),
            ("application/pdf", "pdf"),
            ("video/mp4", "mp4"),
            ("video/quicktime", "mov"),
            ("video/webm", "webm"),
            ("audio/mpeg", "mp3"),
            ("text/plain; charset=utf-8", "txt"),
            ("text/rtf", "rtf"),
            ("application/rtf", "rtf"),
            ("application/msword", "doc"),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "docx",
            ),
        ] {
            assert_eq!(extension_for(mime), Some(ext), "{mime}");
        }
        assert_eq!(extension_for(""), None);
    }

    fn record(key: &str, url: &str, stamp: Option<&str>) -> Value {
        json!({"MediaKey": key, "MediaURL": url, "MediaModificationTimestamp": stamp})
    }

    #[test]
    fn new_record_is_pending() {
        let mut index = MediaIndex::default();
        assert!(index.add_record(&record("k", "https://x/1", Some("2026-01-01T00:00:00Z"))));
        assert!(index.entries["k"].pending);
    }

    #[test]
    fn url_change_alone_is_not_a_change_but_is_kept() {
        let mut index = MediaIndex::default();
        index.add_record(&record(
            "k",
            "https://x/1?sig=a",
            Some("2026-01-01T00:00:00Z"),
        ));
        index.entries.get_mut("k").unwrap().pending = false;
        index.add_record(&record(
            "k",
            "https://x/1?sig=b",
            Some("2026-01-01T00:00:00Z"),
        ));
        let e = &index.entries["k"];
        assert!(!e.pending);
        assert_eq!(e.url.as_deref(), Some("https://x/1?sig=b"));
    }

    #[test]
    fn timestamp_change_is_a_change() {
        let mut index = MediaIndex::default();
        index.add_record(&record("k", "https://x/1", Some("2026-01-01T00:00:00Z")));
        index.entries.get_mut("k").unwrap().pending = false;
        index.add_record(&record("k", "https://x/1", Some("2026-02-01T00:00:00Z")));
        assert!(index.entries["k"].pending);
    }

    #[test]
    fn falls_back_to_modification_timestamp_then_url() {
        let mut index = MediaIndex::default();
        let r = json!({"MediaKey": 7, "MediaURL": "u1", "ModificationTimestamp": "t1"});
        index.add_record(&r);
        assert_eq!(index.entries["7"].stamp.as_deref(), Some("t1"));

        index.add_record(&json!({"MediaKey": "n", "MediaURL": "u1"}));
        index.entries.get_mut("n").unwrap().pending = false;
        index.add_record(&json!({"MediaKey": "n", "MediaURL": "u1"}));
        assert!(!index.entries["n"].pending);
        index.add_record(&json!({"MediaKey": "n", "MediaURL": "u2"}));
        assert!(index.entries["n"].pending);
    }

    #[test]
    fn nested_media_use_their_own_keys_and_count_keyless_ones() {
        let mut index = MediaIndex::default();
        let parent = json!({"ListingKey": "L", "Media": [
            {"MediaKey": "m1", "MediaURL": "u1"}, {"MediaURL": "u2"}, {"MediaKey": "m2"}]});
        let mut keys = Vec::new();
        assert_eq!(index.add_nested(&parent, "Media", &mut keys), 1);
        assert_eq!(keys, ["m1", "m2"]);
        assert_eq!(index.add_nested(&parent, "Photos", &mut keys), 0);
    }

    #[test]
    fn record_without_key_is_refused() {
        let mut index = MediaIndex::default();
        assert!(!index.add_record(&json!({"MediaURL": "u"})));
        assert!(index.entries.is_empty());
    }

    fn state_dir(name: &str) -> (PathBuf, crate::FileStateStore) {
        let dir = std::env::temp_dir().join(format!("media-index-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (dir.clone(), crate::FileStateStore::new(dir))
    }

    async fn keys(store: &dyn StateStore) -> Vec<String> {
        let mut k = store.list().await.unwrap();
        k.sort();
        k
    }

    #[tokio::test]
    async fn saves_write_only_the_changed_entries() {
        let (dir, store) = state_dir("delta");
        let mut index = MediaIndex::default();
        for i in 0..100 {
            index.add_record(&record(&format!("k{i:03}"), "u", Some("t1")));
        }
        index.mark_read("a.ndjson".into());
        index.save_changes(&store, "mls").await.unwrap();
        // Nothing changed: nothing written
        index.save_changes(&store, "mls").await.unwrap();
        assert_eq!(keys(&store).await, ["media/mls/000000000001.json"]);

        // One new entry, one changed, one removed, one URL-only change, one unchanged record
        index.add_record(&record("new", "u", Some("t1")));
        index.add_record(&record("k001", "u", Some("t2")));
        index.add_record(&record("k002", "u2", Some("t1")));
        index.add_record(&record("k003", "u", Some("t1")));
        index.remove("k004");
        index.save_changes(&store, "mls").await.unwrap();
        let delta: Delta = serde_json::from_slice(
            &store
                .get("media/mls/000000000002.json")
                .await
                .unwrap()
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert_eq!(
            delta.entries.keys().collect::<Vec<_>>(),
            ["k001", "k002", "new"]
        );
        assert_eq!(delta.removed, ["k004"]);
        assert!(delta.read_files.is_empty());

        let loaded = MediaIndex::load(&store, "mls").await.unwrap();
        assert_eq!(loaded, index);
        assert_eq!(loaded.seq, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_snapshot_replaces_the_deltas_once_they_outgrow_the_index() {
        let (dir, store) = state_dir("compact");
        let mut index = MediaIndex::default();
        let mut snapshots = 0;
        // 25 saves of 1000 changed entries over 5000 keys: deltas reach 10 000 entries (the
        // minimum) after 10 saves, so a snapshot is due every 11th save
        for round in 0..25u32 {
            for i in 0..1000 {
                let key = format!("k{}", (round * 1000 + i) % 5000);
                index.add_record(&record(&key, "u", Some(&format!("t{round}"))));
            }
            index.save_changes(&store, "mls").await.unwrap();
            if store.get(&MediaIndex::key("mls")).await.unwrap().is_some() && snapshots == 0 {
                snapshots = round + 1;
            }
        }
        assert_eq!(snapshots, 11);
        let k = keys(&store).await;
        // Two snapshots so far (saves 11 and 22); the three saves after the last are deltas
        assert_eq!(k.len(), 4, "{k:?}");
        assert_eq!(k[0], "media/mls.json");
        assert_eq!(k[1], "media/mls/000000000023.json");
        let loaded = MediaIndex::load(&store, "mls").await.unwrap();
        assert_eq!(loaded, index);
        assert_eq!(loaded.entries.len(), 5000);
        assert_eq!(loaded.entries["k4999"].stamp.as_deref(), Some("t24"));

        // A delta a snapshot already holds (left behind by a crash) is ignored, then deleted
        store
            .put("media/mls/000000000005.json", br#"{"removed":["k1"]}"#)
            .await
            .unwrap();
        let loaded = MediaIndex::load(&store, "mls").await.unwrap();
        assert!(loaded.entries.contains_key("k1"));
        loaded.save(&store, "mls").await.unwrap();
        assert_eq!(keys(&store).await, ["media/mls.json"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failed_save_is_made_good_by_the_next_one() {
        let (dir, store) = state_dir("fail");
        let mut index = MediaIndex::default();
        index.add_record(&record("a", "u", Some("t1")));
        index.save_changes(&store, "mls").await.unwrap();
        index.add_record(&record("b", "u", Some("t1")));
        // A file where the delta directory should be makes the write fail
        fs::remove_dir_all(dir.join("media/mls")).unwrap();
        fs::write(dir.join("media/mls"), b"x").unwrap();
        assert!(index.save_changes(&store, "mls").await.is_err());
        fs::remove_file(dir.join("media/mls")).unwrap();
        index.add_record(&record("c", "u", Some("t1")));
        index.save_changes(&store, "mls").await.unwrap();
        assert_eq!(keys(&store).await, ["media/mls.json"]);
        let loaded = MediaIndex::load(&store, "mls").await.unwrap();
        assert_eq!(loaded.entries.len(), 3);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn index_keys_are_the_snapshot_and_numbered_deltas() {
        assert!(MediaIndex::is_index_key("a", "media/a.json"));
        assert!(MediaIndex::is_index_key("a", "media/a/000000000007.json"));
        assert!(!MediaIndex::is_index_key("a", "media/ab.json"));
        assert!(!MediaIndex::is_index_key("a", "media/ab/000000000007.json"));
        assert!(!MediaIndex::is_index_key("a", "media/a/x.json"));
        assert!(!MediaIndex::is_index_key("a", "a.Media.json"));
    }
}
