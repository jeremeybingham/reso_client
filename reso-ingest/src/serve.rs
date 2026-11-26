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
//! | `/status` | JSON: every feed's state (`paused` included), last run, failures, time to the next run, and the configuration reloads |
//! | `/` | With [`ServeOptions::ui`]: an HTML page with one row per feed |
//! | `/feed/{server}/{resource}` | With `ui`: one feed's last error, checkpoint and recent runs |
//! | `/runs` | With `ui`: the latest runs of every feed |
//!
//! The pages (Cargo feature `ui`, on by default) are read-only HTML with no scripts that reload
//! every 15 seconds. They show what `/status` shows, plus each feed's last
//! [`RECENT_RUNS`] runs, which are kept in memory and lost on restart.
//!
//! [`Daemon::reload`] applies a changed configuration while the daemon runs (the command does so
//! on SIGHUP and with `--reload-every`).

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, VecDeque};
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
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::audit::AccessLog;
use crate::cli::{sync_server_with, Format, SyncOptions};
use crate::config::{Config, ResourceConfig, ServerConfig, MAX_BACKOFF};
use crate::error::IngestError;
use crate::manifest::{ResourceEntry, RunManifest};
use crate::media::MediaReport;
use crate::metrics::{escape, render_media, render_prometheus, MediaCounts};
use crate::output::now_rfc3339;
use crate::runs;
use crate::secrets::ClientHandle;
use crate::shutdown::Shutdown;
use crate::state::{FileStateStore, StateStore};

#[cfg(feature = "ui")]
mod ui;

/// How many runs of each feed the daemon keeps in memory for the status pages
pub const RECENT_RUNS: usize = 20;

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
    /// Download the media files of each page into this directory or bucket (`sync --media`)
    pub media: Option<crate::media_store::MediaRoot>,
    /// Start a new data file after this many bytes
    pub max_file_bytes: u64,
    /// Data file format
    pub format: Format,
    /// Hive-style partition folders for Parquet files (`--partition`)
    pub partition: bool,
    /// Also serve the read-only status pages (`/`, `/feed/{server}/{resource}`, `/runs`); needs
    /// the `ui` feature
    pub ui: bool,
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
            media: None,
            max_file_bytes: 256 * 1024 * 1024,
            format: Format::Ndjson,
            partition: false,
            ui: false,
        }
    }
}

/// What the daemon knows about one feed
#[derive(Debug, Clone)]
struct FeedStatus {
    server: String,
    resource: String,
    interval: Duration,
    /// Bumped each time the feed's task is replaced, so a stopped task's late updates are dropped
    generation: u64,
    /// `queued`, `running`, `waiting` or `paused`
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
    /// Media files of the feed's syncs added up (with `media` set)
    media: Option<MediaCounts>,
    /// The last [`RECENT_RUNS`] runs, oldest first
    recent: VecDeque<RunSummary>,
}

/// One finished run of a feed, for the status pages
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "ui"), allow(dead_code))]
struct RunSummary {
    run_id: String,
    started: String,
    duration: Duration,
    /// `ok`, `interrupted`, `skipped` (another process holds the feed) or `failed`
    outcome: &'static str,
    records: u64,
    deletes: u64,
    /// Response body bytes received
    bytes: u64,
    error: Option<String>,
}

impl FeedStatus {
    fn new(server: &str, resource: &str, interval: Duration) -> Self {
        Self {
            server: server.to_owned(),
            resource: resource.to_owned(),
            interval,
            generation: 0,
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
            media: None,
            recent: VecDeque::new(),
        }
    }
}

/// The configuration reloads since start, for `/status`
#[derive(Debug, Clone, Default)]
struct ReloadInfo {
    reloads: u64,
    last_reload: Option<String>,
    last_error: Option<String>,
}

type FeedKey = (String, String);

struct Shared {
    feeds: Mutex<BTreeMap<FeedKey, FeedStatus>>,
    /// Each scheduled server's client, for its rate-limit use in `/metrics`
    clients: Mutex<BTreeMap<String, Arc<ClientHandle>>>,
    reload: Mutex<ReloadInfo>,
    state: Arc<dyn StateStore>,
    started_at: String,
}

