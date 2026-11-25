//! The run-summary webhook

use std::time::Duration;

use serde_json::{json, Value};

use crate::error::IngestError;
use crate::manifest::RunManifest;

/// The JSON a webhook receives after a run
///
/// `run_id`, `started_at`, `finished_at`; `clean` (no resource or server failed); totals
/// (`resources`, `records`, `deletes`, `deletes_held`); `failures` (one `server/resource: error`
/// string each); `warnings` (check findings, see [`RunManifest::warnings`]); `manifest` (the
/// path of the manifest file, when it was written).
pub fn run_summary(manifest: &RunManifest, manifest_path: Option<&str>) -> Value {
    let failures: Vec<String> = manifest
        .server_failures
        .iter()
        .map(|f| format!("{}: {}", f.server, f.error))
        .chain(manifest.resources.iter().filter_map(|r| {
            r.error
                .as_ref()
                .map(|e| format!("{}/{}: {e}", r.server, r.resource))
        }))
        .collect();
    json!({
        "run_id": manifest.run_id,
        "started_at": manifest.started_at,
        "finished_at": manifest.finished_at,
        "clean": manifest.is_clean(),
        "interrupted": manifest.interrupted,
        "resources": manifest.resources.len(),
        "records": manifest.resources.iter().map(|r| r.records).sum::<u64>(),
        "deletes": manifest.resources.iter().map(|r| r.deletes).sum::<u64>(),
        "deletes_held": manifest.resources.iter().map(|r| r.deletes_held).sum::<u64>(),
        "failures": failures,
        "warnings": manifest.warnings(),
        "manifest": manifest_path,
    })
}

/// POST `summary` as JSON to `url`
///
/// # Errors
///
/// [`IngestError::Output`] (with the URL's host only, since the path often holds a secret) when
/// the request fails or the answer is not a 2xx.
pub async fn post_summary(url: &str, summary: &Value) -> Result<(), IngestError> {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?']).next())
        .unwrap_or("webhook");
    let fail = |message: String| IngestError::Output {
        path: format!("webhook {host}"),
        message,
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| fail(e.without_url().to_string()))?;
    let response = client
        .post(url)
        .json(summary)
        .send()
        .await
        .map_err(|e| fail(e.without_url().to_string()))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(fail(format!("answered {}", response.status())))
    }
}
