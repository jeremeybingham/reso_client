//! Media file download: the `Media` records `sync` wrote, turned into files on disk
//!
//! [`sync_media`] reads the NDJSON data files of the `Media` resource that the run manifests in
//! the output directory list, keeps the newest record per `MediaKey`, and downloads each new or
//! changed `MediaURL` with a [`Downloader`] (its own retry, rate limit and token rule). A
//! per-server [`MediaIndex`] (state store key `media/<server>.json`) records which data files were read
//! and what each file was saved from, so a run only downloads what changed and picks up failed
//! files again.
//!
//! A file is downloaded again when the record's `MediaModificationTimestamp` (or, when absent,
//! `ModificationTimestamp`) changes. A changed URL alone does not count, because signed CDN URLs
//! change on every pull; the index keeps the newest URL for the next download. A record with no
//! timestamp at all is downloaded again when its URL changes. Re-downloads send the saved
//! `ETag`/`Last-Modified`, so an unchanged file costs a 304. A delete event for a `MediaKey`
//! removes its file and index entry.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reso_client::{DownloadConfig, DownloadOutcome, ResoClient, Validators};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::IngestError;
use crate::output::now_rfc3339;
use crate::state::{invalid, StateStore};

/// The resource whose records point at files
pub const MEDIA_RESOURCE: &str = "Media";
const KEY_FIELD: &str = "MediaKey";
const URL_FIELD: &str = "MediaURL";
const STAMP_FIELDS: [&str; 2] = ["MediaModificationTimestamp", "ModificationTimestamp"];
/// Save the index after this many downloads, so a crash loses little
const SAVE_EVERY: u64 = 50;

/// What is known about one `MediaKey`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaEntry {
    /// Newest `MediaURL` seen
    pub url: Option<String>,
    /// Timestamp of the newest record (`MediaModificationTimestamp`, else `ModificationTimestamp`)
    pub stamp: Option<String>,
    /// The file still has to be downloaded (new, changed, or the last attempt failed)
    pub pending: bool,
    /// File name in the server's media directory, once saved
    pub file: Option<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaIndex {
    /// Data files (names in the output directory) already read
    pub read_files: BTreeSet<String>,
    /// One entry per `MediaKey`
    pub entries: BTreeMap<String, MediaEntry>,
}

impl MediaIndex {
    /// The state store key of `server`'s index: `media/<server>.json` (with a
    /// [`FileStateStore`](crate::FileStateStore), the file `<state>/media/<server>.json`)
    pub fn key(server: &str) -> String {
        format!("media/{}.json", encode_name(server))
    }

    /// Load the index of `server`, or an empty one when there is none yet
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value is unreadable or not a valid index (a damaged index
    /// is an error, not a fresh start, so it never triggers a full re-download silently).
    pub async fn load(store: &dyn StateStore, server: &str) -> Result<Self, IngestError> {
        let key = Self::key(server);
        match store.get(&key).await? {
            Some(v) => {
                serde_json::from_slice(&v.bytes).map_err(|e| invalid(store, &key, e.to_string()))
            }
            None => Ok(Self::default()),
        }
    }

    /// Save the index of `server`, replacing the old one whole
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value cannot be written.
    pub async fn save(&self, store: &dyn StateStore, server: &str) -> Result<(), IngestError> {
        let key = Self::key(server);
        let text = serde_json::to_string(self).map_err(|e| invalid(store, &key, e.to_string()))?;
        store.put(&key, text.as_bytes()).await
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
        let entry = self.entries.entry(key).or_default();
        let fresh = entry.url.is_none() && entry.stamp.is_none() && entry.file.is_none();
        let changed = fresh || entry.stamp != stamp || (stamp.is_none() && entry.url != url);
        entry.url = url;
        if changed {
            entry.stamp = stamp;
            entry.pending = true;
        }
        true
    }
}

impl MediaIndex {
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
}

/// The nested-array field that holds media records when none is configured
pub const DEFAULT_NESTED_FIELD: &str = "Media";

/// Downloads the media files of each page `sync --media` writes, while the page's URLs are fresh
///
/// For the top-level [`MEDIA_RESOURCE`] each record is a media record; for any other resource the
/// media records nested in each record (`$expand=Media`) are taken. Only files that are new or
/// changed (or failed earlier) *and* whose record is in this page are downloaded, so a signed
/// URL is never older than one page. A file that fails stays pending and is fetched again when
/// a later sync sees its record again.
pub struct PageMedia<'a> {
    nested_field: String,
    dir: PathBuf,
    store: Arc<dyn StateStore>,
    server: String,
    index: MediaIndex,
    downloader: reso_client::Downloader<'a>,
    report: MediaReport,
}

