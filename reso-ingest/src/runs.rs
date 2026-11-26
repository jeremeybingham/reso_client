//! Run records when output goes to a [`FileSink`]: run id reservation shared between workers,
//! delivery of the run and media manifests, and the "last run" copy `status` reads
//!
//! Without a sink nothing here changes what `reso-ingest` does: the run id is reserved in `--out`
//! ([`reserve_run_id`]) and manifests stay there. With a sink (a bucket) a fresh worker has an empty
//! `--out`, so the id is also reserved in the [`StateStore`] (key `runs/<id>.reserved`, created
//! atomically) and checked against the manifests the sink already holds; the manifests go to the
//! sink next to the data files they list, and a copy of the run manifest is kept in the state store
//! under [`LAST_RUN_KEY`].
//!
//! A run killed before its manifest is delivered never releases its reservation. Each reservation
//! records when it was made, and [`reserve`] removes ones older than [`STALE_AFTER`] (at most once
//! an hour per process, see [`sweep_stale`]), so the keys do not build up (#132).

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::IngestError;
use crate::metrics::parse_rfc3339;
use crate::output::{format_rfc3339, now_rfc3339, output_error, reserve_run_id, sanitize};
use crate::sink::FileSink;
use crate::state::StateStore;

/// The state key holding a copy of the newest run manifest when output goes to a sink
pub(crate) const LAST_RUN_KEY: &str = "runs/last.json";

/// A reservation older than this (seconds) belongs to a run that was killed or crashed: no run
/// takes a week, and a live run's reservation is never older than the run itself
pub(crate) const STALE_AFTER: u64 = 7 * 24 * 3600;

/// How often (seconds) one process looks for stale reservations; `serve` reserves an id per sync
const SWEEP_EVERY: u64 = 3600;

/// When this process last looked for stale reservations (Unix seconds, 0 = never)
static LAST_SWEEP: AtomicU64 = AtomicU64::new(0);

fn reservation_key(run_id: &str) -> String {
    format!("runs/{}.reserved", sanitize(run_id))
}

fn reservation_body() -> Vec<u8> {
    serde_json::json!({ "reserved_at": now_rfc3339() })
        .to_string()
        .into_bytes()
}

/// When a reservation was made, from its body; `None` for the empty body earlier versions wrote
fn reserved_at(bytes: &[u8]) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    parse_rfc3339(value.get("reserved_at")?.as_str()?)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A run id unused in `out` and, with a sink, in the sink and the shared state
pub(crate) async fn reserve(
    out: &Path,
    state: &dyn StateStore,
    sink: Option<&dyn FileSink>,
) -> Result<String, IngestError> {
    loop {
        // Each try leaves its local marker, so the next one gets the following suffix
        let id = reserve_run_id(out)?;
        let Some(sink) = sink else { return Ok(id) };
        // Reserve first, then look at the sink: a worker delivers its manifest before it releases
        // the reservation, so a reservation won here means any earlier run with this id has its
        // manifest in the sink already. Checking the sink first lost that race: the other worker
        // could deliver and release between the check and the create (two runs, one id).
        let key = reservation_key(&id);
        if !state.create(&key, &reservation_body()).await? {
            continue;
        }
        let name = sanitize(&id);
        if sink.exists(&format!("manifest.{name}.json"))?
            || sink.exists(&format!("media.{name}.json"))?
        {
            // Taken by a finished run: give the reservation back and try the next id
            state.delete(&key, None).await?;
            continue;
        }
        let now = now_secs();
        let last = LAST_SWEEP.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= SWEEP_EVERY
            && LAST_SWEEP
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            if let Err(e) = sweep_stale(state, now, STALE_AFTER, Some(&key)).await {
                tracing::warn!(error = %e, "cannot look for stale run id reservations");
            }
        }
        return Ok(id);
    }
}

