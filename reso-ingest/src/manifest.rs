//! The per-run manifest and a helper that streams one resource into a writer

use std::fs;
use std::path::{Path, PathBuf};

use reso_client::{Checkpoint, ClientStats, RateUsage, ResoClient, StandardReplication};
use serde::Serialize;
use serde_json::{json, Value};

use crate::checkpoint::CheckpointStore;
use crate::error::IngestError;
use crate::media::PageMedia;
use crate::output::{now_rfc3339, RecordWriter};
use crate::shutdown::Shutdown;

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

/// How long a resource took and what its requests did (the client's counters over the pull)
///
/// The request counters cover the whole resource step: the pull, and delete detection when it ran.
/// Media downloads use their own client and are not counted here.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ResourceStats {
    /// Wall-clock seconds the resource took
    pub duration_seconds: f64,
    /// HTTP requests sent (retries included)
    pub requests: u64,
    /// Requests repeated because of a retryable failure
    pub retries: u64,
    /// Response body bytes received
    pub http_bytes: u64,
    /// Responses with a 2xx or 3xx status
    pub responses_ok: u64,
    /// Responses with status 429
    pub responses_throttled: u64,
    /// Responses with a 4xx status other than 429
    pub responses_client_error: u64,
    /// Responses with a 5xx status
    pub responses_server_error: u64,
}

impl ResourceStats {
    /// Build from the time taken and the client counters over that time (see
    /// [`ClientStats::since`])
    pub fn new(duration: std::time::Duration, delta: &ClientStats) -> Self {
        Self {
            duration_seconds: duration.as_secs_f64(),
            requests: delta.requests,
            retries: delta.retries,
            http_bytes: delta.bytes,
            responses_ok: delta.ok,
            responses_throttled: delta.throttled,
            responses_client_error: delta.client_errors,
            responses_server_error: delta.server_errors,
        }
    }
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
    /// Delete events written (see [`reconcile_deletes`](crate::reconcile_deletes))
    pub deletes: u64,
    /// Files written
    pub files: Vec<FileEntry>,
    /// Position after the last record written, when the pull used replication
    pub checkpoint: Option<Value>,
    /// The error that stopped the pull, if any (records before it are kept)
    pub error: Option<String>,
    /// Time and request counters; all zero where the caller did not measure them
    pub stats: ResourceStats,
    /// Check findings for this resource: data checks, count comparison, held deletes. They do not make
    /// the run fail.
    pub warnings: Vec<String>,
    /// Delete events found but not written because they passed the configured limit
    pub deletes_held: u64,
    /// True when a shutdown request stopped this resource before its end; what was written and
    /// checkpointed is consistent and the next run continues from it
    pub interrupted: bool,
    /// Set for a `sync --backfill`: its window, the count of records left at the start and its
    /// rate share
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backfill: Option<crate::backfill::BackfillEntry>,
}

/// A server that failed as a whole (for example a task error from `run_servers`)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerFailure {
    /// Server name
    pub server: String,
    /// Error text
    pub error: String,
}

/// What changed in a server's `$metadata` since the previous run
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchemaChanges {
    /// Server name
    pub server: String,
    /// One line per change, for example `Property.ListPrice: type Edm.Decimal -> Edm.String`
    pub changes: Vec<String>,
}

/// How much of one rate-limit window a server's client had used when its sync finished
///
/// One per configured window (`[server.rate_limit]`); servers with no limit have none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RateBudget {
    /// Server name
    pub server: String,
    /// `requests` or `bytes`
    pub unit: &'static str,
    /// `second`, `hour` or `day` (a rolling window ending when the sync finished)
    pub window: &'static str,
    /// Requests started or response bytes received in the window
    pub used: u64,
    /// The ceiling the client paces to (a backfill's or sync's share of `[server.rate_limit]`
    /// while a backfill runs)
    pub limit: u64,
}

