//! Deletes: finding records that left a server's feed

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use reso_client::{Metadata, QueryBuilder, ResoClient};
use serde_json::Value;

use crate::checkpoint::{keys_header, CheckpointStore};
use crate::error::IngestError;
use crate::keyset::{diff, ExternalSorter, KeyLines, ScratchDir, CHUNK_KEYS};
use crate::manifest::FileEntry;
use crate::manifest::{ResourceEntry, ResourceStats};
use crate::output::{now_rfc3339, RecordWriter};

/// A server resource that lists changes, used instead of the key comparison
///
/// RESO's `EntityEvent` resource has one row per create, update or delete, with an increasing
/// sequence number, the `ResourceName` and the `ResourceRecordKey`. It does not say what kind of
/// change a row is, so [`reconcile_deletes`] looks each named record up and reports the ones that
/// are gone. Vendor feeds with the same shape can be described by setting the fields by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFeed {
    /// The event resource, `EntityEvent` in the standard
    pub resource: String,
    /// Its increasing sequence field, `EntityEventSequence` in the standard
    pub sequence_field: String,
}

impl EventFeed {
    /// The RESO-standard `EntityEvent` feed
    pub fn entity_event() -> Self {
        Self {
            resource: "EntityEvent".to_string(),
            sequence_field: "EntityEventSequence".to_string(),
        }
    }

    /// The standard feed when `metadata` declares it (a resource `EntityEvent` with an
    /// `EntityEventSequence` property), otherwise `None`
    pub fn detect(metadata: &Metadata) -> Option<Self> {
        let feed = Self::entity_event();
        let has_resource = metadata.resource_names().contains(&feed.resource.as_str());
        let has_field = metadata
            .resource(&feed.resource)
            .is_some_and(|t| t.property(&feed.sequence_field).is_some());
        (has_resource && has_field).then_some(feed)
    }
}

/// The resource whose deletes [`reconcile_deletes`] looks for
#[derive(Debug, Clone, Copy)]
pub struct DeleteTarget<'a> {
    /// Resource name, for example `"Property"`
    pub resource: &'a str,
    /// Its single key field, for example `"ListingKey"`
    pub key_field: &'a str,
    /// A filter added (by `and`) to the key pull and to the lookups of `resource`, so a server
    /// that needs one in every request accepts them and a key diff covers the same records as
    /// the sync (see [`ServerConfig::filter_for`](crate::ServerConfig::filter_for)). It is not
    /// added to the event resource query.
    pub filter: Option<&'a str>,
    /// When to hold deletes back instead of writing them
    pub limits: DeleteLimits,
    /// Where the key comparison keeps its temporary files (sorted runs, the next key set, the
    /// deleted keys), in a new subdirectory removed after the pass; `None` is the system temporary
    /// directory. Needs about three times the size of the key set (some 40 MB per million keys).
    pub scratch_dir: Option<&'a Path>,
    /// Keys held in memory before they are sorted and spilled to a file (100 000 by default,
    /// some 10 MB); memory use of the key comparison stays near this whatever the key count
    pub chunk_keys: usize,
}

/// When [`reconcile_deletes`] holds deletes back for review instead of writing them
///
/// A vendor outage that empties a result set looks like "everything was deleted". With a limit set,
/// a pass that finds more deletes than allowed writes none, saves no new state (so the next run
/// finds the same deletes again) and reports them in [`ResourceEntry::deletes_held`] with a
/// warning. Run once with `allow` to release them after checking. Limits apply to the key
/// comparison and the event feed; flag-field deletes are written as the records stream by and are
/// not held.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DeleteLimits {
    /// Most delete events one pass may write
    pub max_deletes: Option<u64>,
    /// Most deletes as a percentage of the keys known before the pass (key comparison only)
    pub max_percent: Option<f64>,
    /// Ignore the limits for this pass
    pub allow: bool,
}

impl DeleteLimits {
    /// Why `found` deletes out of `known` keys (when known) pass the limit, or `None` if they do not
    fn exceeded(&self, found: u64, known: Option<u64>) -> Option<String> {
        if self.allow || found == 0 {
            return None;
        }
        if let Some(max) = self.max_deletes.filter(|m| found > *m) {
            return Some(format!("{found} deletes found, over the limit of {max}"));
        }
        let (max, known) = (self.max_percent?, known.filter(|k| *k > 0)?);
        let percent = found as f64 * 100.0 / known as f64;
        (percent > max).then(|| {
            format!(
                "{found} of {known} known keys ({percent:.1}%) deleted, over the limit of {max}%"
            )
        })
    }
}