impl Shared {
    /// Change the feed's status, unless its task has been replaced since `generation`
    fn update(&self, key: &FeedKey, generation: u64, f: impl FnOnce(&mut FeedStatus)) {
        if let Ok(mut feeds) = self.feeds.lock() {
            if let Some(feed) = feeds.get_mut(key).filter(|s| s.generation == generation) {
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

/// What a [`Daemon::reload`] changed, feed by feed (`server/resource` names)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadSummary {
    /// Feeds that were not scheduled before and now are
    pub added: Vec<String>,
    /// Feeds that are no longer in the configuration (or no longer have an interval)
    pub removed: Vec<String>,
    /// Feeds stopped and started again because their server's settings or their interval changed
    pub restarted: Vec<String>,
    /// Feeds set to `paused`
    pub paused: Vec<String>,
    /// Paused feeds scheduled again
    pub resumed: Vec<String>,
}

impl ReloadSummary {
    /// True when the reload changed no feed
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.restarted.is_empty()
            && self.paused.is_empty()
            && self.resumed.is_empty()
    }
}

/// A running feed task and the stop request only it listens to
struct FeedRun {
    interval: Duration,
    stop: Shutdown,
    handle: JoinHandle<()>,
}

/// One scheduled server: the settings its feeds run with, its shared client and its feeds
struct ServerRun {
    config: ServerConfig,
    client: Arc<ClientHandle>,
    permits: Arc<Semaphore>,
    feeds: BTreeMap<String, FeedRun>,
}

/// The feed tasks, changed by [`Daemon::reload`]
struct Controller {
    options: Arc<ServeOptions>,
    shared: Arc<Shared>,
    shutdown: Shutdown,
    servers: BTreeMap<String, ServerRun>,
    /// Stopped tasks that may still be finishing a page
    retired: Vec<JoinHandle<()>>,
}

/// The server's settings without what only schedules feeds (intervals and `paused`): when only
/// these differ the server's client and its other feeds keep running
fn sync_settings(server: &ServerConfig) -> ServerConfig {
    let mut s = server.clone();
    s.interval = None;
    s.paused = false;
    for r in s.resources.values_mut() {
        r.interval = None;
        r.paused = false;
    }
    s.resources.retain(|_, r| *r != ResourceConfig::default());
    s
}

fn permits_for(server: &ServerConfig) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(server.concurrency.unwrap_or(2).max(1)))
}

fn label(server: &str, resource: &str) -> String {
    format!("{server}/{resource}")
}

