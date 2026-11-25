//! Prometheus text for a run: freshness lag, volumes, request counts, duration, success
//!
//! The numbers come from the [`RunManifest`], so a one-shot run, the manifest file and the
//! metrics file agree. All values describe the run just finished ("last run"), except the
//! freshness lag, which is measured at render time against the newest `ModificationTimestamp`
//! written (the checkpoint). A resource that the run could not pull still reports its lag,
//! so a feed that keeps failing shows a growing number.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::IngestError;
use crate::manifest::{ResourceEntry, RunManifest};

/// Seconds since the Unix epoch for an RFC 3339 timestamp (`2026-11-24T17:30:05.123Z`, or with a
/// `+hh:mm` offset), or `None` if it does not parse
///
/// Fractions of a second are kept.
pub fn parse_rfc3339(text: &str) -> Option<f64> {
    let (date, rest) = text.trim().split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, offset) = match rest.find(['Z', 'z', '+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "Z"),
    };
    let mut c = clock.splitn(3, ':');
    let hour: i64 = c.next()?.parse().ok()?;
    let minute: i64 = c.next()?.parse().ok()?;
    let seconds: f64 = c.next().unwrap_or("0").parse().ok()?;
    if hour > 23 || minute > 59 || !(0.0..61.0).contains(&seconds) {
        return None;
    }
    let offset_secs = match offset {
        "Z" | "z" => 0,
        o => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let (h, m) = o[1..].split_once(':').unwrap_or((&o[1..], "0"));
            sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
        }
    };
    // Days since 1970-01-01 from the civil date (Howard Hinnant's algorithm)
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 - offset_secs) as f64 + seconds)
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// One metric family: name, type, help and samples
struct Family {
    name: &'static str,
    kind: &'static str,
    help: &'static str,
    samples: Vec<(String, f64)>,
}

impl Family {
    fn new(name: &'static str, help: &'static str) -> Self {
        Self {
            name,
            kind: "gauge",
            help,
            samples: Vec::new(),
        }
    }
}

fn labels(r: &ResourceEntry) -> String {
    format!(
        "server=\"{}\",resource=\"{}\"",
        escape(&r.server),
        escape(&r.resource)
    )
}

/// The checkpoint's `ModificationTimestamp`, in epoch seconds
fn checkpoint_secs(r: &ResourceEntry) -> Option<f64> {
    r.checkpoint
        .as_ref()?
        .get("timestamp")?
        .as_str()
        .and_then(parse_rfc3339)
}

