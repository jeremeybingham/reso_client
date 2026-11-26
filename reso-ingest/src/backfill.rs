//! Backfill mode (`sync --backfill`, #88): the first pull of a large feed, with progress, its own
//! share of the rate limits, and optional time windows
//!
//! A backfill is a `sync` that:
//!
//! - asks the server's count of the records left before it starts (`$count`, where the server
//!   has it) and prints `##progress` lines with the records done, the records left and the time
//!   left after every page; a `backfill progress` log line at most every 30 seconds; and a
//!   `backfill` block in the manifest entry and metrics;
//! - holds a marker lease (`<resource>~backfill`, with the window in the name) while it runs, so
//!   other syncs of the same server (another process, or `serve`) see it: they use the rest of the
//!   rate limits (`[server.backfill] rate_percent`) and do not start a full load of a feed a
//!   backfill is loading;
//! - with `--from`/`--to`, loads only the records whose `ModificationTimestamp` is in
//!   `[from, to)`, with a checkpoint of its own, so a backfill can be split across workers or
//!   redone for a period. A window with no `--to` ends at the present; when it finishes and the feed
//!   has no checkpoint yet, its checkpoint becomes the feed's, so incremental syncs carry on from it.

use std::time::{Duration, Instant};

use reso_client::{Checkpoint, QueryBuilder, RawRecords, ResoClient};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::IngestError;
use crate::manifest::FileEntry;
use crate::metrics::parse_rfc3339;
use crate::output::RecordWriter;

/// Marks the lease of a running backfill: `<resource>~backfill`, then ` <from>..<to>` for a window
const MARKER: &str = "~backfill";

/// How often a running backfill logs its progress
const LOG_EVERY: Duration = Duration::from_secs(30);

/// The `ModificationTimestamp` range of a backfill; both ends open means the whole feed
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillWindow {
    /// First timestamp included (RFC 3339)
    pub from: Option<String>,
    /// First timestamp no longer included (RFC 3339)
    pub to: Option<String>,
}

impl BackfillWindow {
    /// A window from `--from` and `--to`
    ///
    /// # Errors
    ///
    /// [`IngestError::Parse`] when a bound is not an RFC 3339 timestamp with a zone, or `from` is
    /// not before `to`.
    pub fn new(from: Option<String>, to: Option<String>) -> Result<Self, IngestError> {
        let check = |name: &str, v: &Option<String>| -> Result<Option<f64>, IngestError> {
            match v {
                None => Ok(None),
                Some(t) if has_zone(t) => parse_rfc3339(t).map(Some).ok_or_else(|| {
                    IngestError::Parse(format!("--{name} `{t}` is not an RFC 3339 timestamp"))
                }),
                Some(t) => Err(IngestError::Parse(format!(
                    "--{name} `{t}` needs a time zone (`Z` or `+hh:mm`), e.g. 2024-01-01T00:00:00Z"
                ))),
            }
        };
        if let (Some(f), Some(t)) = (check("from", &from)?, check("to", &to)?) {
            if f >= t {
                return Err(IngestError::Parse("--from must be before --to".into()));
            }
        }
        Ok(Self { from, to })
    }

