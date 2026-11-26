//! Parquet output: the same tagged rows as the NDJSON files, typed from `$metadata`

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_json::reader::{Decoder, ReaderBuilder};
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef, TimeUnit};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use reso_client::EntityType;
use serde::Serialize;
use serde_json::Value;

use crate::error::IngestError;
use crate::manifest::FileEntry;
use crate::output::{
    create_new_file, hive_value, output_error, partition_prefix, sanitize, RecordWriter,
};
use crate::sink::FileSink;

/// Rows handed to the Arrow decoder at a time
const DECODE_BATCH_ROWS: usize = 8192;

/// The Arrow type for an OData type name; the fallback is `Utf8`
///
/// | OData type | Arrow type |
/// |---|---|
/// | `Edm.String`, `Edm.Guid`, `Edm.Duration`, `Edm.TimeOfDay`, `Edm.Binary` | `Utf8` |
/// | `Edm.Boolean` | `Boolean` |
/// | `Edm.Byte`, `Edm.SByte`, `Edm.Int16`, `Edm.Int32`, `Edm.Int64` | `UInt8`, `Int8`, `Int16`, `Int32`, `Int64` |
/// | `Edm.Single`, `Edm.Double`, `Edm.Decimal` | `Float32`, `Float64`, `Float64` (decimals lose exactness) |
/// | `Edm.Date` | `Date32` |
/// | `Edm.DateTimeOffset` | `Timestamp(Millisecond, "+00:00")` |
/// | `Collection(T)` | `List` of the type of `T` |
/// | anything else (enum types, complex types, unknown types) | `Utf8`; an object or array value is stored as its JSON text |
pub fn arrow_type(odata_type: &str) -> DataType {
    if let Some(inner) = odata_type
        .strip_prefix("Collection(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return DataType::List(Arc::new(Field::new("item", arrow_type(inner), true)));
    }
    match odata_type {
        "Edm.Boolean" => DataType::Boolean,
        "Edm.Byte" => DataType::UInt8,
        "Edm.SByte" => DataType::Int8,
        "Edm.Int16" => DataType::Int16,
        "Edm.Int32" => DataType::Int32,
        "Edm.Int64" => DataType::Int64,
        "Edm.Single" => DataType::Float32,
        "Edm.Double" | "Edm.Decimal" => DataType::Float64,
        "Edm.Date" => DataType::Date32,
        "Edm.DateTimeOffset" => DataType::Timestamp(TimeUnit::Millisecond, Some("+00:00".into())),
        _ => DataType::Utf8,
    }
}

/// The file schema for a resource
///
/// Columns: `source`, `resource`, `fetched_at` (timestamp), `run_id`, `event` (null for a record,
/// `"delete"` for a delete event), `key` (the deleted key as text; null for a record) and `record`,
/// a struct with one nullable field per property of `entity` (null for a delete event).
/// Properties the server sends that `$metadata` does not declare are not kept.
pub fn schema_for(entity: &EntityType) -> SchemaRef {
    schema_from(&record_types(entity))
}

/// (name, Arrow type) for each declared property of `entity`
fn record_types(entity: &EntityType) -> Vec<(String, DataType)> {
    entity
        .properties
        .iter()
        .map(|p| (p.name.clone(), arrow_type(&p.type_name)))
        .collect()
}

fn schema_from(record_types: &[(String, DataType)]) -> SchemaRef {
    let fields: Vec<Field> = record_types
        .iter()
        .map(|(name, ty)| Field::new(name, ty.clone(), true))
        .collect();
    Arc::new(Schema::new(vec![
        Field::new("source", DataType::Utf8, false),
        Field::new("resource", DataType::Utf8, false),
        Field::new(
            "fetched_at",
            DataType::Timestamp(TimeUnit::Millisecond, Some("+00:00".into())),
            false,
        ),
        Field::new("run_id", DataType::Utf8, false),
        Field::new("event", DataType::Utf8, true),
        Field::new("key", DataType::Utf8, true),
        Field::new("record", DataType::Struct(Fields::from(fields)), true),
    ]))
}

#[derive(Serialize)]
struct Row<'a> {
    source: &'a str,
    resource: &'a str,
    fetched_at: &'a str,
    run_id: &'a str,
    event: Option<&'static str>,
    key: Option<&'a Value>,
    record: Option<Value>,
}

/// Make `value` fit `ty`: a `Utf8` column takes objects and arrays as JSON text
fn conform(value: &mut Value, ty: &DataType) {
    match (ty, &mut *value) {
        (DataType::List(item), Value::Array(items)) => {
            items.iter_mut().for_each(|v| conform(v, item.data_type()));
        }
        (DataType::Utf8, Value::Object(_) | Value::Array(_)) => {
            *value = Value::String(value.to_string());
        }
        _ => {}
    }
}

/// An open file and the decoder that turns JSON rows into Arrow batches
struct Open {
    writer: ArrowWriter<File>,
    file: File,
    part_path: PathBuf,
    final_path: PathBuf,
    rows: u64,
    /// The `date=` partition the file is in (partitioned output only)
    date: Option<String>,
}

