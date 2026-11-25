//! The daemon: a schedule per feed, endpoints for health and metrics
//!
//! [`Daemon::start`] runs one task per scheduled feed (a server and resource with an interval). A
//! feed syncs, waits its interval plus a little jitter, and syncs again; a failing feed waits
//! longer each time (doubling, up to 15 minutes or its own interval when that is longer). At most
//! `concurrency` feeds of one server sync at the same time, and all feeds of a server share one
//! client, so its rate limit and token cover them together. Every sync is the same code as
//! `reso-ingest sync` for that one resource: it takes the feed's lease, writes tagged files, saves
//! the checkpoint after each page and stops between pages on a [`Shutdown`].
//!
//! The endpoints (plain HTTP, no authentication: bind to a private address):
//!
//! | Path | Answers |
//! |---|---|
//! | `/healthz` | 200 while the process is up |
//! | `/readyz` | 200 when the state directory can be written, else 503 |
//! | `/metrics` | Prometheus text: the last result of every feed, plus the daemon's own numbers |
//! | `/status` | JSON: every feed's state, last run, failures and time to the next run |

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::{mpsc::unbounded_channel, Semaphore};
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::audit::AccessLog;
use crate::cli::{sync_server_with, Format, SyncOptions};
use crate::config::{Config, ServerConfig, MAX_BACKOFF};
use crate::error::IngestError;
use crate::manifest::{ResourceEntry, RunManifest};
use crate::metrics::{escape, render_prometheus};
use crate::output::now_rfc3339;
use crate::runs;
use crate::secrets::ClientHandle;
use crate::shutdown::Shutdown;
use crate::state::{FileStateStore, StateStore};

/// How `serve` runs
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Address for the endpoints (port 0 picks a free one; see [`Daemon::local_addr`])
    pub listen: SocketAddr,
    /// Output directory for data files and manifests
    pub out: PathBuf,
    /// Where checkpoints, leases and media indexes are kept
    pub state: Arc<dyn StateStore>,
    /// Also detect deleted records on every sync
    pub deletes: bool,
    /// Write deletes even when they pass the configured limits
    pub allow_mass_deletes: bool,
    /// Where finished data files go; `None` leaves them in `out`
    pub sink: Option<Arc<dyn crate::sink::FileSink>>,
    /// Download media files of each page into this directory
    pub media_dir: Option<PathBuf>,
    /// Start a new data file after this many bytes
    pub max_file_bytes: u64,
    /// Data file format
    pub format: Format,
}

impl ServeOptions {
    /// Defaults: listen on `127.0.0.1:8080`, write under `out`, state in `out/state`, NDJSON
    pub fn new(out: impl Into<PathBuf>) -> Self {
        let out = out.into();
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            state: Arc::new(FileStateStore::new(out.join("state"))),
            out,
            sink: None,
            deletes: false,
            allow_mass_deletes: false,
            media_dir: None,
            max_file_bytes: 256 * 1024 * 1024,
            format: Format::Ndjson,
        }
    }
}

/// What the daemon knows about one feed
#[derive(Debug, Clone)]
struct FeedStatus {
    server: String,
    resource: String,
    interval: Duration,
    /// `queued`, `running` or `waiting`
    state: &'static str,
    runs: u64,
    failed_runs: u64,
    consecutive_failures: u32,
    last_started: Option<String>,
    last_finished: Option<String>,
    last_success: Option<String>,
    last_error: Option<String>,
    last_records: u64,
    last_deletes: u64,
    next_at: Option<Instant>,
    /// The result of the last sync that reached the server, for the metrics
    last_entry: Option<ResourceEntry>,
}

struct Shared {
    feeds: Mutex<BTreeMap<(String, String), FeedStatus>>,
    state: Arc<dyn StateStore>,
    started_at: String,
}