impl<'a> DeleteTarget<'a> {
    /// A target with no filter
    pub fn new(resource: &'a str, key_field: &'a str) -> Self {
        Self {
            resource,
            key_field,
            filter: None,
            limits: DeleteLimits::default(),
            scratch_dir: None,
            chunk_keys: CHUNK_KEYS,
        }
    }

    /// Keep the key comparison's temporary files under `dir`
    pub fn with_scratch_dir(mut self, dir: &'a Path) -> Self {
        self.scratch_dir = Some(dir);
        self
    }

    /// Hold `n` keys in memory before spilling a sorted run (at least 1)
    pub fn with_chunk_keys(mut self, n: usize) -> Self {
        self.chunk_keys = n.max(1);
        self
    }

    /// Hold deletes back when they pass `limits`
    pub fn with_limits(mut self, limits: DeleteLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Add the server's filter to every query on the resource
    pub fn with_filter(mut self, filter: Option<&'a str>) -> Self {
        self.filter = filter;
        self
    }

    /// A query on the resource with the target's filter and `extra` joined by `and`
    fn query(&self, extra: Option<String>) -> QueryBuilder {
        let filter = match (self.filter, extra) {
            (Some(f), Some(e)) => Some(format!("({f}) and ({e})")),
            (Some(f), None) => Some(f.to_owned()),
            (None, e) => e,
        };
        let query = QueryBuilder::new(self.resource);
        match filter {
            Some(f) => query.filter(f),
            None => query,
        }
    }
}

/// Find records of `target.resource` that left the feed since the last call and write them as delete events
///
/// With `feed` (see [`EventFeed::detect`]) the server's event resource is read from the saved
/// sequence on; each record it names is looked up by `target.key_field`, and the ones no longer
/// there are deleted. Without it the keys of the whole resource are pulled
/// (`$select=<key_field>`) and compared with the key set saved by the previous call.
///
/// The first call only records a baseline (the key set, or the newest event sequence) and writes
/// no deletes: a first full load already holds exactly what the server has. Run it after
/// [`sync_resource`](crate::sync_resource) in the same run.
///
/// Nothing is emitted from partial information: if any request or write fails, the error is
/// recorded in the returned entry, the saved state is left as it was, and no delete is written
/// from the failed pass (event mode may already have written the deletes of earlier lookups; the
/// next run repeats them, like [`sync_resource`](crate::sync_resource) it is at-least-once, and
/// consumers treat a repeated delete as a no-op). A key pull that returns no keys when the saved
/// set is not empty is refused as a likely server fault, not taken as "everything was deleted".
///
/// Keys are compared as JSON values; event mode assumes string keys (the event feed carries the
/// key as text). The key comparison keeps key sets as sorted files and diffs them by merging,
/// so memory stays near [`DeleteTarget::chunk_keys`] keys however many the resource holds; its
/// temporary files go to [`DeleteTarget::scratch_dir`].
pub async fn reconcile_deletes(
    client: &ResoClient,
    server: &str,
    target: &DeleteTarget<'_>,
    feed: Option<&EventFeed>,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
) -> ResourceEntry {
    let resource = target.resource;
    let result = match feed {
        Some(feed) => by_events(client, server, target, feed, writer, store).await,
        None => by_key_diff(client, server, target, writer, store).await,
    };
    let mut warnings = Vec::new();
    let (deletes, held, mut error) = match result {
        Ok(Pass::Written(n)) => (n, 0, None),
        Ok(Pass::Held(n, why)) => {
            warnings.push(format!(
                "{n} deletes held, not written: {why}; check the server, then rerun with --allow-mass-deletes"
            ));
            (0, n, None)
        }
        Err(e) => (0, 0, Some(e)),
    };
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
        records: 0,
        deletes,
        files,
        checkpoint: None,
        error,
        stats: ResourceStats::default(),
        warnings,
        deletes_held: held,
        interrupted: false,
    }
}

/// What one delete pass did: wrote this many events, or held this many back for the reason
enum Pass {
    Written(u64),
    Held(u64, String),
}

type Outcome = Result<Pass, String>;

fn text<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn emit(writer: &mut dyn RecordWriter, keys: &[Value]) -> Result<(), String> {
    writer.write_deletes(keys, &now_rfc3339()).map_err(text)?;
    writer.flush().map_err(text)
}

