// src/standard_replication.rs

//! RESO-standard replication: walk a resource in `ModificationTimestamp` order
//!
//! Most RESO servers (MLS Grid, Trestle, Spark and RESO's reference server) have no special
//! replication endpoint. A full or incremental pull is an ordinary query:
//! `$filter=ModificationTimestamp gt <checkpoint>`, `$orderby=ModificationTimestamp,<Key>`, and
//! the server's `@odata.nextLink` for the next page. [`StandardReplication`] describes that
//! query and [`ReplicationRun`] walks it, keeping a [`Checkpoint`] (the timestamp and key of the
//! last record seen) that can be stored and used to resume later.
//!
//! Bridge Interactive's `{Resource}/replication` endpoint is a vendor mode and lives in
//! [`ReplicationQuery`](crate::ReplicationQuery) and
//! [`ResoClient::execute_replication`](crate::ResoClient::execute_replication).
//!
//! # Ties
//!
//! Many records can share one timestamp. A `gt <timestamp>` checkpoint would skip the rest of
//! them, and `ge` would repeat the ones already seen, so the checkpoint holds the key too and
//! the resume filter is `(TS gt X or (TS eq X and Key gt K))`, which is strict on the pair
//! (timestamp, key). The key therefore has to be a field the server can order and compare;
//! the caller names it (`ListingKey`, `MemberKey`, ...).
//!
//! # Server behaviour handled
//!
//! - **Next link in the body:** followed as given.
//! - **No next link, or a `$top` cap smaller than requested:** when a page has no next link
//!   the run asks again from its checkpoint and ends on an empty page. A full pull therefore
//!   costs one extra (empty) request, and a server that silently truncates pages is still
//!   read to the end.
//! - **`$orderby` or `$filter` ignored:** a record older than the one before it (or than the
//!   starting checkpoint) is an error, [`ResoError::OutOfOrder`], not silently accepted.
//!   Equal timestamps are not compared by key, because servers collate keys differently.
//!
//! Not confirmed against a real MLS: the crate's tests use invented data and a mock server (#32).

use std::borrow::Cow;
use std::fmt;

use serde_json::value::RawValue;
use serde_json::Value as JsonValue;
use tracing::debug;

use crate::client::ResoClient;
use crate::error::{ResoError, Result};
use crate::paging::{raw_str, record_fields, RawRecords};

/// Where a replication run stopped: the last record's timestamp and key
///
/// Store it after each batch and pass it to [`StandardReplication::from_checkpoint`] to resume.
///
/// # Examples
///
/// ```
/// # use reso_client::Checkpoint;
/// let cp = Checkpoint::new("2026-01-01T00:00:00.000Z", serde_json::json!("L100"));
/// assert_eq!(cp.timestamp, "2026-01-01T00:00:00.000Z");
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Checkpoint {
    /// `ModificationTimestamp` of the last record, exactly as the server sent it
    pub timestamp: String,
    /// The key of the last record, as the server sent it (string or number). `None` means
    /// "everything after this timestamp", which can skip records that share it.
    pub key: Option<JsonValue>,
}

impl Checkpoint {
    /// A checkpoint at a timestamp and key
    pub fn new(timestamp: impl Into<String>, key: JsonValue) -> Self {
        Self {
            timestamp: timestamp.into(),
            key: Some(key),
        }
    }

    /// A checkpoint at a timestamp only
    ///
    /// Use it to start from a date. Records that share this exact timestamp are skipped.
    pub fn at(timestamp: impl Into<String>) -> Self {
        Self {
            timestamp: timestamp.into(),
            key: None,
        }
    }
}

/// A RESO-standard replication request for one resource
///
/// # Examples
///
/// ```no_run
/// # use reso_client::{ResoClient, StandardReplication};
/// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
/// let spec = StandardReplication::new("Property", "ListingKey").page_size(1000);
/// let mut run = client.replicate(spec);
/// while let Some(batch) = run.next_batch().await? {
///     println!("{} records, checkpoint {:?}", batch.len(), run.checkpoint());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct StandardReplication {
    resource: String,
    key_field: String,
    timestamp_field: String,
    page_size: Option<u32>,
    filter: Option<String>,
    select: Option<Vec<String>>,
    expand: Option<Vec<String>>,
    from: Option<Checkpoint>,
}

