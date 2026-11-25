//! Leases: one writer per (server, resource) feed
//!
//! Two `reso-ingest` processes syncing the same feed would read the same checkpoint and key set and
//! the last writer would win. A [`Lease`] is a value in the state store next to the checkpoints
//! ([`StateStore`]); it records the holder (process id, host) and an expiry that a background task
//! keeps pushing forward while the holder is alive. A holder that crashes stops renewing, so its
//! lease expires and the next process takes the feed over. Taking, renewing and releasing use the
//! store's conditional writes, so a lease is as strong as the backend's (see [`FileStateStore`](crate::FileStateStore)).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::checkpoint::sanitize;
use crate::error::IngestError;
use crate::state::{invalid, StateStore, Versioned};

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
    /// The caller holds the feed until the [`Lease`] is released or dropped
    Acquired(Lease),
    /// Another live process holds it
    Held(LeaseHolder),
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
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

fn holder(f: LeaseFile) -> LeaseOutcome {
    LeaseOutcome::Held(LeaseHolder {
        pid: f.pid,
        host: f.host,
        expires_at: f.expires_at,
    })
}

/// The lease in a stored value; `None` when the bytes are not a lease (counted as stale)
fn parse(v: &Versioned) -> Option<LeaseFile> {
    serde_json::from_slice(&v.bytes).ok()
}

/// The live holder of the lease stored under `key`, without changing it; `None` when there is no
/// lease, it has expired or the value is not a lease
pub(crate) async fn live_holder(
    store: &dyn StateStore,
    key: &str,
) -> Result<Option<LeaseHolder>, IngestError> {
    Ok(store
        .get(key)
        .await?
        .as_ref()
        .and_then(parse)
        .filter(|f| f.expires_at > now_secs())
        .map(|f| LeaseHolder {
            pid: f.pid,
            host: f.host,
            expires_at: f.expires_at,
        }))
}

fn encode(store: &dyn StateStore, key: &str, lease: &LeaseFile) -> Result<Vec<u8>, IngestError> {
    serde_json::to_vec(lease).map_err(|e| invalid(store, key, e.to_string()))
}

/// A lease on one (server, resource) feed
///
/// The value `{server}.{resource}.lock` in the state store holds the process id, host and expiry.
/// While the lease lives, a background task renews the expiry every third of the time-to-live; if
/// the process dies the value is left behind and is taken over once it has expired. Call
/// [`release`](Self::release) when done; dropping the lease instead stops the renewal and releases
/// it in the background. With a [`FileStateStore`](crate::FileStateStore), taking over an expired lease is not atomic
/// against another process doing the same at the same instant (a window of a few microseconds); the
/// loser notices at the next renewal, see [`is_lost`](Self::is_lost).
///
/// # Examples
///
/// ```
/// # async fn demo() -> Result<(), reso_ingest::IngestError> {
/// use std::sync::Arc;
/// use std::time::Duration;
/// use reso_ingest::{FileStateStore, Lease, LeaseOutcome, StateStore};
///
/// let dir = std::env::temp_dir().join(format!("reso-ingest-lease-doc-{}", std::process::id()));
/// let store: Arc<dyn StateStore> = Arc::new(FileStateStore::new(&dir));
/// let ttl = Duration::from_secs(60);
/// let LeaseOutcome::Acquired(lease) = Lease::acquire(&store, "mls", "Property", ttl).await? else {
///     return Ok(()); // someone else is syncing this feed
/// };
/// // a second request for the same feed is refused while `lease` lives
/// assert!(matches!(Lease::acquire(&store, "mls", "Property", ttl).await?, LeaseOutcome::Held(_)));
/// lease.release().await;
/// # let _ = std::fs::remove_dir_all(&dir);
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Lease {
    store: Arc<dyn StateStore>,
    key: String,
    token: String,
    lost: Arc<AtomicBool>,
    heartbeat: Option<JoinHandle<()>>,
}