impl RateBudget {
    /// One entry per configured window of `usage`
    pub fn from_usage(server: &str, usage: &RateUsage) -> Vec<RateBudget> {
        [
            ("requests", "second", usage.per_second),
            ("requests", "hour", usage.per_hour),
            ("requests", "day", usage.per_day),
            ("bytes", "hour", usage.bytes_per_hour),
            ("bytes", "day", usage.bytes_per_day),
        ]
        .into_iter()
        .filter_map(|(unit, window, w)| {
            w.map(|w| RateBudget {
                server: server.to_owned(),
                unit,
                window,
                used: w.used,
                limit: w.limit,
            })
        })
        .collect()
    }
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
    /// `$metadata` changes since the previous run, per server (servers with none are absent)
    pub schema_changes: Vec<SchemaChanges>,
    /// Rate-limit windows of each server's client when its sync finished (servers with a
    /// `[server.rate_limit]` only)
    pub rate_budget: Vec<RateBudget>,
    /// True when a shutdown request (SIGTERM, Ctrl-C) ended the run early: resources in the list
    /// are consistent with their checkpoints, and resources not started are absent
    pub interrupted: bool,
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
            schema_changes: Vec::new(),
            rate_budget: Vec::new(),
            interrupted: false,
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

    /// Add a server's schema changes (ignored when there are none)
    pub fn add_schema_changes(&mut self, server: impl Into<String>, changes: Vec<String>) {
        if !changes.is_empty() {
            self.schema_changes.push(SchemaChanges {
                server: server.into(),
                changes,
            });
        }
    }

    /// Add a server's rate-limit use (ignored when it has no limit)
    pub fn add_rate_usage(&mut self, server: &str, usage: &RateUsage) {
        self.rate_budget
            .extend(RateBudget::from_usage(server, usage));
    }

