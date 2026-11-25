//! Leases: one writer per (server, resource) feed
//!
//! Two `reso-ingest` processes syncing the same feed would read the same checkpoint and key set and
//! the last writer would win. A [`Lease`] is a lock file next to the checkpoints; it records the
//! holder (process id, host) and an expiry that a background task keeps pushing forward while the
//! holder is alive. A holder that crashes stops renewing, so its lease expires and the next process
//! takes the feed over.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::error::IngestError;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct LeaseFile {
    server: String,
    resource: String,
    pid: u32,
    host: String,
    /// Identifies this holder, so a process never removes or renews a lease that is no longer its own
    token: String,
    /// Seconds since the Unix epoch
    expires_at: u64,
}

/// Who holds a feed that could not be leased
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseHolder {
    /// Process id of the holder
    pub pid: u32,
    /// Host name of the holder
    pub host: String,
    /// When the lease runs out unless renewed (seconds since the Unix epoch)
    pub expires_at: u64,
}

impl std::fmt::Display for LeaseHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "held by pid {} on {}, runs out in {}s unless renewed",
            self.pid,
            self.host,
            self.expires_at.saturating_sub(now_secs())
        )
    }
}

/// The result of asking for a lease
#[derive(Debug)]
pub enum LeaseOutcome {
    /// The caller holds the feed until the [`Lease`] is dropped
    Acquired(Lease),
    /// Another live process holds it
    Held(LeaseHolder),
}

/// A lease on one (server, resource) feed, released when dropped
///
/// A file in the state directory (`{server}.{resource}.lock`) holds the process id, host and expiry.
/// While the value lives, a background task renews the expiry every third of the time-to-live; if
/// the process dies the file is left behind and is taken over once it has expired. Taking over an
/// expired lease is not atomic against another process doing the same at the same instant (a window
/// of a few microseconds); the loser notices at the next renewal, see [`is_lost`](Self::is_lost).
/// The lock works between processes that share the state directory on a local disk; shared
/// backends are a later step (#85).
///
/// # Examples
///
/// ```
/// # async fn demo() -> Result<(), reso_ingest::IngestError> {
/// use std::time::Duration;
/// use reso_ingest::{Lease, LeaseOutcome};
///
/// let dir = std::env::temp_dir().join(format!("reso-ingest-lease-doc-{}", std::process::id()));
/// let ttl = Duration::from_secs(60);
/// let LeaseOutcome::Acquired(lease) = Lease::acquire(&dir, "mls", "Property", ttl)? else {
///     return Ok(()); // someone else is syncing this feed
/// };
/// // a second request for the same feed is refused while `lease` lives
/// assert!(matches!(Lease::acquire(&dir, "mls", "Property", ttl)?, LeaseOutcome::Held(_)));
/// drop(lease);
/// # let _ = std::fs::remove_dir_all(&dir);
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
    token: String,
    lost: Arc<AtomicBool>,
    heartbeat: JoinHandle<()>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host_name() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn new_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

fn io_err(path: &Path, e: impl std::fmt::Display) -> IngestError {
    IngestError::Output {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

fn read(path: &Path) -> Result<Option<LeaseFile>, IngestError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str(&text).ok()), // an unreadable body counts as stale
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path, e)),
    }
}

/// Write a temporary file next to `path` and rename it over, so a reader never sees half a file
fn replace(path: &Path, lease: &LeaseFile) -> Result<(), IngestError> {
    let tmp = path.with_extension(format!("lock.{}.tmp", lease.token));
    let body = serde_json::to_vec(lease).map_err(|e| io_err(path, e))?;
    fs::write(&tmp, body).map_err(|e| io_err(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err(path, e)
    })
}

fn holder(f: LeaseFile) -> LeaseOutcome {
    LeaseOutcome::Held(LeaseHolder {
        pid: f.pid,
        host: f.host,
        expires_at: f.expires_at,
    })
}