impl<'a> PageMedia<'a> {
    /// Open the index of `server` and make a downloader
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the index is unreadable or the HTTP client cannot be built.
    pub async fn new(
        client: &'a ResoClient,
        server: &str,
        download: DownloadConfig,
        state: Arc<dyn StateStore>,
        media_dir: &Path,
        nested_field: Option<&str>,
    ) -> Result<Self, IngestError> {
        let dir = media_dir.join(encode_name(server));
        let index = MediaIndex::load(state.as_ref(), server).await?;
        let downloader = client
            .downloader(download)
            .map_err(|e| output_error(&dir, e))?;
        Ok(Self {
            nested_field: nested_field.unwrap_or(DEFAULT_NESTED_FIELD).to_string(),
            dir,
            store: state,
            server: server.to_string(),
            index,
            downloader,
            report: MediaReport {
                server: server.to_string(),
                ..MediaReport::default()
            },
        })
    }

    /// Take in one written page of `resource` and download its pending files
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the index cannot be saved.
    pub async fn page(&mut self, resource: &str, batch: &[Value]) -> Result<(), IngestError> {
        let mut keys = Vec::new();
        for record in batch {
            if resource == MEDIA_RESOURCE {
                match key_string(&record[KEY_FIELD]) {
                    Some(key) if self.index.add_record(record) => keys.push(key),
                    _ => self.report.records_without_key += 1,
                }
                self.report.records += 1;
            } else {
                self.report.records_without_key +=
                    self.index.add_nested(record, &self.nested_field, &mut keys);
            }
        }
        keys.sort();
        keys.dedup();
        for key in keys {
            let Some(entry) = self.index.entries.get_mut(&key) else {
                continue;
            };
            if !entry.pending {
                continue;
            }
            if entry.url.is_none() {
                self.report.without_url += 1;
                continue;
            }
            download_one(&self.downloader, &self.dir, &key, entry, &mut self.report).await;
        }
        self.index.save(self.store.as_ref(), &self.server).await
    }