/// Delete events go to the writer in batches of this many keys
const EMIT_BATCH: usize = 10_000;

async fn by_key_diff(
    client: &ResoClient,
    server: &str,
    target: &DeleteTarget<'_>,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
) -> Outcome {
    let (resource, key_field) = (target.resource, target.key_field);
    let base = target
        .scratch_dir
        .map_or_else(std::env::temp_dir, Path::to_path_buf);
    let scratch = ScratchDir::new(&base).map_err(text)?;
    let previous = store
        .fetch_keys(server, resource, &scratch.file("previous"))
        .await
        .map_err(text)?;
    let had_previous = previous.is_some();

    // Pull every key first; nothing is decided until the whole pull has succeeded.
    let query = target
        .query(None)
        .select(&[key_field])
        .build()
        .map_err(text)?;
    let mut sorter = ExternalSorter::new(&scratch, target.chunk_keys);
    let mut page = client.execute_page(&query).await.map_err(text)?;
    loop {
        for record in &page.records {
            let key = record
                .get(key_field)
                .filter(|k| !k.is_null())
                .ok_or_else(|| {
                    format!("a record has no `{key_field}`; the key pull is not trusted")
                })?;
            sorter.push(key.to_string()).map_err(text)?;
        }
        match client.next_page(&page).await.map_err(text)? {
            Some(next) => page = next,
            None => break,
        }
    }
    let mut current = sorter.finish().map_err(text)?;

    // One merge pass writes the next key set and the deleted keys, and counts both
    let (next_path, deleted_path) = (scratch.file("next"), scratch.file("deleted"));
    let create = |p: &Path| {
        File::create(p)
            .map(BufWriter::new)
            .map_err(|e| e.to_string())
    };
    let (mut next, mut deleted_out) = (create(&next_path)?, create(&deleted_path)?);
    next.write_all(keys_header(server, resource).as_bytes())
        .map_err(text)?;
    let counts = diff(
        previous,
        &mut current,
        &mut next,
        &next_path,
        &mut deleted_out,
        &deleted_path,
    )
    .map_err(text)?;
    next.flush().map_err(text)?;
    deleted_out.flush().map_err(text)?;
    drop((next, deleted_out, current));

    if had_previous {
        if counts.current == 0 && counts.previous > 0 {
            return Err(format!(
                "the key pull returned no keys but {} were known; refusing to delete them all",
                counts.previous
            ));
        }
        if let Some(why) = target
            .limits
            .exceeded(counts.deleted, Some(counts.previous))
        {
            // The key set is not replaced, so the next run finds the same deletes
            return Ok(Pass::Held(counts.deleted, why));
        }
        let mut lines = KeyLines::open(&deleted_path).map_err(text)?;
        let mut batch = Vec::with_capacity(EMIT_BATCH.min(counts.deleted as usize));
        while let Some(line) = lines.next_key().map_err(text)? {
            batch.push(serde_json::from_str(&line).map_err(text)?);
            if batch.len() == EMIT_BATCH {
                emit(writer, &batch)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            emit(writer, &batch)?;
        }
    }
    store
        .store_keys(server, resource, &next_path)
        .await
        .map_err(text)?;
    Ok(Pass::Written(if had_previous { counts.deleted } else { 0 }))
}

async fn by_events(
    client: &ResoClient,
    server: &str,
    target: &DeleteTarget<'_>,
    feed: &EventFeed,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
) -> Outcome {
    let resource = target.resource;
    let seq = feed.sequence_field.as_str();
    let Some(after) = store
        .load_event_sequence(server, resource)
        .await
        .map_err(text)?
    else {
        // First call: remember where the feed is now; earlier events are already in the full load.
        let query = QueryBuilder::new(&feed.resource)
            .select(&[seq])
            .order_by(seq, "desc")
            .top(1)
            .build()
            .map_err(text)?;
        let page = client.execute_page(&query).await.map_err(text)?;
        let newest = page.records.iter().filter_map(|r| r[seq].as_i64()).max();
        store
            .save_event_sequence(server, resource, newest.unwrap_or(0))
            .await
            .map_err(text)?;
        return Ok(Pass::Written(0));
    };

    let filter = format!(
        "ResourceName eq {} and {seq} gt {after}",
        odata_string(resource)
    );
    let query = QueryBuilder::new(&feed.resource)
        .filter(filter)
        .select(&[seq, "ResourceRecordKey"])
        .order_by(seq, "asc")
        .build()
        .map_err(text)?;
    let mut newest = after;
    let mut checked: HashSet<String> = HashSet::new();
    let mut deleted: Vec<Value> = Vec::new();
    let mut page = client.execute_page(&query).await.map_err(text)?;
    loop {
        let mut gone = Vec::new();
        for event in &page.records {
            if let Some(n) = event[seq].as_i64() {
                newest = newest.max(n);
            }
            let Some(key) = event["ResourceRecordKey"].as_str() else {
                continue;
            };
            if !checked.insert(key.to_string()) {
                continue;
            }
            if !exists(client, target, key).await? {
                gone.push(Value::String(key.to_string()));
            }
        }
        deleted.extend(gone);
        match client.next_page(&page).await.map_err(text)? {
            Some(next) => page = next,
            None => break,
        }
    }
    // Nothing is written or saved before the whole feed was read, so the order the server returns
    // events in does not matter and a held pass can be repeated.
    if let Some(why) = target.limits.exceeded(deleted.len() as u64, None) {
        return Ok(Pass::Held(deleted.len() as u64, why));
    }
    emit(writer, &deleted)?;
    store
        .save_event_sequence(server, resource, newest)
        .await
        .map_err(text)?;
    Ok(Pass::Written(deleted.len() as u64))
}

/// Whether a record with this key is still on the server (a filter lookup, so a 404 is an error,
/// not "deleted")
async fn exists(client: &ResoClient, target: &DeleteTarget<'_>, key: &str) -> Result<bool, String> {
    let key_field = target.key_field;
    let query = target
        .query(Some(format!("{key_field} eq {}", odata_string(key))))
        .select(&[key_field])
        .top(1)
        .build()
        .map_err(text)?;
    let page = client.execute_page(&query).await.map_err(text)?;
    Ok(!page.records.is_empty())
}

fn odata_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A writer that turns a flag field into delete events (vendor mode: MLS Grid's `MlgCanView`)
///
/// Some servers have no event resource: a removed record stays in the feed for a while with a
/// field set to `false` (MLS Grid: `MlgCanView`). Wrap the real writer in this one and pass it to
/// [`sync_resource`](crate::sync_resource): every record is still written raw, and each record
/// whose `flag_field` is the JSON value `false` also gets a delete event for its `key_field`
/// right after its batch. A missing, null or any other value means the record is live. Nothing
/// is changed in the records, and checkpoints are not affected. Expanded children carry no flag
/// and get no events.
///
/// A flagged record without its key fails the write (the pull stops and records the error), as
/// a delete that cannot name its record must not be dropped silently.
pub struct FlagDeleteWriter<'a> {
    inner: &'a mut dyn RecordWriter,
    flag_field: String,
    key_field: String,
    deletes: u64,
}

impl<'a> FlagDeleteWriter<'a> {
    /// Wrap `inner`; `flag_field` is false on deleted records, `key_field` names the record
    pub fn new(
        inner: &'a mut dyn RecordWriter,
        flag_field: impl Into<String>,
        key_field: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            flag_field: flag_field.into(),
            key_field: key_field.into(),
            deletes: 0,
        }
    }

    /// Delete events written so far
    pub fn deletes(&self) -> u64 {
        self.deletes
    }
}