impl Shared {
    fn update(&self, key: &(String, String), f: impl FnOnce(&mut FeedStatus)) {
        if let Ok(mut feeds) = self.feeds.lock() {
            if let Some(feed) = feeds.get_mut(key) {
                f(feed);
            }
        }
    }

    fn snapshot(&self) -> Vec<FeedStatus> {
        self.feeds
            .lock()
            .map(|f| f.values().cloned().collect())
            .unwrap_or_default()
    }
}

/// One server to schedule: its settings, the shared client and its feeds with intervals
type Plan = (ServerConfig, Arc<ClientHandle>, Vec<(String, Duration)>);

/// A running daemon
///
/// Dropping it does not stop it; trigger the [`Shutdown`] given to [`start`](Self::start) and
/// [`wait`](Self::wait).
pub struct Daemon {
    addr: SocketAddr,
    tasks: JoinSet<()>,
}

impl Daemon {
    /// Build one client per server, bind the endpoints and start every feed's schedule
    ///
    /// # Errors
    ///
    /// [`IngestError`] when no server has a scheduled resource, a client cannot be built (for
    /// example a variable that holds a secret is not set) or the address cannot be bound.
    pub async fn start(
        config: Config,
        options: ServeOptions,
        shutdown: Shutdown,
    ) -> Result<Self, IngestError> {
        let mut plans: Vec<Plan> = Vec::new();
        for server in &config.servers {
            let feeds = server.feeds();
            if feeds.is_empty() {
                tracing::info!(server = %server.name, "no resource has an interval: server not scheduled");
                continue;
            }
            let client = ClientHandle::new(server)?;
            plans.push((server.clone(), Arc::new(client), feeds));
        }
        if plans.is_empty() {
            return Err(IngestError::Parse(
                "serve needs at least one resource with an interval (`interval` on a resource, or on a server with `include`)"
                    .into(),
            ));
        }
        // Fail now rather than at the first sync when state cannot be saved
        probe(options.state.as_ref()).await?;

        let shared = Arc::new(Shared {
            feeds: Mutex::new(BTreeMap::new()),
            state: Arc::clone(&options.state),
            started_at: now_rfc3339(),
        });
        let listener = tokio::net::TcpListener::bind(options.listen)
            .await
            .map_err(|e| IngestError::Output {
                path: options.listen.to_string(),
                message: format!("cannot listen: {e}"),
            })?;
        let addr = listener.local_addr().map_err(|e| IngestError::Output {
            path: options.listen.to_string(),
            message: e.to_string(),
        })?;

        let mut tasks = JoinSet::new();
        let options = Arc::new(options);
        for (server, client, feeds) in plans {
            let permits = Arc::new(Semaphore::new(server.concurrency.unwrap_or(2).max(1)));
            for (resource, interval) in feeds {
                let key = (server.name.clone(), resource.clone());
                if let Ok(mut map) = shared.feeds.lock() {
                    map.insert(
                        key.clone(),
                        FeedStatus {
                            server: server.name.clone(),
                            resource: resource.clone(),
                            interval,
                            state: "waiting",
                            runs: 0,
                            failed_runs: 0,
                            consecutive_failures: 0,
                            last_started: None,
                            last_finished: None,
                            last_success: None,
                            last_error: None,
                            last_records: 0,
                            last_deletes: 0,
                            next_at: None,
                            last_entry: None,
                        },
                    );
                }
                let span = tracing::info_span!("feed", server = %server.name, resource = %resource);
                tasks.spawn(
                    run_feed(
                        server.clone(),
                        resource,
                        interval,
                        Arc::clone(&client),
                        Arc::clone(&permits),
                        Arc::clone(&options),
                        Arc::clone(&shared),
                        shutdown.clone(),
                    )
                    .instrument(span),
                );
            }
        }

        let app = Router::new()
            .route("/healthz", get(|| async { "ok\n" }))
            .route("/readyz", get(readyz))
            .route("/metrics", get(metrics))
            .route("/status", get(status))
            .with_state(shared);
        let stop = shutdown.clone();
        tasks.spawn(async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(async move { stop.triggered().await })
                .await;
            if let Err(e) = served {
                tracing::warn!(error = %e, "endpoint server stopped");
            }
        });
        tracing::info!(%addr, "serve started");
        Ok(Self { addr, tasks })
    }

    /// The address the endpoints listen on
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Wait until the [`Shutdown`] is triggered and every feed has stopped
    ///
    /// A feed in the middle of a sync stops after its current page, with checkpoint and output
    /// consistent, as `sync` does.
    pub async fn wait(mut self) {
        while self.tasks.join_next().await.is_some() {}
    }
}