impl StandardReplication {
    /// Replicate `resource`, ordering ties by `key_field` (for example `"ListingKey"`)
    pub fn new(resource: impl Into<String>, key_field: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            key_field: key_field.into(),
            timestamp_field: "ModificationTimestamp".to_string(),
            page_size: None,
            filter: None,
            select: None,
            expand: None,
            from: None,
        }
    }

    /// Use another timestamp field (default `ModificationTimestamp`)
    pub fn timestamp_field(mut self, field: impl Into<String>) -> Self {
        self.timestamp_field = field.into();
        self
    }

    /// Ask for this many records per page (`$top`); the server may return fewer
    pub fn page_size(mut self, n: u32) -> Self {
        self.page_size = Some(n);
        self
    }

    /// Add a filter, combined with the replication filter by `and`
    pub fn filter(mut self, expression: impl Into<String>) -> Self {
        self.filter = Some(expression.into());
        self
    }

    /// Return only these fields
    ///
    /// The key and timestamp fields are added when missing, because the checkpoint needs them.
    pub fn select(mut self, fields: &[&str]) -> Self {
        self.select = Some(fields.iter().map(|f| f.to_string()).collect());
        self
    }

    /// Expand these navigation properties (`$expand`) on every page, for example `&["Media"]`
    ///
    /// Some servers serve related records only this way (MLS Grid returns `Media` inside
    /// `Property`, `Member` and `Office`). Servers may allow fewer records per page with `$expand`;
    /// set [`page_size`](Self::page_size) to match. An empty list sends no `$expand`.
    pub fn expand(mut self, navigation_properties: &[&str]) -> Self {
        self.expand = Some(
            navigation_properties
                .iter()
                .map(|f| f.to_string())
                .collect(),
        );
        self
    }

    /// Start after a checkpoint instead of from the beginning
    pub fn from_checkpoint(mut self, checkpoint: Checkpoint) -> Self {
        self.from = Some(checkpoint);
        self
    }

    /// Path and query for the first request, or for a re-query from `checkpoint`
    fn path_and_query(&self, checkpoint: Option<&Checkpoint>) -> String {
        let ts = &self.timestamp_field;
        let key = &self.key_field;

        let mut filters = Vec::new();
        if let Some(user) = &self.filter {
            filters.push(format!("({user})"));
        }
        if let Some(cp) = checkpoint {
            filters.push(match &cp.key {
                Some(k) => {
                    let k = odata_literal(k);
                    format!(
                        "({ts} gt {t} or ({ts} eq {t} and {key} gt {k}))",
                        t = cp.timestamp
                    )
                }
                None => format!("{ts} gt {}", cp.timestamp),
            });
        }

        let mut params = Vec::new();
        if !filters.is_empty() {
            params.push(format!(
                "$filter={}",
                urlencoding::encode(&filters.join(" and "))
            ));
        }
        params.push(format!(
            "$orderby={}",
            urlencoding::encode(&format!("{ts},{key}"))
        ));
        if let Some(select) = &self.select {
            let mut fields = select.clone();
            for needed in [ts, key] {
                if !fields.iter().any(|f| f == needed) {
                    fields.push(needed.clone());
                }
            }
            params.push(format!(
                "$select={}",
                urlencoding::encode(&fields.join(","))
            ));
        }
        if let Some(expand) = self.expand.as_ref().filter(|e| !e.is_empty()) {
            params.push(format!(
                "$expand={}",
                urlencoding::encode(&expand.join(","))
            ));
        }
        if let Some(n) = self.page_size {
            params.push(format!("$top={n}"));
        }
        format!("{}?{}", self.resource, params.join("&"))
    }
}

/// An OData literal for a key: strings are quoted (`'` doubled), numbers are bare
fn odata_literal(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// A walk over one resource; get it from [`ResoClient::replicate`]
///
/// Call [`next_batch`](Self::next_batch) until it returns `None`. After each batch,
/// [`checkpoint`](Self::checkpoint) is the position to store.
pub struct ReplicationRun<'a> {
    client: &'a ResoClient,
    spec: StandardReplication,
    checkpoint: Option<Checkpoint>,
    /// Newest timestamp seen so far, as (seconds, nanoseconds) since the epoch
    newest: Option<(i64, u32)>,
    next_link: Option<String>,
    done: bool,
}

/// The key of one record as the order check reads it: a parsed value or the record's text
enum KeyRef<'r> {
    Value(&'r JsonValue),
    Raw(&'r RawValue),
}

impl KeyRef<'_> {
    fn to_value(&self) -> Result<JsonValue> {
        match self {
            KeyRef::Value(v) => Ok((*v).clone()),
            KeyRef::Raw(r) => serde_json::from_str(r.get())
                .map_err(|e| ResoError::Parse(format!("Failed to parse JSON: {e}"))),
        }
    }
}