/// The UTC date of an RFC 3339 time (its first ten characters)
fn date_of(fetched_at: &str) -> String {
    fetched_at.get(..10).unwrap_or(fetched_at).to_string()
}

/// Writes tagged records as Parquet files typed from `$metadata`, rotating files by size
///
/// The rows are the NDJSON lines as columns (see [`schema_for`]); files are named
/// `{source}.{resource}.{run_id}.{NNNN}.parquet`, written as `….parquet.part` and renamed when
/// complete, so a file with the final name is always readable. Compression is Snappy.
///
/// **Commit does not close the file.** A Parquet file is unreadable until its footer is written.
/// [`RecordWriter::commit`] returns `true` only when no file is open, so `sync_resource` holds a
/// checkpoint back until the file holding its rows is finished (at the size limit or at the end of
/// the resource) and a checkpoint never points past rows that are not in a readable file. A run
/// that stops mid-file leaves a `.part` file and resumes from the last finished file, which
/// re-fetches the pages since (at-least-once). Rows are streamed to the `.part` file, so memory
/// holds one row group, not the pages. [`RecordWriter::flush`] still finishes the current file
/// (the delete step uses it). Size is checked after each batch, so a file can pass the limit by
/// one batch.
///
/// A value that does not fit its column (for example text in an integer column) makes the write
/// fail with [`IngestError::Output`], and the resource stops.
///
/// **Partitioned output** ([`partitioned`](Self::partitioned)) puts each file in a Hive-style folder,
/// `source=<server>/resource=<resource>/date=<YYYY-MM-DD>/`, where the date is the UTC date of
/// `fetched_at`. A file holds one date: a batch fetched on a new date starts a new file. The file
/// names and the rows stay the same, so the `source` and `resource` columns match the folder values.
pub struct ParquetWriter {
    dir: PathBuf,
    source: String,
    resource: String,
    run_id: String,
    max_file_bytes: u64,
    schema: SchemaRef,
    record_types: Vec<(String, DataType)>,
    current: Option<Open>,
    files: Vec<FileEntry>,
    sink: Option<Arc<dyn FileSink>>,
    partitioned: bool,
}