/// Remove run id reservations older than `max_age` seconds at `now`; returns how many went
///
/// Each removal is conditional on the version just read, so a reservation that changed in between
/// stays. A reservation with no time in it (written before #132) is stamped with `now` instead, so
/// it goes `max_age` later. Removing a stale reservation never frees an id that was used: a run
/// that delivered its manifest is found by [`reserve`] in the sink.
pub(crate) async fn sweep_stale(
    state: &dyn StateStore,
    now: u64,
    max_age: u64,
    fresh: Option<&str>,
) -> Result<usize, IngestError> {
    let mut removed = 0;
    for key in state.list_dir("runs").await? {
        let Some(name) = key.strip_prefix("runs/") else {
            continue;
        };
        // `fresh` is the reservation just made: no need to read it
        if name.contains('/') || !name.ends_with(".reserved") || fresh == Some(key.as_str()) {
            continue;
        }
        let Some(found) = state.get(&key).await? else {
            continue;
        };
        match reserved_at(&found.bytes) {
            Some(at) if (now as f64) - at > max_age as f64 => {
                if state.delete(&key, Some(&found.version)).await? {
                    removed += 1;
                }
            }
            Some(_) => {}
            None => {
                let body = serde_json::json!({ "reserved_at": format_rfc3339(now) });
                state
                    .replace(&key, body.to_string().as_bytes(), &found.version)
                    .await?;
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed stale run id reservations");
    }
    Ok(removed)
}

/// Drop the shared reservation once the run's manifest is delivered (or it wrote nothing)
///
/// A delivered manifest keeps the id taken; the reservation only covers the run itself, so the
/// state does not collect one key per run. Failure only leaves a stale key, so it is logged.
pub(crate) async fn release(state: &dyn StateStore, sink: Option<&dyn FileSink>, run_id: &str) {
    if sink.is_none() {
        return;
    }
    if let Err(e) = state.delete(&reservation_key(run_id), None).await {
        tracing::warn!(error = %e, "cannot remove the run id reservation");
    }
}

/// Hand a finished local file (a manifest) to the sink and remove the local copy; returns where
/// it is now. Without a sink the file stays where it is.
pub(crate) fn deliver(local: &Path, sink: Option<&dyn FileSink>) -> Result<String, IngestError> {
    let Some(sink) = sink else {
        return Ok(local.display().to_string());
    };
    let name = local
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| IngestError::Output {
            path: local.display().to_string(),
            message: "not a file name".to_string(),
        })?;
    sink.put_file(name, local)?;
    fs::remove_file(local).map_err(|e| output_error(local, e))?;
    Ok(sink.location(name))
}

/// Deliver a written run manifest and, with a sink, keep a copy as the last run in the state
pub(crate) async fn deliver_manifest(
    local: &Path,
    state: &dyn StateStore,
    sink: Option<&dyn FileSink>,
) -> Result<String, IngestError> {
    if sink.is_some() {
        let bytes = fs::read(local).map_err(|e| output_error(local, e))?;
        state.put(LAST_RUN_KEY, &bytes).await?;
    }
    deliver(local, sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::state::FileStateStore;

    /// Says the first name it is asked about exists; records the questions
    #[derive(Debug, Default)]
    struct OneTaken(Mutex<Vec<String>>);

    impl FileSink for OneTaken {
        fn put_file(&self, _name: &str, _local: &Path) -> Result<(), IngestError> {
            Ok(())
        }
        fn exists(&self, name: &str) -> Result<bool, IngestError> {
            let mut asked = self.0.lock().unwrap();
            asked.push(name.to_string());
            Ok(asked.len() == 1)
        }
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("reso-runs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[tokio::test]
    async fn a_run_id_the_sink_or_state_holds_is_skipped() {
        let dir = temp("reserve");
        let state = FileStateStore::new(dir.join("state"));
        let sink = OneTaken::default();
        let id = reserve(&dir.join("out"), &state, Some(&sink))
            .await
            .unwrap();
        let asked = sink.0.lock().unwrap().clone();
        assert_ne!(asked[0], format!("manifest.{}.json", sanitize(&id)));
        assert!(state.get(&reservation_key(&id)).await.unwrap().is_some());

        // A reservation another worker holds is skipped too
        let other = reserve(
            &dir.join("out2"),
            &state,
            Some(&OneTaken(Mutex::new(vec!["x".into()]))),
        )
        .await
        .unwrap();
        assert_ne!(other, id);
        release(&state, Some(&sink), &id).await;
        assert!(state.get(&reservation_key(&id)).await.unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Another worker that picked the same id: it holds the reservation for the first id it is
    /// seen with, and if the sink is asked first, it delivers its manifest and releases right
    /// after that question (the interleaving that gave two runs one id)
    #[derive(Debug, Default)]
    struct OtherWorker {
        inner: Option<FileStateStore>,
        /// The other worker's id (sanitized) and whether it has delivered and released
        other: Mutex<Option<(String, bool)>>,
    }

    impl OtherWorker {
        fn state(&self) -> &FileStateStore {
            self.inner.as_ref().unwrap()
        }
    }

    impl FileSink for OtherWorker {
        fn put_file(&self, _name: &str, _local: &Path) -> Result<(), IngestError> {
            Ok(())
        }
        fn exists(&self, name: &str) -> Result<bool, IngestError> {
            let Some(id) = name
                .strip_prefix("manifest.")
                .and_then(|n| n.strip_suffix(".json"))
            else {
                return Ok(false);
            };
            let mut other = self.other.lock().unwrap();
            match &*other {
                None => {
                    // Not delivered at the time of this question; delivered just after
                    *other = Some((id.to_string(), true));
                    Ok(false)
                }
                Some((o, done)) => Ok(o == id && *done),
            }
        }
    }

    impl StateStore for OtherWorker {
        fn get<'a>(
            &'a self,
            key: &'a str,
        ) -> crate::state::StoreFuture<'a, Option<crate::state::Versioned>> {
            self.state().get(key)
        }
        fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> crate::state::StoreFuture<'a, ()> {
            self.state().put(key, bytes)
        }
        fn create<'a>(
            &'a self,
            key: &'a str,
            bytes: &'a [u8],
        ) -> crate::state::StoreFuture<'a, bool> {
            let id = key
                .strip_prefix("runs/")
                .and_then(|k| k.strip_suffix(".reserved"))
                .unwrap_or_default()
                .to_string();
            let held = {
                let mut other = self.other.lock().unwrap();
                match &*other {
                    None => {
                        *other = Some((id, false));
                        true
                    }
                    Some((o, done)) => *o == id && !*done,
                }
            };
            if held {
                Box::pin(async { Ok(false) })
            } else {
                self.state().create(key, bytes)
            }
        }
        fn replace<'a>(
            &'a self,
            key: &'a str,
            bytes: &'a [u8],
            version: &'a str,
        ) -> crate::state::StoreFuture<'a, bool> {
            self.state().replace(key, bytes, version)
        }
        fn delete<'a>(
            &'a self,
            key: &'a str,
            version: Option<&'a str>,
        ) -> crate::state::StoreFuture<'a, bool> {
            self.state().delete(key, version)
        }
        fn list(&self) -> crate::state::StoreFuture<'_, Vec<String>> {
            self.state().list()
        }
        fn location(&self, key: &str) -> String {
            self.state().location(key)
        }
    }

    #[tokio::test]
    async fn a_run_id_released_after_its_manifest_is_delivered_is_not_reused() {
        let dir = temp("race");
        let worker = OtherWorker {
            inner: Some(FileStateStore::new(dir.join("state"))),
            ..OtherWorker::default()
        };
        let id = reserve(&dir.join("out"), &worker, Some(&worker))
            .await
            .unwrap();
        let other = worker.other.lock().unwrap().clone().unwrap().0;
        assert_ne!(sanitize(&id), other, "two runs got one id");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_reservations_are_removed_and_fresh_ones_kept() {
        let dir = temp("sweep");
        let state = FileStateStore::new(dir.join("state"));
        let id = reserve(
            &dir.join("out"),
            &state,
            Some(&OneTaken(Mutex::new(vec!["x".into()]))),
        )
        .await
        .unwrap();
        let key = reservation_key(&id);
        let body = state.get(&key).await.unwrap().unwrap().bytes;
        let at = reserved_at(&body).expect("a reservation records its time") as u64;

        // An old killed run, a reservation from before #132 (empty body) and unrelated keys
        let old = serde_json::json!({ "reserved_at": format_rfc3339(at - STALE_AFTER - 1) });
        state
            .put("runs/old.reserved", old.to_string().as_bytes())
            .await
            .unwrap();
        state.put("runs/legacy.reserved", b"").await.unwrap();
        state.put(LAST_RUN_KEY, b"{}").await.unwrap();
        state.put("mls.Property.json", b"{}").await.unwrap();
        state.put("runs/x/deep.reserved", b"").await.unwrap();

        assert_eq!(sweep_stale(&state, at, STALE_AFTER, None).await.unwrap(), 1);
        assert!(state.get("runs/old.reserved").await.unwrap().is_none());
        assert!(
            state.get(&key).await.unwrap().is_some(),
            "a live run's key went"
        );
        // The legacy key is stamped now and goes once it is stale itself
        let legacy = state.get("runs/legacy.reserved").await.unwrap().unwrap();
        assert_eq!(reserved_at(&legacy.bytes), Some(at as f64));
        assert_eq!(
            sweep_stale(&state, at + STALE_AFTER + 1, STALE_AFTER, None)
                .await
                .unwrap(),
            2
        );
        let mut left = state.list().await.unwrap();
        left.sort();
        assert_eq!(
            left,
            ["mls.Property.json", LAST_RUN_KEY, "runs/x/deep.reserved"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn without_a_sink_nothing_goes_to_the_state() {
        let dir = temp("plain");
        let state = FileStateStore::new(dir.join("state"));
        let local = dir.join("manifest.x.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&local, "{}").unwrap();
        let id = reserve(&dir, &state, None).await.unwrap();
        assert_eq!(
            deliver_manifest(&local, &state, None).await.unwrap(),
            local.display().to_string()
        );
        assert!(local.exists());
        release(&state, None, &id).await;
        assert!(state.list().await.unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