impl Controller {
    /// Make the running feeds match `config`; a server whose client cannot be built keeps its
    /// running feeds as they were and is named in the error list
    fn apply(&mut self, config: &Config) -> (ReloadSummary, Vec<String>) {
        let mut summary = ReloadSummary::default();
        let mut errors = Vec::new();
        let wanted: BTreeMap<String, &ServerConfig> =
            config.servers.iter().map(|s| (s.name.clone(), s)).collect();

        let gone: Vec<String> = self
            .servers
            .keys()
            .filter(|n| !wanted.contains_key(*n))
            .cloned()
            .collect();
        for name in gone {
            if let Some(run) = self.servers.remove(&name) {
                for (resource, feed) in run.feeds {
                    self.retire(feed);
                    summary.removed.push(label(&name, &resource));
                }
            }
            if let Ok(mut c) = self.shared.clients.lock() {
                c.remove(&name);
            }
        }

        for (name, server) in &wanted {
            let active = server.feeds();
            let paused = server.paused_feeds();
            let rebuild = self
                .servers
                .get(name)
                .is_none_or(|run| sync_settings(&run.config) != sync_settings(server));
            let old = self.servers.remove(name);
            let client = match &old {
                _ if active.is_empty() => None,
                Some(run) if !rebuild => Some(Arc::clone(&run.client)),
                _ => match ClientHandle::new(server) {
                    Ok(c) => Some(Arc::new(c)),
                    Err(e) => {
                        errors.push(format!("server `{name}`: {e}"));
                        if let Some(run) = old {
                            self.servers.insert(name.clone(), run);
                        }
                        continue;
                    }
                },
            };
            let (mut old_feeds, permits) = match old {
                Some(run) if !rebuild => (run.feeds, run.permits),
                Some(run) => (run.feeds, permits_for(server)),
                None => (BTreeMap::new(), permits_for(server)),
            };
            let mut run = client.map(|client| ServerRun {
                config: (*server).clone(),
                client,
                permits,
                feeds: BTreeMap::new(),
            });
            if let Some(run) = &mut run {
                for (resource, interval) in &active {
                    let feed = match old_feeds.remove(resource) {
                        Some(f) if !rebuild && f.interval == *interval => f,
                        Some(f) => {
                            f.stop.trigger();
                            summary.restarted.push(label(name, resource));
                            self.spawn_feed(run, resource, *interval, Some(f.handle))
                        }
                        None => {
                            if self.status_state(name, resource) == Some("paused") {
                                summary.resumed.push(label(name, resource));
                            } else {
                                summary.added.push(label(name, resource));
                            }
                            self.spawn_feed(run, resource, *interval, None)
                        }
                    };
                    run.feeds.insert(resource.clone(), feed);
                }
            }
            for (resource, feed) in old_feeds {
                self.retire(feed);
                if !paused.iter().any(|(r, _)| *r == resource) {
                    summary.removed.push(label(name, &resource));
                }
            }
            // Paused feeds have no task; /status lists them
            for (resource, interval) in &paused {
                let before = self.status_state(name, resource);
                if before.is_some() && before != Some("paused") {
                    summary.paused.push(label(name, resource));
                }
                if let Ok(mut map) = self.shared.feeds.lock() {
                    let entry = map
                        .entry((name.clone(), resource.clone()))
                        .or_insert_with(|| FeedStatus::new(name, resource, *interval));
                    if entry.state != "paused" {
                        entry.generation += 1;
                        entry.state = "paused";
                        entry.next_at = None;
                    }
                    entry.interval = *interval;
                }
            }
            if let Ok(mut c) = self.shared.clients.lock() {
                match &run {
                    Some(run) => c.insert(name.clone(), Arc::clone(&run.client)),
                    None => c.remove(name),
                };
            }
            if let Some(run) = run {
                self.servers.insert(name.clone(), run);
            }
        }

        // Forget the status of feeds that are neither scheduled nor paused
        let servers = &self.servers;
        if let Ok(mut map) = self.shared.feeds.lock() {
            map.retain(|(server, resource), _| {
                servers
                    .get(server)
                    .is_some_and(|run| run.feeds.contains_key(resource))
                    || wanted
                        .get(server)
                        .is_some_and(|s| s.paused_feeds().iter().any(|(r, _)| r == resource))
            });
        }
        self.retired.retain(|h| !h.is_finished());
        (summary, errors)
    }

    fn status_state(&self, server: &str, resource: &str) -> Option<&'static str> {
        self.shared
            .feeds
            .lock()
            .ok()?
            .get(&(server.to_owned(), resource.to_owned()))
            .map(|f| f.state)
    }

    /// Ask a feed to stop after its current page; it is awaited by [`Daemon::wait`]
    fn retire(&mut self, feed: FeedRun) {
        feed.stop.trigger();
        self.retired.push(feed.handle);
    }

    /// Start a feed's task, after `after` (the task it replaces) has finished
    fn spawn_feed(
        &self,
        run: &ServerRun,
        resource: &str,
        interval: Duration,
        after: Option<JoinHandle<()>>,
    ) -> FeedRun {
        let server = &run.config;
        let key = (server.name.clone(), resource.to_owned());
        let mut generation = 0;
        if let Ok(mut map) = self.shared.feeds.lock() {
            let entry = map
                .entry(key)
                .or_insert_with(|| FeedStatus::new(&server.name, resource, interval));
            entry.generation += 1;
            entry.interval = interval;
            entry.state = "waiting";
            entry.next_at = None;
            generation = entry.generation;
        }
        // The feed stops on its own request or the daemon's
        let stop = Shutdown::new();
        let global = self.shutdown.clone();
        let task = {
            let stop = stop.clone();
            let feed = run_feed(
                server.clone(),
                resource.to_owned(),
                interval,
                generation,
                Arc::clone(&run.client),
                Arc::clone(&run.permits),
                Arc::clone(&self.options),
                Arc::clone(&self.shared),
                stop.clone(),
            );
            async move {
                if let Some(previous) = after {
                    // The old task holds the feed's lease until its page is done
                    let _ = previous.await;
                }
                let forward = async {
                    global.triggered().await;
                    stop.trigger();
                    std::future::pending::<()>().await;
                };
                tokio::select! {
                    () = feed => {}
                    () = forward => {}
                }
            }
        };
        let span = tracing::info_span!("feed", server = %server.name, resource = %resource);
        FeedRun {
            interval,
            stop,
            handle: tokio::spawn(task.instrument(span)),
        }
    }
}