/// A pseudo-random duration in `0..max` (jitter does not need a random number crate)
fn jitter(max: Duration, seed: &str, n: u64) -> Duration {
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    n.hash(&mut h);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    let max_ms = max.as_millis().max(1) as u64;
    Duration::from_millis(h.finish() % max_ms)
}

/// The wait after a sync: the interval plus up to 10% jitter, or after failures the interval
/// doubled for each one in a row, up to [`MAX_BACKOFF`] (or the interval, when it is longer)
fn next_delay(interval: Duration, failures: u32, seed: &str, n: u64) -> Duration {
    let base = if failures == 0 {
        interval
    } else {
        interval
            .saturating_mul(1u32 << failures.min(10))
            .min(MAX_BACKOFF.max(interval))
    };
    base + jitter(base / 10, seed, n)
}

#[allow(clippy::too_many_arguments)]
async fn run_feed(
    server: ServerConfig,
    resource: String,
    interval: Duration,
    client: Arc<ClientHandle>,
    permits: Arc<Semaphore>,
    options: Arc<ServeOptions>,
    shared: Arc<Shared>,
    shutdown: Shutdown,
) {
    let key = (server.name.clone(), resource.clone());
    let seed = format!("{}/{}", key.0, key.1);
    // Feeds do not all start at the same moment
    let mut wait = jitter(interval.min(Duration::from_secs(10)), &seed, 0);
    loop {
        shared.update(&key, |f| {
            f.state = "waiting";
            f.next_at = Some(Instant::now() + wait);
        });
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = shutdown.triggered() => return,
        }
        shared.update(&key, |f| {
            f.state = "queued";
            f.next_at = None;
        });
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => p.ok(),
            () = shutdown.triggered() => return,
        };
        shared.update(&key, |f| {
            f.state = "running";
            f.last_started = Some(now_rfc3339());
        });
        let outcome = sync_once(&server, &resource, &client, &options, &shutdown).await;
        drop(permit);

        let mut runs = 0;
        let mut failures = 0;
        shared.update(&key, |f| {
            f.runs += 1;
            f.last_finished = Some(now_rfc3339());
            match &outcome {
                Outcome::Done(entry) => {
                    f.consecutive_failures = 0;
                    f.last_error = None;
                    f.last_success = f.last_finished.clone();
                    f.last_records = entry.records;
                    f.last_deletes = entry.deletes;
                    f.last_entry = Some((**entry).clone());
                }
                Outcome::Skipped => {}
                Outcome::Failed(message, entry) => {
                    f.failed_runs += 1;
                    f.consecutive_failures += 1;
                    f.last_error = Some(message.clone());
                    if let Some(e) = entry {
                        f.last_records = e.records;
                        f.last_deletes = e.deletes;
                        f.last_entry = Some((**e).clone());
                    }
                }
            }
            runs = f.runs;
            failures = f.consecutive_failures;
        });
        if shutdown.is_triggered() {
            return;
        }
        wait = next_delay(interval, failures, &seed, runs);
        if failures > 0 {
            tracing::warn!(
                failures,
                retry_in_secs = wait.as_secs_f64(),
                "feed failing: backing off"
            );
        }
    }
}

