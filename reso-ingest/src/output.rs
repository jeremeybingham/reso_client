//! Raw output: NDJSON files, one record per line, wrapped in source tags

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::error::IngestError;
use crate::manifest::FileEntry;

/// A sink for the records of one resource on one server in one run
///
/// Implemented by [`NdjsonWriter`] and, with the `parquet` feature, `ParquetWriter`.
pub trait RecordWriter: Send {
    /// Write a batch, tagging each record with `fetched_at` (RFC 3339, UTC)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when a file cannot be written.
    fn write_batch(&mut self, records: &[Value], fetched_at: &str) -> Result<(), IngestError>;

    /// Write delete events for records that left the feed, with the same tags as records
    ///
    /// Each line has `"event":"delete"` and the record's `key` instead of a `record`.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when a file cannot be written.
    fn write_deletes(&mut self, keys: &[Value], fetched_at: &str) -> Result<(), IngestError>;

    /// Push everything written so far to disk-level storage (the OS), so a checkpoint saved
    /// afterwards never points past data that could still be lost with the process
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the flush fails.
    fn flush(&mut self) -> Result<(), IngestError>;

    /// A safe point after a page: make what was written durable if that is cheap, and say whether
    /// everything written so far now is
    ///
    /// `sync_resource` saves a checkpoint only when this returns `true`; otherwise it holds the
    /// checkpoint back until a later `commit` returns `true` or [`RecordWriter::finish`] succeeds.
    /// The default flushes and returns `true`. A writer whose files are unreadable until finished
    /// (Parquet) returns `false` while rows sit in an open file.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the flush fails.
    fn commit(&mut self) -> Result<bool, IngestError> {
        self.flush().map(|()| true)
    }

    /// Flush and close the output; returns the files written, in order
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the final flush fails.
    fn finish(&mut self) -> Result<Vec<FileEntry>, IngestError>;
}

/// One output line: the four tags, then the record untouched
#[derive(Serialize)]
struct TaggedLine<'a> {
    source: &'a str,
    resource: &'a str,
    fetched_at: &'a str,
    run_id: &'a str,
    record: &'a Value,
}

/// One delete event: the same four tags, then `event` and the key of the record that is gone
#[derive(Serialize)]
struct DeleteLine<'a> {
    source: &'a str,
    resource: &'a str,
    fetched_at: &'a str,
    run_id: &'a str,
    event: &'static str,
    key: &'a Value,
}

/// Writes tagged records as NDJSON, rotating files by size
///
/// Each line is `{"source":…,"resource":…,"fetched_at":…,"run_id":…,"record":{…}}`; `record` is
/// exactly what the server sent. Files are named `{source}.{resource}.{run_id}.{NNNN}.ndjson`
/// (characters other than letters, digits, `-` and `_` become `_`). A new file starts when the
/// next line would take the current one past `max_file_bytes`, so no line is split; a single
/// line larger than the limit gets a file of its own. Records are written as they arrive, so
/// memory use does not grow with the size of the pull. Delete events share the files:
/// `{"source":…,"resource":…,"fetched_at":…,"run_id":…,"event":"delete","key":…}`; a line with
/// an `event` is an event, a line with a `record` is a record.
pub struct NdjsonWriter {
    dir: PathBuf,
    source: String,
    resource: String,
    run_id: String,
    max_file_bytes: u64,
    current: Option<BufWriter<File>>,
    files: Vec<FileEntry>,
}

impl NdjsonWriter {
    /// Create a writer; `dir` is created if missing and no file is opened until the first record
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when `dir` cannot be created.
    pub fn new(
        dir: impl AsRef<Path>,
        source: &str,
        resource: &str,
        run_id: &str,
        max_file_bytes: u64,
    ) -> Result<Self, IngestError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| output_error(&dir, e))?;
        Ok(Self {
            dir,
            source: source.to_string(),
            resource: resource.to_string(),
            run_id: run_id.to_string(),
            max_file_bytes,
            current: None,
            files: Vec::new(),
        })
    }

    fn open_next(&mut self) -> Result<(), IngestError> {
        let name = format!(
            "{}.{}.{}.{:04}.ndjson",
            sanitize(&self.source),
            sanitize(&self.resource),
            sanitize(&self.run_id),
            self.files.len() + 1
        );
        let path = self.dir.join(&name);
        let file = create_new_file(&path)?;
        self.current = Some(BufWriter::new(file));
        self.files.push(FileEntry {
            name,
            records: 0,
            bytes: 0,
        });
        Ok(())
    }

    fn close_current(&mut self) -> Result<(), IngestError> {
        if let Some(mut w) = self.current.take() {
            let path = self
                .dir
                .join(self.files.last().map_or("", |f| f.name.as_str()));
            w.flush().map_err(|e| output_error(&path, e))?;
        }
        Ok(())
    }
}