impl Lease {
    /// Take the lease on `server`/`resource`, or report who holds it
    ///
    /// `ttl` is how long the lease lasts without renewal; a crashed holder blocks the feed for at
    /// most that long. The state directory is created when missing. Call it from inside a tokio
    /// runtime (the renewal task is spawned on it).
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the lock file cannot be read or written.
    pub fn acquire(
        dir: &Path,
        server: &str,
        resource: &str,
        ttl: Duration,
    ) -> Result<LeaseOutcome, IngestError> {
        fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        let path = dir.join(format!(
            "{}.{}.lock",
            crate::checkpoint::sanitize(server),
            crate::checkpoint::sanitize(resource)
        ));
        let mine = LeaseFile {
            server: server.into(),
            resource: resource.into(),
            pid: std::process::id(),
            host: host_name(),
            token: new_token(),
            expires_at: now_secs() + ttl.as_secs().max(1),
        };
        let body = serde_json::to_vec(&mine).map_err(|e| io_err(&path, e))?;
        // Two tries: the second one follows removing a stale lease
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    f.write_all(&body)
                        .and_then(|()| f.sync_all())
                        .map_err(|e| io_err(&path, e))?;
                    return Ok(LeaseOutcome::Acquired(Self::start(path, mine, ttl)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err(&path, e)),
            }
            match read(&path)? {
                Some(held) if held.expires_at > now_secs() => return Ok(holder(held)),
                stale => {
                    // Expired or unreadable: remove it only if it is still the one judged stale
                    if read(&path)? == stale {
                        match fs::remove_file(&path) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(io_err(&path, e)),
                        }
                    }
                }
            }
        }
        // Lost the race for the stale lease to another process, which is now the holder
        match read(&path)? {
            Some(held) => Ok(holder(held)),
            None => Err(io_err(&path, "could not take the lease")),
        }
    }

    fn start(path: PathBuf, mut file: LeaseFile, ttl: Duration) -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        let token = file.token.clone();
        let heartbeat = {
            let (path, lost) = (path.clone(), Arc::clone(&lost));
            let every = (ttl / 3).max(Duration::from_secs(1));
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    // Renew only while the file is still ours
                    match read(&path) {
                        Ok(Some(f)) if f.token == file.token => {}
                        Ok(_) => {
                            lost.store(true, Ordering::SeqCst);
                            tracing::warn!(path = %path.display(), "lease lost: another process took the feed");
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "cannot read the lease file; will retry");
                            continue;
                        }
                    }
                    file.expires_at = now_secs() + ttl.as_secs().max(1);
                    if let Err(e) = replace(&path, &file) {
                        tracing::warn!(error = %e, "cannot renew the lease; will retry");
                    }
                }
            })
        };
        Self {
            path,
            token,
            lost,
            heartbeat,
        }
    }

    /// True once another process has taken the feed over (the lease was not renewed in time)
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.heartbeat.abort();
        // Remove the file only when it is still ours
        if matches!(read(&self.path), Ok(Some(f)) if f.token == self.token) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lease-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    const TTL: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn second_request_is_held_until_release() {
        let dir = tmp("held");
        let LeaseOutcome::Acquired(a) = Lease::acquire(&dir, "s", "R", TTL).unwrap() else {
            panic!("first must acquire");
        };
        let LeaseOutcome::Held(h) = Lease::acquire(&dir, "s", "R", TTL).unwrap() else {
            panic!("second must be held");
        };
        assert_eq!(h.pid, std::process::id());
        // another feed is independent
        assert!(matches!(
            Lease::acquire(&dir, "s", "Other", TTL).unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        drop(a);
        assert!(matches!(
            Lease::acquire(&dir, "s", "R", TTL).unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn crashed_holders_lease_expires() {
        let dir = tmp("stale");
        fs::create_dir_all(&dir).unwrap();
        let stale = LeaseFile {
            server: "s".into(),
            resource: "R".into(),
            pid: 999_999,
            host: "gone".into(),
            token: "dead".into(),
            expires_at: now_secs() - 5,
        };
        fs::write(dir.join("s.R.lock"), serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(matches!(
            Lease::acquire(&dir, "s", "R", TTL).unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        // a lock file whose body is not a lease is treated as stale too
        fs::write(dir.join("s.Q.lock"), b"garbage").unwrap();
        assert!(matches!(
            Lease::acquire(&dir, "s", "Q", TTL).unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn live_holder_renews_and_keeps_the_feed() {
        let dir = tmp("renew");
        let ttl = Duration::from_secs(3);
        let LeaseOutcome::Acquired(a) = Lease::acquire(&dir, "s", "R", ttl).unwrap() else {
            panic!("first must acquire");
        };
        let before = read(&dir.join("s.R.lock")).unwrap().unwrap().expires_at;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let after = read(&dir.join("s.R.lock")).unwrap().unwrap().expires_at;
        assert!(after >= before);
        assert!(!a.is_lost());
        drop(a);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_acquires_have_one_winner() {
        let dir = tmp("race");
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let dir = dir.clone();
            tasks.push(tokio::spawn(
                async move { Lease::acquire(&dir, "s", "R", TTL) },
            ));
        }
        let mut held = 0;
        let mut keep = Vec::new();
        for t in tasks {
            match t.await.unwrap().unwrap() {
                LeaseOutcome::Acquired(l) => keep.push(l),
                LeaseOutcome::Held(_) => held += 1,
            }
        }
        assert_eq!(keep.len(), 1);
        assert_eq!(held, 7);
        drop(keep);
        let _ = fs::remove_dir_all(&dir);
    }
}
