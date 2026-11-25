//! Run records when output goes to a [`FileSink`]: run id reservation shared between workers,
//! delivery of the run and media manifests, and the "last run" copy `status` reads
//!
//! Without a sink nothing here changes what `reso-ingest` does: the run id is reserved in `--out`
//! ([`reserve_run_id`]) and manifests stay there. With a sink (a bucket) a fresh worker has an empty
//! `--out`, so the id is also reserved in the [`StateStore`] (key `runs/<id>.reserved`, created
//! atomically) and checked against the manifests the sink already holds; the manifests go to the
//! sink next to the data files they list, and a copy of the run manifest is kept in the state store
//! under [`LAST_RUN_KEY`].

use std::fs;
use std::path::Path;

use crate::error::IngestError;
use crate::output::{output_error, reserve_run_id, sanitize};
use crate::sink::FileSink;
use crate::state::StateStore;

/// The state key holding a copy of the newest run manifest when output goes to a sink
pub(crate) const LAST_RUN_KEY: &str = "runs/last.json";

fn reservation_key(run_id: &str) -> String {
    format!("runs/{}.reserved", sanitize(run_id))
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
        let name = sanitize(&id);
        if sink.exists(&format!("manifest.{name}.json"))?
            || sink.exists(&format!("media.{name}.json"))?
        {
            continue;
        }
        if state.create(&reservation_key(&id), b"").await? {
            return Ok(id);
        }
    }
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