enum Outcome {
    /// The sync finished without an error
    Done(Box<ResourceEntry>),
    /// Another process holds the feed; nothing was done
    Skipped,
    /// The sync failed; the entry is there when the server was reached
    Failed(String, Option<Box<ResourceEntry>>),
}

/// One sync of one feed, written like `reso-ingest sync --resource R --server S`
async fn sync_once(
    server: &ServerConfig,
    resource: &str,
    client: &ClientHandle,
    options: &ServeOptions,
    shutdown: &Shutdown,
) -> Outcome {
    let sink = options.sink.as_deref();
    let run_id = match runs::reserve(&options.out, options.state.as_ref(), sink).await {
        Ok(id) => id,
        Err(e) => return Outcome::Failed(e.to_string(), None),
    };
    let sync_options = Arc::new(SyncOptions {
        out: options.out.clone(),
        state: Arc::clone(&options.state),
        run_id: run_id.clone(),
        resources: vec![resource.to_owned()],
        deletes: options.deletes,
        allow_mass_deletes: options.allow_mass_deletes,
        media: options.media_dir.clone(),
        start_over: false,
        max_file_bytes: options.max_file_bytes,
        format: options.format,
        sink: options.sink.clone(),
        shutdown: shutdown.clone(),
        access: AccessLog::new(&options.out),
    });
    // The lines `sync` prints as progress are in the log already
    let (tx, rx) = unbounded_channel::<String>();
    drop(rx);
    let span = tracing::info_span!("run", run_id = %run_id);
    let result = sync_server_with(client, server.clone(), Arc::clone(&sync_options), tx)
        .instrument(span)
        .await;

    let mut manifest = RunManifest::new(&run_id);
    let outcome = match result {
        Err(message) => {
            manifest.add_server_failure(&server.name, &message);
            Outcome::Failed(message, None)
        }
        Ok((entries, _media, changes)) => {
            manifest.add_schema_changes(&server.name, changes);
            let entry = entries.into_iter().find(|e| e.resource == resource);
            if let Some(e) = &entry {
                manifest.add_resource(e.clone());
            }
            match entry {
                None => Outcome::Failed("the resource was not synced".into(), None),
                Some(e) if e.error.is_some() => {
                    Outcome::Failed(e.error.clone().unwrap_or_default(), Some(Box::new(e)))
                }
                Some(e) if e.interrupted => Outcome::Done(Box::new(e)),
                Some(e) if e.stats.requests == 0 && e.records == 0 && !e.warnings.is_empty() => {
                    Outcome::Skipped
                }
                Some(e) => Outcome::Done(Box::new(e)),
            }
        }
    };
    // An unchanged feed is polled every few minutes: keep a manifest only for a run that wrote
    // something or went wrong, so the output directory does not fill with empty ones
    let eventful = manifest.interrupted
        || !manifest.is_clean()
        || !manifest.schema_changes.is_empty()
        || manifest
            .resources
            .iter()
            .any(|r| r.records > 0 || r.deletes > 0 || !r.warnings.is_empty());
    let mut delivered = true;
    if eventful {
        let written = match manifest.write(&options.out) {
            Ok(p) => runs::deliver_manifest(&p, options.state.as_ref(), sink).await,
            Err(e) => Err(e),
        };
        if let Err(e) = written {
            tracing::warn!(error = %e, "cannot write the manifest");
            delivered = false;
        }
        // With a bucket, the run's access-log lines go next to its manifest
        if let Err(e) = sync_options.access.deliver(&options.out, &run_id, sink) {
            tracing::error!(error = %e, "cannot deliver the access log");
        }
    }
    if delivered {
        runs::release(options.state.as_ref(), sink, &run_id).await;
    }
    let marker = options.out.join(format!(".run.{}", sanitize_id(&run_id)));
    let _ = std::fs::remove_file(marker);
    outcome
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write and remove a probe value: can state be saved?
async fn probe(store: &dyn StateStore) -> Result<(), IngestError> {
    store.put(".readyz", b"ok").await?;
    store.delete(".readyz", None).await.map(|_| ())
}

async fn readyz(State(shared): State<Arc<Shared>>) -> (StatusCode, String) {
    match probe(shared.state.as_ref()).await {
        Ok(()) => (StatusCode::OK, "ready\n".into()),
        Err(e) => {
            tracing::warn!(error = %e, "not ready: state cannot be saved");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "not ready: state cannot be saved\n".into(),
            )
        }
    }
}