impl fmt::Display for KeyRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyRef::Value(v) => write!(f, "{v}"),
            KeyRef::Raw(r) => f.write_str(r.get()),
        }
    }
}

impl ResoClient {
    /// Start a RESO-standard replication run (see [`StandardReplication`])
    pub fn replicate(&self, spec: StandardReplication) -> ReplicationRun<'_> {
        let checkpoint = spec.from.clone();
        let newest = checkpoint
            .as_ref()
            .and_then(|c| parse_timestamp(&c.timestamp));
        ReplicationRun {
            client: self,
            spec,
            checkpoint,
            newest,
            next_link: None,
            done: false,
        }
    }
}

impl ReplicationRun<'_> {
    /// The position after the last record returned, or the starting checkpoint
    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
    }

    /// Fetch the next batch of records, or `None` when the resource is exhausted
    ///
    /// Records are returned exactly as the server sent them.
    ///
    /// # Errors
    ///
    /// [`ResoError::OutOfOrder`] when the server ignored `$orderby` or `$filter`;
    /// [`ResoError::Parse`] when a record has no usable timestamp or key; and the errors of
    /// [`ResoClient::fetch_page`].
    pub async fn next_batch(&mut self) -> Result<Option<Vec<JsonValue>>> {
        Ok(self.next(false).await?.map(|(records, _)| records))
    }

    /// [`next_batch`](Self::next_batch) that returns each record's JSON text as the server sent
    /// it, without parsing the records into values
    ///
    /// For writers that store records unchanged: copying the text keeps the server's member order
    /// and number formatting, and skips building and dropping a `Value` per record, which is most
    /// of a sync's CPU time (#162). The order check and the checkpoint read only the timestamp
    /// and key members. Read other members with [`record_fields`] or parse
    /// whole records with [`RawRecords::values`].
    ///
    /// # Errors
    ///
    /// As [`next_batch`](Self::next_batch).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use reso_client::{ResoClient, StandardReplication};
    /// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut run = client.replicate(StandardReplication::new("Property", "ListingKey"));
    /// while let Some(records) = run.next_batch_raw().await? {
    ///     for line in records.iter() {
    ///         println!("{line}");
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn next_batch_raw(&mut self) -> Result<Option<RawRecords>> {
        Ok(self.next(true).await?.map(|(_, raw)| raw))
    }

    /// The next non-empty page: parsed records, or (with `keep_raw`) the records' text only
    async fn next(&mut self, keep_raw: bool) -> Result<Option<(Vec<JsonValue>, RawRecords)>> {
        let ts_field = self.spec.timestamp_field.clone();
        let key_field = self.spec.key_field.clone();
        while !self.done {
            let url = match self.next_link.take() {
                Some(link) => link,
                None => self
                    .client
                    .build_url(&self.spec.path_and_query(self.checkpoint.as_ref())),
            };
            let (page, mut raw) = if keep_raw {
                self.client.fetch_page_raw(&url).await?
            } else {
                (self.client.fetch_page(&url).await?, RawRecords::default())
            };

            if page.next_link.as_deref() == Some(page.url.as_str()) {
                return Err(ResoError::Parse(format!(
                    "Server returned the page's own URL as its next link: {}",
                    page.url
                )));
            }
            self.next_link = page.next_link.clone();

            let len = if keep_raw {
                raw.len()
            } else {
                page.records.len()
            };
            if len == 0 {
                // An empty page with a next link is odd but legal; follow it.
                self.done = self.next_link.is_none();
                continue;
            }

            if keep_raw {
                // The keys found here are kept, so a writer reading them again need not scan
                let base = raw.text_start();
                let mut keys = Vec::with_capacity(len);
                self.accept(raw.iter().map(|text| {
                    let fields = fields_of_text(text, &ts_field, &key_field)?;
                    if let KeyRef::Raw(key) = fields.1 {
                        let start = key.get().as_ptr() as usize - base;
                        keys.push(start..start + key.get().len());
                    }
                    Ok(fields)
                }))?;
                raw.remember(&key_field, keys);
            } else {
                self.accept(
                    page.records
                        .iter()
                        .map(|record| fields_of_value(record, &ts_field, &key_field)),
                )?;
            }
            debug!(
                "replication batch of {} records, checkpoint {:?}",
                len, self.checkpoint
            );
            return Ok(Some((page.records, raw)));
        }
        Ok(None)
    }

    /// Check a page's order and move the checkpoint to its last record
    ///
    /// `records` gives each record's timestamp text and key, or the error for a record without
    /// them.
    fn accept<'r>(
        &mut self,
        records: impl Iterator<Item = Result<(Cow<'r, str>, KeyRef<'r>)>>,
    ) -> Result<()> {
        let ts_field = &self.spec.timestamp_field;
        let key_field = &self.spec.key_field;
        let start_key = self.checkpoint.as_ref().and_then(|c| c.key.clone());
        let start_ts = self.checkpoint.as_ref().map(|c| c.timestamp.clone());

        let mut last: Option<(Cow<'r, str>, KeyRef<'r>)> = None;
        for record in records {
            let (ts_text, key) = record?;
            let ts = parse_timestamp(&ts_text).ok_or_else(|| {
                ResoError::Parse(format!(
                    "{ts_field} is not an RFC 3339 timestamp: {ts_text}"
                ))
            })?;

            if let Some(newest) = self.newest {
                if ts < newest {
                    return Err(ResoError::OutOfOrder(format!(
                        "{ts_field} {ts_text} came after a newer record: the server ignored \
                         $orderby or $filter"
                    )));
                }
            }
            // A page that starts at the checkpoint itself would loop forever.
            if start_ts.as_deref() == Some(&*ts_text)
                && start_key.is_some()
                && start_key == Some(key.to_value()?)
            {
                return Err(ResoError::OutOfOrder(format!(
                    "{key_field} {key} at {ts_text} was returned again after the checkpoint"
                )));
            }
            self.newest = Some(ts);
            last = Some((ts_text, key));
        }

        if let Some((timestamp, key)) = last {
            self.checkpoint = Some(Checkpoint {
                timestamp: timestamp.into_owned(),
                key: Some(key.to_value()?),
            });
        }
        Ok(())
    }
}