/// A running daemon
///
/// Dropping it does not stop it; trigger the [`Shutdown`] given to [`start`](Self::start) and
/// [`wait`](Self::wait).
pub struct Daemon {
    addr: SocketAddr,
    controller: Mutex<Controller>,
    endpoints: Mutex<Option<JoinHandle<()>>>,
    shutdown: Shutdown,
}

impl Daemon {
    /// Build one client per server, bind the endpoints and start every feed's schedule
    ///
    /// Paused feeds are listed in `/status` but not scheduled.
    ///
    /// # Errors
    ///
    /// [`IngestError`] when no server has a scheduled or paused resource, a client cannot be
    /// built (for example a variable that holds a secret is not set) or the address cannot be
    /// bound.
    pub async fn start(
        config: Config,
        options: ServeOptions,
        shutdown: Shutdown,
    ) -> Result<Self, IngestError> {
        if config
            .servers
            .iter()
            .all(|s| s.feeds().is_empty() && s.paused_feeds().is_empty())
        {
            return Err(IngestError::Parse(
                "serve needs at least one resource with an interval (`interval` on a resource, or on a server with `include`)"
                    .into(),
            ));
        }
        for server in &config.servers {
            if server.feeds().is_empty() && server.paused_feeds().is_empty() {
                tracing::info!(server = %server.name, "no resource has an interval: server not scheduled");
            } else if !server.feeds().is_empty() {
                // Fail at start rather than at the first sync
                ClientHandle::new(server)?;
            }
        }
        if options.ui && !cfg!(feature = "ui") {
            return Err(IngestError::Parse(
                "--ui needs reso-ingest built with the `ui` feature".into(),
            ));
        }
        // Fail now rather than at the first sync when state cannot be saved
        probe(options.state.as_ref()).await?;

        let shared = Arc::new(Shared {
            feeds: Mutex::new(BTreeMap::new()),
            clients: Mutex::new(BTreeMap::new()),
            reload: Mutex::new(ReloadInfo::default()),
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

        let ui = options.ui;
        if ui && !is_private(addr.ip()) {
            tracing::warn!(%addr, "status pages on a public address: they have no authentication");
        }
        let mut controller = Controller {
            options: Arc::new(options),
            shared: Arc::clone(&shared),
            shutdown: shutdown.clone(),
            servers: BTreeMap::new(),
            retired: Vec::new(),
        };
        // Clients were built once above, so this only fails if a secret vanished since
        let (_, errors) = controller.apply(&config);
        for e in errors {
            tracing::error!(error = %e, "server not scheduled");
        }

        let app = Router::new()
            .route("/healthz", get(|| async { "ok\n" }))
            .route("/readyz", get(readyz))
            .route("/metrics", get(metrics))
            .route("/status", get(status));
        #[cfg(feature = "ui")]
        let app = if ui { app.merge(ui::routes()) } else { app };
        let app = app.with_state(shared);
        let stop = shutdown.clone();
        let endpoints = tokio::spawn(async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(async move { stop.triggered().await })
                .await;
            if let Err(e) = served {
                tracing::warn!(error = %e, "endpoint server stopped");
            }
        });
        tracing::info!(%addr, "serve started");
        Ok(Self {
            addr,
            controller: Mutex::new(controller),
            endpoints: Mutex::new(Some(endpoints)),
            shutdown,
        })
    }

    /// The address the endpoints listen on
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Make the scheduled feeds match a new configuration, without a restart
    ///
    /// New feeds start; feeds no longer configured stop after their current page; paused feeds
    /// stop and stay listed in `/status` as `paused`; resumed feeds start again from their
    /// checkpoint. A feed whose interval changed is restarted alone. When a server's other
    /// settings change (URL, auth, filters, limits, `concurrency`), its client is rebuilt and all
    /// its feeds restart. A restarted feed starts once its old task has finished. Unchanged feeds
    /// are not touched. A server whose client cannot be built keeps running as before.
    ///
    /// # Errors
    ///
    /// [`IngestError`] when the daemon is shutting down or a server's client cannot be built (the
    /// other servers are still applied).
    pub fn reload(&self, config: &Config) -> Result<ReloadSummary, IngestError> {
        if self.shutdown.is_triggered() {
            return Err(IngestError::Parse("serve is shutting down".into()));
        }
        let Ok(mut controller) = self.controller.lock() else {
            return Err(IngestError::Parse("serve: controller unavailable".into()));
        };
        let (summary, errors) = controller.apply(config);
        let error = (!errors.is_empty()).then(|| errors.join("; "));
        if let Ok(mut info) = controller.shared.reload.lock() {
            info.reloads += 1;
            info.last_reload = Some(now_rfc3339());
            info.last_error.clone_from(&error);
        }
        tracing::info!(
            added = ?summary.added,
            removed = ?summary.removed,
            restarted = ?summary.restarted,
            paused = ?summary.paused,
            resumed = ?summary.resumed,
            "configuration reloaded"
        );
        match error {
            Some(e) => Err(IngestError::Parse(e)),
            None => Ok(summary),
        }
    }

    /// Note a reload that failed before it reached [`reload`](Self::reload) (an unreadable
    /// file), for `/status`
    pub fn reload_failed(&self, message: &str) {
        if let Ok(controller) = self.controller.lock() {
            if let Ok(mut info) = controller.shared.reload.lock() {
                info.last_error = Some(message.to_owned());
            }
        }
    }

    /// Wait until the [`Shutdown`] is triggered and every feed has stopped
    ///
    /// A feed in the middle of a sync stops after its current page, with checkpoint and output
    /// consistent, as `sync` does.
    pub async fn wait(&self) {
        self.shutdown.triggered().await;
        let mut handles = Vec::new();
        if let Ok(mut c) = self.controller.lock() {
            handles.append(&mut c.retired);
            for run in std::mem::take(&mut c.servers).into_values() {
                handles.extend(run.feeds.into_values().map(|f| f.handle));
            }
        }
        if let Some(e) = self.endpoints.lock().ok().and_then(|mut e| e.take()) {
            handles.push(e);
        }
        for h in handles {
            let _ = h.await;
        }
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
    generation: u64,
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
        shared.update(&key, generation, |f| {
            f.state = "waiting";
            f.next_at = Some(Instant::now() + wait);
        });
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = shutdown.triggered() => return,
        }
        shared.update(&key, generation, |f| {
            f.state = "queued";
            f.next_at = None;
        });
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => p.ok(),
            () = shutdown.triggered() => return,
        };
        let started = now_rfc3339();
        let clock = Instant::now();
        shared.update(&key, generation, |f| {
            f.state = "running";
            f.last_started = Some(started.clone());
        });
        let (run_id, outcome, media) =
            sync_once(&server, &resource, &client, &options, &shutdown).await;
        drop(permit);
        let summary = RunSummary::new(run_id, started, clock.elapsed(), &outcome);