    /// Every check finding of the run as `server/resource: text` or `server: text` lines
    pub fn warnings(&self) -> Vec<String> {
        let schema = self.schema_changes.iter().flat_map(|s| {
            s.changes
                .iter()
                .map(move |c| format!("{}: schema: {c}", s.server))
        });
        let data = self.resources.iter().flat_map(|r| {
            r.warnings
                .iter()
                .map(move |w| format!("{}/{}: {w}", r.server, r.resource))
        });
        schema.chain(data).collect()
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
/// checkpoint in the entry covers every batch successfully written. Nothing is saved to disk;
/// see [`sync_resource`] for resumable, incremental pulls.
pub async fn pull_resource(
    client: &ResoClient,
    server: &str,
    resource: &str,
    spec: StandardReplication,
    writer: &mut dyn RecordWriter,
) -> ResourceEntry {
    pull(
        client,
        server,
        resource,
        spec,
        writer,
        None,
        None,
        &Shutdown::new(),
    )
    .await
}

/// Incremental, resumable pull of one resource
///
/// Loads the saved checkpoint from `store` and continues after it (a first run, with no
/// checkpoint, is the full load). After each batch is written and flushed, the checkpoint is
/// saved, so it is never ahead of data on disk. With `start_over` the saved checkpoint is
/// deleted first and everything is reloaded.
///
/// The guarantee is **at-least-once**: a crash between a flush and the save, or a failed save,
/// means the next run fetches that last batch again. Re-fetched records land in the new run's
/// files with the same key and a newer `fetched_at`; consumers de-duplicate on the record key
/// (an upsert). Any `from_checkpoint` already on `spec` is replaced by the saved one when there
/// is one, and kept as the starting point when there is not.
///
/// As with [`pull_resource`], errors (including an unreadable checkpoint file or a failed save)
/// stop this resource and are recorded in the entry.
pub async fn sync_resource(
    client: &ResoClient,
    server: &str,
    resource: &str,
    spec: StandardReplication,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
    start_over: bool,
) -> ResourceEntry {
    sync_resource_media(
        client,
        server,
        resource,
        spec,
        writer,
        store,
        start_over,
        None,
        &Shutdown::new(),
    )
    .await
}

/// [`sync_resource`] that also queues the media files of each page right after the page is
/// written and checkpointed (see [`PageMedia`]), so signed URLs are used while fresh; the
/// download workers fetch them while the next pages are pulled
///
/// A media error (an unreadable or unwritable index) stops this resource and is recorded in the
/// entry; single file failures are only in the [`PageMedia`] report.
#[allow(clippy::too_many_arguments)]
pub async fn sync_resource_media(
    client: &ResoClient,
    server: &str,
    resource: &str,
    mut spec: StandardReplication,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
    start_over: bool,
    media: Option<&mut PageMedia>,
    shutdown: &Shutdown,
) -> ResourceEntry {
    let setup = if start_over {
        store.clear(server, resource).await.map(|()| None)
    } else {
        store.load(server, resource).await
    };
    match setup {
        Ok(Some(cp)) => spec = spec.from_checkpoint(cp),
        Ok(None) => {}
        Err(e) => {
            return ResourceEntry {
                server: server.to_string(),
                resource: resource.to_string(),
                records: 0,
                deletes: 0,
                files: Vec::new(),
                checkpoint: None,
                error: Some(e.to_string()),
                stats: ResourceStats::default(),
                warnings: Vec::new(),
                deletes_held: 0,
                interrupted: false,
                backfill: None,
            };
        }
    }
    pull(
        client,
        server,
        resource,
        spec,
        writer,
        Some(store),
        media,
        shutdown,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn pull(
    client: &ResoClient,
    server: &str,
    resource: &str,
    spec: StandardReplication,
    writer: &mut dyn RecordWriter,
    store: Option<&CheckpointStore>,
    mut media: Option<&mut PageMedia>,
    shutdown: &Shutdown,
) -> ResourceEntry {
    let mut run = client.replicate(spec);
    let mut records = 0u64;
    let mut checkpoint = checkpoint_json(run.checkpoint());
    let mut error = None;
    let mut interrupted = false;
    // The checkpoint of the last page written in full, not yet saved because its rows are still
    // in an open file; saved once the file is finished
    let mut pending: Option<Checkpoint> = None;

    loop {
        // A shutdown request ends the loop between pages, or drops a request still in flight: the
        // checkpoint only moves after a page is written, so an abandoned page is fetched again.
        let next = tokio::select! {
            biased;
            () = shutdown.triggered() => {
                interrupted = true;
                break;
            }
            next = run.next_batch_raw() => next,
        };
        match next {
            Ok(Some(batch)) => {
                // Records stay text: NDJSON copies it, Parquet and media parse it (#162)
                if let Err(e) = writer.write_raw_batch(&batch, &now_rfc3339()) {
                    error = Some(e.to_string());
                    break;
                }
                records += batch.len() as u64;
                if let Some(store) = store {
                    // Save only when every row so far is in a readable file; otherwise hold the
                    // checkpoint back (Parquet keeps one file open across pages)
                    let saved = match writer.commit() {
                        Ok(durable) => {
                            pending = None;
                            match run.checkpoint() {
                                Some(cp) if durable => store.save(server, resource, cp).await,
                                Some(cp) => {
                                    pending = Some(cp.clone());
                                    Ok(())
                                }
                                None => Ok(()),
                            }
                        }
                        Err(e) => Err(e),
                    };
                    if let Err(e) = saved {
                        error = Some(e.to_string());
                        break;
                    }
                }
                checkpoint = checkpoint_json(run.checkpoint());
                if let Some(media) = media.as_deref_mut() {
                    let parsed = batch.values().map_err(|e| e.to_string());
                    let paged = match parsed {
                        Ok(values) => media
                            .page(resource, &values)
                            .await
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(e),
                    };
                    if let Err(e) = paged {
                        error = Some(e.to_string());
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        }
    }

    let files = match writer.finish() {
        Ok(files) => {
            // The open file is finished: its rows are readable, so the held checkpoint is safe
            if let (Some(store), Some(cp)) = (store, pending.as_ref()) {
                if let Err(e) = store.save(server, resource, cp).await {
                    error.get_or_insert_with(|| e.to_string());
                }
            }
            files
        }
        Err(e) => {
            error.get_or_insert_with(|| e.to_string());
            Vec::new()
        }
    };
    ResourceEntry {
        server: server.to_string(),
        resource: resource.to_string(),
        records,
        deletes: 0,
        files,
        checkpoint,
        error,
        stats: ResourceStats::default(),
        warnings: Vec::new(),
        deletes_held: 0,
        interrupted,
        backfill: None,
    }
}

fn checkpoint_json(c: Option<&Checkpoint>) -> Option<Value> {
    c.map(|c| json!({ "timestamp": c.timestamp, "key": c.key }))
}
