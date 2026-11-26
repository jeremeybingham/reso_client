//! Source purge: delete what `reso-ingest` wrote for one server when its licence ends (#93)
//!
//! [`plan_purge`] lists what belongs to a server, without changing anything:
//!
//! - **Data files** in the output directory whose `source` tag is the server (NDJSON files, read
//!   line by line, including `.part` files a killed run left), and the files the run manifests list
//!   under the server (this covers Parquet files). Ownership comes from the source tag, never from a
//!   file name; a file whose name starts like the server's but that neither check claims is listed
//!   as not purged.
//! - With the data files in a bucket (a [`FileSink`], #139), the run manifests in the bucket name
//!   the server's files, since reading every object for its tag would be slow and costly. Only the
//!   NDJSON objects named like the server's that no manifest lists (a run killed before its
//!   manifest was delivered) are downloaded and judged by their tag.
//! - **Media files**: the server's directory under the media directory (`<media_dir>/<server>/`).
//! - **State keys**: the server's checkpoints, key sets, event sequences, schema snapshot and lease
//!   keys (`<server>.*`) and its media index (`media/<server>.json`).
//!
//! [`execute_purge`] refuses while another process holds a lease on one of the server's feeds,
//! takes those leases itself so no sync of the server starts meanwhile, deletes, and then writes a
//! **purge record** (`<out>/audit/purge.<id>.json`, with its SHA-256 in `purge.<id>.json.sha256`)
//! and one line in the access log (status `purged`, the record's name and hash). The record is
//! tamper-evident through the hash kept in the append-only access log; it is not signed. With a
//! bucket, the record, its `.sha256` and the access line (`access.<id>.ndjson`) are also put in
//! the bucket next to the data files, as `sync` does with its access lines.
//!
//! Kept on purpose: the access log (the proof of what was pulled), run manifests and media
//! manifests (run records that list file names and counts, not records, and cover other servers).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::audit::{AccessEntry, AccessLog, PurgeNote};
use crate::checkpoint::sanitize;
use crate::error::IngestError;
use crate::lease::{live_holder, Lease, LeaseHolder, LeaseOutcome};
use crate::media::{encode_name, MediaIndex};
use crate::output::{new_run_id, now_rfc3339, output_error};
use crate::sink::{FileSink, LocalDirSink};
use crate::state::StateStore;

/// The directory, inside the output directory, that holds the access log and purge records
pub const AUDIT_DIR: &str = "audit";

/// How long purge's own leases last without renewal (the same as a sync's)
const LEASE_TTL: Duration = Duration::from_secs(300);

/// One data file of the server
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgedFile {
    /// File name in the output directory
    pub name: String,
    /// Lines in the file (records and delete events); from the manifest for files that are not NDJSON
    pub records: u64,
    /// Size in bytes
    pub bytes: u64,
}

/// What belongs to one server, found by [`plan_purge`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgePlan {
    /// Server name
    pub server: String,
    /// Data files in the output directory
    pub files: Vec<PurgedFile>,
    /// The server's media directory
    pub media_dir: PathBuf,
    /// Files in the media directory
    pub media_files: u64,
    /// Bytes in the media directory
    pub media_bytes: u64,
    /// State keys of the server
    pub state_keys: Vec<String>,
    /// Files whose name starts with the server's but that no source tag or manifest assigns to it;
    /// they are left alone
    pub not_purged: Vec<String>,
    /// Lease keys another process holds, and by whom; purge refuses while any is held
    pub held: Vec<(String, LeaseHolder)>,
    /// Resources with state keys (the feeds purge leases)
    resources: BTreeSet<String>,
}

impl PurgePlan {
    /// Records (lines) in the data files
    pub fn records(&self) -> u64 {
        self.files.iter().map(|f| f.records).sum()
    }

    /// Bytes in the data files
    pub fn file_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }
}

/// The purge record: what was deleted, when and by whom
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeRecord {
    /// Purge id, also in the record's file name
    pub id: String,
    /// Server name
    pub server: String,
    /// Who ran the purge (`--by`)
    pub by: String,
    /// When deleting started (RFC 3339, UTC)
    pub started_at: String,
    /// When deleting ended
    pub finished_at: String,
    /// `purged`, or `failed` when a deletion failed (what is listed was deleted)
    pub status: String,
    /// The error that stopped the purge
    pub error: Option<String>,
    /// Data files deleted
    pub files: Vec<PurgedFile>,
    /// Records (lines) in the deleted data files
    pub records: u64,
    /// The media directory deleted
    pub media_dir: String,
    /// Media files deleted
    pub media_files: u64,
    /// Bytes of media deleted
    pub media_bytes: u64,
    /// State keys deleted
    pub state_keys: Vec<String>,
    /// Files left alone (see [`PurgePlan::not_purged`])
    pub not_purged: Vec<String>,
}