async fn metrics(
    State(shared): State<Arc<Shared>>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    let feeds = shared.snapshot();
    let mut manifest = RunManifest::new("serve");
    manifest.started_at = shared.started_at.clone();
    manifest.resources = feeds.iter().filter_map(|f| f.last_entry.clone()).collect();
    let mut text = render_prometheus(&manifest);
    let mut family = |name: &str, kind: &str, help: &str, value: &dyn Fn(&FeedStatus) -> u64| {
        text.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        for f in &feeds {
            text.push_str(&format!(
                "{name}{{server=\"{}\",resource=\"{}\"}} {}\n",
                escape(&f.server),
                escape(&f.resource),
                value(f)
            ));
        }
    };
    family(
        "reso_ingest_feed_runs_total",
        "counter",
        "Syncs of the feed since the daemon started",
        &|f| f.runs,
    );
    family(
        "reso_ingest_feed_failed_runs_total",
        "counter",
        "Syncs of the feed that failed since the daemon started",
        &|f| f.failed_runs,
    );
    family(
        "reso_ingest_feed_consecutive_failures",
        "gauge",
        "Failed syncs in a row (0 after a success)",
        &|f| u64::from(f.consecutive_failures),
    );
    family(
        "reso_ingest_feed_interval_seconds",
        "gauge",
        "Configured time between syncs of the feed",
        &|f| f.interval.as_secs(),
    );
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
}

async fn status(
    State(shared): State<Arc<Shared>>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    let now = Instant::now();
    let feeds: Vec<Value> = shared
        .snapshot()
        .into_iter()
        .map(|f| {
            json!({
                "server": f.server,
                "resource": f.resource,
                "interval_secs": f.interval.as_secs_f64(),
                "state": f.state,
                "runs": f.runs,
                "failed_runs": f.failed_runs,
                "consecutive_failures": f.consecutive_failures,
                "last_started": f.last_started,
                "last_finished": f.last_finished,
                "last_success": f.last_success,
                "last_error": f.last_error,
                "last_records": f.last_records,
                "last_deletes": f.last_deletes,
                "next_run_in_secs": f.next_at.map(|t| t.saturating_duration_since(now).as_secs_f64()),
            })
        })
        .collect();
    let body = json!({"started_at": shared.started_at, "feeds": feeds});
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&body).unwrap_or_default() + "\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_grows_with_failures_and_is_capped() {
        let i = Duration::from_secs(60);
        let d0 = next_delay(i, 0, "s", 1);
        assert!(d0 >= i && d0 < i + i / 10);
        let d2 = next_delay(i, 2, "s", 1);
        assert!(d2 >= Duration::from_secs(240));
        let d9 = next_delay(i, 9, "s", 1);
        assert!(d9 >= MAX_BACKOFF && d9 <= MAX_BACKOFF + MAX_BACKOFF / 10);
        // an interval longer than the cap is never shortened
        let long = Duration::from_secs(3600);
        assert!(next_delay(long, 3, "s", 1) >= long);
    }

    #[test]
    fn jitter_stays_below_the_maximum() {
        for n in 0..50 {
            assert!(jitter(Duration::from_millis(100), "x", n) < Duration::from_millis(100));
        }
        assert_eq!(jitter(Duration::ZERO, "x", 1), Duration::ZERO);
    }
}
