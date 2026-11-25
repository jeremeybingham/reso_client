//! Running one task per server, independently and with a concurrency cap

use std::future::Future;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::AbortHandle;
use tracing::Instrument;

use crate::config::ServerConfig;

/// The outcome of one server's task
#[derive(Debug)]
pub struct ServerReport<T> {
    /// The server's name
    pub name: String,
    /// The task's value, or the error text (a panic is reported as an error too)
    pub result: Result<T, String>,
}

/// Run `task` once per server, at most `max_concurrent` at a time, and collect every outcome
///
/// Servers are independent: an error or panic in one is recorded in its [`ServerReport`] and the
/// others carry on. Reports come back in the order of `servers`. Each task runs inside a `server`
/// span (field `server`), a child of the caller's current span, so its log events name the server.
/// Dropping the returned future before it completes aborts every server task it started.
///
/// # Examples
///
/// ```
/// # use reso_ingest::{run_servers, Config};
/// # async fn demo(config: Config) {
/// let reports = run_servers(config.servers, config.max_concurrent_servers, |server| async move {
///     // build a client from server.client_config()? and sync here
///     Ok::<_, String>(server.name.len())
/// })
/// .await;
/// # }
/// ```
pub async fn run_servers<T, E, F, Fut>(
    servers: Vec<ServerConfig>,
    max_concurrent: usize,
    task: F,
) -> Vec<ServerReport<T>>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: Fn(ServerConfig) -> Fut,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    let permits = Arc::new(Semaphore::new(max_concurrent.max(1)));
    let mut handles = Vec::with_capacity(servers.len());
    let mut guard = AbortOnDrop(Vec::with_capacity(servers.len()));
    for server in servers {
        let name = server.name.clone();
        let permits = Arc::clone(&permits);
        let fut = task(server);
        // A child of the caller's current span (the `run` span of the command line)
        let span = tracing::info_span!("server", server = %name);
        let handle = tokio::spawn(
            async move {
                // The semaphore is never closed, so acquiring cannot fail
                let _permit = permits.acquire_owned().await.ok();
                fut.await.map_err(|e| e.to_string())
            }
            .instrument(span),
        );
        guard.0.push(handle.abort_handle());
        handles.push((name, handle));
    }

    let mut reports = Vec::with_capacity(handles.len());
    for (name, handle) in handles {
        let result = match handle.await {
            Ok(r) => r,
            Err(e) => Err(format!("task failed: {e}")),
        };
        if let Err(message) = &result {
            tracing::warn!(server = %name, error = %message, "server failed");
        }
        reports.push(ServerReport { name, result });
    }
    // Every task has finished; nothing is left to abort
    guard.0.clear();
    reports
}

/// Aborts the server tasks when `run_servers` is dropped part-way (spawned tasks are otherwise
/// detached and would keep running)
struct AbortOnDrop(Vec<AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}
