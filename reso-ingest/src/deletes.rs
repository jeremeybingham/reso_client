//! Deletes: finding records that left a server's feed

use std::collections::{BTreeMap, HashSet};

use reso_client::{Metadata, QueryBuilder, ResoClient};
use serde_json::Value;

use crate::checkpoint::CheckpointStore;
use crate::manifest::ResourceEntry;
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

/// Find records of `resource` that left the feed since the last call and write them as delete events
///
/// With `feed` (see [`EventFeed::detect`]) the server's event resource is read from the saved
/// sequence on; each record it names is looked up by `key_field`, and the ones no longer there are
/// deleted. Without it the keys of the whole resource are pulled (`$select=<key_field>`) and
/// compared with the key set saved by the previous call.
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
/// key as text). Memory use grows with the number of keys in the resource.
pub async fn reconcile_deletes(
    client: &ResoClient,
    server: &str,
    resource: &str,
    key_field: &str,
    feed: Option<&EventFeed>,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
) -> ResourceEntry {
    let result = match feed {
        Some(feed) => by_events(client, server, resource, key_field, feed, writer, store).await,
        None => by_key_diff(client, server, resource, key_field, writer, store).await,
    };
    let (deletes, mut error) = match result {
        Ok(n) => (n, None),
        Err(e) => (0, Some(e)),
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
    }
}

type Outcome = Result<u64, String>;

fn text<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn emit(writer: &mut dyn RecordWriter, keys: &[Value]) -> Result<(), String> {
    writer.write_deletes(keys, &now_rfc3339()).map_err(text)?;
    writer.flush().map_err(text)
}

async fn by_key_diff(
    client: &ResoClient,
    server: &str,
    resource: &str,
    key_field: &str,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
) -> Outcome {
    let previous = store.load_keys(server, resource).map_err(text)?;

    // Pull every key first; nothing is decided until the whole pull has succeeded.
    let query = QueryBuilder::new(resource)
        .select(&[key_field])
        .build()
        .map_err(text)?;
    let mut current: BTreeMap<String, Value> = BTreeMap::new();
    let mut page = client.execute_page(&query).await.map_err(text)?;
    loop {
        for record in &page.records {
            let key = record
                .get(key_field)
                .filter(|k| !k.is_null())
                .ok_or_else(|| {
                    format!("a record has no `{key_field}`; the key pull is not trusted")
                })?;
            current.insert(key.to_string(), key.clone());
        }
        match client.next_page(&page).await.map_err(text)? {
            Some(next) => page = next,
            None => break,
        }
    }

    let mut deleted: Vec<Value> = Vec::new();
    if let Some(previous) = &previous {
        if current.is_empty() && !previous.is_empty() {
            return Err(format!(
                "the key pull returned no keys but {} were known; refusing to delete them all",
                previous.len()
            ));
        }
        let mut seen = HashSet::new();
        for key in previous {
            let id = key.to_string();
            if seen.insert(id.clone()) && !current.contains_key(&id) {
                deleted.push(key.clone());
            }
        }
        emit(writer, &deleted)?;
    }
    let keys: Vec<Value> = current.into_values().collect();
    store.save_keys(server, resource, &keys).map_err(text)?;
    Ok(deleted.len() as u64)
}

async fn by_events(
    client: &ResoClient,
    server: &str,
    resource: &str,
    key_field: &str,
    feed: &EventFeed,
    writer: &mut dyn RecordWriter,
    store: &CheckpointStore,
) -> Outcome {
    let seq = feed.sequence_field.as_str();
    let Some(after) = store.load_event_sequence(server, resource).map_err(text)? else {
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
            .map_err(text)?;
        return Ok(0);
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
    let mut deleted = 0u64;
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
            if !exists(client, resource, key_field, key).await? {
                gone.push(Value::String(key.to_string()));
            }
        }
        emit(writer, &gone)?;
        deleted += gone.len() as u64;
        match client.next_page(&page).await.map_err(text)? {
            Some(next) => page = next,
            None => break,
        }
    }
    // Saved only after the whole feed was read, so the order the server returns events in does not matter.
    store
        .save_event_sequence(server, resource, newest)
        .map_err(text)?;
    Ok(deleted)
}

/// Whether a record with this key is still on the server (a filter lookup, so a 404 is an error,
/// not "deleted")
async fn exists(
    client: &ResoClient,
    resource: &str,
    key_field: &str,
    key: &str,
) -> Result<bool, String> {
    let query = QueryBuilder::new(resource)
        .filter(format!("{key_field} eq {}", odata_string(key)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_odata_strings() {
        assert_eq!(odata_string("O'Neil"), "'O''Neil'");
    }
}
