//! Running one task per server, independently and with a concurrency cap

use std::future::Future;
use std::sync::Arc;

use tokio::sync::Semaphore;

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
/// others carry on. Reports come back in the order of `servers`.
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
    for server in servers {
        let name = server.name.clone();
        let permits = Arc::clone(&permits);
        let fut = task(server);
        let handle = tokio::spawn(async move {
            // The semaphore is never closed, so acquiring cannot fail
            let _permit = permits.acquire_owned().await.ok();
            fut.await.map_err(|e| e.to_string())
        });
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
    reports
}