    /// True when the window is the whole feed (no `--from`, no `--to`)
    pub fn is_whole(&self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    /// The `$filter` term for the window, or `None` for the whole feed
    pub fn filter(&self, timestamp_field: &str) -> Option<String> {
        let mut terms = Vec::new();
        if let Some(f) = &self.from {
            terms.push(format!("{timestamp_field} ge {f}"));
        }
        if let Some(t) = &self.to {
            terms.push(format!("{timestamp_field} lt {t}"));
        }
        (!terms.is_empty()).then(|| terms.join(" and "))
    }

    /// The name of the marker lease of a backfill of `resource` in this window
    pub fn marker(&self, resource: &str) -> String {
        format!("{resource}{}", self.suffix())
    }

    /// Added to the resource name of the window's checkpoint; empty for the whole feed, which uses
    /// the feed's own checkpoint
    pub fn checkpoint_suffix(&self) -> String {
        if self.is_whole() {
            String::new()
        } else {
            self.suffix()
        }
    }

    fn suffix(&self) -> String {
        if self.is_whole() {
            MARKER.to_string()
        } else {
            format!(
                "{MARKER} {}..{}",
                self.from.as_deref().unwrap_or(""),
                self.to.as_deref().unwrap_or("")
            )
        }
    }
}

/// `2024-01-01T00:00:00Z` or `...+01:00`: the part after the date has a zone
fn has_zone(t: &str) -> bool {
    t.split_once(['T', 't'])
        .is_some_and(|(_, time)| time.contains(['Z', 'z', '+', '-']))
}

/// Whether the lease name `leased` is the marker of a backfill (of `resource`, when given)
pub(crate) fn is_marker(leased: &str, resource: Option<&str>) -> bool {
    match resource {
        None => leased.contains(MARKER),
        Some(r) => leased
            .strip_prefix(r)
            .is_some_and(|rest| rest == MARKER || rest.starts_with(&format!("{MARKER} "))),
    }
}

/// The filter that continues after `cp`, as replication sends it (strictly after the timestamp and key)
pub(crate) fn after_checkpoint(cp: &Checkpoint, timestamp_field: &str, key_field: &str) -> String {
    let t = &cp.timestamp;
    match &cp.key {
        Some(k) => {
            let k = match k {
                Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                other => other.to_string(),
            };
            format!(
                "({timestamp_field} gt {t} or ({timestamp_field} eq {t} and {key_field} gt {k}))"
            )
        }
        None => format!("{timestamp_field} gt {t}"),
    }
}

/// The server's count of `resource` records matching `filters` (joined by `and`), or `None` when it
/// gives none
pub(crate) async fn count(client: &ResoClient, resource: &str, filters: &[String]) -> Option<u64> {
    let mut query = QueryBuilder::new(resource);
    if !filters.is_empty() {
        let joined: Vec<String> = filters.iter().map(|f| format!("({f})")).collect();
        query = query.filter(joined.join(" and "));
    }
    let query = query.count().build().ok()?;
    match client.execute_count(&query).await {
        Ok(n) => Some(n),
        Err(e) => {
            tracing::info!(resource, error = %e, "no server count: backfill progress without a total");
            None
        }
    }
}

/// What a backfill did, in the manifest entry of its resource
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackfillEntry {
    /// `--from`, when given
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// `--to`, when given
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The server's count of the records left when this run started (`None`: no `$count`)
    pub remaining_at_start: Option<u64>,
    /// Percent of the rate limits the backfill ran with
    pub rate_percent: u8,
}

impl BackfillEntry {
    /// Records still to load after this run, by the count at its start (`None` without a count)
    pub fn remaining(&self, written: u64) -> Option<u64> {
        self.remaining_at_start.map(|n| n.saturating_sub(written))
    }
}

/// A writer that reports a backfill's progress after each page: `##progress` lines on `tx`
/// and a log line now and then
pub(crate) struct ProgressWriter<'a> {
    inner: &'a mut dyn RecordWriter,
    tx: UnboundedSender<String>,
    /// `##progress step=N/T label=server/resource`
    prefix: String,
    remaining: Option<u64>,
    done: u64,
    started: Instant,
    logged: Instant,
}

impl<'a> ProgressWriter<'a> {
    pub(crate) fn new(
        inner: &'a mut dyn RecordWriter,
        tx: UnboundedSender<String>,
        prefix: String,
        remaining: Option<u64>,
    ) -> Self {
        let now = Instant::now();
        Self {
            inner,
            tx,
            prefix,
            remaining,
            done: 0,
            started: now,
            logged: now,
        }
    }

    fn report(&mut self) {
        let elapsed = self.started.elapsed().as_secs_f64();
        let eta = self.remaining.and_then(|total| {
            let left = total.saturating_sub(self.done);
            (self.done > 0 && elapsed > 0.0)
                .then(|| Duration::from_secs_f64(left as f64 * elapsed / self.done as f64))
        });
        let mut line = match self.remaining {
            Some(total) if total > 0 => {
                format!("{} sub={}/{total}", self.prefix, self.done.min(total))
            }
            _ => format!("{} msg={}_records", self.prefix, self.done),
        };
        if let Some(eta) = eta {
            line.push_str(&format!(" eta={}", short_duration(eta)));
        }
        let _ = self.tx.send(line);
        if self.logged.elapsed() >= LOG_EVERY {
            self.logged = Instant::now();
            tracing::info!(
                done = self.done,
                remaining = self.remaining.map(|t| t.saturating_sub(self.done)),
                eta_secs = eta.map(|d| d.as_secs()),
                "backfill progress"
            );
        }
    }
}