impl ParquetWriter {
    /// Create a writer for `entity`; `dir` is created if missing and no file is opened until the first row
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
        entity: &EntityType,
    ) -> Result<Self, IngestError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| output_error(&dir, e))?;
        Ok(Self {
            dir,
            source: source.to_string(),
            resource: resource.to_string(),
            run_id: run_id.to_string(),
            max_file_bytes,
            schema: schema_for(entity),
            record_types: record_types(entity),
            current: None,
            files: Vec::new(),
            sink: None,
            partitioned: false,
        })
    }

    /// Deliver finished files to `sink` instead of leaving them in the directory given to `new`
    ///
    /// The directory becomes a staging area: a finished file is handed to the sink, then removed.
    /// Call before the first row.
    #[must_use]
    pub fn with_sink(mut self, sink: Arc<dyn FileSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Write each file into a Hive-style partition folder (`source=…/resource=…/date=…/`)
    ///
    /// File names in the manifest then carry the folder, e.g.
    /// `source=mls/resource=Property/date=2026-11-26/mls.Property.<run>.0001.parquet`.
    /// Call before the first row.
    #[must_use]
    pub fn partitioned(mut self, on: bool) -> Self {
        self.partitioned = on;
        self
    }

    /// Keep these expanded navigation properties (`$expand`) as columns of JSON text
    ///
    /// `$metadata` declares them as navigation properties, not properties, so without this the
    /// related records a server sends inline (for example `Media` inside `Property`) are not kept.
    /// Call before the first row; a name that is already a column is left as it is.
    pub fn with_expanded(mut self, names: &[String]) -> Self {
        for name in names {
            if !self.record_types.iter().any(|(n, _)| n == name) {
                self.record_types.push((name.clone(), DataType::Utf8));
            }
        }
        self.schema = schema_from(&self.record_types);
        self
    }

    fn fail(&self, message: impl ToString) -> IngestError {
        IngestError::Output {
            path: self.dir.display().to_string(),
            message: message.to_string(),
        }
    }

    fn decoder(&self) -> Result<Decoder, IngestError> {
        ReaderBuilder::new(Arc::clone(&self.schema))
            .with_batch_size(DECODE_BATCH_ROWS)
            .with_coerce_primitive(true)
            .build_decoder()
            .map_err(|e| self.fail(e))
    }

    fn open_next(&mut self, fetched_at: &str) -> Result<(), IngestError> {
        let date = self.partitioned.then(|| date_of(fetched_at));
        let folder = date.as_ref().map_or(String::new(), |d| {
            format!(
                "{}date={}/",
                partition_prefix(&self.source, &self.resource),
                hive_value(d)
            )
        });
        let name = format!(
            "{folder}{}.{}.{}.{:04}.parquet",
            sanitize(&self.source),
            sanitize(&self.resource),
            sanitize(&self.run_id),
            self.files.len() + 1
        );
        let final_path = self.dir.join(&name);
        let part_path = self.dir.join(format!("{name}.part"));
        if let Some(parent) = part_path.parent() {
            fs::create_dir_all(parent).map_err(|e| output_error(parent, e))?;
        }
        let taken = match &self.sink {
            Some(sink) => sink.exists(&name)?,
            None => final_path.exists(),
        };
        if taken {
            return Err(self.fail("file already exists; another run may share this run id"));
        }
        let file = create_new_file(&part_path)?;
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        let writer = ArrowWriter::try_new(
            file.try_clone().map_err(|e| output_error(&part_path, e))?,
            Arc::clone(&self.schema),
            Some(props),
        )
        .map_err(|e| self.fail(e))?;
        self.files.push(FileEntry {
            name,
            records: 0,
            bytes: 0,
        });
        self.current = Some(Open {
            writer,
            file,
            part_path,
            final_path,
            rows: 0,
            date,
        });
        Ok(())
    }

    /// Turn rows into one Arrow batch (typed per the schema)
    fn encode(&self, rows: &[Row<'_>]) -> Result<Option<RecordBatch>, IngestError> {
        if rows.is_empty() {
            return Ok(None);
        }
        let mut decoder = self.decoder()?;
        decoder.serialize(rows).map_err(|e| self.fail(e))?;
        decoder.flush().map_err(|e| self.fail(e))
    }

    /// Append batches to the current file, opening one if needed (or, partitioned, when the date
    /// changed), and rotate when it is big enough
    fn append(&mut self, batches: &[RecordBatch], fetched_at: &str) -> Result<(), IngestError> {
        if batches.is_empty() {
            return Ok(());
        }
        let new_date = self.partitioned
            && self
                .current
                .as_ref()
                .is_some_and(|c| c.date.as_deref() != Some(date_of(fetched_at).as_str()));
        if new_date {
            self.close_current()?;
        }
        if self.current.is_none() {
            self.open_next(fetched_at)?;
        }
        let cur = self.current.as_mut().expect("file was just opened");
        for batch in batches {
            cur.writer
                .write(batch)
                .map_err(|e| output_error(&cur.part_path, std::io::Error::other(e)))?;
            cur.rows += batch.num_rows() as u64;
        }
        let size = cur.writer.bytes_written() + cur.writer.in_progress_size();
        if size as u64 >= self.max_file_bytes {
            self.close_current()?;
        }
        Ok(())
    }

    /// Write the footer, sync, and rename the file to its final name
    fn close_current(&mut self) -> Result<(), IngestError> {
        let Some(cur) = self.current.take() else {
            return Ok(());
        };
        let Open {
            writer,
            file,
            part_path,
            final_path,
            rows,
            date: _,
        } = cur;
        writer
            .close()
            .map_err(|e| output_error(&part_path, std::io::Error::other(e)))?;
        file.sync_all().map_err(|e| output_error(&part_path, e))?;
        let bytes = fs::metadata(&part_path).map_or(0, |m| m.len());
        if let Some(sink) = &self.sink {
            let name = self.files.last().map_or("", |f| f.name.as_str());
            sink.put_file(name, &part_path)?;
            fs::remove_file(&part_path).map_err(|e| output_error(&part_path, e))?;
            // Empty partition folders of the staging area go too
            let mut folder = part_path.parent();
            while let Some(d) = folder.filter(|d| *d != self.dir && d.starts_with(&self.dir)) {
                if fs::remove_dir(d).is_err() {
                    break;
                }
                folder = d.parent();
            }
        } else {
            fs::rename(&part_path, &final_path).map_err(|e| output_error(&final_path, e))?;
        }
        if let Some(entry) = self.files.last_mut() {
            entry.records = rows;
            entry.bytes = bytes;
        }
        Ok(())
    }

    fn record_value(&self, record: &Value) -> Value {
        let mut record = record.clone();
        if let Value::Object(map) = &mut record {
            for (name, ty) in &self.record_types {
                if let Some(v) = map.get_mut(name) {
                    conform(v, ty);
                }
            }
        }
        record
    }
}

impl RecordWriter for ParquetWriter {
    fn write_batch(&mut self, records: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        let rows: Vec<Row<'_>> = records
            .iter()
            .map(|r| Row {
                source: &self.source,
                resource: &self.resource,
                fetched_at,
                run_id: &self.run_id,
                event: None,
                key: None,
                record: Some(self.record_value(r)),
            })
            .collect();
        let batch = self.encode(&rows)?;
        self.append(batch.as_slice(), fetched_at)
    }

    fn write_deletes(&mut self, keys: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        let rows: Vec<Row<'_>> = keys
            .iter()
            .map(|k| Row {
                source: &self.source,
                resource: &self.resource,
                fetched_at,
                run_id: &self.run_id,
                event: Some("delete"),
                key: Some(k),
                record: None,
            })
            .collect();
        let batch = self.encode(&rows)?;
        self.append(batch.as_slice(), fetched_at)
    }

    fn flush(&mut self) -> Result<(), IngestError> {
        self.close_current()
    }

    fn commit(&mut self) -> Result<bool, IngestError> {
        Ok(self.current.is_none())
    }

    fn finish(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.close_current()?;
        Ok(self.files.clone())
    }
}
