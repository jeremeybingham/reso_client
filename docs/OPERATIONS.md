# Operating reso-ingest

A deployment guide, a runbook for each alert in
[`reso-ingest/monitoring/alert-rules.yml`](../reso-ingest/monitoring/alert-rules.yml), and what support to expect.
The details of every command and setting are in [`reso-ingest/README.md`](../reso-ingest/README.md).

Nothing here has run against a vendor server yet ([#32](https://github.com/jeremeybingham/reso_client/issues/32),
[#76](https://github.com/jeremeybingham/reso_client/issues/76)); see [`COMPATIBILITY.md`](COMPATIBILITY.md).

## Deployment guide

### 1. Choose how it runs

| Shape | Use when | Example |
|---|---|---|
| `sync` on a schedule (cron, Kubernetes CronJob) | A few feeds, polled every 15 minutes or more | [`deploy/kubernetes/cronjob.yaml`](../deploy/kubernetes/cronjob.yaml) |
| `serve` daemon (Kubernetes Deployment, systemd) | Many feeds, each on its own interval, with `/metrics` and `/status` | [`deploy/kubernetes/deployment.yaml`](../deploy/kubernetes/deployment.yaml), [`deploy/systemd/reso-ingest.service`](../deploy/systemd/reso-ingest.service) |

Build the image from the [`Dockerfile`](../Dockerfile) (no image is published yet) or the binary with
`cargo build --release -p reso-ingest --features parquet,s3`.

### 2. Choose where state and data live

- **One volume** (`--out /data`): output, checkpoints and the access log on one disk. One writer per volume: one
  CronJob with `concurrencyPolicy: Forbid`, or one `serve` replica with the `Recreate` strategy.
- **A bucket** (`--s3-bucket` or `--gcs-bucket`): state and data files in S3, an S3-compatible store or GCS. Leases let
  several workers share it: a feed another process holds is skipped, so two `serve` replicas do not pull the same feed.
  Give the bucket's IAM principal read, write, list and delete on the prefix.

### 3. Onboard each server

1. Write a draft with `name`, `base_url` and `[server.auth]`; put secrets in environment variables (`*_env`) or mounted
   files (`*_file`), never in the file.
2. `reso-ingest discover --config draft.toml --starter`: it prints an `include` list of the readable resources.
3. Set `[server.limits]` from the vendor's published limits (servers do not announce them), and `filter`, `deletes` and
   `media` as the vendor needs (see [`COMPATIBILITY.md`](COMPATIBILITY.md) for known vendors).
4. First load: `sync --backfill --server NAME` runs on its own share of the rate limits with progress lines.
5. Then add the server to the scheduled `sync` or to `serve`'s configuration directory (`serve` picks it up on reload).

### 4. Monitoring

- Scrape `serve`'s `/metrics`, or point node_exporter's textfile collector at `sync --metrics-file`.
- Load the alert rules and the Grafana dashboard from [`reso-ingest/monitoring/`](../reso-ingest/monitoring/); set the
  thresholds from how often each feed runs.
- Optionally set `webhook_url_env` for a JSON summary of each run.

### 5. Before going live

- [ ] A stop (`docker stop`, pod deletion) during a sync ends with exit code 3 and the next run continues.
- [ ] Secrets do not appear in logs (`--log-format json`), manifests or the webhook body.
- [ ] Rate limits are below the vendor's; check the vendor's usage page after the first day.
- [ ] Back up the state (checkpoints and key sets): losing it means a full load, not lost data.
- [ ] The licence's end-of-access terms are known; `purge` removes one server's data when they apply.

## Runbooks

Each alert's `runbook_url` points at its section here. Start every investigation with:

```sh
reso-ingest status --out /data              # checkpoints and the last run
reso-ingest history --out /data --server S  # recent runs of one server
```

and, for `serve`, `curl http://HOST:9090/status` (or, with `serve --ui`, open `http://HOST:9090/` in a browser: failing
feeds are highlighted and each feed's page shows its last error and recent runs). Logs are on stderr; `--log-format json` adds `run_id`, `server` and
`resource` to every line.

### ResoFeedStale

The newest record written for a feed is older than the threshold.

1. Is the feed failing? Look for **ResoResourceFailed** on the same feed and follow it.
2. Is it running? For `serve`, `/status` shows the feed's last sync and whether it is `paused`; for `sync`, check the
   scheduler (**ResoRunsStopped**).
3. Runs succeed but bring nothing: the MLS may be quiet (nights, holidays). Compare with the vendor's own count for
   records modified since the checkpoint; if the vendor has newer records, re-pull from before the checkpoint with
   `replay --server S --resource R --after TIME`.
4. If quiet periods are normal for the feed, raise its threshold.

### ResoResourceFailed

The last pull of a resource ended in an error. The error is in the run summary, `history` and the manifest.

- **401/403**: credentials expired or the licence no longer covers the resource. Rotate the secret (secret files are re-read
  before each resource; an environment variable changes only with a restart); a resource the licence dropped can be excluded or set `paused = true`.
- **429 or 5xx after retries**: the vendor is throttling or down. See **ResoRepeatedThrottling**; for an outage, wait.
  The next run continues from the checkpoint.
- **`OutOfOrder` or a timestamp before the checkpoint**: the server ignored `$orderby` or `$filter`. Do not reset the
  checkpoint to get past it; collect the request from the logs and raise it with the vendor and in #76 (section 3).
- **A lease held by another process**: another worker is pulling the feed; it clears when that worker finishes or
  its lease expires (5 minutes).
- **Disk or bucket errors**: fix the storage; nothing was checkpointed past the last finished file.

### ResoServerFailed

A whole server failed: usually `$metadata` could not be read or the token request failed.

1. `curl` the server's `$metadata` URL with the same token from the same network (proxy and CA settings in
   `[server.network]`).
2. A token error: check the client id and secret and the token URL; `transport` and `scope` must match the vendor.
3. A TLS error: the vendor's CA or a TLS-inspecting proxy; add its PEM to `[server.network] ca_files`.

### ResoRepeatedThrottling

The server keeps answering 429. The client honours `Retry-After`, but the configured pace is above what the vendor
allows.

1. Lower `[server.limits]` (`per_second`, `per_hour`, `per_day`, `bytes_per_hour`); the
   `reso_ingest_rate_limit_used` metric shows how close each window is to its ceiling.
2. Remember that other systems using the same credential count too.
3. During a backfill, lower `[server.backfill] rate_percent`.

### ResoRunsStopped

No run has finished for hours: the scheduler, the process or the metrics file stopped.

1. CronJob: `kubectl get jobs`, a pod stuck in `Pending` or `CrashLoopBackOff`, `concurrencyPolicy: Forbid` waiting on a
   job that never ended. systemd: `systemctl status reso-ingest`, `journalctl -u reso-ingest`.
2. `serve`: `/healthz` and `/readyz`; a configuration that failed to reload shows as `last_reload_error` in `/status`
   (the old one keeps running).
3. With `--metrics-file`, check the file's directory is writable and the collector reads it.

### ResoDeletesHeld

A delete pass found more deletes than `[server.deletes]` allows and wrote none.

1. Check the server: a partial outage or a changed filter can make records look deleted. Compare `$count` with the
   number of keys known before the pass.
2. If the deletes are real (a large expiry, an MLS clean-up), run `sync --deletes --allow-mass-deletes --server S
   --resource R` once.
3. If not, leave them held and raise it with the vendor; the next pass checks again.

### ResoSchemaChanged

The server's `$metadata` differs from the last run's; the changes are listed in the manifest's `schema_changes`.

- Added fields and resources need nothing: records are raw. Added resources are pulled if `include` allows them.
- A removed field or changed type can break consumers downstream, and Parquet output, whose columns are typed from
  `$metadata`, changes schema. Tell the consumers of the data.
- The alert clears after the next run.

### ResoDataWarnings

Run checks found something: repeated keys in a page, or a full-load count that differs from the server's `$count`.
The data written is fine; findings are in the manifest's `warnings`.

- Repeated keys: usually a vendor paging bug; downstream should de-duplicate on the key (delivery is at least once).
- Count mismatch: records changed during the load (rerun and compare), or the server's count uses other filters.

## Support

- `reso-client` and `reso-ingest` are maintained by one person on a best-effort basis. There is no SLA and no paid
  support.
- Questions and bug reports: [GitHub issues](https://github.com/jeremeybingham/reso_client/issues). Include the
  versions, the server vendor, the command and the error, with secrets and record data removed.
- Security issues: see [`SECURITY.md`](../SECURITY.md); do not open a public issue.
- Supported versions: the latest release of `reso-client` and `main` of `reso-ingest`. Fixes are not backported to
  older 0.x releases.
- Vendor-specific behaviour is supported once it is verified ([`COMPATIBILITY.md`](COMPATIBILITY.md)); until then it
  is best effort.