impl Lease {
    /// Take the lease on `server`/`resource`, or report who holds it
    ///
    /// `ttl` is how long the lease lasts without renewal; a crashed holder blocks the feed for at
    /// most that long. Call it from inside a tokio runtime (the renewal task is spawned on it).
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the store cannot be read or written.
    pub async fn acquire(
        store: &Arc<dyn StateStore>,
        server: &str,
        resource: &str,
        ttl: Duration,
    ) -> Result<LeaseOutcome, IngestError> {
        let key = format!("{}.{}.lock", sanitize(server), sanitize(resource));
        let mine = LeaseFile {
            server: server.into(),
            resource: resource.into(),
            pid: std::process::id(),
            host: host_name(),
            token: new_token(),
            expires_at: now_secs() + ttl.as_secs().max(1),
        };
        let body = encode(store.as_ref(), &key, &mine)?;
        // Two tries: the second one follows removing a stale lease
        for _ in 0..2 {
            if store.create(&key, &body).await? {
                let lease = Self::start(Arc::clone(store), key, mine, ttl);
                return Ok(LeaseOutcome::Acquired(lease));
            }
            let Some(found) = store.get(&key).await? else {
                continue; // released in between
            };
            match parse(&found) {
                Some(held) if held.expires_at > now_secs() => return Ok(holder(held)),
                // Expired or unreadable: remove it only if it is still the one judged stale
                _ => {
                    store.delete(&key, Some(&found.version)).await?;
                }
            }
        }
        // Lost the race for the stale lease to another process, which is now the holder
        match store.get(&key).await?.as_ref().and_then(parse) {
            Some(held) => Ok(holder(held)),
            None => Err(invalid(
                store.as_ref(),
                &key,
                "could not take the lease".into(),
            )),
        }
    }

    fn start(store: Arc<dyn StateStore>, key: String, mut file: LeaseFile, ttl: Duration) -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        let token = file.token.clone();
        let heartbeat = {
            let (store, key, lost) = (Arc::clone(&store), key.clone(), Arc::clone(&lost));
            let every = (ttl / 3).max(Duration::from_secs(1));
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    // Renew only while the value is still ours
                    let found = match store.get(&key).await {
                        Ok(Some(v)) if parse(&v).is_some_and(|f| f.token == file.token) => v,
                        Ok(_) => {
                            lost.store(true, Ordering::SeqCst);
                            tracing::warn!(lease = %store.location(&key), "lease lost: another process took the feed");
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "cannot read the lease; will retry");
                            continue;
                        }
                    };
                    file.expires_at = now_secs() + ttl.as_secs().max(1);
                    let renewed = match encode(store.as_ref(), &key, &file) {
                        Ok(body) => store.replace(&key, &body, &found.version).await,
                        Err(e) => Err(e),
                    };
                    // `Ok(false)`: changed since it was read; the next round finds out by whom
                    if let Err(e) = renewed {
                        tracing::warn!(error = %e, "cannot renew the lease; will retry");
                    }
                }
            })
        };
        Self {
            store,
            key,
            token,
            lost,
            heartbeat: Some(heartbeat),
        }
    }

    /// True once another process has taken the feed over (the lease was not renewed in time)
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Stop renewing and remove the lease if it is still ours
    ///
    /// A failure is logged, not returned: the lease then runs out on its own.
    pub async fn release(mut self) {
        if let Some(h) = self.heartbeat.take() {
            h.abort();
        }
        release(&self.store, &self.key, &self.token).await;
    }
}

