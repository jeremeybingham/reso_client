//! Read-only HTML status pages of `serve --ui` (#161)
//!
//! Hand-written HTML with one inline stylesheet, no scripts and no external assets; each page
//! reloads every 15 seconds. Every value from configuration, a server or the state is escaped
//! ([`html`]): error messages come from remote servers.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::{FeedStatus, RunSummary, Shared};
use crate::metrics::{checkpoint_secs, parse_rfc3339};

/// How many runs `/runs` lists
const RUNS_PAGE: usize = 100;

/// The uid of `deploy/observability/grafana/dashboards/reso-ingest-serve.json`
const GRAFANA_DASHBOARD: &str = "reso-ingest-serve";

/// The page routes, merged into the endpoint router when `ui` is on
pub(super) fn routes() -> Router<Arc<Shared>> {
    Router::new()
        .route("/", get(index))
        .route("/feed/{server}/{resource}", get(feed))
        .route("/runs", get(runs))
}

/// `text` with `& < > " '` replaced by entities, safe in element text and quoted attributes
fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `text` as one URL path segment: every byte but `A-Z a-z 0-9 - . _ ~` percent-encoded
fn segment(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn feed_link(server: &str, resource: &str) -> String {
    format!(
        "<a href=\"/feed/{}/{}\">{}/{}</a>",
        segment(server),
        segment(resource),
        html(server),
        html(resource)
    )
}

/// The kit's dashboard (`deploy/observability`) filtered to one feed, as a link; `None` without
/// a Grafana URL
fn grafana_link(base: Option<&str>, server: &str, resource: &str) -> Option<String> {
    base.map(|base| {
        let url = format!(
            "{base}/d/{GRAFANA_DASHBOARD}?var-server={}&var-resource={}",
            segment(server),
            segment(resource)
        );
        format!("<a href=\"{}\" rel=\"noreferrer\">Grafana</a>", html(&url))
    })
}

/// A duration as `850 ms`, `12.3 s`, `4m 05s`, `2h 07m` or `3d 04h`
fn span(d: Duration) -> String {
    let s = d.as_secs();
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else if s < 60 {
        format!("{:.1} s", d.as_secs_f64())
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {:02}h", s / 86_400, (s % 86_400) / 3600)
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// An RFC 3339 time as `<time title="2026-…">4m 05s ago</time>`, or `–` when there is none
fn ago(time: Option<&str>) -> String {
    let Some(t) = time else {
        return "–".into();
    };
    match parse_rfc3339(t) {
        Some(secs) => format!(
            "<time datetime=\"{0}\" title=\"{0}\">{1} ago</time>",
            html(t),
            span(Duration::from_secs_f64((now_secs() - secs).max(0.0)))
        ),
        None => html(t),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Seconds between now and the feed's checkpoint (`reso_ingest_freshness_lag_seconds`)
fn freshness_lag(f: &FeedStatus) -> String {
    f.last_entry
        .as_ref()
        .and_then(checkpoint_secs)
        .map_or("–".into(), |t| {
            span(Duration::from_secs_f64((now_secs() - t).max(0.0)))
        })
}

fn next_run(f: &FeedStatus, now: Instant) -> String {
    match (f.state, f.next_at) {
        ("running", _) => "now".into(),
        ("queued", _) => "waiting for a slot".into(),
        (_, Some(t)) => format!("in {}", span(t.saturating_duration_since(now))),
        _ => "–".into(),
    }
}

/// The row class: `fail` for a failing feed, `paused`, or none
fn row_class(f: &FeedStatus) -> &'static str {
    if f.consecutive_failures > 0 {
        " class=\"fail\""
    } else if f.state == "paused" {
        " class=\"paused\""
    } else {
        ""
    }
}

const STYLE: &str = "body{font:14px/1.4 system-ui,sans-serif;margin:1.5em;color:#1b1b1b;background:#fff}\
h1{font-size:1.3em;margin:0 0 .3em}h2{font-size:1.1em;margin:1.4em 0 .4em}\
nav{margin-bottom:1em}nav a{margin-right:1em}\
table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:.3em .6em;border-bottom:1px solid #ddd;vertical-align:top}\
th{background:#f3f3f3}td.n{text-align:right;font-variant-numeric:tabular-nums}\
tr.fail{background:#fde8e8}tr.paused{background:#f0f0f0;color:#666}\
.summary{margin:.4em 0 1em}.bad{color:#b00020;font-weight:600}\
pre{background:#f6f6f6;padding:.6em;white-space:pre-wrap;word-break:break-word}\
dl{display:grid;grid-template-columns:max-content auto;gap:.2em 1em}dt{font-weight:600}dd{margin:0}\
@media (prefers-color-scheme:dark){body{color:#e4e4e4;background:#151515}th{background:#262626}\
th,td{border-color:#333}tr.fail{background:#4a1d1d}tr.paused{background:#222;color:#999}\
pre{background:#222}.bad{color:#ff7a8a}a{color:#8ab4f8}}";

fn page(title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta http-equiv=\"refresh\" content=\"15\"><title>{} · reso-ingest</title><style>{STYLE}</style></head>\
<body><nav><a href=\"/\">Feeds</a><a href=\"/runs\">Runs</a><a href=\"/status\">JSON</a>\
<a href=\"/metrics\">Metrics</a></nav>\n{body}</body></html>\n",
        html(title)
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

async fn index(State(shared): State<Arc<Shared>>) -> Response {
    let feeds = shared.snapshot();
    let now = Instant::now();
    let failing = feeds.iter().filter(|f| f.consecutive_failures > 0).count();
    let paused = feeds.iter().filter(|f| f.state == "paused").count();
    let reload = shared.reload.lock().map(|r| r.clone()).unwrap_or_default();

    let mut body = String::from("<h1>reso-ingest serve</h1>\n<p class=\"summary\">");
    let _ = write!(
        body,
        "{} feeds, <span{}>{failing} failing</span>, {paused} paused. Started {}. ",
        feeds.len(),
        if failing > 0 { " class=\"bad\"" } else { "" },
        ago(Some(&shared.started_at)),
    );
    match &reload.last_reload {
        Some(t) => {
            let _ = write!(body, "Last configuration reload {} ", ago(Some(t)));
            let _ = write!(body, "({} since start).", reload.reloads);
        }
        None => body.push_str("No configuration reload yet."),
    }
    if let Some(e) = &reload.last_error {
        let _ = write!(
            body,
            " <span class=\"bad\">Last reload failed: {}</span>",
            html(e)
        );
    }
    let grafana = shared.grafana_url.as_deref();
    body.push_str(
        "</p>\n<table><thead><tr><th>Feed</th><th>State</th><th>Last run</th><th>Records</th>\
<th>Took</th><th>Result</th><th>Failures in a row</th><th>Next run</th><th>Freshness lag</th>",
    );
    if grafana.is_some() {
        body.push_str("<th>History</th>");
    }
    body.push_str("</tr></thead><tbody>\n");
    for f in &feeds {
        let last = f.recent.back();
        let result = match last {
            Some(r) if r.outcome == "failed" => "<span class=\"bad\">failed</span>".to_string(),
            Some(r) => r.outcome.to_string(),
            None => "–".into(),
        };
        let history = grafana_link(grafana, &f.server, &f.resource)
            .map(|a| format!("<td>{a}</td>"))
            .unwrap_or_default();
        let _ = writeln!(
            body,
            "<tr{}><td>{}</td><td>{}</td><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td>\
<td>{result}</td><td class=\"n\">{}</td><td>{}</td><td class=\"n\">{}</td>{history}</tr>",
            row_class(f),
            feed_link(&f.server, &f.resource),
            f.state,
            ago(f.last_finished.as_deref()),
            last.map_or("–".into(), |r| r.records.to_string()),
            last.map_or("–".into(), |r| span(r.duration)),
            f.consecutive_failures,
            next_run(f, now),
            freshness_lag(f),
        );
    }
    body.push_str("</tbody></table>\n");
    page("Feeds", &body)
}

fn runs_table(body: &mut String, runs: &[(&FeedStatus, &RunSummary)], with_feed: bool) {
    body.push_str("<table><thead><tr>");
    if with_feed {
        body.push_str("<th>Feed</th>");
    }
    body.push_str(
        "<th>Started</th><th>Took</th><th>Result</th><th>Records</th><th>Deletes</th>\
<th>Received</th><th>Run id</th><th>Error</th></tr></thead><tbody>\n",
    );
    for (f, r) in runs {
        let failed = r.outcome == "failed";
        let _ = write!(body, "<tr{}>", if failed { " class=\"fail\"" } else { "" });
        if with_feed {
            let _ = write!(body, "<td>{}</td>", feed_link(&f.server, &f.resource));
        }
        let _ = writeln!(
            body,
            "<td>{}</td><td class=\"n\">{}</td><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td>\
<td class=\"n\">{}</td><td>{}</td><td>{}</td></tr>",
            ago(Some(&r.started)),
            span(r.duration),
            r.outcome,
            r.records,
            r.deletes,
            bytes(r.bytes),
            html(&r.run_id),
            r.error.as_deref().map(html).unwrap_or_default(),
        );
    }
    body.push_str("</tbody></table>\n");
}

async fn feed(
    State(shared): State<Arc<Shared>>,
    Path((server, resource)): Path<(String, String)>,
) -> Response {
    let found = shared
        .feeds
        .lock()
        .ok()
        .and_then(|m| m.get(&(server.clone(), resource.clone())).cloned());
    let Some(f) = found else {
        let body = format!(
            "<h1>No such feed</h1><p>{}/{} is not scheduled or paused in this daemon.</p>",
            html(&server),
            html(&resource)
        );
        let mut r = page("No such feed", &body);
        *r.status_mut() = StatusCode::NOT_FOUND;
        return r;
    };
    let name = format!("{}/{}", f.server, f.resource);
    let mut body = format!("<h1>{}</h1>\n<dl>", html(&name));
    let mut item = |k: &str, v: String| {
        let _ = write!(body, "<dt>{k}</dt><dd>{v}</dd>");
    };
    item("State", f.state.into());
    item("Interval", span(f.interval));
    item("Next run", next_run(&f, Instant::now()));
    item(
        "Runs since start",
        format!("{} ({} failed)", f.runs, f.failed_runs),
    );
    item("Failures in a row", f.consecutive_failures.to_string());
    item("Last success", ago(f.last_success.as_deref()));
    item("Freshness lag", freshness_lag(&f));
    if let Some(a) = grafana_link(shared.grafana_url.as_deref(), &f.server, &f.resource) {
        item("History", a);
    }
    if let Some(m) = &f.media {
        item(
            "Media files",
            format!(
                "{} downloaded, {} unchanged, {} failed",
                m.downloaded, m.unchanged, m.failed
            ),
        );
    }
    body.push_str("</dl>\n<h2>Last error</h2>\n");
    match &f.last_error {
        Some(e) => {
            let _ = writeln!(body, "<pre>{}</pre>", html(e));
        }
        None => body.push_str("<p>None.</p>\n"),
    }
    body.push_str("<h2>Checkpoint</h2>\n");
    match f.last_entry.as_ref().and_then(|e| e.checkpoint.as_ref()) {
        Some(c) => {
            let text = serde_json::to_string_pretty(c).unwrap_or_default();
            let _ = writeln!(body, "<pre>{}</pre>", html(&text));
        }
        None => body.push_str("<p>Not known yet: no run since start reached the server.</p>\n"),
    }
    let _ = writeln!(
        body,
        "<h2>Recent runs</h2>\n<p>The last {} runs since the daemon started, newest first.</p>",
        super::RECENT_RUNS
    );
    let runs: Vec<(&FeedStatus, &RunSummary)> = f.recent.iter().rev().map(|r| (&f, r)).collect();
    runs_table(&mut body, &runs, false);
    page(&name, &body)
}

async fn runs(State(shared): State<Arc<Shared>>) -> Response {
    let feeds = shared.snapshot();
    let mut all: Vec<(&FeedStatus, &RunSummary)> = feeds
        .iter()
        .flat_map(|f| f.recent.iter().map(move |r| (f, r)))
        .collect();
    // RFC 3339 UTC times from one clock sort as text
    all.sort_by(|a, b| b.1.started.cmp(&a.1.started));
    all.truncate(RUNS_PAGE);
    let mut body = format!(
        "<h1>Recent runs</h1>\n<p>The latest {RUNS_PAGE} runs of every feed since the daemon started \
(at most {} per feed), newest first. Longer history is in Prometheus and the run manifests.</p>\n",
        super::RECENT_RUNS
    );
    runs_table(&mut body, &all, true);
    page("Runs", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escapes_markup_and_quotes() {
        assert_eq!(
            html(r#"<script>alert("x")</script> & 'y'"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;y&#39;"
        );
    }

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(segment("Prop erty/a?b"), "Prop%20erty%2Fa%3Fb");
        assert_eq!(segment("mls-1_x.y~z"), "mls-1_x.y~z");
        assert_eq!(segment("é"), "%C3%A9");
    }

    #[test]
    fn grafana_links_filter_the_dashboard_to_one_feed() {
        assert_eq!(grafana_link(None, "mls", "Property"), None);
        assert_eq!(
            grafana_link(Some("http://g:3000/grafana"), "my mls", "Prop&\"x").unwrap(),
            "<a href=\"http://g:3000/grafana/d/reso-ingest-serve?var-server=my%20mls&amp;\
var-resource=Prop%26%22x\" rel=\"noreferrer\">Grafana</a>"
        );
    }

    #[test]
    fn spans_and_sizes_read_well() {
        assert_eq!(span(Duration::from_millis(850)), "850 ms");
        assert_eq!(span(Duration::from_millis(12_340)), "12.3 s");
        assert_eq!(span(Duration::from_secs(245)), "4m 05s");
        assert_eq!(span(Duration::from_secs(7620)), "2h 07m");
        assert_eq!(span(Duration::from_secs(273_600)), "3d 04h");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