        let mut runs = 0;
        let mut failures = 0;
        shared.update(&key, generation, |f| {
            for report in &media {
                f.media.get_or_insert_with(MediaCounts::default).add(report);
            }
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
            if f.recent.len() >= RECENT_RUNS {
                f.recent.pop_front();
            }
            f.recent.push_back(summary);
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

impl RunSummary {
    fn new(run_id: String, started: String, duration: Duration, outcome: &Outcome) -> Self {
        let (outcome, entry, error) = match outcome {
            Outcome::Done(e) if e.interrupted => ("interrupted", Some(e), None),
            Outcome::Done(e) => ("ok", Some(e), None),
            Outcome::Skipped => ("skipped", None, None),
            Outcome::Failed(m, e) => ("failed", e.as_ref(), Some(m.clone())),
        };
        Self {
            run_id,
            started,
            duration,
            outcome,
            records: entry.map_or(0, |e| e.records),
            deletes: entry.map_or(0, |e| e.deletes),
            bytes: entry.map_or(0, |e| e.stats.http_bytes),
            error,
        }
    }
}

/// One sync of one feed, written like `reso-ingest sync --resource R --server S`: its run id
/// (empty when none could be reserved), outcome and media report (with `media` set)
async fn sync_once(
    server: &ServerConfig,
    resource: &str,
    client: &ClientHandle,
    options: &ServeOptions,
    shutdown: &Shutdown,
) -> (String, Outcome, Vec<MediaReport>) {
    let sink = options.sink.as_deref();
    let run_id = match runs::reserve(&options.out, options.state.as_ref(), sink).await {
        Ok(id) => id,
        Err(e) => {
            return (
                String::new(),
                Outcome::Failed(e.to_string(), None),
                Vec::new(),
            )
        }
    };
    let sync_options = Arc::new(SyncOptions {
        out: options.out.clone(),
        state: Arc::clone(&options.state),
        run_id: run_id.clone(),
        resources: vec![resource.to_owned()],
        deletes: options.deletes,
        allow_mass_deletes: options.allow_mass_deletes,
        media: options.media.clone(),
        start_over: false,
        backfill: None,
        max_file_bytes: options.max_file_bytes,
        format: options.format,
        partition: options.partition,
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
    let mut media_reports = Vec::new();
    let outcome = match result {
        Err(message) => {
            manifest.add_server_failure(&server.name, &message);
            Outcome::Failed(message, None)
        }
        Ok(done) => {
            media_reports.extend(done.media);
            manifest.add_schema_changes(&server.name, done.schema_changes);
            manifest.add_rate_usage(&server.name, &done.rate_usage);
            let entry = done.entries.into_iter().find(|e| e.resource == resource);
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
    (run_id, outcome, media_reports)
}

/// Loopback, private (RFC 1918, unique-local IPv6) or link-local: where pages without
/// authentication may be served without a warning
fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(v4.into()))
        }
    }
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
    let clients: Vec<(String, Arc<ClientHandle>)> = shared
        .clients
        .lock()
        .map(|c| c.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect())
        .unwrap_or_default();
    for (server, client) in &clients {
        manifest.add_rate_usage(server, &client.rate_usage());
    }
    let mut text = render_prometheus(&manifest);
    let media: Vec<(String, MediaCounts)> = feeds
        .iter()
        .filter_map(|f| {
            let labels = format!(
                "server=\"{}\",resource=\"{}\"",
                escape(&f.server),
                escape(&f.resource)
            );
            f.media.clone().map(|m| (labels, m))
        })
        .collect();
    text.push_str(&render_media(&media, true));
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
        "reso_ingest_feed_paused",
        "gauge",
        "1 while the feed is paused in the configuration",
        &|f| u64::from(f.state == "paused"),
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
    let reload = shared.reload.lock().map(|r| r.clone()).unwrap_or_default();
    let body = json!({
        "started_at": shared.started_at,
        "reloads": reload.reloads,
        "last_reload": reload.last_reload,
        "last_reload_error": reload.last_error,
        "feeds": feeds,
    });
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
    fn private_addresses_are_recognised() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.5",
            "172.16.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private(a.parse().unwrap()), "{a}");
        }
        for a in ["0.0.0.0", "8.8.8.8", "::", "2001:db8::1"] {
            assert!(!is_private(a.parse().unwrap()), "{a}");
        }
    }

    #[test]
    fn jitter_stays_below_the_maximum() {
        for n in 0..50 {
            assert!(jitter(Duration::from_millis(100), "x", n) < Duration::from_millis(100));
        }
        assert_eq!(jitter(Duration::ZERO, "x", 1), Duration::ZERO);
    }
}