/// `45s`, `12m05s`, `3h07m`, `2d04h`
pub(crate) fn short_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{:02}h", s / 86_400, s % 86_400 / 3600),
    }
}

impl RecordWriter for ProgressWriter<'_> {
    fn write_batch(&mut self, records: &[Value], fetched_at: &str) -> Result<(), IngestError> {
        self.inner.write_batch(records, fetched_at)?;
        self.done += records.len() as u64;
        self.report();
        Ok(())
    }

    fn write_raw_batch(&mut self, raw: &RawRecords, fetched_at: &str) -> Result<(), IngestError> {
        self.inner.write_raw_batch(raw, fetched_at)?;
        self.done += raw.len() as u64;
        self.report();
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
    use serde_json::json;

    #[test]
    fn windows_are_checked() {
        assert!(BackfillWindow::new(None, None).unwrap().is_whole());
        let w = BackfillWindow::new(
            Some("2024-01-01T00:00:00Z".into()),
            Some("2025-01-01T00:00:00+01:00".into()),
        )
        .unwrap();
        assert_eq!(
            w.filter("ModificationTimestamp").unwrap(),
            "ModificationTimestamp ge 2024-01-01T00:00:00Z and ModificationTimestamp lt 2025-01-01T00:00:00+01:00"
        );
        for (from, to) in [
            (Some("2024-01-01"), None),
            (Some("2024-01-01T00:00:00"), None),
            (None, Some("yesterday")),
            (Some("2025-01-01T00:00:00Z"), Some("2024-01-01T00:00:00Z")),
            (Some("2025-01-01T00:00:00Z"), Some("2025-01-01T00:00:00Z")),
        ] {
            assert!(
                BackfillWindow::new(from.map(Into::into), to.map(Into::into)).is_err(),
                "{from:?} {to:?}"
            );
        }
    }

    #[test]
    fn markers_and_checkpoint_names() {
        let whole = BackfillWindow::default();
        assert_eq!(whole.marker("Property"), "Property~backfill");
        assert_eq!(whole.checkpoint_suffix(), "");
        assert_eq!(whole.filter("T"), None);
        let from = BackfillWindow::new(Some("2024-01-01T00:00:00Z".into()), None).unwrap();
        assert_eq!(
            from.marker("Property"),
            "Property~backfill 2024-01-01T00:00:00Z.."
        );
        assert_eq!(from.checkpoint_suffix(), "~backfill 2024-01-01T00:00:00Z..");

        assert!(is_marker("Property~backfill", Some("Property")));
        assert!(is_marker(&from.marker("Property"), Some("Property")));
        assert!(!is_marker("PropertyRooms~backfill", Some("Property")));
        assert!(!is_marker("Property", Some("Property")));
        assert!(is_marker("Member~backfill", None));
        assert!(!is_marker("Member", None));
    }

    #[test]
    fn continues_after_the_checkpoint() {
        let cp = Checkpoint::new("2026-01-01T00:00:00Z", json!("L'1"));
        assert_eq!(
            after_checkpoint(&cp, "T", "K"),
            "(T gt 2026-01-01T00:00:00Z or (T eq 2026-01-01T00:00:00Z and K gt 'L''1'))"
        );
        let cp = Checkpoint::new("2026-01-01T00:00:00Z", json!(7));
        assert!(after_checkpoint(&cp, "T", "K").ends_with("K gt 7))"));
        assert_eq!(
            after_checkpoint(&Checkpoint::at("2026-01-01T00:00:00Z"), "T", "K"),
            "T gt 2026-01-01T00:00:00Z"
        );
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(short_duration(Duration::from_secs(45)), "45s");
        assert_eq!(short_duration(Duration::from_secs(725)), "12m05s");
        assert_eq!(short_duration(Duration::from_secs(3 * 3600 + 420)), "3h07m");
        assert_eq!(
            short_duration(Duration::from_secs(2 * 86_400 + 4 * 3600)),
            "2d04h"
        );
    }

    #[test]
    fn remaining_never_goes_below_zero() {
        let e = BackfillEntry {
            from: None,
            to: None,
            remaining_at_start: Some(10),
            rate_percent: 50,
        };
        assert_eq!(e.remaining(4), Some(6));
        assert_eq!(e.remaining(12), Some(0));
    }
}