impl NdjsonWriter {
    /// Append one serialized line, starting a new file first when the size limit would be passed
    fn write_line(&mut self, mut bytes: Vec<u8>) -> Result<(), IngestError> {
        bytes.push(b'\n');
        let size = bytes.len() as u64;
        let needs_new = match self.files.last() {
            Some(f) if self.current.is_some() => {
                f.bytes > 0 && f.bytes + size > self.max_file_bytes
            }
            _ => true,
        };
        if needs_new {
            self.close_current()?;
            self.open_next()?;
        }
        let path = self
            .dir
            .join(self.files.last().map_or("", |f| f.name.as_str()));
        if let Some(w) = self.current.as_mut() {
            w.write_all(&bytes).map_err(|e| output_error(&path, e))?;
        }
        if let Some(f) = self.files.last_mut() {
            f.records += 1;
            f.bytes += size;
        }
        Ok(())
    }

    fn serialize(&self, line: &impl Serialize) -> Result<Vec<u8>, IngestError> {
        serde_json::to_vec(line).map_err(|e| IngestError::Output {
            path: self.dir.display().to_string(),
            message: e.to_string(),
        })
    }
}

impl RecordWriter for NdjsonWriter {
    fn write_batch(&mut self, records: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        for record in records {
            let bytes = self.serialize(&TaggedLine {
                source: &self.source,
                resource: &self.resource,
                fetched_at,
                run_id: &self.run_id,
                record,
            })?;
            self.write_line(bytes)?;
        }
        Ok(())
    }

    fn write_deletes(&mut self, keys: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        for key in keys {
            let bytes = self.serialize(&DeleteLine {
                source: &self.source,
                resource: &self.resource,
                fetched_at,
                run_id: &self.run_id,
                event: "delete",
                key,
            })?;
            self.write_line(bytes)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IngestError> {
        if let Some(w) = self.current.as_mut() {
            let path = self
                .dir
                .join(self.files.last().map_or("", |f| f.name.as_str()));
            w.flush().map_err(|e| output_error(&path, e))?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.close_current()?;
        Ok(self.files.clone())
    }
}

pub(crate) fn output_error(path: &Path, e: std::io::Error) -> IngestError {
    IngestError::Output {
        path: path.display().to_string(),
        message: e.to_string(),
    }
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

/// The current UTC time as RFC 3339 with whole seconds, e.g. `2026-11-24T17:30:05Z`
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_rfc3339(secs)
}

/// A run id from the current time, e.g. `20261124T173005Z`; unique per second only
///
/// Runs that write into one directory should use [`reserve_run_id`] instead.
pub fn new_run_id() -> String {
    now_rfc3339().replace(['-', ':'], "")
}

/// A run id that no other run in `dir` has, reserved by creating the marker file `.run.{id}`
///
/// The id is the timestamp form of [`new_run_id`]; when a run in `dir` already holds it (a marker, or a
/// `manifest.{id}.json` or `media.{id}.json` from before markers), `-02`, `-03`, … is added, so ids
/// still sort by start time. The marker is created atomically, so two processes started in the same
/// second get different ids. `dir` is created if missing.
///
/// # Errors
///
/// [`IngestError::Output`] when `dir` or the marker cannot be created.
pub fn reserve_run_id(dir: impl AsRef<Path>) -> Result<String, IngestError> {
    let dir = dir.as_ref();
    fs::create_dir_all(dir).map_err(|e| output_error(dir, e))?;
    let base = new_run_id();
    for n in 1u32.. {
        let id = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n:02}")
        };
        let name = sanitize(&id);
        if dir.join(format!("manifest.{name}.json")).exists()
            || dir.join(format!("media.{name}.json")).exists()
        {
            continue;
        }
        let marker = dir.join(format!(".run.{name}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => return Ok(id),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(output_error(&marker, e)),
        }
    }
    unreachable!("the run id counter does not run out")
}

/// Create a new output file, failing rather than overwriting one that exists
pub(crate) fn create_new_file(path: &Path) -> Result<File, IngestError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                IngestError::Output {
                    path: path.display().to_string(),
                    message: "file already exists; another run may share this run id".to_string(),
                }
            } else {
                output_error(path, e)
            }
        })
}

fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_795_541_405), "2026-11-24T17:30:05Z");
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("mls/one two"), "mls_one_two");
    }
}