impl RecordWriter for FlagDeleteWriter<'_> {
    fn write_batch(&mut self, records: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        self.inner.write_batch(records, fetched_at)?;
        let mut keys = Vec::new();
        for record in records {
            if record.get(&self.flag_field) == Some(&Value::Bool(false)) {
                match record.get(&self.key_field).filter(|k| !k.is_null()) {
                    Some(key) => keys.push(key.clone()),
                    None => {
                        return Err(IngestError::Output {
                            path: String::new(),
                            message: format!(
                            "a record with `{}` false has no `{}`; its delete cannot be written",
                            self.flag_field, self.key_field
                        ),
                        })
                    }
                }
            }
        }
        if !keys.is_empty() {
            self.inner.write_deletes(&keys, fetched_at)?;
            self.deletes += keys.len() as u64;
        }
        Ok(())
    }

    fn write_deletes(&mut self, keys: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        self.inner.write_deletes(keys, fetched_at)
    }

    fn flush(&mut self) -> Result<(), IngestError> {
        self.inner.flush()
    }

    fn commit(&mut self) -> Result<bool, IngestError> {
        self.inner.commit()
    }

    fn finish(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.inner.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_odata_strings() {
        assert_eq!(odata_string("O'Neil"), "'O''Neil'");
    }
}
