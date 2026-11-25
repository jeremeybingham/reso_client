# reso-ingest

Pulls every resource from one or more [RESO Web API](https://www.reso.org/reso-web-api/) servers into files, keeps
them current with incremental runs, records deletes, and downloads the photos and documents that `Media` records
point at. Built on [`reso-client`](../README.md).

Status: in this repository only (`publish = false`, version 0.0.1). Checked end to end against RESO's reference server
run locally; not yet run against a vendor server
([#32](https://github.com/jeremeybingham/reso_client/issues/32)).

## Commands

```sh
cargo run -p reso-ingest -- sync --config servers.toml --out data/
cargo run -p reso-ingest -- sync --config servers.toml --server mls-one --resource Property --deletes
cargo run -p reso-ingest -- sync --config servers.toml --server mlsgrid --media
cargo run -p reso-ingest -- media --config servers.toml --out data/ --limit 1000
cargo run -p reso-ingest -- discover --config servers.toml --server mls-one
cargo run -p reso-ingest -- status --out data/
cargo run -p reso-ingest -- serve --config servers.toml --out data/ --listen 0.0.0.0:9090
cargo run -p reso-ingest -- purge --server mls-one --out data/                          # dry run
cargo run -p reso-ingest -- purge --server mls-one --out data/ --execute --by "J. Smith"
```

- **`sync`** pulls each resource the server's `$metadata` lists (after the config's `include`/`exclude` filters),
  continuing from the saved checkpoint; the first run is the full load. `--server` and `--resource` narrow it (each can
  be repeated). `--deletes` also writes delete events. `--start-over` drops the selected checkpoints first.
  `--max-file-mb` (default 256) starts a new data file after that size. `--media` downloads each page's media files
  right after the page is written, into `--media-dir` (see [Media during sync](#media-during-sync-sync---media)).
- **`media`** downloads the files that the top-level `Media` records from `sync` point at (see
  [Media files](#media-files)).
- **`discover`** prints each server's `$metadata` model as JSON: resources, keys, properties, enums.
- **`sync --metrics-file PATH`** also writes the run's metrics (see Metrics below).
- **`status`** shows the saved checkpoints and the last run's manifest.
- **`serve`** runs as a daemon: each resource syncs on its own interval, and health, metrics and status endpoints answer
  over HTTP (see [Daemon mode](#daemon-mode-serve)).
- **`purge`** deletes one server's data files, media files and state when its licence ends (see
  [Purging a source](#purging-a-source)).

Progress lines (`##progress step=N/T label=server/resource`) go to stderr and the summary to stdout. Exit code `0` on
success, `1` if a server or resource failed (the summary names it), `2` for bad usage or configuration, `3` when a shutdown request ended a `sync` early (see Shutdown). `--help` on
any command lists every option.

## Logs

Log lines go to stderr, between the progress lines. The level comes from `RUST_LOG` (default `info`; for example
`RUST_LOG=reso_ingest=info,reso_client=warn` or `RUST_LOG=debug`). `--log-format json` (or `RESO_LOG_FORMAT=json`;
the flag wins) writes one JSON object per line for log shippers; the default is `text`. Every line inside a run carries
`run_id`, `server` and `resource` (when it belongs to one), so one feed can be filtered out of a busy log:

```json
{"timestamp":"2026-11-25T04:36:53.112587Z","level":"INFO","fields":{"message":"resource synced","records":17,"deletes":0},"target":"reso_ingest::cli","spans":[{"run_id":"20261125T043653Z","name":"run"},{"server":"ref","name":"server"},{"resource":"Office","name":"resource"}]}
```

In text format the spans are a prefix: `run{run_id=...}:server{server=ref}:resource{resource=Office}: ... resource
synced records=17 deletes=0`. The progress and per-resource lines are plain text in both formats; keep only the JSON
lines with `grep '^{'`. Logs never hold tokens, client secrets or record contents. At `info` each server logs its
`$metadata` fetch and query URLs, each resource one `resource synced` (or `resource failed`) line, and each
`media` server one summary line; `debug` adds retries, batch sizes and checkpoints.

## Metrics

`sync` records, per server and resource, how long the pull took and what its requests did (`stats` in the manifest).
`--metrics-file PATH` also writes them as Prometheus text when the run ends (replaced atomically; point the
node_exporter textfile collector at its directory). A long-running process serves the same numbers at `/metrics`
(see [Daemon mode](#daemon-mode-serve)).

The main number is **freshness lag**: now minus the newest `ModificationTimestamp` written, taken from the saved
checkpoint, so it keeps growing for a feed that fails or goes quiet, and is absent for one that never wrote a record.
The rest describe the run just finished. Labels are `server` and `resource` unless noted.

| Metric | Meaning |
|---|---|
| `reso_ingest_freshness_lag_seconds` | Seconds behind (see above) |
| `reso_ingest_last_run_success` | 1 if the pull ended without error, else 0 |
| `reso_ingest_last_success_timestamp_seconds` | End of the last error-free run (absent after a failed run) |
| `reso_ingest_last_run_records`, `_deletes`, `_files`, `_file_bytes` | Records, delete events, data files and their size |
| `reso_ingest_last_run_duration_seconds`, `_records_per_second` | Time taken, and records over time |
| `reso_ingest_last_run_requests`, `_retries`, `_http_bytes` | HTTP requests (retries included), retries, body bytes |
| `reso_ingest_last_run_responses` (also `class`) | Responses by `2xx` (3xx included), `429`, `4xx`, `5xx` |
| `reso_ingest_last_run_warnings`, `_deletes_held` | Check findings and held deletes (see [Run checks](#run-checks)) |
| `reso_ingest_schema_changes` (`server` only) | `$metadata` changes against the previous run (absent when none) |
| `reso_ingest_server_failed` (`server` only) | 1 for a server that failed as a whole |
| `reso_ingest_last_run_finished_timestamp_seconds`, `_resources` | No labels: when the run ended, resources handled |

Not measured yet: the rate-limit budget used (the limiter does not report it) and checkpoint age (the freshness lag
covers the same question). The request counters cover the pull and delete detection, not media downloads. OpenTelemetry
is not exported; the text format needs no new dependency.

`monitoring/` holds an example Grafana dashboard (`grafana-dashboard.json`: import it and pick the Prometheus data
source) and Prometheus alert rules (`alert-rules.yml`: stale feed, failed resource or server, repeated 429s, no run
finished, held deletes, schema changes, check findings). A test checks that both use only the metrics above. Their thresholds are starting points.

## Configuration

One TOML file lists the servers; [servers.example.toml](servers.example.toml) shows every setting. Secrets are never
written in it: `token_env`, `client_id_env` and `client_secret_env` name the environment variables that hold them, and
a file with a literal secret is rejected. Each can instead be read from a file with `token_file`, `client_id_file` or
`client_secret_file` (for example a Kubernetes Secret mounted as files); surrounding whitespace, such as a final
newline, is removed. Each secret takes one of the two forms.

**Rotation.** Secrets are read again before each resource is synced, and a new client is built when one changed, so a
rotated secret file is used without a restart (an environment variable cannot change in a running process). When the
server or its token endpoint refuses the credentials (401, or a failed token request), the secrets are read once more:
if they changed, the resource is tried again once with the new ones, from its checkpoint; if not, it fails as before.
A new client starts with an empty rate-limit window. Secret values never appear in logs, errors or output. Secret
managers (Vault, AWS Secrets Manager) are not built in; a file written by their agents works.

```toml
max_concurrent_servers = 2          # default 4

[[server]]
name = "mls-one"
base_url = "https://api.mls-one.example/odata"
include = ["Property", "Member", "Office", "Media"]   # empty or absent: every resource
exclude = ["OpenHouse"]             # wins over include

[server.auth]
type = "client_credentials"         # or "bearer" with token_env
token_url = "https://auth.mls-one.example/oauth/token"
client_id_env = "MLS_ONE_CLIENT_ID"
client_secret_env = "MLS_ONE_CLIENT_SECRET"

[server.limits]                     # optional; default 5 retries, no rate limit
per_second = 2

[server.media]                      # optional; file downloads, separate from the API limits
per_second = 4                      # also per_hour, per_day, bytes_per_hour, bytes_per_day, max_retries, *_backoff_ms
timeout_secs = 300                  # per file; default 120
send_token = "same_host"            # never, same_host (default) or always
```

Each server runs as its own task: one server failing does not stop the others.

Some servers need a filter on every request or serve related records only inline. `filter` on a server is added (by
`and`) to every query sent to it: sync pulls, and the key pulls and lookups of `--deletes` (not the `EntityEvent`
query). `[server.resources.<Name>]` adds a filter, an `expand` list and a `page_size` for one resource; `expand` goes on
sync pulls only. For example, from MLS Grid's public documentation (not yet tried against it,
[#32](https://github.com/jeremeybingham/reso_client/issues/32)):

```toml
[[server]]
name = "mlsgrid"
base_url = "https://api.mlsgrid.com/v2"
include = ["Property", "Member", "Office", "OpenHouse"]
filter = "OriginatingSystemName eq 'example'"   # required on every MLS Grid request
deletes = { flag_field = "MlgCanView" }         # false on a removed record: written raw, plus a delete event

[server.resources.Property]
expand = ["Media", "Rooms", "UnitTypes"]
page_size = 1000                                # MLS Grid's limit with $expand

[server.limits]
per_second = 2
per_hour = 7200
```

A server that signals deletes with a flag field instead of an event resource (MLS Grid keeps a removed record in the
feed for 7 days with `MlgCanView` false) is described with `deletes = { flag_field = "MlgCanView" }` on the server.
During sync the record is still written raw, and a delete event for its key follows it; this happens on every sync,
with or without `--deletes`, and `--deletes` does no key comparison for that server. For the first load, MLS Grid
advises `MlgCanView eq true`: add it to the server's or resource's `filter`. Expanded records stay inside their parent
record, untouched; they carry no flag and get no events, so a consumer replaces the whole child set each time it stores
a new copy of the parent.

To download the files of expanded `Media` arrays, use `sync --media` (see [Media files](#media-files)).

## Run checks

Each run compares itself with the last and writes what it finds to the manifest (`warnings` per resource,
`schema_changes` per server, `deletes_held`), to the log at `warn` level, and to the metrics. Findings do not change the
exit code: the run's data is fine, you decide what to do. The webhook (below) and the alert rules in `monitoring/` are
how someone hears about them.

| Check | What it reports | Where |
|---|---|---|
| `$metadata` changes | Resources and fields added or removed, a field's type changed, enum members added or removed, since the previous run (a snapshot per server in `state/<server>.@schema.json`; the first run only saves it). Covers the resources the configuration allows, not only the ones a `--resource` run pulled. Lookup values that come from a `Lookup` resource are data, not `$metadata`, and are not compared. | `schema_changes` |
| Count | On a full load (no checkpoint at the start), the records written against `$count=true&$top=0` with the same filters. A server that gives no count is skipped without a finding. | `warnings` |
| Repeated keys | The same key twice in one page. | `warnings` |
| Records with no key, or a `ModificationTimestamp` older than the checkpoint or than the record before it | Not warnings: the replication stops the resource with an error (a server that ignored `$filter` or `$orderby`), as before. | `error` |
| Delete volume | With `deletes.max_deletes` and/or `deletes.max_percent` (1 to 100, of the keys known before the pass; key comparison only) set, a `--deletes` pass that finds more writes none, saves no new state, and reports `deletes_held`. The next run finds the same deletes again; after checking the server, run `sync --deletes --allow-mass-deletes` once to write them. | `deletes_held`, `warnings` |

Delete limits apply to the key comparison and to the `EntityEvent` feed (`max_deletes` only, as the feed has no
key total); `flag_field` deletes are written as the records stream by and are not held. Limits are set per server:

```toml
deletes = { max_deletes = 5000, max_percent = 20 }
```

### Webhook

`webhook_url_env = "RESO_WEBHOOK"` at the top of the configuration names the environment variable that holds the URL
(it often holds a token, so it is not written in the file). When a run ends, after the manifest and metrics file, the
run summary is POSTed to it as JSON (30-second timeout, no retries). A failed POST or an unset variable is reported on
stderr (the URL is never printed) and the exit code is `1`.

```json
{"run_id": "20261125T120000Z", "started_at": "...", "finished_at": "...", "clean": true,
 "resources": 3, "records": 120, "deletes": 2, "deletes_held": 0,
 "failures": ["mls-one/Property: ..."],
 "warnings": ["mls-one/Property: 1 records repeat a `ListingKey` already in the same page",
              "mls-one: schema: Property.ListPrice: type Edm.Decimal -> Edm.String"],
 "manifest": "reso-out/manifest.20261125T120000Z.json"}
```

`clean` is false when a resource or server failed; `warnings` are the check findings above.

## What it writes

In `--out` (default `reso-out`):

- `<server>.<resource>.<run>.<NNNN>.ndjson`: one line per record, the record untouched inside four source tags:

  ```json
  {"source":"mls-one","resource":"Property","fetched_at":"2026-11-24T12:00:00Z","run_id":"...","record":{...}}
  ```

  A delete event has the same tags, then `"event":"delete"` and the record's `key` instead of `record`.
- `manifest.<run>.json`: counts, files, checkpoints and errors for the run.
- `audit/access.ndjson`: the access log (#93), one line per resource pull, appended at the end of each resource step of
  `sync` and of each `serve` feed sync, also when the pull failed:

  ```json
  {"at":"2026-11-25T12:00:00Z","run_id":"...","server":"mls-one","resource":"Property","credential":"env:MLS_ONE_TOKEN","window_start":"2026-11-24T00:00:00Z","window_end":"2026-11-25T11:59:58Z","records":120,"deletes":2,"bytes_received":815302,"bytes_written":402113,"status":"ok","error":null}
  ```

  `credential` names where the token (or, for client credentials, the client id) is read from: `env:VAR` or
  `file:/path`, never the secret. `window_start` is the checkpoint the pull started after (`null` on a full load),
  `window_end` the timestamp of the last record pulled. `status` is `ok`, `interrupted` or `failed` (with `error`).
  A feed skipped because another process holds its lease is not a pull and gets no line. The file is opened in append
  mode only; nothing in `reso-ingest` rewrites or truncates it, and a line that cannot be written fails that resource.
  With `--s3-bucket` the lines a run appended are also delivered as `<prefix>/data/access.<run>.ndjson` (objects cannot
  be appended to); `serve` delivers them only for runs that keep a manifest (a poll that found nothing new stays in the
  local log only).
- `.run.<run>`: an empty marker that reserves the run id. Ids are the start time (`20261124T173005Z`); a run started in
  the same second as another in the same `--out` gets `-02`, `-03`, … added. A writer never overwrites an existing file:
  it stops with an error instead.
- `state/` (or `--state`): one JSON checkpoint per server and resource (and one `$metadata` snapshot per server), saved after each batch is written, so a rerun
  after a crash continues from there. Delivery is at least once: a record can appear twice, never zero times.
- `state/<server>.<resource>.lock`: the feed's lease while a `sync` runs (process id, host, expiry; the expiry is renewed
  every 100 seconds and removed when the sync ends). A second `sync` on the same state directory skips that feed with a
  warning in the manifest and log (`skipped: another process is syncing this feed`), without failing the run. After a
  crash the lease runs out in at most 5 minutes, then the next run takes the feed over. It works for processes that share
  the state directory on a local disk; `media` does not take leases yet.

All of `state/` goes through one key-value interface, `StateStore` (get, put, create-if-absent, replace-if-unchanged,
delete, list). The command uses the file backend, `FileStateStore`: one file per key, written to a temporary file,
synced and renamed. A library user can pass another backend (object storage, a database) to `CheckpointStore::with_store`,
`Lease::acquire`, `MediaOptions` and `ServeOptions`; leases rely on its conditional writes. Shared backends are #85. With the `s3` feature, `S3StateStore` keeps state as objects under a bucket prefix (AWS S3,
R2, MinIO, GCS interoperability; version = ETag, `create` and `replace` use `If-None-Match` and `If-Match`; credentials
from `AWS_ACCESS_KEY_ID` and friends, never the config file; `S3Options.single_delete` for GCS, which has no
multi-delete). `S3FileSink` uploads finished data files to the same kind of bucket (one conditional `PUT` up to 32 MiB,
multipart above; never replaces a name; use it from a multi-threaded runtime).

**Running against a bucket (`--features s3`).** `sync`, `serve`, `media` and `status` take `--s3-bucket` (env
`RESO_S3_BUCKET`), `--s3-prefix`, `--s3-endpoint` (R2, MinIO, GCS), `--s3-region`, `--s3-allow-http` (a local test
server) and `--s3-single-delete` (GCS). State then goes to `<prefix>/state/` and finished data files to `<prefix>/data/`,
so a killed run resumes from the bucket and workers share feeds through leases. `--s3-bucket` conflicts with `--state`.
A checkpoint is saved only once the file holding its rows is in the bucket, so NDJSON files are then closed at the first
page end past `--max-file-mb` (a file can pass the limit by up to one page, as Parquet files do) rather than in the
middle of a page; a killed run loses at most the open file, whose pages the next run fetches again. A killed process
leaves its lease behind, so its feeds wait up to 5 minutes before another worker takes them over.
Credentials are the standard `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`. Run manifests (`manifest.<run>.json`) and
`sync --media` manifests (`media.<run>.json`) go to `<prefix>/data/` next to the data files they list, and a copy of the
newest run manifest is kept at `<prefix>/state/runs/last.json`, which `status` reads, so a worker with an empty `--out`
sees the last run. Run ids are reserved in the bucket (`state/runs/<run>.reserved`, created with `If-None-Match`, removed
once the manifest is delivered), so two workers started in the same second get different ids. `--out` is only a staging
directory then (files are removed once the bucket has them; the `.run.<run>` markers stay). Not routed: the metrics file
(`--metrics-file` feeds a node exporter on the same host; `serve` has `/metrics`) and downloaded media files
(`--media-dir`). The `media` command refuses `--s3-bucket`, since the data files it reads are in the bucket: use
`sync --media` instead.
`cargo test -p reso-ingest --features s3 --test s3_live` runs against a real server when `RESO_S3_TEST_BUCKET` is set
with `RESO_S3_TEST_ENDPOINT` (a non-AWS server) or `RESO_S3_TEST_REGION` (AWS S3, no endpoint); add
`RESO_S3_TEST_SINGLE_DELETE=1` for GCS. Checked against RustFS 2026-11-25 (MinIO no longer publishes binaries or images);
AWS S3 and GCS are not confirmed yet (#115).

Finished data files can go to a `FileSink` instead of staying in `--out`: a library user calls `with_sink` on
`NdjsonWriter` or `ParquetWriter`. The writer stages each file, delivers it when full or at the end, and reports a safe
point (so `sync` saves a checkpoint) only after the sink has it. `LocalDirSink` copies into a directory. The command does
not select a sink yet (#107).

## Purging a source

When an MLS licence ends, `purge --server NAME` removes what `reso-ingest` wrote for that server (#93):

- the data files in `--out` whose `source` tag is the server (NDJSON files, read line by line, including `.part` files a
  killed run left), and the files the run manifests list under the server (Parquet files);
- its media directory, `<media-dir>/<server>/` (default `<out>/media/<server>/`);
- its state keys: checkpoints, key sets, event sequences, the schema snapshot and leases (`<server>.*`) and the media
  index (`media/<server>.json`).

Nothing else is touched: other servers' files, the access log, run manifests and media manifests stay (the manifests list
file names and counts, not records). A file whose name starts like the server's but that neither its tags nor a manifest
assign to it is listed as *not purged* and left alone.

Without `--execute` it is a dry run that lists what would go. `--execute` requires `--by NAME` (who ran it). It refuses
(exit 1, nothing deleted) while another process holds a lease on one of the server's feeds, and holds those leases itself
while deleting, so a sync of the server skips them. After deleting it writes:

- `audit/purge.<id>.json`: server, who, start and end time, status, every data file deleted with its record and byte
  counts, the media file and byte counts, the state keys removed, and the files not purged;
- `audit/purge.<id>.json.sha256`: the record's SHA-256 (`sha256sum -c` checks it);
- one line in `audit/access.ndjson` with `"resource":"*"`, `"status":"purged"` and a `purge` object (who, the record's
  name and hash, counts).

The hash in the append-only access log makes a later change to the record visible; the record is not signed. If a
deletion fails, the purge stops, the record lists what was deleted with `"status":"failed"` and the error, and the
command exits 1. Remove the server from the configuration afterwards: a later `sync` would start a full load again, and
`media` for it would look for data files that are gone.

`purge` works on local storage only. With `--s3-bucket` it refuses: deleting data files in a bucket needs `list` and
`delete` on the file sink (#107).

## Daemon mode (`serve`)

`cron` starting `sync` syncs every resource at the same pace. `serve` is one long-running process that gives each
resource its own pace: Property every two minutes, Member every hour. A resource is scheduled when it has an
`interval` (`"500ms"`, `"30s"`, `"2m"`, `"1h"`, `"1d"`); a server's own `interval` covers the resources in its `include`
list that have none. Nothing is scheduled by default, and `serve` refuses to start when no resource has an interval.

```toml
[[server]]
name = "mls-one"
include = ["Property", "Member", "Office"]
interval = "1h"             # Member and Office
concurrency = 2             # feeds of this server syncing at once (default 2)

[server.resources.Property]
interval = "2m"
```

- **Each sync is `sync --server S --resource R`** for that one feed: it takes the feed's lease, writes tagged files,
  checkpoints after each page, and does the options you gave `serve` (`--deletes`, `--allow-mass-deletes`, `--media`,
  `--format`, `--max-file-mb`). `$metadata` is read at the start of every sync.
- **Timing.** The first sync of each feed starts after a random delay of up to 10 seconds (or its interval, if shorter) so
  the feeds do not hit a server together; after a sync the feed waits its interval plus up to 10% jitter. A feed
  that fails waits twice as long after each failure in a row, up to 15 minutes (or its interval, if that is longer), and
  goes back to its interval after one success. Failing feeds do not stop the others.
- **Rate limits.** All feeds of a server share one client, so its `[server.limits]` (requests per second, minute, hour,
  day) and its OAuth token cover them together; `concurrency` caps how many of them run at once.
- **Output.** Data files are written as in `sync`. A manifest is written only for a sync that wrote records or deletes,
  failed, or had findings; a poll that found nothing leaves no manifest and no `.run.*` marker.
- **Stopping.** On SIGTERM or Ctrl-C every feed stops after its current page, with output and checkpoint consistent (see
  Shutdown), and `serve` exits `0`. Exit `2` for bad configuration, no scheduled resource, a missing secret variable or file, or an
  address that cannot be bound.

The endpoints speak plain HTTP with no authentication and no TLS: bind to a private address (default `127.0.0.1:8080`,
`--listen ADDR:PORT`) and let the platform's network rules decide who can reach it.

| Path | Answer |
|---|---|
| `/healthz` | `200` while the process is up |
| `/readyz` | `200` when state can be saved (a probe key `.readyz` is written and deleted), `503` when not (config is read before the server starts, so a bad one never gets this far) |
| `/metrics` | Prometheus text: the numbers under Metrics for each feed's last sync, plus `reso_ingest_feed_runs_total`, `reso_ingest_feed_failed_runs_total`, `reso_ingest_feed_consecutive_failures` and `reso_ingest_feed_interval_seconds` (labels `server`, `resource`) |
| `/status` | JSON: `started_at` and, per feed, `state` (`waiting`, `queued`, `running`), `interval_secs`, `runs`, `failed_runs`, `consecutive_failures`, `last_started`, `last_finished`, `last_success`, `last_error`, `last_records`, `last_deletes`, `next_run_in_secs` |

Unconfirmed against a real server (#32): everything here is tested against mock servers only. With concurrent feeds of one
server the per-feed request counters in the metrics overlap, because they are differences of one shared client's counters.

## Containers and deployment

The [`Dockerfile`](../Dockerfile) at the repository root builds one image with the `parquet` and `s3` features: a
static musl binary on `scratch`, about 30 MB, running as user 65532. It holds no libc, shell or package manager. The only
C code is two crypto libraries, compiled in the build stage and linked statically: `ring` (rustls for RESO servers and
the webhook) and `aws-lc` (rustls inside `object_store`, used by the `--s3-*` options).

TLS roots: requests to RESO servers and the webhook use roots built into the binary (webpki-roots). The S3 client checks
certificates against the system CA bundle, so the image carries one at `/etc/ssl/certs/ca-certificates.crt` (Alpine's).
For a store with a private CA, mount a bundle that includes it over that file. Without any bundle, every S3 request
fails with `Generic HTTP client error: builder error`.

```sh
docker build -t reso-ingest .
docker run --rm -v "$PWD/servers.toml:/config/servers.toml:ro" -v "$PWD/data:/data" -e MLS_ONE_TOKEN \
  reso-ingest sync --config /config/servers.toml --out /data
```

Everything comes from arguments, environment variables (the `*_env` secrets, `RUST_LOG`, `RESO_LOG_FORMAT`) and
mounted files (the `*_file` secrets, if used): the configuration read-only, and one writable volume for output and checkpoints (`/data`, the working
directory). The root filesystem can be read-only. CI builds the image on tags and on pull requests that change it, and
runs the binary inside it; it pushes nothing (publishing an image is a release decision).

Examples in [`deploy/`](../deploy/):

- [`kubernetes/config.yaml`](../deploy/kubernetes/config.yaml): the configuration as a ConfigMap, secrets as a Secret
  (passed with `envFrom`), a PersistentVolumeClaim for `/data`.
- [`kubernetes/cronjob.yaml`](../deploy/kubernetes/cronjob.yaml): one-shot `sync` on a schedule, `concurrencyPolicy:
  Forbid`, no retries (the next schedule continues from the checkpoints).
- [`kubernetes/deployment.yaml`](../deploy/kubernetes/deployment.yaml): `serve` with one replica and `Recreate`
  (one writer per volume), liveness on `/healthz`, readiness on `/readyz`, and a Service for `/metrics` and `/status`.
- [`systemd/reso-ingest.service`](../deploy/systemd/reso-ingest.service): `serve` on a single host, without containers,
  with `DynamicUser` and a state directory.

All of them set a stop grace period (120 s) longer than one page request, so a SIGTERM ends with output and checkpoints
consistent (see [Shutdown](#shutdown)). Smoke-tested against RESO's reference server in a local
[kind](https://kind.sigs.k8s.io/) cluster (the CronJob and the Deployment, probes, a pod deletion) and with `docker
run`/`docker stop`; the systemd unit is checked with `systemd-analyze verify` only.

## Shutdown

On SIGTERM or Ctrl-C a `sync` stops asking for pages. A page request still in flight is dropped (its records were never
written, so the next run fetches that page again); everything already written is flushed and checkpointed. Resources
not yet started are left out, the manifest and the summary say `interrupted`, the webhook summary has `"interrupted":
true`, and the exit code is `3`. Nothing is lost or repeated: the next `sync` continues from the checkpoints. A second
signal exits at once with code `130`, with no manifest. In a container, give the stop a grace period longer than one
page request (`docker stop -t`, Kubernetes `terminationGracePeriodSeconds`). `media` does not handle signals yet.

## How it syncs

- Replication is RESO-standard: filter on `ModificationTimestamp`, order by timestamp then key, follow
  `@odata.nextLink`, resume strictly after the last (timestamp, key). Resources without a single-field key or a
  `ModificationTimestamp` are skipped (an error if named with `--resource`).
- Filters: the server's `filter` and the resource's `filter` are added (by `and`) to the replication filter;
  `expand` and `page_size` from `[server.resources.<Name>]` go on every page.
- Deletes (`--deletes`): when `$metadata` declares an `EntityEvent` resource, its feed is read from the last sequence
  number and each named record is looked up; otherwise every key is pulled and compared with the previous run's (the
  first run is the baseline and reports no deletes). With `deletes.flag_field` set, flagged records produce the events
  instead (see above).

## Media files

`reso-ingest media` turns the `Media` records that `sync` wrote into files:

1. It reads the NDJSON `Media` data files listed in the `manifest.*.json` files in `--out`, skipping the ones it has
   read before, and keeps the newest record per `MediaKey`. Parquet data files are not read (listed as skipped).
2. It downloads each new or changed `MediaURL` into `--media-dir` (default `<out>/media`) as `<server>/<MediaKey>.<ext>`.
   The key is kept as is when it has only letters, digits, `-` and `_`; other characters are percent-encoded. The
   extension comes from the response's `Content-Type` (none when unknown); `MediaType` is not used, as it is often wrong.
3. It writes `media.<run>.json` (per server: downloaded, not modified, deleted, failed with the error, still pending,
   bytes) and prints a summary.

What counts as changed: a new `MediaKey`, or a new `MediaModificationTimestamp` (`ModificationTimestamp` when that is
absent). A new URL alone does **not** count, because signed CDN URLs change on every pull; the newest URL is kept for
the next download. A record with no timestamp at all is fetched again when its URL changes. A re-download sends the
saved `ETag` and `Last-Modified`, so an unchanged file costs a 304 and no transfer. A delete event removes the file.

A file that fails (404, an expired signed URL, a timeout after retries) is listed in the media manifest and stays
pending; the run carries on, exits `1`, and the next run tries it again. `--limit N` caps downloads per server per run;
the rest stay pending. Files are written to `<name>.part` and renamed when complete, so a partial file never sits under
the final name.

The index of what was read and saved is `state/media/<server>.json`. Downloads have their own limits (`[server.media]`),
so they do not use up the API's. The API token goes only to the API's own host by default (`send_token = "same_host"`):
media usually lives on a CDN that does not need it. Downloads run one at a time per server; servers run in parallel.

### Media during sync (`sync --media`)

Some vendors (MLS Grid) have no top-level `Media` resource: media come as an array inside `Property`, `Member` and
`Office` records (`expand = ["Media"]`), and their URLs are signed, single-use and expire within the hour, so a later
`reso-ingest media` run cannot use them. `sync --media` downloads the files of each page right after the page is written
and checkpointed, before the next page is fetched:

- For the `Media` resource each record is a media record; for any other resource the media records in each record's
  `Media` array are taken (`[server.media] nested_field = "..."` names another field). Each uses its own `MediaKey`,
  `MediaURL` and `MediaModificationTimestamp`, and the same change rule and index as above.
- Only files that are new or changed and whose record is in the page are fetched. Files go to `--media-dir` (default
  `<out>/media`), with the `[server.media]` limits. The run writes `media.<run>.json` and prints one line per server.
- A failed file stays pending and the run exits `1`. It is tried again only when a later sync sees its parent record
  again (a changed parent brings a fresh URL); with single-use URLs a pending file otherwise needs `--start-over` for the
  resource. Re-fetching parents by key for fresh URLs is not done.
- Delete events and deleted parents do not remove files during `sync --media`; `reso-ingest media` applies delete events
  of the top-level `Media` resource. `reso-ingest media` still reads only top-level `Media` data files.

Not yet checked against a vendor file host
([#32](https://github.com/jeremeybingham/reso_client/issues/32)): signed URLs that expire before the run reaches them
(they fail with 403 until the next `sync` brings a fresh URL), vendor media limits, and real volumes.

## Parquet

Build with the `parquet` feature and pass `--format parquet` for typed columns instead of NDJSON:

```sh
cargo run -p reso-ingest --features parquet -- sync --config servers.toml --format parquet
```

Columns are the four tags, `fetched_at` (timestamp), `event`, `key` (delete events) and a `record` struct with one
field per `$metadata` property: strings, booleans, integers, doubles, `Date32`, UTC millisecond timestamps, lists for
collections. Decimals become doubles; enum, complex and unknown types become text. Properties missing from `$metadata`
are dropped, except navigation properties named in `expand`, which are kept as JSON text. A Parquet file is readable only once finished, so `sync` keeps one file open across pages and saves a checkpoint only
when the file holding its rows is finished (at `--max-file-mb` or the end of the resource). A run that dies mid-file leaves
a `.part` file and resumes from the last finished file, so those pages are fetched again (at-least-once).

## As a library

The same pieces are public:

- Configuration: `Config` (parse the TOML), `ServerConfig` (with `filter_for`, `expand_for`, `page_size_for`),
  `ResourceConfig`, `DeletesConfig`, `LimitsConfig`, `MediaConfig`.
- Running: `run_servers` (one task per server, capped), `sync_resource` and `pull_resource` (one resource into a
  `RecordWriter`), `sync_resource_media` (the same with `PageMedia` downloading each page's files), `reserve_run_id`.
- Output and state: `NdjsonWriter` / `ParquetWriter`, `RunManifest`, `CheckpointStore`, `StateStore` (the state
  backend interface) and `FileStateStore` (and `S3StateStore` with feature `s3`), `FileSink` and `LocalDirSink` (where finished data files go), `Lease`.
- Deletes: `reconcile_deletes` (with a `DeleteTarget`), `EventFeed`, `FlagDeleteWriter` (wraps a writer and adds an
  event after each flagged record).
- Media: `sync_media`, `MediaIndex` (with `add_nested` for expanded arrays), `MediaManifest`, `PageMedia`.

See the crate docs: `cargo doc -p reso-ingest --open`.