/// The timestamp text and key of a parsed record
fn fields_of_value<'r>(
    record: &'r JsonValue,
    ts_field: &str,
    key_field: &str,
) -> Result<(Cow<'r, str>, KeyRef<'r>)> {
    let ts = record
        .get(ts_field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| no_timestamp(ts_field))?;
    let key = record
        .get(key_field)
        .filter(|v| v.is_string() || v.is_number())
        .ok_or_else(|| no_key(key_field))?;
    Ok((Cow::Borrowed(ts), KeyRef::Value(key)))
}

/// The timestamp text and key of a record's JSON text, read without parsing the rest
fn fields_of_text<'r>(
    text: &'r str,
    ts_field: &str,
    key_field: &str,
) -> Result<(Cow<'r, str>, KeyRef<'r>)> {
    let [ts, key] = record_fields(text, [ts_field, key_field])?;
    let ts = ts.and_then(raw_str).ok_or_else(|| no_timestamp(ts_field))?;
    let key = key
        .filter(|k| {
            k.get()
                .as_bytes()
                .first()
                .is_some_and(|c| *c == b'"' || *c == b'-' || c.is_ascii_digit())
        })
        .ok_or_else(|| no_key(key_field))?;
    Ok((ts, KeyRef::Raw(key)))
}

fn no_timestamp(field: &str) -> ResoError {
    ResoError::Parse(format!("Record has no {field} string"))
}