/// The result of [`execute_purge`]
#[derive(Debug)]
pub enum PurgeOutcome {
    /// The purge ran; `record.status` says whether every deletion succeeded
    Done {
        /// The record written
        record: Box<PurgeRecord>,
        /// Where it was written
        path: PathBuf,
        /// SHA-256 of the record file, as hex
        sha256: String,
        /// Where copies of the record, its checksum and the access line went (a bucket)
        delivered: Vec<String>,
        /// Why a copy to the bucket failed; the local record and access line are written
        delivery_error: Option<String>,
    },
    /// Another process holds a lease on one of the server's feeds; nothing was deleted
    Held(Vec<(String, LeaseHolder)>),
}

/// List what belongs to `server`, changing nothing
///
/// The data files are in `files` (a bucket), or in `out` when it is `None`. With a bucket, `out`
/// is the local directory where objects are downloaded to be read.
///
/// # Errors
///
/// [`IngestError::Output`] when the output directory, a data file, a manifest or the state store
/// cannot be read, or a data file mixes this server's lines with another's.
pub async fn plan_purge(
    server: &str,
    out: &Path,
    files: Option<&dyn FileSink>,
    media_dir: &Path,
    state: &dyn StateStore,
) -> Result<PurgePlan, IngestError> {
    let local = LocalDirSink::existing(out);
    let files = files.unwrap_or(&local);
    plan(server, out, files, media_dir, state, &BTreeSet::new()).await
}

async fn plan(
    server: &str,
    out: &Path,
    sink: &dyn FileSink,
    media_dir: &Path,
    state: &dyn StateStore,
    own_leases: &BTreeSet<String>,
) -> Result<PurgePlan, IngestError> {
    let (files, not_purged) = data_files(server, sink, out)?;
    let media_dir = media_dir.join(encode_name(server));
    let (media_files, media_bytes) = dir_size(&media_dir)?;

    let prefix = format!("{}.", sanitize(server));
    let media_key = MediaIndex::key(server);
    let mut state_keys = Vec::new();
    let mut resources = BTreeSet::new();
    let mut held = Vec::new();
    let mut keys = state.list().await?;
    keys.sort();
    for key in keys {
        if key == media_key {
            state_keys.push(key);
            continue;
        }
        if key.contains('/') || !key.starts_with(&prefix) {
            continue;
        }
        if own_leases.contains(&key) {
            continue;
        }
        if let Some(resource) = key.split('.').nth(1).filter(|r| !r.starts_with('@')) {
            resources.insert(resource.to_string());
        }
        if key.ends_with(".lock") {
            if let Some(holder) = live_holder(state, &key).await? {
                held.push((key.clone(), holder));
            }
        }
        state_keys.push(key);
    }
    Ok(PurgePlan {
        server: server.to_string(),
        files,
        media_dir,
        media_files,
        media_bytes,
        state_keys,
        not_purged,
        held,
        resources,
    })
}