/// The metrics of a finished run in the Prometheus text exposition format
///
/// `now` is the render time in epoch seconds; use [`render_prometheus`] for the current time. Names
/// start with `reso_ingest_`; per-resource samples carry `server` and `resource` labels. The list
/// is in `reso-ingest/README.md`.
pub fn render_prometheus_at(manifest: &RunManifest, now: f64) -> String {
    let mut lag = Family::new(
        "reso_ingest_freshness_lag_seconds",
        "Seconds between now and the newest ModificationTimestamp written (the checkpoint)",
    );
    let mut ok = Family::new(
        "reso_ingest_last_run_success",
        "1 if the resource's last pull finished without error, else 0",
    );
    let mut ok_at = Family::new(
        "reso_ingest_last_success_timestamp_seconds",
        "End of the last run in which the resource finished without error (absent after a failed run)",
    );
    let mut records = Family::new(
        "reso_ingest_last_run_records",
        "Records written in the last run",
    );
    let mut deletes = Family::new(
        "reso_ingest_last_run_deletes",
        "Delete events written in the last run",
    );
    let mut files = Family::new("reso_ingest_last_run_files", "Data files written");
    let mut file_bytes = Family::new(
        "reso_ingest_last_run_file_bytes",
        "Size in bytes of the data files written",
    );
    let mut duration = Family::new(
        "reso_ingest_last_run_duration_seconds",
        "Seconds the resource took in the last run",
    );
    let mut rate = Family::new(
        "reso_ingest_last_run_records_per_second",
        "Records written per second in the last run",
    );
    let mut requests = Family::new(
        "reso_ingest_last_run_requests",
        "HTTP requests sent in the last run, retries included",
    );
    let mut retries = Family::new(
        "reso_ingest_last_run_retries",
        "Requests repeated after a retryable failure in the last run",
    );
    let mut http_bytes = Family::new(
        "reso_ingest_last_run_http_bytes",
        "Response body bytes received in the last run",
    );
    let mut responses = Family::new(
        "reso_ingest_last_run_responses",
        "Responses received in the last run by status class (2xx includes 3xx; 429 is not in 4xx)",
    );
    let mut failed = Family::new(
        "reso_ingest_server_failed",
        "1 for a server that failed as a whole in the last run",
    );
    let mut finished = Family::new(
        "reso_ingest_last_run_finished_timestamp_seconds",
        "When the last run finished",
    );
    let mut resources = Family::new(
        "reso_ingest_last_run_resources",
        "Resources the last run handled",
    );

    let finished_at = manifest.finished_at.as_deref().and_then(parse_rfc3339);
    for r in &manifest.resources {
        let l = labels(r);
        if let Some(ts) = checkpoint_secs(r) {
            lag.samples.push((l.clone(), (now - ts).max(0.0)));
        }
        ok.samples
            .push((l.clone(), if r.error.is_none() { 1.0 } else { 0.0 }));
        if let (None, Some(t)) = (&r.error, finished_at) {
            ok_at.samples.push((l.clone(), t));
        }
        records.samples.push((l.clone(), r.records as f64));
        deletes.samples.push((l.clone(), r.deletes as f64));
        files.samples.push((l.clone(), r.files.len() as f64));
        file_bytes.samples.push((
            l.clone(),
            r.files.iter().map(|f| f.bytes).sum::<u64>() as f64,
        ));
        duration.samples.push((l.clone(), r.stats.duration_seconds));
        if r.stats.duration_seconds > 0.0 {
            rate.samples
                .push((l.clone(), r.records as f64 / r.stats.duration_seconds));
        }
        requests.samples.push((l.clone(), r.stats.requests as f64));
        retries.samples.push((l.clone(), r.stats.retries as f64));
        http_bytes
            .samples
            .push((l.clone(), r.stats.http_bytes as f64));
        for (class, n) in [
            ("2xx", r.stats.responses_ok),
            ("429", r.stats.responses_throttled),
            ("4xx", r.stats.responses_client_error),
            ("5xx", r.stats.responses_server_error),
        ] {
            responses
                .samples
                .push((format!("{l},class=\"{class}\""), n as f64));
        }
    }
    for f in &manifest.server_failures {
        failed
            .samples
            .push((format!("server=\"{}\"", escape(&f.server)), 1.0));
    }
    if let Some(t) = finished_at {
        finished.samples.push((String::new(), t));
    }
    resources
        .samples
        .push((String::new(), manifest.resources.len() as f64));

    let mut out = String::new();
    for f in [
        lag, ok, ok_at, records, deletes, files, file_bytes, duration, rate, requests, retries,
        http_bytes, responses, failed, finished, resources,
    ] {
        if f.samples.is_empty() {
            continue;
        }
        let _ = writeln!(out, "# HELP {} {}", f.name, f.help);
        let _ = writeln!(out, "# TYPE {} {}", f.name, f.kind);
        for (l, v) in &f.samples {
            if l.is_empty() {
                let _ = writeln!(out, "{} {v}", f.name);
            } else {
                let _ = writeln!(out, "{}{{{l}}} {v}", f.name);
            }
        }
    }
    out
}

/// [`render_prometheus_at`] for the current time
pub fn render_prometheus(manifest: &RunManifest) -> String {
    render_prometheus_at(manifest, now_secs())
}

/// Write [`render_prometheus`] to `path`, replacing the file atomically (a temporary file in the
/// same directory, then a rename), so a collector never reads half a file
///
/// # Errors
///
/// [`IngestError::Output`] when the directory or file cannot be written.
pub fn write_metrics_file(manifest: &RunManifest, path: &Path) -> Result<(), IngestError> {
    let err = |e: &dyn std::fmt::Display| IngestError::Output {
        path: path.display().to_string(),
        message: e.to_string(),
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| err(&e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    fs::write(&tmp, render_prometheus(manifest)).map_err(|e| err(&e))?;
    fs::rename(&tmp, path).map_err(|e| err(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_rfc3339("2026-11-24T17:30:05Z"), Some(1_795_541_405.0));
        assert_eq!(
            parse_rfc3339("2026-11-24T17:30:05.250Z"),
            Some(1_795_541_405.25)
        );
        assert_eq!(
            parse_rfc3339("2026-11-24T12:30:05-05:00"),
            Some(1_795_541_405.0)
        );
        assert_eq!(parse_rfc3339("2026-11-24T17:30:05"), Some(1_795_541_405.0));
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn escapes_label_values() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
