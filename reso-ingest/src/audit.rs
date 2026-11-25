//! The access log: an append-only audit trail of what was pulled, when and with which credential (#93)
//!
//! Each resource step of `sync` (and of each `serve` feed sync) appends one JSON line to
//! `<out>/audit/access.ndjson`: run id, server, resource, the credential's id, the replication window,
//! record and byte counts and the status. The file is only ever opened in append mode; nothing in
//! `reso-ingest` rewrites or truncates it.
//!
//! The credential id names where the secret is read from (`env:VAR` or `file:/path`), never the
//! secret. With a [`FileSink`] (a bucket, where objects cannot be appended to), the lines a run
//! wrote are also delivered as one file per run, `access.<run id>.ndjson`, next to its manifest.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::{AuthConfig, SecretSource};
use crate::error::IngestError;
use crate::manifest::ResourceEntry;
use crate::output::{now_rfc3339, output_error, sanitize};
use crate::sink::FileSink;

/// The access log's path inside the output directory
pub const ACCESS_LOG: &str = "audit/access.ndjson";

/// Appends to one file from many tasks: one write per line, one writer at a time in this process
static APPEND: Mutex<()> = Mutex::new(());

/// One line of the access log: one resource pull
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessEntry {
    /// When the line was written (RFC 3339, UTC)
    pub at: String,
    /// The run the pull belongs to
    pub run_id: String,
    /// Server name
    pub server: String,
    /// Resource name
    pub resource: String,
    /// Where the credential is read from (`env:VAR` or `file:/path`); never the secret.
    /// For client credentials it is the client id's source.
    pub credential: String,
    /// `ModificationTimestamp` the pull started after; `None` for a full load
    pub window_start: Option<String>,
    /// `ModificationTimestamp` of the last record pulled; `None` when nothing was pulled
    pub window_end: Option<String>,
    /// Records written
    pub records: u64,
    /// Delete events written
    pub deletes: u64,
    /// Response body bytes received from the server
    pub bytes_received: u64,
    /// Bytes in the files written
    pub bytes_written: u64,
    /// `ok`, `interrupted` or `failed`
    pub status: String,
    /// The error of a failed pull
    pub error: Option<String>,
    /// Set on the line a purge writes (status `purged` or `failed`); absent on pull lines
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge: Option<PurgeNote>,
}

/// What a purge removed, on its access-log line (the full list is in the purge record)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeNote {
    /// Who ran the purge (`--by`)
    pub by: String,
    /// The purge record's file name in the audit directory
    pub record: String,
    /// SHA-256 of the purge record file, as hex
    pub sha256: String,
    /// Data files removed
    pub files: u64,
    /// Media files removed
    pub media_files: u64,
    /// State keys removed
    pub state_keys: u64,
    /// Bytes removed (data and media files)
    pub bytes: u64,
}

impl AccessEntry {
    /// The line for one resource step
    ///
    /// `window_start` is the checkpoint timestamp the pull started from, if any.
    pub fn new(
        run_id: &str,
        credential: &str,
        window_start: Option<String>,
        entry: &ResourceEntry,
    ) -> Self {
        let status = match (&entry.error, entry.interrupted) {
            (Some(_), _) => "failed",
            (None, true) => "interrupted",
            (None, false) => "ok",
        };
        let window_end = entry
            .checkpoint
            .as_ref()
            .and_then(|c| c.get("timestamp"))
            .and_then(|t| t.as_str())
            .map(str::to_owned);
        Self {
            at: now_rfc3339(),
            run_id: run_id.to_owned(),
            server: entry.server.clone(),
            resource: entry.resource.clone(),
            credential: credential.to_owned(),
            window_start,
            window_end,
            records: entry.records,
            deletes: entry.deletes,
            bytes_received: entry.stats.http_bytes,
            bytes_written: entry.files.iter().map(|f| f.bytes).sum(),
            status: status.to_owned(),
            error: entry.error.clone(),
            purge: None,
        }
    }
}

/// The id of a server's credential: where its token, or its client id, is read from
///
/// `env:VAR` or `file:/path`; `unknown` when the configuration does not name one (it is
/// validated before a run, so a run never sees that).
pub fn credential_id(auth: &AuthConfig) -> String {
    match auth.sources().ok().and_then(|s| s.into_iter().next()) {
        Some(SecretSource::Env(var)) => format!("env:{var}"),
        Some(SecretSource::File(path)) => format!("file:{}", path.display()),
        None => "unknown".to_owned(),
    }
}

/// The access log of one output directory, and the lines this run added to it
#[derive(Debug)]
pub struct AccessLog {
    path: PathBuf,
    written: Mutex<Vec<String>>,
}

impl AccessLog {
    /// The log in `out` ([`ACCESS_LOG`]); nothing is created until the first line
    pub fn new(out: &Path) -> Self {
        Self {
            path: out.join(ACCESS_LOG),
            written: Mutex::new(Vec::new()),
        }
    }