/// Delete what belongs to `server` and write the purge record and its access-log line
///
/// The data files are in `files` (a bucket), or in `out` when it is `None`; the record and the
/// access log are always written to `<out>/audit/`, and with a bucket copied to it as well. `by`
/// names who ran the purge. Leases on the server's feeds are taken for the duration, so a
/// sync of the server waits (it skips held feeds); when another process holds one, nothing is
/// deleted and [`PurgeOutcome::Held`] says who.
///
/// # Errors
///
/// [`IngestError::Output`] when planning fails, or the record or its access-log line cannot be
/// written. A failed deletion is not an error: it ends the purge with status `failed` in the
/// record, which lists what was deleted before it.
pub async fn execute_purge(
    server: &str,
    out: &Path,
    files: Option<&dyn FileSink>,
    media_dir: &Path,
    state: &Arc<dyn StateStore>,
    by: &str,
) -> Result<PurgeOutcome, IngestError> {
    let first = plan_purge(server, out, files, media_dir, state.as_ref()).await?;
    if !first.held.is_empty() {
        return Ok(PurgeOutcome::Held(first.held));
    }
    // Hold every feed of the server while deleting
    let mut leases = Vec::new();
    let mut own = BTreeSet::new();
    for resource in &first.resources {
        match Lease::acquire(state, server, resource, LEASE_TTL).await {
            Ok(LeaseOutcome::Acquired(lease)) => {
                leases.push(lease);
                own.insert(format!("{}.{resource}.lock", sanitize(server)));
            }
            Ok(LeaseOutcome::Held(holder)) => {
                release_all(leases).await;
                let key = format!("{}.{resource}.lock", sanitize(server));
                return Ok(PurgeOutcome::Held(vec![(key, holder)]));
            }
            Err(e) => {
                release_all(leases).await;
                return Err(e);
            }
        }
    }
    let result = delete(server, out, files, media_dir, state.as_ref(), &own, by).await;
    release_all(leases).await;
    result
}

