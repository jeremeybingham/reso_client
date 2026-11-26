//! Checks that compare a run with the last one: `$metadata` changes, data problems, the server's count
//!
//! Findings are warnings: they are written to the manifest, logged and exported as metrics, and
//! do not fail the run. Held deletes are in [`crate::DeleteLimits`].

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use reso_client::{Metadata, QueryBuilder, RawRecords, ResoClient};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;

use crate::error::IngestError;
use crate::manifest::FileEntry;
use crate::output::RecordWriter;

/// The shape of a server's `$metadata` that matters for downstream tables: for each resource
/// its fields and their types, and the members of each enum type
///
/// Saved after every run (see [`CheckpointStore::save_schema`](crate::CheckpointStore::save_schema))
/// and compared with the next one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// Resource name to field name to declared type
    pub resources: BTreeMap<String, BTreeMap<String, String>>,
    /// Enum type name to its member names
    pub enums: BTreeMap<String, BTreeSet<String>>,
}

impl SchemaSnapshot {
    /// The snapshot of `resources` (the ones the configuration allows) and every enum type
    pub fn from_metadata(metadata: &Metadata, resources: &[String]) -> Self {
        let resources = resources
            .iter()
            .filter_map(|name| {
                let ty = metadata.resource(name)?;
                let fields = ty
                    .properties
                    .iter()
                    .map(|p| (p.name.clone(), p.type_name.clone()))
                    .collect();
                Some((name.clone(), fields))
            })
            .collect();
        let enums = metadata
            .enum_types
            .iter()
            .map(|e| {
                (
                    e.name.clone(),
                    e.members.iter().map(|m| m.name.clone()).collect(),
                )
            })
            .collect();
        Self { resources, enums }
    }

    /// What changed from `self` (the previous run) to `now`, one line per change, in a stable order
    pub fn changes_to(&self, now: &Self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, old) in &self.resources {
            let Some(new) = now.resources.get(name) else {
                out.push(format!("{name}: resource removed"));
                continue;
            };
            for (field, ty) in old {
                match new.get(field) {
                    None => out.push(format!("{name}.{field}: field removed")),
                    Some(t) if t != ty => {
                        out.push(format!("{name}.{field}: type {ty} -> {t}"));
                    }
                    Some(_) => {}
                }
            }
            for (field, ty) in new.iter().filter(|(f, _)| !old.contains_key(*f)) {
                out.push(format!("{name}.{field}: field added ({ty})"));
            }
        }
        for name in now
            .resources
            .keys()
            .filter(|n| !self.resources.contains_key(*n))
        {
            out.push(format!("{name}: resource added"));
        }
        for (name, old) in &self.enums {
            let Some(new) = now.enums.get(name) else {
                out.push(format!("enum {name}: removed"));
                continue;
            };
            for m in old.difference(new) {
                out.push(format!("enum {name}: value {m} removed"));
            }
            for m in new.difference(old) {
                out.push(format!("enum {name}: value {m} added"));
            }
        }
        for name in now.enums.keys().filter(|n| !self.enums.contains_key(*n)) {
            out.push(format!("enum {name}: added"));
        }
        out
    }
}

/// A record key as the repeated-key check hashes it
///
/// String keys (what RESO keys are) are borrowed, so the check allocates nothing per record
/// (#166). Other keys fall back to their JSON text, which keeps the string `"1"` and the number
/// `1` distinct.
#[derive(PartialEq, Eq, Hash)]
enum SeenKey<'a> {
    Str(Cow<'a, str>),
    Json(String),
}

impl<'a> SeenKey<'a> {
    fn of(key: &'a Value) -> Self {
        match key {
            Value::String(s) => SeenKey::Str(Cow::Borrowed(s)),
            other => SeenKey::Json(other.to_string()),
        }
    }

    /// The key from its JSON text; `None` for `null` (#162)
    fn of_raw(key: &'a RawValue) -> Option<Self> {
        let text = key.get();
        if text == "null" {
            return None;
        }
        if let Ok(s) = serde_json::from_str::<&str>(text) {
            return Some(SeenKey::Str(Cow::Borrowed(s)));
        }
        // An escaped string, or a number or other value: compared as a parse would compare it
        match serde_json::from_str::<Value>(text).ok()? {
            Value::String(s) => Some(SeenKey::Str(Cow::Owned(s))),
            other => Some(SeenKey::of(&other).into_owned()),
        }
    }

    fn into_owned(self) -> SeenKey<'static> {
        match self {
            SeenKey::Str(s) => SeenKey::Str(Cow::Owned(s.into_owned())),
            SeenKey::Json(s) => SeenKey::Json(s),
        }
    }
}

/// A writer that counts keys repeated within one batch (page) and passes the records on unchanged
///
/// The replication itself already stops a resource, with an error, on a record without a key and
/// on a timestamp older than the checkpoint or than the record before it (`$orderby` or `$filter`
/// ignored); this covers what it lets through. The same key on different pages is normal (a record
/// updated while the run was paging) and is not counted. Wrap the real writer; a flag-delete writer
/// can wrap this one.
pub struct CheckingWriter<'a> {
    inner: &'a mut dyn RecordWriter,
    key_field: String,
    duplicates: u64,
}

impl<'a> CheckingWriter<'a> {
    /// Wrap `inner`; `key_field` names the record key
    pub fn new(inner: &'a mut dyn RecordWriter, key_field: impl Into<String>) -> Self {
        Self {
            inner,
            key_field: key_field.into(),
            duplicates: 0,
        }
    }

