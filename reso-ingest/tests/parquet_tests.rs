//! Parquet writer: types from `$metadata`, round trip, rotation, flush (offline)
#![cfg(feature = "parquet")]

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow_array::RecordBatch;
use arrow_schema::{DataType, TimeUnit};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use reso_client::Metadata;
use reso_ingest::{arrow_type, IngestError, ParquetWriter, RecordWriter};
use serde_json::{json, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("reso-ingest-pq-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const METADATA: &str = r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>
<Property Name="ListingKey" Type="Edm.String" Nullable="false"/>
<Property Name="ListPrice" Type="Edm.Decimal"/>
<Property Name="Bedrooms" Type="Edm.Int32"/>
<Property Name="Views" Type="Edm.Int64"/>
<Property Name="HasPool" Type="Edm.Boolean"/>
<Property Name="ListingDate" Type="Edm.Date"/>
<Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/>
<Property Name="Appliances" Type="Collection(Edm.String)"/>
<Property Name="Status" Type="org.reso.metadata.enums.StandardStatus"/>
<Property Name="Extra" Type="org.reso.metadata.Odd"/></EntityType>
<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

fn writer(dir: &TempDir, max_bytes: u64) -> ParquetWriter {
    let metadata = Metadata::parse(METADATA).unwrap();
    let entity = metadata.resource("Property").unwrap();
    ParquetWriter::new(&dir.0, "mls-one", "Property", "run1", max_bytes, entity).unwrap()
}

/// Every row of a file as JSON (arrow's own JSON writer)
fn read_rows(path: &Path) -> Vec<Value> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap();
    let batches: Vec<RecordBatch> = reader.map(Result::unwrap).collect();
    let refs: Vec<&RecordBatch> = batches.iter().collect();
    let mut w = arrow_json::ArrayWriter::new(Vec::new());
    w.write_batches(&refs).unwrap();
    w.finish().unwrap();
    let bytes = w.into_inner();
    if bytes.is_empty() {
        return Vec::new();
    }
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn maps_odata_types_to_arrow() {
    assert_eq!(arrow_type("Edm.String"), DataType::Utf8);
    assert_eq!(arrow_type("Edm.Boolean"), DataType::Boolean);
    assert_eq!(arrow_type("Edm.Int32"), DataType::Int32);
    assert_eq!(arrow_type("Edm.Int64"), DataType::Int64);
    assert_eq!(arrow_type("Edm.Decimal"), DataType::Float64);
    assert_eq!(arrow_type("Edm.Date"), DataType::Date32);
    assert_eq!(
        arrow_type("Edm.DateTimeOffset"),
        DataType::Timestamp(TimeUnit::Millisecond, Some("+00:00".into()))
    );
    // Unknown, enum and complex types fall back to text
    assert_eq!(
        arrow_type("org.reso.metadata.enums.StandardStatus"),
        DataType::Utf8
    );
    assert_eq!(arrow_type("Edm.Whatever"), DataType::Utf8);
    let DataType::List(item) = arrow_type("Collection(Edm.Int16)") else {
        panic!("not a list");
    };
    assert_eq!(item.data_type(), &DataType::Int16);
}

#[test]
fn round_trips_records_and_delete_events() {
    let dir = TempDir::new("roundtrip");
    let records = vec![
        json!({
            "ListingKey": "A1", "ListPrice": 250000.5, "Bedrooms": 3, "Views": 9_000_000_000_i64,
            "HasPool": true, "ListingDate": "2026-03-04",
            "ModificationTimestamp": "2026-11-24T10:00:00.123Z",
            "Appliances": ["Oven", "Fridge"], "Status": "Active",
            "Extra": {"a": [1, 2]}
        }),
        // Missing and null fields become nulls; a decimal sent as text is read as a number
        json!({"ListingKey": "A2", "ListPrice": "99.5", "Bedrooms": null, "Appliances": []}),
    ];
    let mut w = writer(&dir, 1 << 30);
    w.write_batch(&records, "2026-11-24T10:00:05Z").unwrap();
    w.write_deletes(&[json!("A9"), json!(42)], "2026-11-24T10:00:06Z")
        .unwrap();
    let files = w.finish().unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "mls-one.Property.run1.0001.parquet");
    assert_eq!(files[0].records, 4);
    let path = dir.0.join(&files[0].name);
    assert_eq!(files[0].bytes, std::fs::metadata(&path).unwrap().len());
    assert!(!dir
        .0
        .join("mls-one.Property.run1.0001.parquet.part")
        .exists());

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert_eq!(row["source"], "mls-one");
        assert_eq!(row["resource"], "Property");
        assert_eq!(row["run_id"], "run1");
    }
    assert_eq!(rows[0]["fetched_at"], "2026-11-24T10:00:05Z");
    assert!(rows[0].get("event").is_none() || rows[0]["event"].is_null());

    let a1 = &rows[0]["record"];
    assert_eq!(a1["ListingKey"], "A1");
    assert_eq!(a1["ListPrice"], 250000.5);
    assert_eq!(a1["Bedrooms"], 3);
    assert_eq!(a1["Views"], 9_000_000_000_i64);
    assert_eq!(a1["HasPool"], true);
    assert_eq!(a1["ListingDate"], "2026-03-04");
    assert_eq!(a1["ModificationTimestamp"], "2026-11-24T10:00:00.123Z");
    assert_eq!(a1["Appliances"], json!(["Oven", "Fridge"]));
    assert_eq!(a1["Status"], "Active");
    // A complex value is kept as its JSON text
    let extra: Value = serde_json::from_str(a1["Extra"].as_str().unwrap()).unwrap();
    assert_eq!(extra, json!({"a": [1, 2]}));

    let a2 = &rows[1]["record"];
    assert_eq!(a2["ListPrice"], 99.5);
    assert!(a2.get("Bedrooms").is_none_or(Value::is_null));
    assert_eq!(a2["Appliances"], json!([]));

    assert_eq!(rows[2]["event"], "delete");
    assert_eq!(rows[2]["key"], "A9");
    assert_eq!(rows[3]["key"], "42");
    assert!(rows[2].get("record").is_none_or(Value::is_null));
}