    /// Where the log is
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line; the file and its directory are created when missing
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the line cannot be written.
    pub fn append(&self, entry: &AccessEntry) -> Result<(), IngestError> {
        let mut line = serde_json::to_string(entry).map_err(|e| IngestError::Output {
            path: self.path.display().to_string(),
            message: e.to_string(),
        })?;
        line.push('\n');
        {
            let _guard = APPEND
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(dir) = self.path.parent() {
                fs::create_dir_all(dir).map_err(|e| output_error(dir, e))?;
            }
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.path)
                .map_err(|e| output_error(&self.path, e))?;
            file.write_all(line.as_bytes())
                .and_then(|()| file.sync_data())
                .map_err(|e| output_error(&self.path, e))?;
        }
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(line);
        Ok(())
    }

    /// With a sink, deliver the lines this run appended as `access.<run id>.ndjson`; returns where
    /// they went. Nothing happens without a sink (the log in `out` holds them) or without lines.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the file cannot be written or delivered.
    pub fn deliver(
        &self,
        out: &Path,
        run_id: &str,
        sink: Option<&dyn FileSink>,
    ) -> Result<Option<String>, IngestError> {
        let Some(sink) = sink else { return Ok(None) };
        let lines = self
            .written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .concat();
        if lines.is_empty() {
            return Ok(None);
        }
        let name = format!("access.{}.ndjson", sanitize(run_id));
        let local = out.join(&name);
        fs::write(&local, lines).map_err(|e| output_error(&local, e))?;
        sink.put_file(&name, &local)?;
        fs::remove_file(&local).map_err(|e| output_error(&local, e))?;
        Ok(Some(sink.location(&name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::manifest::{FileEntry, ResourceStats};
    use serde_json::json;

    fn entry() -> ResourceEntry {
        ResourceEntry {
            server: "alpha".into(),
            resource: "Property".into(),
            records: 3,
            deletes: 1,
            files: vec![
                FileEntry {
                    name: "a".into(),
                    records: 2,
                    bytes: 100,
                },
                FileEntry {
                    name: "b".into(),
                    records: 1,
                    bytes: 50,
                },
            ],
            checkpoint: Some(json!({"timestamp": "2026-01-02T00:00:00Z", "key": "K3"})),
            error: None,
            stats: ResourceStats {
                http_bytes: 900,
                ..ResourceStats::default()
            },
            warnings: vec![],
            deletes_held: 0,
            interrupted: false,
        }
    }

    #[test]
    fn a_line_carries_the_window_counts_and_status() {
        let e = AccessEntry::new(
            "run1",
            "env:TOKEN",
            Some("2026-01-01T00:00:00Z".into()),
            &entry(),
        );
        assert_eq!(e.window_start.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(e.window_end.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!((e.records, e.deletes), (3, 1));
        assert_eq!((e.bytes_received, e.bytes_written), (900, 150));
        assert_eq!(e.status, "ok");

        let mut failed = entry();
        failed.error = Some("HTTP 500".into());
        assert_eq!(AccessEntry::new("r", "c", None, &failed).status, "failed");
        let mut cut = entry();
        cut.interrupted = true;
        assert_eq!(AccessEntry::new("r", "c", None, &cut).status, "interrupted");
    }

    #[test]
    fn the_credential_id_names_the_source_not_the_secret() {
        let bearer = AuthConfig::Bearer {
            token_env: Some("MLS_TOKEN".into()),
            token_file: None,
        };
        assert_eq!(credential_id(&bearer), "env:MLS_TOKEN");
        let cc = AuthConfig::ClientCredentials {
            token_url: "https://x/token".into(),
            client_id_env: None,
            client_id_file: Some("/run/secrets/id".into()),
            client_secret_env: Some("SECRET".into()),
            client_secret_file: None,
            scope: None,
            transport: None,
        };
        assert_eq!(credential_id(&cc), "file:/run/secrets/id");
    }

    #[test]
    fn appends_never_rewrite_and_a_sink_gets_the_runs_lines() {
        let dir = std::env::temp_dir().join(format!("reso-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = AccessLog::new(&dir);
        first
            .append(&AccessEntry::new("r1", "env:T", None, &entry()))
            .unwrap();
        let second = AccessLog::new(&dir);
        second
            .append(&AccessEntry::new("r2", "env:T", None, &entry()))
            .unwrap();
        let text = fs::read_to_string(dir.join(ACCESS_LOG)).unwrap();
        let runs: Vec<String> = text
            .lines()
            .map(|l| serde_json::from_str::<AccessEntry>(l).unwrap().run_id)
            .collect();
        assert_eq!(runs, ["r1", "r2"]);

        let bucket = dir.join("bucket");
        let sink = crate::sink::LocalDirSink::new(&bucket).unwrap();
        let at = second.deliver(&dir, "r2", Some(&sink)).unwrap().unwrap();
        assert!(at.ends_with("access.r2.ndjson"));
        let delivered = fs::read_to_string(bucket.join("access.r2.ndjson")).unwrap();
        assert_eq!(delivered.lines().count(), 1);
        assert!(!dir.join("access.r2.ndjson").exists());
        assert_eq!(second.deliver(&dir, "r2", None).unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
