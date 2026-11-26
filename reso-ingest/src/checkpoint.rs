//! Checkpoints: where each server's resource was last pulled up to

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use reso_client::Checkpoint;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::checks::SchemaSnapshot;
use crate::error::IngestError;
use crate::keyset::{io_error, KeyLines};
use crate::output::now_rfc3339;
use crate::state::{invalid, FileStateStore, StateStore};

#[derive(Serialize, Deserialize)]
struct CheckpointFile {
    server: String,
    resource: String,
    timestamp: String,
    key: Option<Value>,
    updated_at: String,
}

#[derive(Serialize, Deserialize)]
struct KeysFile {
    server: String,
    resource: String,
    updated_at: String,
    keys: Vec<Value>,
}

/// First line of a key set in the line format; one key's JSON text per line follows, sorted
#[derive(Serialize, Deserialize)]
struct KeysHeader {
    server: String,
    resource: String,
    updated_at: String,
}

/// The header line of a key set for `server`/`resource`, ending in a newline
pub(crate) fn keys_header(server: &str, resource: &str) -> String {
    let header = KeysHeader {
        server: server.to_string(),
        resource: resource.to_string(),
        updated_at: now_rfc3339(),
    };
    // Plain strings always serialize
    let mut line = serde_json::to_string(&header).unwrap_or_default();
    line.push('\n');
    line
}

#[derive(Serialize, Deserialize)]
struct EventsFile {
    server: String,
    resource: String,
    updated_at: String,
    sequence: i64,
}

/// A saved checkpoint with the server and resource it belongs to, as listed by [`CheckpointStore::list`]
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointInfo {
    /// Server name
    pub server: String,
    /// Resource name
    pub resource: String,
    /// Timestamp of the last record written
    pub timestamp: String,
    /// Key of the last record written
    pub key: Option<Value>,
    /// When the checkpoint was saved (RFC 3339, UTC)
    pub updated_at: String,
}

/// Checkpoints and the other per-feed state, as JSON values in a [`StateStore`]
///
/// Each checkpoint is named `{server}.{resource}.json` (characters other than letters, digits, `-`
/// and `_` become `_`) and also records the real server and resource names, which are checked on
/// load. [`save`](Self::save) replaces the value whole, so a crash leaves either the old or the new
/// checkpoint, never a torn one. The same store holds the state of delete reconciliation
/// (`{server}.{resource}.keys.ndjson`, the key set last seen as a header line and one sorted key
/// per line, and `{server}.{resource}.events.json`,
/// the last event-feed sequence handled) and the schema snapshot (`{server}.@schema.json`).
/// [`new`](Self::new) keeps them as files in a directory ([`FileStateStore`]);
/// [`with_store`](Self::with_store) takes any other backend.
///
/// # Examples
///
/// ```
/// # async fn demo() -> Result<(), reso_ingest::IngestError> {
/// use reso_client::Checkpoint;
/// use reso_ingest::CheckpointStore;
///
/// let dir = std::env::temp_dir().join(format!("reso-ingest-doc-{}", std::process::id()));
/// let store = CheckpointStore::new(&dir);
/// store.save("mls", "Property", &Checkpoint::new("2026-01-01T00:00:00Z", "A1".into())).await?;
/// assert_eq!(store.load("mls", "Property").await?.unwrap().timestamp, "2026-01-01T00:00:00Z");
/// store.clear("mls", "Property").await?;
/// assert!(store.load("mls", "Property").await?.is_none());
/// # let _ = std::fs::remove_dir_all(&dir);
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    store: Arc<dyn StateStore>,
    /// Added to resource names of checkpoints (not key sets or events): a backfill window keeps
    /// its own position (#88)
    suffix: String,
}