    /// Final report: files still pending (their records were not seen again) and bytes
    pub fn finish(mut self) -> MediaReport {
        self.report.bytes = self.downloader.stats().bytes;
        self.report.pending = self
            .index
            .entries
            .values()
            .filter(|e| e.pending && e.url.is_some())
            .count() as u64;
        self.report
    }
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

/// What `sync_media` did for one server
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
    /// Files downloaded and saved
    pub downloaded: u64,
    /// Files the server said were unchanged (304)
    pub not_modified: u64,
    /// Files removed after a delete event
    pub deleted: u64,
    /// Entries with no `MediaURL` (nothing to download)
    pub without_url: u64,
    /// Files that failed; they stay pending for the next run
    pub failed: Vec<MediaFailure>,
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

/// Download the new and changed media files of `server`
///
/// Reads the NDJSON data files of the `Media` resource that the run manifests (`manifest.*.json`)
/// in `options.out` list and that the [`MediaIndex`] has not seen, then downloads each pending
/// `MediaURL` into `<media_dir>/<server>/`, named by [`encode_name`] of the `MediaKey` plus an
/// extension from `Content-Type`.
///
/// A file is downloaded again when the record's `MediaModificationTimestamp` (or, when absent,
/// `ModificationTimestamp`) changes. A changed URL alone does not count, because signed CDN URLs
/// change on every pull; the index keeps the newest URL for the next download. A record with no
/// timestamp at all is downloaded again when its URL changes. Re-downloads send the saved
/// `ETag`/`Last-Modified`, so an unchanged file costs a 304. A delete event for a `MediaKey`
/// removes its file and index entry.
///
/// Errors on single files are recorded in the report's `failed` list and the run goes on;
/// an error that stops the whole server (an unreadable manifest or index) is the report's
/// `error`. `progress` receives `##progress` lines.
pub async fn sync_media(
    client: &ResoClient,
    server: &str,
    download: DownloadConfig,
    options: &MediaOptions,
    progress: impl Fn(String),
) -> MediaReport {
    let mut report = MediaReport {
        server: server.to_string(),
        ..MediaReport::default()
    };
    if let Err(e) = run(client, server, download, options, &progress, &mut report).await {
        report.error = Some(e.to_string());
    }
    report
}

async fn run(
    client: &ResoClient,
    server: &str,
    download: DownloadConfig,
    options: &MediaOptions,
    progress: &impl Fn(String),
    report: &mut MediaReport,
) -> Result<(), IngestError> {
    let store = options.state.as_ref();
    let mut index = MediaIndex::load(store, server).await?;
    let dir = options.media_dir.join(encode_name(server));

    // 1. Read the Media data files not read before, oldest run first
    for name in media_files(&options.out, server)? {
        if index.read_files.contains(&name) {
            continue;
        }
        if !name.ends_with(".ndjson") {
            report.files_skipped.push(name);
            continue;
        }
        read_data_file(&options.out.join(&name), server, &mut index, &dir, report)?;
        index.read_files.insert(name);
        report.files_read += 1;
    }
    index.save(store, server).await?;

    // 2. Download what is pending
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
    let downloader = client
        .downloader(download)
        .map_err(|e| output_error(&dir, e))?;
    for (i, key) in todo.iter().take(total).enumerate() {
        if i.is_multiple_of(10) {
            progress(format!(
                "##progress step={}/{total} label={server}/media",
                i + 1
            ));
        }
        let Some(entry) = index.entries.get_mut(key) else {
            continue;
        };
        download_one(&downloader, &dir, key, entry, report).await;
        if (i as u64 + 1).is_multiple_of(SAVE_EVERY) {
            index.save(store, server).await?;
        }
    }
    report.bytes = downloader.stats().bytes;
    report.pending = index
        .entries
        .values()
        .filter(|e| e.pending && e.url.is_some())
        .count() as u64;
    index.save(store, server).await
}

async fn download_one(
    downloader: &reso_client::Downloader<'_>,
    dir: &Path,
    key: &str,
    entry: &mut MediaEntry,
    report: &mut MediaReport,
) {
    let Some(url) = entry.url.clone() else {
        return;
    };
    let base = encode_name(key);
    let old = entry.file.as_ref().map(|f| dir.join(f));
    // Validators only make sense while the saved copy is still on disk
    let validators = match &old {
        Some(path) if path.exists() => Validators {
            etag: entry.etag.clone(),
            last_modified: entry.last_modified.clone(),
        },
        _ => Validators::default(),
    };
    let fail = |entry: &mut MediaEntry, report: &mut MediaReport, error: String| {
        entry.error = Some(error.clone());
        report.failed.push(MediaFailure {
            key: key.to_string(),
            error,
        });
    };

    match downloader
        .download_to(&url, &dir.join(&base), &validators)
        .await
    {
        Ok(DownloadOutcome::NotModified) => {
            entry.pending = false;
            entry.error = None;
            report.not_modified += 1;
        }
        Ok(DownloadOutcome::Saved(saved)) => {
            let name = match saved.content_type.as_deref().and_then(extension_for) {
                Some(ext) => format!("{base}.{ext}"),
                None => base.clone(),
            };
            if name != base {
                if let Err(e) = fs::rename(dir.join(&base), dir.join(&name)) {
                    fail(entry, report, format!("cannot rename: {}", e.kind()));
                    return;
                }
            }
            if let Some(old) = old.filter(|p| *p != dir.join(&name)) {
                let _ = fs::remove_file(old);
            }
            entry.file = Some(name);
            entry.etag = saved.etag;
            entry.last_modified = saved.last_modified;
            entry.content_type = saved.content_type;
            entry.bytes = Some(saved.bytes);
            entry.pending = false;
            entry.error = None;
            report.downloaded += 1;
        }
        Err(e) => fail(entry, report, e.to_string()),
    }
}

/// Apply one NDJSON data file to the index: records update entries, delete events remove files
fn read_data_file(
    path: &Path,
    server: &str,
    index: &mut MediaIndex,
    dir: &Path,
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
                if let Some(entry) = index.entries.remove(&key) {
                    if let Some(f) = entry.file {
                        let _ = fs::remove_file(dir.join(f));
                    }
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

fn output_error(path: &Path, e: impl std::fmt::Display) -> IngestError {
    IngestError::Output {
        path: path.display().to_string(),
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
}