async fn delete(
    server: &str,
    out: &Path,
    bucket: Option<&dyn FileSink>,
    media_dir: &Path,
    state: &dyn StateStore,
    own: &BTreeSet<String>,
    by: &str,
) -> Result<PurgeOutcome, IngestError> {
    let local = LocalDirSink::existing(out);
    let files = bucket.unwrap_or(&local);
    // Plan again now that the feeds are held, so nothing written in between is missed
    let plan = plan(server, out, files, media_dir, state, own).await?;
    if !plan.held.is_empty() {
        return Ok(PurgeOutcome::Held(plan.held));
    }
    let (id, path, mut file) = reserve_record(out, server)?;
    let started_at = now_rfc3339();
    let mut record = PurgeRecord {
        id: id.clone(),
        server: server.to_string(),
        by: by.to_string(),
        started_at,
        finished_at: String::new(),
        status: "purged".into(),
        error: None,
        files: Vec::new(),
        records: 0,
        media_dir: plan.media_dir.display().to_string(),
        media_files: 0,
        media_bytes: 0,
        state_keys: Vec::new(),
        not_purged: plan.not_purged.clone(),
    };
    let error = delete_planned(&plan, files, state, &mut record).await.err();
    if let Some(e) = error {
        tracing::error!(server, error = %e, "purge stopped");
        record.status = "failed".into();
        record.error = Some(e.to_string());
    }
    record.records = record.files.iter().map(|f| f.records).sum();
    record.finished_at = now_rfc3339();

    let mut text = serde_json::to_vec_pretty(&record).map_err(|e| IngestError::Output {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    text.push(b'\n');
    file.write_all(&text)
        .and_then(|()| file.sync_all())
        .map_err(|e| output_error(&path, e))?;
    let sha256 = hex(&Sha256::digest(&text));
    let name = file_name(&path);
    let sum_path = path.with_file_name(format!("{name}.sha256"));
    fs::write(&sum_path, format!("{sha256}  {name}\n")).map_err(|e| output_error(&sum_path, e))?;

    let line = AccessEntry {
        at: record.finished_at.clone(),
        run_id: id,
        server: server.to_string(),
        resource: "*".into(),
        credential: "none".into(),
        window_start: None,
        window_end: None,
        records: record.records,
        deletes: 0,
        bytes_received: 0,
        bytes_written: 0,
        status: record.status.clone(),
        error: record.error.clone(),
        purge: Some(PurgeNote {
            by: by.to_string(),
            record: name.clone(),
            sha256: sha256.clone(),
            files: record.files.len() as u64,
            media_files: record.media_files,
            state_keys: record.state_keys.len() as u64,
            bytes: record.files.iter().map(|f| f.bytes).sum::<u64>() + record.media_bytes,
        }),
    };
    let log = AccessLog::new(out);
    log.append(&line)?;

    // With a bucket, copies go next to the data files; the local record and log line stand alone
    let mut delivered = Vec::new();
    let mut delivery_error = None;
    if let Some(sink) = bucket {
        let copies = (|| {
            for (stored, local) in [(name.clone(), &path), (format!("{name}.sha256"), &sum_path)] {
                sink.put_file(&stored, local)?;
                delivered.push(sink.location(&stored));
            }
            delivered.extend(log.deliver(out, &record.id, Some(sink))?);
            Ok::<(), IngestError>(())
        })();
        if let Err(e) = copies {
            tracing::error!(server, error = %e, "purge record not copied to the bucket");
            delivery_error = Some(e.to_string());
        }
    }
    Ok(PurgeOutcome::Done {
        record: Box::new(record),
        path,
        sha256,
        delivered,
        delivery_error,
    })
}

/// Delete in order: data files, media, state; each success goes into `record` as it happens
async fn delete_planned(
    plan: &PurgePlan,
    files: &dyn FileSink,
    state: &dyn StateStore,
    record: &mut PurgeRecord,
) -> Result<(), IngestError> {
    for f in &plan.files {
        if files.delete(&f.name)? {
            record.files.push(f.clone());
        }
    }
    match fs::remove_dir_all(&plan.media_dir) {
        Ok(()) => {
            record.media_files = plan.media_files;
            record.media_bytes = plan.media_bytes;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(output_error(&plan.media_dir, e)),
    }
    for key in &plan.state_keys {
        if state.delete(key, None).await? {
            record.state_keys.push(key.clone());
        }
    }
    Ok(())
}

async fn release_all(leases: Vec<Lease>) {
    for lease in leases {
        lease.release().await;
    }
}

/// Create `<out>/audit/purge.<id>.json` exclusively, so no record is ever replaced
fn reserve_record(out: &Path, server: &str) -> Result<(String, PathBuf, fs::File), IngestError> {
    let dir = out.join(AUDIT_DIR);
    fs::create_dir_all(&dir).map_err(|e| output_error(&dir, e))?;
    let base = format!("{}.{}", new_run_id(), encode_name(server));
    for n in 0..100 {
        let id = if n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let path = dir.join(format!("purge.{id}.json"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((id, path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(output_error(&path, e)),
        }
    }
    Err(IngestError::Output {
        path: dir.display().to_string(),
        message: "no free purge record name".into(),
    })
}

/// The server's data files, and the files with a name like the server's that are not
///
/// On local disk every NDJSON file is read for its tag, then the manifests add the rest. In a
/// bucket the manifests come first, and only unlisted NDJSON objects named like the server's are
/// downloaded (into `scratch`) and read.
fn data_files(
    server: &str,
    sink: &dyn FileSink,
    scratch: &Path,
) -> Result<(Vec<PurgedFile>, Vec<String>), IngestError> {
    let sizes: BTreeMap<String, u64> = sink.list()?.into_iter().collect();
    let local = sink.local_path("").is_some();
    let prefix = format!("{}.", sanitize(server));
    let is_ndjson = |n: &str| n.ends_with(".ndjson") || n.ends_with(".ndjson.part");

    let mut files = Vec::new();
    let mut claimed = BTreeSet::new();
    let by_tag = |name: &str, files: &mut Vec<PurgedFile>, claimed: &mut BTreeSet<String>| {
        if let Some(Some(records)) = read_file(sink, name, scratch, |p| tagged_lines(p, server))? {
            files.push(PurgedFile {
                name: name.to_string(),
                records,
                bytes: sizes.get(name).copied().unwrap_or(0),
            });
            claimed.insert(name.to_string());
        }
        Ok::<(), IngestError>(())
    };
    if local {
        for name in sizes.keys().filter(|n| is_ndjson(n)) {
            by_tag(name, &mut files, &mut claimed)?;
        }
    }
    // Files the manifests list under the server (Parquet, and NDJSON)
    for name in sizes
        .keys()
        .filter(|n| n.starts_with("manifest.") && n.ends_with(".json"))
    {
        let Some(text) = read_file(sink, name, scratch, |p| {
            fs::read_to_string(p).map_err(|e| output_error(p, e))
        })?
        else {
            continue;
        };
        let manifest: Value = serde_json::from_str(&text).map_err(|e| IngestError::Output {
            path: sink.location(name),
            message: e.to_string(),
        })?;
        for r in manifest["resources"].as_array().into_iter().flatten() {
            if r["server"].as_str() != Some(server) {
                continue;
            }
            for f in r["files"].as_array().into_iter().flatten() {
                let Some(file) = f["name"].as_str().filter(|n| plain_name(n)) else {
                    continue;
                };
                let Some(&bytes) = sizes.get(file) else {
                    continue;
                };
                if claimed.insert(file.to_string()) {
                    files.push(PurgedFile {
                        name: file.to_string(),
                        records: f["records"].as_u64().unwrap_or(0),
                        bytes,
                    });
                }
            }
        }
    }
    if !local {
        let unlisted: Vec<&String> = sizes
            .keys()
            .filter(|n| n.starts_with(&prefix) && is_ndjson(n) && !claimed.contains(*n))
            .collect();
        for name in unlisted {
            by_tag(name, &mut files, &mut claimed)?;
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let not_purged = sizes
        .into_keys()
        .filter(|n| n.starts_with(&prefix) && !claimed.contains(n))
        .collect();
    Ok((files, not_purged))
}

/// Run `read` on `name`: in place on local disk, else on a copy downloaded into `scratch`;
/// `None` when the file is gone
fn read_file<T>(
    sink: &dyn FileSink,
    name: &str,
    scratch: &Path,
    read: impl FnOnce(&Path) -> Result<T, IngestError>,
) -> Result<Option<T>, IngestError> {
    if let Some(path) = sink.local_path(name) {
        return if path.is_file() {
            read(&path).map(Some)
        } else {
            Ok(None)
        };
    }
    fs::create_dir_all(scratch).map_err(|e| output_error(scratch, e))?;
    let copy = scratch.join(format!(".purge-{}-{name}", std::process::id()));
    let result = match sink.get_file(name, &copy) {
        Ok(true) => read(&copy).map(Some),
        Ok(false) => Ok(None),
        Err(e) => Err(e),
    };
    let _ = fs::remove_file(&copy);
    result
}

/// Lines in `path` when its lines are `server`'s; `None` when the file is another server's or has
/// no readable tag. A file that mixes servers is an error (the writers never make one).
fn tagged_lines(path: &Path, server: &str) -> Result<Option<u64>, IngestError> {
    let file = fs::File::open(path).map_err(|e| output_error(path, e))?;
    let mut lines = 0u64;
    let mut owner: Option<bool> = None;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| output_error(path, e))?;
        if line.trim().is_empty() {
            continue;
        }
        // A killed run can leave a cut last line in a .part file: count it, judge by the others
        let tag = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|v| v["source"].as_str().map(|s| s == server));
        match (owner, tag) {
            (None, Some(false)) => return Ok(None),
            (None, Some(true)) => owner = Some(true),
            (Some(true), Some(false)) => {
                return Err(IngestError::Output {
                    path: path.display().to_string(),
                    message: format!("mixes lines of {server} and other sources; not purged"),
                })
            }
            _ => {}
        }
        lines += 1;
    }
    Ok(owner.map(|_| lines))
}

/// A bare file name: no directory part, so a manifest cannot point outside the output directory
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && !name.starts_with('.')
}

/// Files and bytes under `dir`, at all levels; zero when it does not exist
fn dir_size(dir: &Path) -> Result<(u64, u64), IngestError> {
    let (mut files, mut bytes) = (0, 0);
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(output_error(&d, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| output_error(&d, e))?;
            let meta = entry
                .metadata()
                .map_err(|e| output_error(&entry.path(), e))?;
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((files, bytes))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 of `bytes` as hex, the form purge records are hashed in
///
/// Check a record with `sha256sum -c purge.<id>.json.sha256` in the audit directory, or compare
/// this with the `sha256` of the purge's access-log line.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_only() {
        assert!(plain_name("a.Property.r.0001.ndjson"));
        for bad in ["", "..", "../x", "a/b", "a\\b", ".run.x"] {
            assert!(!plain_name(bad), "{bad}");
        }
    }

    #[test]
    fn hashes_as_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_file_is_judged_by_its_tags() {
        let dir = std::env::temp_dir().join(format!("reso-purge-tags-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("mine.ndjson");
        fs::write(&mine, "{\"source\":\"a\"}\n{\"source\":\"a\"}\n{\"sour").unwrap();
        assert_eq!(tagged_lines(&mine, "a").unwrap(), Some(3));
        assert_eq!(tagged_lines(&mine, "b").unwrap(), None);
        let mixed = dir.join("mixed.ndjson");
        fs::write(&mixed, "{\"source\":\"a\"}\n{\"source\":\"b\"}\n").unwrap();
        assert!(tagged_lines(&mixed, "a").is_err());
        let untagged = dir.join("x.ndjson");
        fs::write(&untagged, "not json\n").unwrap();
        assert_eq!(tagged_lines(&untagged, "a").unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