async fn release(store: &Arc<dyn StateStore>, key: &str, token: &str) {
    let result = match store.get(key).await {
        Ok(Some(v)) if parse(&v).is_some_and(|f| f.token == token) => {
            store.delete(key, Some(&v.version)).await.map(|_| ())
        }
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "cannot release the lease; it runs out on its own");
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(heartbeat) = self.heartbeat.take() else {
            return; // released
        };
        heartbeat.abort();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let store = Arc::clone(&self.store);
            let key = std::mem::take(&mut self.key);
            let token = std::mem::take(&mut self.token);
            rt.spawn(async move { release(&store, &key, &token).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FileStateStore;

    fn store(name: &str) -> (std::path::PathBuf, Arc<dyn StateStore>) {
        let d = std::env::temp_dir().join(format!("lease-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        (d.clone(), Arc::new(FileStateStore::new(&d)))
    }

    async fn stored(store: &Arc<dyn StateStore>, key: &str) -> LeaseFile {
        parse(&store.get(key).await.unwrap().unwrap()).unwrap()
    }

    const TTL: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn second_request_is_held_until_release() {
        let (dir, s) = store("held");
        let LeaseOutcome::Acquired(a) = Lease::acquire(&s, "s", "R", TTL).await.unwrap() else {
            panic!("first must acquire");
        };
        let LeaseOutcome::Held(h) = Lease::acquire(&s, "s", "R", TTL).await.unwrap() else {
            panic!("second must be held");
        };
        assert_eq!(h.pid, std::process::id());
        // another feed is independent
        assert!(matches!(
            Lease::acquire(&s, "s", "Other", TTL).await.unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        a.release().await;
        assert!(matches!(
            Lease::acquire(&s, "s", "R", TTL).await.unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dropping_releases_in_the_background() {
        let (dir, s) = store("drop");
        let LeaseOutcome::Acquired(a) = Lease::acquire(&s, "s", "R", TTL).await.unwrap() else {
            panic!("first must acquire");
        };
        drop(a);
        let mut released = false;
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if s.get("s.R.lock").await.unwrap().is_none() {
                released = true;
                break;
            }
        }
        assert!(released, "the lease value should be removed after a drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn crashed_holders_lease_expires() {
        let (dir, s) = store("stale");
        let stale = LeaseFile {
            server: "s".into(),
            resource: "R".into(),
            pid: 999_999,
            host: "gone".into(),
            token: "dead".into(),
            expires_at: now_secs() - 5,
        };
        s.put("s.R.lock", &serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            Lease::acquire(&s, "s", "R", TTL).await.unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        // a lock whose body is not a lease is treated as stale too
        s.put("s.Q.lock", b"garbage").await.unwrap();
        assert!(matches!(
            Lease::acquire(&s, "s", "Q", TTL).await.unwrap(),
            LeaseOutcome::Acquired(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn live_holder_renews_and_keeps_the_feed() {
        let (dir, s) = store("renew");
        let ttl = Duration::from_secs(3);
        let LeaseOutcome::Acquired(a) = Lease::acquire(&s, "s", "R", ttl).await.unwrap() else {
            panic!("first must acquire");
        };
        let before = stored(&s, "s.R.lock").await.expires_at;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let after = stored(&s, "s.R.lock").await.expires_at;
        assert!(after >= before);
        assert!(!a.is_lost());
        a.release().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_holder_whose_lease_was_taken_notices() {
        let (dir, s) = store("lost");
        let ttl = Duration::from_secs(3);
        let LeaseOutcome::Acquired(a) = Lease::acquire(&s, "s", "R", ttl).await.unwrap() else {
            panic!("first must acquire");
        };
        let mut other = stored(&s, "s.R.lock").await;
        other.token = "other".into();
        s.put("s.R.lock", &serde_json::to_vec(&other).unwrap())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(a.is_lost());
        // releasing leaves the other holder's lease alone
        a.release().await;
        assert_eq!(stored(&s, "s.R.lock").await.token, "other");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_acquires_have_one_winner() {
        let (dir, s) = store("race");
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let s = Arc::clone(&s);
            tasks.push(tokio::spawn(async move {
                Lease::acquire(&s, "s", "R", TTL).await
            }));
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