fn no_key(field: &str) -> ResoError {
    ResoError::Parse(format!("Record has no string or number {field}"))
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)` into (seconds, nanoseconds) since
/// the Unix epoch, so timestamps in different offsets compare correctly
fn parse_timestamp(text: &str) -> Option<(i64, u32)> {
    let b = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let s = text.get(from..from + len)?;
        if s.bytes().all(|c| c.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, s) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }

    let mut i = 19;
    let mut nanos = 0u32;
    if b[i] == b'.' {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let digits = &text[start..i.min(start + 9)];
        nanos = format!("{digits:0<9}").parse().ok()?;
    }

    let offset = match text.get(i..)? {
        "Z" | "z" => 0,
        rest if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) => {
            let oh = num(i + 1, 2)?;
            let om = num(i + 4, 2)?;
            if b[i + 3] != b':' {
                return None;
            }
            let secs = oh * 3600 + om * 60;
            if rest.starts_with('-') {
                -secs
            } else {
                secs
            }
        }
        _ => return None,
    };

    // Days from civil date (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some((days * 86_400 + h * 3600 + mi * 60 + s - offset, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn first_request_orders_by_timestamp_and_key() {
        let q = StandardReplication::new("Property", "ListingKey")
            .page_size(500)
            .path_and_query(None);
        assert_eq!(
            q,
            "Property?$orderby=ModificationTimestamp%2CListingKey&$top=500"
        );
    }

    #[test]
    fn resume_filter_is_strict_on_timestamp_and_key() {
        let cp = Checkpoint::new("2026-01-01T00:00:00.000Z", json!("L'1"));
        let q = StandardReplication::new("Property", "ListingKey").path_and_query(Some(&cp));
        let filter = urlencoding::decode(
            q.split('&')
                .next()
                .unwrap()
                .trim_start_matches("Property?$filter="),
        )
        .unwrap()
        .into_owned();
        assert_eq!(
            filter,
            "(ModificationTimestamp gt 2026-01-01T00:00:00.000Z or \
             (ModificationTimestamp eq 2026-01-01T00:00:00.000Z and ListingKey gt 'L''1'))"
        );
    }

    #[test]
    fn numeric_key_is_not_quoted_and_user_filter_is_combined() {
        let cp = Checkpoint::new("2026-01-01T00:00:00Z", json!(42));
        let q = StandardReplication::new("Member", "MemberKey")
            .filter("MemberStatus eq 'Active'")
            .path_and_query(Some(&cp));
        let decoded = urlencoding::decode(&q).unwrap();
        assert!(decoded.contains("(MemberStatus eq 'Active') and (ModificationTimestamp gt"));
        assert!(decoded.contains("MemberKey gt 42))"));
    }

    #[test]
    fn timestamp_only_checkpoint_uses_gt() {
        let q = StandardReplication::new("Property", "ListingKey")
            .path_and_query(Some(&Checkpoint::at("2026-01-01T00:00:00Z")));
        assert!(urlencoding::decode(&q)
            .unwrap()
            .contains("$filter=ModificationTimestamp gt 2026-01-01T00:00:00Z&"));
    }

    #[test]
    fn select_gains_key_and_timestamp() {
        let q = StandardReplication::new("Property", "ListingKey")
            .select(&["City", "ListingKey"])
            .path_and_query(None);
        assert!(urlencoding::decode(&q)
            .unwrap()
            .contains("$select=City,ListingKey,ModificationTimestamp"));
    }

    #[test]
    fn expand_is_sent_on_first_request_and_requery() {
        let spec = StandardReplication::new("Property", "ListingKey")
            .filter("OriginatingSystemName eq 'mls'")
            .expand(&["Media", "Rooms"])
            .page_size(1000);
        assert_eq!(
            spec.path_and_query(None),
            "Property?$filter=%28OriginatingSystemName%20eq%20%27mls%27%29\
             &$orderby=ModificationTimestamp%2CListingKey&$expand=Media%2CRooms&$top=1000"
        );
        let requery = spec.path_and_query(Some(&Checkpoint::at("2026-01-01T00:00:00Z")));
        let decoded = urlencoding::decode(&requery).unwrap();
        assert!(decoded
            .contains("$filter=(OriginatingSystemName eq 'mls') and ModificationTimestamp gt"));
        assert!(decoded.contains("&$expand=Media,Rooms&$top=1000"));
    }

    #[test]
    fn empty_expand_sends_nothing() {
        let q = StandardReplication::new("Property", "ListingKey")
            .expand(&[])
            .path_and_query(None);
        assert!(!q.contains("$expand"));
    }

    #[test]
    fn timestamps_compare_across_offsets_and_fractions() {
        let a = parse_timestamp("2026-01-01T00:00:00.5Z").unwrap();
        let b = parse_timestamp("2026-01-01T00:00:00.500Z").unwrap();
        let c = parse_timestamp("2025-12-31T19:00:00.500-05:00").unwrap();
        let d = parse_timestamp("2026-01-01T00:00:01Z").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a < d);
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some((0, 0)));
        assert_eq!(parse_timestamp("1970-01-01T01:00:00+01:00"), Some((0, 0)));
        assert_eq!(parse_timestamp("1970-01-01t00:00:00z"), Some((0, 0)));
        assert_eq!(
            parse_timestamp("2000-03-01T00:00:00Z"),
            Some((951_868_800, 0))
        );
    }

    #[test]
    fn bad_timestamps_are_rejected() {
        for t in [
            "",
            "2026-01-01",
            "2026-13-01T00:00:00Z",
            "2026-01-01T00:00:00",
            "2026-01-01 00:00:00Z",
            "2026-01-01T00-00-00Z",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00+0500",
            "2026-01-01T00:00:00+05-00",
            "2026-01-01T00:00:00+aa:00",
            "2026-01-01T00:00:00UTC",
            "2026-01-0aT00:00:00Z",
        ] {
            assert_eq!(parse_timestamp(t), None, "{t}");
        }
    }
}