#[test]
fn rotates_files_by_size_without_losing_rows() {
    let dir = TempDir::new("rotate");
    let mut w = writer(&dir, 20_000);
    let mut sent = 0;
    for batch in 0..10 {
        let records: Vec<Value> = (0..2000)
            .map(
                |i| json!({"ListingKey": format!("K{batch}-{i}"), "ListPrice": f64::from(i) * 1.5}),
            )
            .collect();
        sent += records.len() as u64;
        w.write_batch(&records, "2026-11-24T10:00:05Z").unwrap();
    }
    let files = w.finish().unwrap();
    assert!(files.len() > 1, "{files:?}");
    assert_eq!(files.iter().map(|f| f.records).sum::<u64>(), sent);
    for (i, f) in files.iter().enumerate() {
        assert_eq!(
            f.name,
            format!("mls-one.Property.run1.{:04}.parquet", i + 1)
        );
        assert_eq!(read_rows(&dir.0.join(&f.name)).len() as u64, f.records);
    }
}

#[test]
fn flush_finishes_the_file_and_the_next_write_starts_another() {
    let dir = TempDir::new("flush");
    let mut w = writer(&dir, 1 << 30);
    w.write_batch(&[json!({"ListingKey": "A"})], "2026-11-24T10:00:05Z")
        .unwrap();
    w.flush().unwrap();
    // Readable at once, before finish
    let first = dir.0.join("mls-one.Property.run1.0001.parquet");
    assert_eq!(read_rows(&first).len(), 1);

    w.write_batch(&[json!({"ListingKey": "B"})], "2026-11-24T10:00:06Z")
        .unwrap();
    let files = w.finish().unwrap();
    assert_eq!(files.len(), 2);
    // finish is repeatable (the CLI finishes once after the pull and again after deletes)
    assert_eq!(w.finish().unwrap(), files);
    assert_eq!(
        read_rows(&dir.0.join(&files[1].name))[0]["record"]["ListingKey"],
        "B"
    );
}

#[test]
fn a_value_that_does_not_fit_fails_the_write() {
    let dir = TempDir::new("badvalue");
    let mut w = writer(&dir, 1 << 30);
    let err = w
        .write_batch(
            &[json!({"ListingKey": "A", "Bedrooms": "many"})],
            "2026-11-24T10:00:05Z",
        )
        .unwrap_err();
    assert!(matches!(err, IngestError::Output { .. }), "{err}");
}

#[test]
fn writes_nothing_when_nothing_arrives() {
    let dir = TempDir::new("empty");
    let mut w = writer(&dir, 1 << 30);
    w.write_batch(&[], "2026-11-24T10:00:05Z").unwrap();
    assert!(w.finish().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn expanded_navigation_properties_are_kept_as_json_text() {
    let dir = TempDir::new("expanded");
    let mut w =
        writer(&dir, 1 << 30).with_expanded(&["Media".to_string(), "ListingKey".to_string()]);
    let media = json!([{"MediaKey": "M1", "MediaURL": "https://media.example/m1.jpg"}]);
    w.write_batch(
        &[
            json!({"ListingKey": "A1", "Media": media.clone()}),
            json!({"ListingKey": "A2"}),
        ],
        "2026-11-24T10:00:05Z",
    )
    .unwrap();
    let files = w.finish().unwrap();
    let rows = read_rows(&dir.0.join(&files[0].name));
    let stored = rows[0]["record"]["Media"].as_str().expect("Media as text");
    assert_eq!(serde_json::from_str::<Value>(stored).unwrap(), media);
    assert_eq!(rows[0]["record"]["ListingKey"], json!("A1"));
    assert!(rows[1]["record"].get("Media").is_none_or(Value::is_null));
}

#[test]
fn a_sink_receives_each_finished_file_and_the_staged_copy_goes() {
    let staging = TempDir::new("sink-stage");
    let dest = TempDir::new("sink-dest");
    let sink = std::sync::Arc::new(reso_ingest::LocalDirSink::new(&dest.0).unwrap());
    let mut w = writer(&staging, 1 << 20).with_sink(sink);
    let records = [json!({"ListingKey": "A1"})];
    w.write_batch(&records, "2026-11-25T00:00:00Z").unwrap();
    assert!(!w.commit().unwrap(), "the file is still open");
    let entries = w.finish().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(read_rows(&dest.0.join(&entries[0].name)).len(), 1);
    assert!(entries[0].bytes > 0);
    assert_eq!(std::fs::read_dir(&staging.0).unwrap().count(), 0);
}