    /// One line per problem seen so far (none when the data was clean)
    pub fn warnings(&self) -> Vec<String> {
        if self.duplicates == 0 {
            return Vec::new();
        }
        vec![format!(
            "{} records repeat a `{}` already in the same page",
            self.duplicates, self.key_field
        )]
    }
}

impl CheckingWriter<'_> {
    fn check(&mut self, records: &[Value]) {
        let mut seen = HashSet::with_capacity(records.len());
        for key in records
            .iter()
            .filter_map(|r| r.get(&self.key_field).filter(|k| !k.is_null()))
        {
            if !seen.insert(SeenKey::of(key)) {
                self.duplicates += 1;
            }
        }
    }

    /// [`check`](Self::check) on the records' text: reads only the key member of each
    fn check_raw(&mut self, raw: &RawRecords) {
        let mut seen = HashSet::with_capacity(raw.len());
        for i in 0..raw.len() {
            // The replication already found each record's key (no scan here); a record it let
            // through without the member has no key to compare
            let Ok(Some(key)) = raw.field(i, &self.key_field) else {
                continue;
            };
            if let Some(key) = SeenKey::of_raw(key) {
                if !seen.insert(key) {
                    self.duplicates += 1;
                }
            }
        }
    }
}

impl RecordWriter for CheckingWriter<'_> {
    fn write_batch(&mut self, records: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        self.check(records);
        self.inner.write_batch(records, fetched_at)
    }

    fn write_raw_batch(&mut self, raw: &RawRecords, fetched_at: &str) -> Result<(), IngestError> {
        self.check_raw(raw);
        self.inner.write_raw_batch(raw, fetched_at)
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

/// Compare `written` records of a full load with the server's own count
///
/// Asks `$count=true&$top=0` (with `filter`, the one the pull used) and returns a warning when the
/// numbers differ. `None` when they match, or when the server gives no count (many do not): that
/// is not a finding. Only meaningful after a pull from the beginning that finished without error.
pub async fn count_warning(
    client: &ResoClient,
    resource: &str,
    filter: Option<&str>,
    written: u64,
) -> Option<String> {
    let mut query = QueryBuilder::new(resource);
    if let Some(f) = filter {
        query = query.filter(f);
    }
    let query = query.count().build().ok()?;
    let count = match client.execute_count(&query).await {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!(resource, error = %e, "no server count to compare with");
            return None;
        }
    };
    (count != written).then(|| format!("{written} records written but the server counts {count}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_keys_borrow_strings_and_keep_string_and_number_apart() {
        let (s, n) = (serde_json::json!("1"), serde_json::json!(1));
        assert!(matches!(SeenKey::of(&s), SeenKey::Str(Cow::Borrowed("1"))));
        let mut seen = HashSet::new();
        assert!(seen.insert(SeenKey::of(&s)));
        assert!(
            seen.insert(SeenKey::of(&n)),
            "the number 1 is not the string \"1\""
        );
        assert!(!seen.insert(SeenKey::of(&n)));
    }

    #[test]
    fn seen_keys_from_text_compare_as_parsed_keys() {
        fn raw(t: &str) -> &RawValue {
            serde_json::from_str(t).unwrap()
        }
        let (plain, escaped, number, number_text, null) = (
            raw(r#""A/1""#),
            raw(r#""A\u002f1""#),
            raw("1"),
            raw(r#""1""#),
            raw("null"),
        );
        assert!(matches!(
            SeenKey::of_raw(plain),
            Some(SeenKey::Str(Cow::Borrowed("A/1")))
        ));
        assert!(SeenKey::of_raw(null).is_none());
        let mut seen = HashSet::new();
        assert!(seen.insert(SeenKey::of_raw(plain).unwrap()));
        assert!(
            !seen.insert(SeenKey::of_raw(escaped).unwrap()),
            "an escaped key is the same key"
        );
        assert!(seen.insert(SeenKey::of_raw(number).unwrap()));
        assert!(seen.insert(SeenKey::of_raw(number_text).unwrap()));
        assert!(!seen.insert(SeenKey::of(&serde_json::json!(1))));
    }

    fn snap(fields: &[(&str, &str, &str)], members: &[(&str, &str)]) -> SchemaSnapshot {
        let mut s = SchemaSnapshot::default();
        for (r, f, t) in fields {
            s.resources
                .entry((*r).into())
                .or_default()
                .insert((*f).into(), (*t).into());
        }
        for (e, m) in members {
            s.enums.entry((*e).into()).or_default().insert((*m).into());
        }
        s
    }

    #[test]
    fn lists_every_kind_of_change() {
        let old = snap(
            &[
                ("Property", "ListPrice", "Edm.Decimal"),
                ("Property", "OldField", "Edm.String"),
                ("Gone", "A", "Edm.String"),
            ],
            &[("Status", "Active"), ("Status", "Pending"), ("Old", "X")],
        );
        let new = snap(
            &[
                ("Property", "ListPrice", "Edm.String"),
                ("Property", "NewField", "Edm.Int32"),
                ("Member", "A", "Edm.String"),
            ],
            &[("Status", "Active"), ("Status", "Closed"), ("New", "Y")],
        );
        assert_eq!(
            old.changes_to(&new),
            [
                "Gone: resource removed",
                "Property.ListPrice: type Edm.Decimal -> Edm.String",
                "Property.OldField: field removed",
                "Property.NewField: field added (Edm.Int32)",
                "Member: resource added",
                "enum Old: removed",
                "enum Status: value Pending removed",
                "enum Status: value Closed added",
                "enum New: added",
            ]
        );
        assert!(old.changes_to(&old).is_empty());
    }
}