impl CheckpointStore {
    /// A store of files in `dir`; the directory is created on the first save
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self::with_store(Arc::new(FileStateStore::new(dir)))
    }

    /// A store on any [`StateStore`] backend
    pub fn with_store(store: Arc<dyn StateStore>) -> Self {
        Self {
            store,
            suffix: String::new(),
        }
    }

    /// The same backend, with `suffix` added to the resource name of every checkpoint this store
    /// loads, saves or clears, so a backfill window (`Property~backfill 2020..2021`) has a position
    /// of its own beside the feed's. Key sets, event sequences and schema snapshots are not affected.
    pub fn scoped(&self, suffix: &str) -> Self {
        Self {
            store: Arc::clone(&self.store),
            suffix: suffix.to_string(),
        }
    }

    fn scoped_name(&self, resource: &str) -> String {
        format!("{resource}{}", self.suffix)
    }

    /// The backend
    pub fn store(&self) -> &Arc<dyn StateStore> {
        &self.store
    }

    /// The saved checkpoint, or `None` when there is none yet
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value is unreadable, is not valid, or belongs to another
    /// server or resource. A bad value is an error rather than "no checkpoint", so damaged state
    /// never silently turns an incremental sync into a full reload.
    pub async fn load(
        &self,
        server: &str,
        resource: &str,
    ) -> Result<Option<Checkpoint>, IngestError> {
        let resource = &self.scoped_name(resource);
        let key = checkpoint_key(server, resource);
        let Some(file) = self.read::<CheckpointFile>(&key, "checkpoint").await? else {
            return Ok(None);
        };
        if file.server != server || file.resource != *resource {
            return Err(invalid(
                self.store.as_ref(),
                &key,
                format!(
                    "checkpoint belongs to `{}`/`{}`, not `{server}`/`{resource}`",
                    file.server, file.resource
                ),
            ));
        }
        Ok(Some(Checkpoint {
            timestamp: file.timestamp,
            key: file.key,
        }))
    }

    /// Save `checkpoint` atomically
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value cannot be written.
    pub async fn save(
        &self,
        server: &str,
        resource: &str,
        checkpoint: &Checkpoint,
    ) -> Result<(), IngestError> {
        let resource = &self.scoped_name(resource);
        let file = CheckpointFile {
            server: server.to_string(),
            resource: resource.to_string(),
            timestamp: checkpoint.timestamp.clone(),
            key: checkpoint.key.clone(),
            updated_at: now_rfc3339(),
        };
        self.write(&checkpoint_key(server, resource), &file).await
    }

    async fn write(&self, key: &str, value: &impl Serialize) -> Result<(), IngestError> {
        let mut text = serde_json::to_string_pretty(value)
            .map_err(|e| invalid(self.store.as_ref(), key, e.to_string()))?;
        text.push('\n');
        self.store.put(key, text.as_bytes()).await
    }

    async fn read<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
        what: &str,
    ) -> Result<Option<T>, IngestError> {
        let Some(found) = self.store.get(key).await? else {
            return Ok(None);
        };
        serde_json::from_slice(&found.bytes)
            .map(Some)
            .map_err(|e| invalid(self.store.as_ref(), key, format!("invalid {what}: {e}")))
    }

    fn check_owner(
        &self,
        key: &str,
        found: (&str, &str),
        server: &str,
        resource: &str,
    ) -> Result<(), IngestError> {
        if found != (server, resource) {
            return Err(invalid(
                self.store.as_ref(),
                key,
                format!(
                    "state belongs to `{}`/`{}`, not `{server}`/`{resource}`",
                    found.0, found.1
                ),
            ));
        }
        Ok(())
    }

    /// The key set saved by the last reconciliation, or `None` before the first one
    ///
    /// Keys come back sorted by their JSON text, without repeats. This holds the whole set in
    /// memory; [`reconcile_deletes`](crate::reconcile_deletes) streams it instead. A key set saved
    /// in the old single-JSON format (`{server}.{resource}.keys.json`) is read too.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value is unreadable, invalid or belongs to another server
    /// or resource (a bad value is never treated as "no key set").
    pub async fn load_keys(
        &self,
        server: &str,
        resource: &str,
    ) -> Result<Option<Vec<Value>>, IngestError> {
        let key = keys_key(server, resource);
        let Some(found) = self.store.get(&key).await? else {
            return self.load_old_keys(server, resource).await;
        };
        let bad = |m: String| invalid(self.store.as_ref(), &key, m);
        let text = std::str::from_utf8(&found.bytes).map_err(|e| bad(e.to_string()))?;
        let mut lines = text.lines();
        self.check_keys_header(&key, lines.next().unwrap_or_default(), server, resource)?;
        lines
            .map(|line| serde_json::from_str(line).map_err(|e| bad(format!("invalid key: {e}"))))
            .collect::<Result<Vec<Value>, _>>()
            .map(Some)
    }

    async fn load_old_keys(
        &self,
        server: &str,
        resource: &str,
    ) -> Result<Option<Vec<Value>>, IngestError> {
        let key = state_key(server, resource, "keys");
        let Some(file) = self.read::<KeysFile>(&key, "state file").await? else {
            return Ok(None);
        };
        self.check_owner(&key, (&file.server, &file.resource), server, resource)?;
        Ok(Some(file.keys))
    }

    /// Save the key set atomically, sorted by JSON text and without repeats
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value cannot be written.
    pub async fn save_keys(
        &self,
        server: &str,
        resource: &str,
        keys: &[Value],
    ) -> Result<(), IngestError> {
        let mut texts: Vec<String> = keys.iter().map(Value::to_string).collect();
        texts.sort_unstable();
        texts.dedup();
        let mut body = keys_header(server, resource);
        for t in texts {
            body.push_str(&t);
            body.push('\n');
        }
        self.store
            .put(&keys_key(server, resource), body.as_bytes())
            .await?;
        self.store
            .delete(&state_key(server, resource, "keys"), None)
            .await
            .map(|_| ())
    }

    /// Copy the saved key set into `dest` and open it after the header, or `None` before the first
    ///
    /// A key set in the old single-JSON format is converted once: written in the new format,
    /// saved, and the old value deleted.
    pub(crate) async fn fetch_keys(
        &self,
        server: &str,
        resource: &str,
        dest: &Path,
    ) -> Result<Option<KeyLines>, IngestError> {
        let key = keys_key(server, resource);
        if !self.store.get_to_file(&key, dest).await? {
            let Some(old) = self.load_old_keys(server, resource).await? else {
                return Ok(None);
            };
            let mut texts: Vec<String> = old.iter().map(Value::to_string).collect();
            drop(old);
            texts.sort_unstable();
            texts.dedup();
            let mut out = BufWriter::new(File::create(dest).map_err(|e| io_error(dest, e))?);
            out.write_all(keys_header(server, resource).as_bytes())
                .map_err(|e| io_error(dest, e))?;
            for t in texts {
                writeln!(out, "{t}").map_err(|e| io_error(dest, e))?;
            }
            out.flush().map_err(|e| io_error(dest, e))?;
            drop(out);
            self.store.put_from_file(&key, dest).await?;
            self.store
                .delete(&state_key(server, resource, "keys"), None)
                .await?;
            tracing::info!(server, resource, "key set converted to the line format");
        }
        let mut lines = KeyLines::open(dest)?;
        let header = lines.next_key()?.unwrap_or_default();
        self.check_keys_header(&key, &header, server, resource)?;
        Ok(Some(lines))
    }

    /// Save the key set in `src` (written by [`keys_header`] then sorted key lines) atomically
    pub(crate) async fn store_keys(
        &self,
        server: &str,
        resource: &str,
        src: &Path,
    ) -> Result<(), IngestError> {
        self.store
            .put_from_file(&keys_key(server, resource), src)
            .await
    }

    fn check_keys_header(
        &self,
        key: &str,
        line: &str,
        server: &str,
        resource: &str,
    ) -> Result<(), IngestError> {
        let header: KeysHeader = serde_json::from_str(line).map_err(|e| {
            invalid(
                self.store.as_ref(),
                key,
                format!("invalid key set header: {e}"),
            )
        })?;
        self.check_owner(key, (&header.server, &header.resource), server, resource)
    }

    /// The last event-feed sequence handled for this resource, or `None` before the first run
    ///
    /// # Errors
    ///
    /// As for [`load_keys`](Self::load_keys).
    pub async fn load_event_sequence(
        &self,
        server: &str,
        resource: &str,
    ) -> Result<Option<i64>, IngestError> {
        let key = state_key(server, resource, "events");
        let Some(file) = self.read::<EventsFile>(&key, "state file").await? else {
            return Ok(None);
        };
        self.check_owner(&key, (&file.server, &file.resource), server, resource)?;
        Ok(Some(file.sequence))
    }

    /// Save the event-feed sequence atomically
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value cannot be written.
    pub async fn save_event_sequence(
        &self,
        server: &str,
        resource: &str,
        sequence: i64,
    ) -> Result<(), IngestError> {
        let file = EventsFile {
            server: server.to_string(),
            resource: resource.to_string(),
            updated_at: now_rfc3339(),
            sequence,
        };
        self.write(&state_key(server, resource, "events"), &file)
            .await
    }

    /// The schema snapshot saved by the last run for this server, or `None` before the first
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value is unreadable or invalid.
    pub async fn load_schema(&self, server: &str) -> Result<Option<SchemaSnapshot>, IngestError> {
        self.read(&schema_key(server), "state file").await
    }

    /// Save the schema snapshot atomically
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value cannot be written.
    pub async fn save_schema(
        &self,
        server: &str,
        schema: &SchemaSnapshot,
    ) -> Result<(), IngestError> {
        self.write(&schema_key(server), schema).await
    }

    /// Every saved checkpoint in the store, sorted by server and resource
    ///
    /// Delete-reconciliation state, schema snapshots, leases and media indexes are not listed. An
    /// empty list when nothing has been saved.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the store cannot be listed or a checkpoint cannot be read or
    /// parsed.
    pub async fn list(&self) -> Result<Vec<CheckpointInfo>, IngestError> {
        let mut found = Vec::new();
        for key in self.store.list().await? {
            if key.contains('/')
                || !key.ends_with(".json")
                || key.ends_with(".keys.json")
                || key.ends_with(".events.json")
                || key.ends_with(".@schema.json")
            {
                continue;
            }
            let Some(file) = self.read::<CheckpointFile>(&key, "checkpoint").await? else {
                continue; // removed since the listing
            };
            found.push(CheckpointInfo {
                server: file.server,
                resource: file.resource,
                timestamp: file.timestamp,
                key: file.key,
                updated_at: file.updated_at,
            });
        }
        found.sort_by(|a, b| (&a.server, &a.resource).cmp(&(&b.server, &b.resource)));
        Ok(found)
    }

    /// Delete the checkpoint ("start over"); no error when there is none
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the value exists but cannot be removed.
    pub async fn clear(&self, server: &str, resource: &str) -> Result<(), IngestError> {
        self.store
            .delete(&checkpoint_key(server, &self.scoped_name(resource)), None)
            .await
            .map(|_| ())
    }
}

fn checkpoint_key(server: &str, resource: &str) -> String {
    format!("{}.{}.json", sanitize(server), sanitize(resource))
}

// `@` cannot appear in a sanitized resource name, so this never collides with a checkpoint
/// Key sets in the line format (#87); `{server}.{resource}.keys.json` is the old single-JSON one
fn keys_key(server: &str, resource: &str) -> String {
    format!("{}.{}.keys.ndjson", sanitize(server), sanitize(resource))
}

fn schema_key(server: &str) -> String {
    format!("{}.@schema.json", sanitize(server))
}

fn state_key(server: &str, resource: &str, kind: &str) -> String {
    format!("{}.{}.{kind}.json", sanitize(server), sanitize(resource))
}

pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
