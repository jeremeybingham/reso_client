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
cargo run -p reso-ingest -- sync --config servers.toml --server mls-one --resource Property --backfill
cargo run -p reso-ingest -- media --config servers.toml --out data/ --limit 1000
cargo run -p reso-ingest -- discover --config servers.toml --server mls-one
cargo run -p reso-ingest -- discover --config draft.toml --starter                     # onboarding
cargo run -p reso-ingest -- status --out data/
cargo run -p reso-ingest -- serve --config servers.d/ --out data/ --listen 0.0.0.0:9090 --reload-every 1m
cargo run -p reso-ingest -- reset --server mls-one --resource Member --out data/
cargo run -p reso-ingest -- replay --server mls-one --resource Property --after 2026-11-01T00:00:00Z --out data/
cargo run -p reso-ingest -- history --out data/ --server mls-one --limit 5
cargo run -p reso-ingest -- purge --server mls-one --out data/                          # dry run
cargo run -p reso-ingest -- purge --server mls-one --out data/ --execute --by "J. Smith"
```

- **`sync`** pulls each resource the server's `$metadata` lists (after the config's `include`/`exclude` filters),
  continuing from the saved checkpoint; the first run is the full load. `--server` and `--resource` narrow it (each can
  be repeated). `--deletes` also writes delete events. `--start-over` drops the selected checkpoints first.
  `--max-file-mb` (default 256) starts a new data file after that size. `--partition` (with `--format parquet`) writes
  files into Hive-style folders (see [Partitioned Parquet](#partitioned-parquet---partition)). `--media` queues each page's media files as soon
  as the page is written, and download workers fetch them while the sync goes on, into `--media-dir` or the bucket (see
  [Media during sync](#media-during-sync-sync---media)).
  `--backfill` runs a first load with progress on its own share of the rate limits, optionally for a
  `--from`/`--to` window (see [Backfill](#backfill)).
- **`media`** downloads the files that the top-level `Media` records from `sync` point at (see
  [Media files](#media-files)).
- **`discover`** prints each server's `$metadata` model as JSON: resources, keys, properties, enums. With `--starter`
  it onboards a server instead (see [Operating many feeds](#operating-many-feeds)).
- **`sync --metrics-file PATH`** also writes the run's metrics (see Metrics below).
- **`status`** shows the saved checkpoints and the last run's manifest.
- **`serve`** runs as a daemon: each resource syncs on its own interval, and health, metrics and status endpoints answer
  over HTTP (see [Daemon mode](#daemon-mode-serve)).
- **`reset`**, **`replay`** and **`history`** clear or move back one feed's checkpoint and list past runs (see
  [Operating many feeds](#operating-many-feeds)).
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
| `reso_ingest_last_run_requests`, `_retries`, `_http_bytes` | HTTP requests (retries included), retries, body bytes; the resource's own requests, also when other feeds of the server use its client at the same time |
| `reso_ingest_last_run_responses` (also `class`) | Responses by `2xx` (3xx included), `429`, `4xx`, `5xx` |
| `reso_ingest_last_run_warnings`, `_deletes_held` | Check findings and held deletes (see [Run checks](#run-checks)) |
| `reso_ingest_schema_changes` (`server` only) | `$metadata` changes against the previous run (absent when none) |
| `reso_ingest_backfill_remaining_records` | With `--backfill`: records left at the end of the run (the server's count at the start, less those written; absent when the server gives no count) |
| `reso_ingest_backfill_rate_percent` | With `--backfill`: the share of the rate limits the backfill ran with |
| `reso_ingest_server_failed` (`server` only) | 1 for a server that failed as a whole |
| `reso_ingest_last_run_finished_timestamp_seconds`, `_resources` | No labels: when the run ended, resources handled |
| `reso_ingest_last_run_media_files` (`server`, `outcome`) | With `--media`: files `downloaded`, `unchanged` (same content under a new stamp), `shared` (content already stored for another key), `not_modified` (304), `failed`, `expired` (401, 403 or 410 from the file host; also in `failed`), `deleted` |
| `reso_ingest_last_run_media_bytes`, `reso_ingest_media_pending` (`server`) | With `--media`: bytes downloaded; files still to fetch |
| `reso_ingest_media_index_ok` (`server`) | With `--media`: 0 when the media index could not be read or saved |
| `reso_ingest_rate_limit_used`, `reso_ingest_rate_limit` (`server`, `unit`, `window`) | Rate-limit budget: requests (`unit="requests"`, `window` `second`, `hour` or `day`) or response bytes (`unit="bytes"`, `hour` or `day`) counted in the rolling window, and the ceiling the client paces to, one pair per window set in `[server.limits]`. Taken when the server's sync ends (`serve`: when scraped). The count starts at zero with each new client: a new process, changed secrets, or a backfill's rate share (the ceiling is then the share); the vendor may still count earlier requests |

Not measured: checkpoint age (the freshness lag covers the same question) and the media downloader's own rate limit
(`[server.media]`). The request counters cover the pull and delete detection, not media downloads (those are the `media`
metrics) or the `$metadata` fetch at the start of a server's sync (that one is in the rate-limit budget). OpenTelemetry
is not exported; the text format needs no new dependency, and collectors can scrape it with their Prometheus receiver.

`monitoring/` holds an example Grafana dashboard (`grafana-dashboard.json`: import it and pick the Prometheus data
source) and Prometheus alert rules (`alert-rules.yml`: stale feed, failed resource or server, repeated 429s, no run
finished, held deletes, schema changes, check findings). A test checks that both use only the metrics above. Their thresholds are starting points.

## Configuration

One TOML file lists the servers, or a directory of them (see [Operating many feeds](#operating-many-feeds));
[servers.example.toml](servers.example.toml) shows every setting. Secrets are never
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

[server.backfill]                   # optional; share of [server.limits] for `sync --backfill`
rate_percent = 50                   # 1 to 99 (default 50); other syncs of the server get the rest meanwhile

[server.media]                      # optional; file downloads, separate from the API limits
per_second = 4                      # also per_hour, per_day, bytes_per_hour, bytes_per_day, max_retries, *_backoff_ms
timeout_secs = 300                  # per file; default 120
send_token = "same_host"            # never, same_host (default) or always
workers = 4                         # download workers per server (default 4)

[server.network]                    # optional; API and media requests of this server
proxy = "http://proxy.corp.example:3128"   # or proxy_env / proxy_file when it holds user:password@
no_proxy = "localhost,.corp.example"
system_proxy = true                 # honour HTTPS_PROXY/HTTP_PROXY/NO_PROXY when no proxy is set (default true)
ca_files = ["/etc/ssl/corp-ca.pem"] # extra trusted CA certificates (PEM)
user_agent = "acme-ingest/1.0 (data@acme.example)"   # default reso-ingest/<version>
pool_max_idle_per_host = 8          # default no cap
pool_idle_timeout_secs = 90         # default 90
```

Each server runs as its own task: one server failing does not stop the others.

**Network.** `[server.network]` sets the proxy, extra CA certificates, User-Agent and connection pool of one server's
API, token and media requests (see `NetworkOptions` in `reso-client`). A proxy URL with a user name or password is a
secret: `proxy` rejects one, and `proxy_env` or `proxy_file` names where it is read from, like the auth secrets (read
again before each resource, so a rotated file is picked up). CA files are read when the client is built. Vendors
that only accept registered IP addresses need a fixed egress IP: a NAT gateway or a forward proxy; see "Fixed egress
IP" in the [top-level README](../README.md#network-proxy-ca-certificates-user-agent-connection-pool). The run-summary
webhook does not use these settings; it honours the proxy variables.

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

## Backfill

The first pull of a large feed can take days. `sync --backfill` runs it so that it can be watched and does not starve
the incremental syncs of the same server:

- **Progress.** At the start it asks the server how many records are left (`$count=true&$top=0` with the same filters,
  after the checkpoint when resuming) and logs `backfill: N records left`. After each page a progress line says how far
  along it is and the time left, `##progress step=1/1 label=mls/Property sub=120000/4500000 eta=3h07m`, and a
  `backfill progress` log line follows at most every 30 seconds. A server that gives no count gets `msg=N_records`
  instead. The manifest's resource entry has a `backfill` object (`remaining_at_start`, `rate_percent`, the window), and
  the metrics file the two `reso_ingest_backfill_*` metrics.
- **Its own rate budget.** The backfill uses `rate_percent` (default 50) of every limit in `[server.limits]`. While it
  holds its lease, every other `sync` or `serve` feed of that server, in any process sharing the state, uses the rest
  (`100 - rate_percent`), and goes back to the whole limits once no backfill is running. Shares are rounded down, with
  at least 1 of each limit. The share is checked at the start of each resource; when it changes, the server's client is
  rebuilt, and its rate window starts empty.
- **Stop and resume.** It saves its checkpoint after each page like any sync, so a stopped backfill (Ctrl-C, SIGTERM,
  a crash) carries on where it stopped when run again. While it runs, a plain `sync` of a feed with no checkpoint yet
  skips it ("a backfill is loading this feed") instead of starting a second full load; feeds that already have one
  sync as usual.
- **Windows.** `--from` and `--to` (RFC 3339 with a time zone, `--from` inclusive, `--to` exclusive) load only records
  whose `ModificationTimestamp` is in that range, with a checkpoint of their own, so several windows can run at once
  and each resumes by itself. When a window with no `--to` finishes and the feed has no checkpoint of its own, the
  window's checkpoint becomes the feed's, and incremental syncs continue from there. Windows cannot be combined with
  `--deletes` (a key comparison needs the whole feed).

The backfill marks itself with a lease named `<resource>~backfill` (plus the window, `state/<server>.<resource>_backfill....lock`
in a state directory); a whole-feed backfill also takes the feed's own lease, a window does not. `serve` has no backfill
mode: run the backfill as a separate `sync` beside it.

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
  the same second as another in the same `--out` gets `-02`, `-03`, … added. `.scratch/` holds the temporary files of
  a `--deletes` key comparison while it runs, in a `keys-<pid>-<n>` directory per pass; a run that is killed leaves its
  directory behind, and the next pass removes any such directory unchanged for a day (#134). A writer never overwrites an existing file:
  it stops with an error instead.
- `state/` (or `--state`): one JSON checkpoint per server and resource (and one `$metadata` snapshot per server), saved after each batch is written, so a rerun
  after a crash continues from there. Delivery is at least once: a record can appear twice, never zero times (see
  [Delivery guarantee](#delivery-guarantee)).
- `state/<server>.<resource>.keys.ndjson` (with `--deletes` and no event feed): the key set last seen, a header line
  (server, resource, time) then one key per line as JSON, sorted, without repeats. A key set in the older one-JSON
  format (`.keys.json`) is converted on the next `--deletes` run.
- `state/<server>.<resource>.lock`: the feed's lease while a `sync` runs (process id, host, expiry; the expiry is renewed
  every 100 seconds and removed when the sync ends). A second `sync` on the same state directory skips that feed with a
  warning in the manifest and log (`skipped: another process is syncing this feed`), without failing the run. After a
  crash the lease runs out in at most 5 minutes, then the next run takes the feed over. It works for processes that share
  the state directory on a local disk; `media` does not take leases yet.

All of `state/` goes through one key-value interface, `StateStore` (get, put, create-if-absent, replace-if-unchanged,
delete, list, and copies to and from local files for large values such as key sets). The command uses the file backend, `FileStateStore`: one file per key, written to a temporary file,
synced and renamed. A library user can pass another backend (object storage, a database) to `CheckpointStore::with_store`,
`Lease::acquire`, `MediaOptions` and `ServeOptions`; leases rely on its conditional writes. Shared backends are #85. With the `s3` feature, `S3StateStore` keeps state as objects under a bucket prefix: AWS S3,
R2, MinIO and RustFS through the S3 API (version = ETag, `create`, `replace` and versioned `delete` use `If-None-Match`
and `If-Match`, the delete as a presigned `DELETE` since `object_store` has no conditional delete (#133);
credentials from `AWS_ACCESS_KEY_ID` and friends, never the config file; `S3Options.single_delete` for a store without
multi-delete), or Google Cloud Storage through its own API (`Provider::Gcs`: version = object generation, `create`,
`replace` and versioned `delete` use `x-goog-if-generation-match`; credentials from a service-account key file). `S3FileSink` uploads finished data files to the same kind of bucket (one conditional `PUT` up to 32 MiB,
multipart above; never replaces a name; use it from a multi-threaded runtime).

**Running against a bucket (`--features s3`).** `sync`, `serve`, `media`, `status` and `purge` take `--s3-bucket` (env
`RESO_S3_BUCKET`), `--s3-prefix`, `--s3-endpoint` (R2, MinIO, RustFS), `--s3-region`, `--s3-allow-http` (a local test
server) and `--s3-single-delete` (a store without multi-delete), or `--gcs-bucket` (env `RESO_GCS_BUCKET`) with
`--gcs-prefix` (an alias of `--s3-prefix`) for Google Cloud Storage. State then goes to `<prefix>/state/` and finished data files to `<prefix>/data/`,
so a killed run resumes from the bucket and workers share feeds through leases. `--s3-bucket` and `--gcs-bucket` conflict with `--state` and with each other.
A checkpoint is saved only once the file holding its rows is in the bucket, so NDJSON files are then closed at the first
page end past `--max-file-mb` (a file can pass the limit by up to one page, as Parquet files do) rather than in the
middle of a page; a killed run loses at most the open file, whose pages the next run fetches again. A killed process
leaves its lease behind, so its feeds wait up to 5 minutes before another worker takes them over.
Credentials are the standard `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, or for GCS `GOOGLE_SERVICE_ACCOUNT` (path of a
service-account JSON key; also `GOOGLE_SERVICE_ACCOUNT_KEY`, `GOOGLE_APPLICATION_CREDENTIALS`, or the metadata server on
Google Cloud). Run manifests (`manifest.<run>.json`) and
`sync --media` manifests (`media.<run>.json`) go to `<prefix>/data/` next to the data files they list, and a copy of the
newest run manifest is kept at `<prefix>/state/runs/last.json`, which `status` reads, so a worker with an empty `--out`
sees the last run. Run ids are reserved in the bucket (`state/runs/<run>.reserved`, created with `If-None-Match`, removed
once the manifest is delivered), so two workers started in the same second get different ids. Each reservation records
when it was made; a run killed before delivery leaves its key behind, and `sync`/`serve` (at most once an hour per
process) delete reservations older than 7 days, conditionally on the version just read (#132). Reservations written by
earlier versions carry no time: the first sweep stamps them and they go 7 days later. All of this relies on
the store enforcing conditional writes, so `sync` and `serve` first probe it (#131): they create a throwaway object
`state/.probe/<pid>-<nanos>`, try to create it again, to replace it and to delete it with a stale version, delete it,
and stop with an error (exit code 2) if any of the three succeeds. That costs up to three `PUT`s and two `DELETE`s per
start.
`--s3-unsafe-no-conditional-writes` skips the probe (logging a warning) for a bucket only one worker ever uses. `--out` is only a staging
directory then (files are removed once the bucket has them; the `.run.<run>` markers stay). `sync --media` and `serve --media`
stream media files to `<prefix>/media/<server>/<sha256>.<ext>` (`--media-dir` is not used): a file up to 8 MiB is held
in memory and created with one conditional `PUT`; a larger one goes up in parts to a `.part-*` name, is copied to its
final name and the staging object deleted. Nothing is written to the local disk. Not routed: the metrics file
(`--metrics-file` feeds a node exporter on the same host; `serve` has `/metrics`). The `media` command refuses a bucket,
since the data files it reads are in the bucket: use `sync --media` instead.
`cargo test -p reso-ingest --features s3 --test s3_live` runs against a real server when `RESO_S3_TEST_BUCKET` is set
with `RESO_S3_TEST_ENDPOINT` (a non-AWS server) or `RESO_S3_TEST_REGION` (AWS S3, no endpoint), or against GCS when
`RESO_GCS_TEST_BUCKET` and `GOOGLE_SERVICE_ACCOUNT` are set. Checked against RustFS 2026-11-25 (MinIO no longer publishes binaries or images)
and **AWS S3** 2026-11-25 (#115): conditional create, replace and delete, multipart uploads and listings past 1000 keys;
`sync`, `status` and the container image against a bucket; two `serve` workers on one prefix; a `kill -9` partway through
a `sync` taken over by a fresh worker once the lease ran out.

**Smallest AWS IAM policy** (checked 2026-11-25, #115: `s3_live`, then `sync --deletes` twice and `status`, all with only
this policy). Put the bucket name in both places.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Effect": "Allow", "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::BUCKET" },
    { "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"],
      "Resource": "arn:aws:s3:::BUCKET/*" }
  ]
}
```

`GetObject` covers reads and existence checks (`HEAD`); `PutObject` covers plain and conditional uploads and every
multipart step but the abort; `DeleteObject` releases leases and run-id reservations; `ListBucket` serves `status` and
state listings. `AbortMultipartUpload` is only used when a large upload fails partway; without it the parts stay until a
lifecycle rule removes them. `s3:ListBucketMultipartUploads` and `s3:ListMultipartUploadParts` are not needed.

**Google Cloud Storage: use `--gcs-bucket`, not the S3 interoperability endpoint.** Through the interoperability API,
GCS accepts `If-None-Match: *` and `If-Match` on `PUT` and `DELETE` and ignores them (checked 2026-11-25, #115): a second
`create` overwrites, so two workers could both take a feed. `--s3-endpoint https://storage.googleapis.com` is therefore
refused with an error. `--gcs-bucket` goes through GCS's own API, where `x-goog-if-generation-match` is enforced, and a
versioned delete (lease release) is a conditional `DELETE`, with no check-then-delete window. The bucket role
`Storage Object User` on the bucket is enough. In the container image, which runs as uid 65532, the mounted key file must be readable by
that uid (or run the container with `-u`). Checked against a real bucket 2026-11-26 (#130): `s3_live`, `sync` twice
and `status`, two `serve` workers on one prefix, the container image, and a `kill -9` partway through a `sync` taken over by a fresh worker.

Finished data files can go to a `FileSink` instead of staying in `--out`: a library user calls `with_sink` on
`NdjsonWriter` or `ParquetWriter`. The writer stages each file, delivers it when full or at the end, and reports a safe
point (so `sync` saves a checkpoint) only after the sink has it. `LocalDirSink` copies into a directory; `S3FileSink`
uploads to a bucket, and the command selects it with `--s3-bucket` or `--gcs-bucket` (see above).

## Delivery guarantee

What a consumer of the data files can rely on, for resources synced incrementally (a single-field key and a
`ModificationTimestamp`; others are skipped):

- **At least once.** Every record the server returns for the replication filter ends up in a data file. A record can
  arrive more than once, never zero times. Repeats come from a run that stopped between writing a page and saving its
  checkpoint (a crash, a failed checkpoint write, or with Parquet or a bucket a file that was never finished): the next
  run starts after the last saved checkpoint and fetches those pages again. A record changed on the server while a run
  is going can also come twice in one run, with two different `ModificationTimestamp`s.
- **Ordered within a resource.** Within one run of one resource, records come in the order the server sorts them:
  `ModificationTimestamp`, then the key. Files of a run are numbered in that order (`0001`, `0002`, …), and run ids
  sort by start time. Delete events of a run follow its records. Not ordered: different resources or servers, and
  backfill windows (`sync --backfill --from/--to`), which run separately.
- **A repeat is the same record.** A repeated record is the same JSON the server sent; only the `fetched_at` and
  `run_id` tags differ (in NDJSON, the text from `"record":` to the end of the line is byte for byte the same).
  `sync_tests.rs` `an_interrupted_run_repeats_identical_records_in_order` checks this.

**Idempotency key:** `(source, resource, <key field>, ModificationTimestamp)`. Two lines with the same idempotency key
hold the same version of a record; keep either. For the latest state of a record, keep, per `(source, resource, key)`,
the line with the newest `fetched_at`, then the newest `ModificationTimestamp`. A delete event (`"event":"delete"`, with
`key` instead of `record`) that is newer than every version of the key means the record is gone. The
[compaction recipe](#compacting-parquet-files) below does this.

## Purging a source

When an MLS licence ends, `purge --server NAME` removes what `reso-ingest` wrote for that server (#93):

- the data files in `--out` whose `source` tag is the server (NDJSON files, read line by line, including `.part` files a
  killed run left), the files the run manifests list under the server (Parquet files), and everything in its partition
  folder, `source=<server>/` (`--partition`; the emptied folders are removed too);
- its media files, `<media-dir>/<server>/` (default `<out>/media/<server>/`), or `<prefix>/media/<server>/` with a bucket;
- its state keys: checkpoints, key sets, event sequences, the schema snapshot and leases (`<server>.*`) and the media
  index (`media/<server>.json` and its deltas, `media/<server>/*.json`).

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

**With a bucket** (`--s3-bucket` or `--gcs-bucket`, #139), the data files and state keys are deleted in the bucket
(`<prefix>/data/`, `<prefix>/state/`), and the media files in `<prefix>/media/<server>/`.
The server's files are found by the run manifests in `<prefix>/data/` rather than by reading every object: only NDJSON
objects named like the server's that no manifest lists (a run killed before delivering its manifest) are downloaded
into `--out` and judged by their tag. The record, its `.sha256` and the access line are written to `<out>/audit/` as
above and copied to `<prefix>/data/` (`purge.<id>.json`, `purge.<id>.json.sha256`, `access.<id>.ndjson`, the last with
the dots of the id as `_`), next to the access copies `sync` leaves there. If a copy fails after the deletes, the
local record and access line stand, the error is printed and the command exits 1. The sink's list, read and delete were checked
against AWS S3 and GCS on 2026-11-26 (`s3_live`); the whole `purge` command only against an in-process S3 fake. It uses
only actions the IAM policy below already grants (`s3:ListBucket`, `s3:GetObject`, `s3:DeleteObject`, `s3:PutObject`).

## Daemon mode (`serve`)

`cron` starting `sync` syncs every resource at the same pace. `serve` is one long-running process that gives each
resource its own pace: Property every two minutes, Member every hour. A resource is scheduled when it has an
`interval` (`"500ms"`, `"30s"`, `"2m"`, `"1h"`, `"1d"`); a server's own `interval` covers the resources in its `include`
list that have none. Nothing is scheduled by default, and `serve` refuses to start when no resource has an interval
(a paused one counts, so a daemon can start with every feed paused). Feeds can be added, removed, paused and resumed
while it runs (see [Operating many feeds](#operating-many-feeds)).

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
  `--format`, `--max-file-mb`, `--partition`). `$metadata` is read at the start of every sync.
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
| `/metrics` | Prometheus text: the numbers under Metrics for each feed's last sync, plus `reso_ingest_feed_runs_total`, `reso_ingest_feed_failed_runs_total`, `reso_ingest_feed_consecutive_failures`, `reso_ingest_feed_interval_seconds` and `reso_ingest_feed_paused` (labels `server`, `resource`); with `--media`, `reso_ingest_media_files_total` (also `outcome`), `reso_ingest_media_bytes_total`, `reso_ingest_media_pending` and `reso_ingest_media_index_ok` per feed |
| `/status` | JSON: `started_at`, `reloads`, `last_reload`, `last_reload_error` and, per feed, `state` (`waiting`, `queued`, `running`, `paused`), `interval_secs`, `runs`, `failed_runs`, `consecutive_failures`, `last_started`, `last_finished`, `last_success`, `last_error`, `last_records`, `last_deletes`, `next_run_in_secs` |

Unconfirmed against a real server (#32): everything here is tested against mock servers only. Feeds of one server
share its client, so `reso_ingest_rate_limit_used` is per server, read live from that client at each scrape; the
per-feed request counters count each feed's own requests.

## Operating many feeds

**A directory of files.** `--config` takes a directory as well as a file: every `*.toml` file in it is read, in name
order (subdirectories and other files are ignored), and together they make one configuration, so each server can live
in its own file (`servers.d/mls-one.toml`). A server defined in two files is an error, and so is a top-level setting
(`max_concurrent_servers`, `webhook_url_env`) set in more than one file; errors name the file.

**Pausing.** `paused = true` on a `[[server]]` or a `[server.resources.X]` table leaves it out: `sync` skips it with a
line on stderr and never contacts a paused server, and `serve` keeps the feed in `/status` (state `paused`, counters
kept) without syncing it.

**Reloading `serve`.** `serve` reads its configuration again on SIGHUP, and every `--reload-every` interval with that
option (`--reload-every 1m`, handy where sending a signal is awkward, such as a Kubernetes ConfigMap). Feeds added to
the configuration start, removed ones stop after their current page, paused ones stop and resumed ones start again; an
`interval` change restarts that feed alone, and any other change to a server's settings (URL, auth, limits, filters)
rebuilds its client and restarts its feeds. A stopped feed always finishes its page before its replacement starts. A
configuration that does not load is logged and shown in `/status` as `last_reload_error`, and the daemon keeps running
the old one; `--server` given to `serve` still applies. Options given on the command line (`--deletes`, `--format`, ...)
are not reloaded.

**Admin commands.** These change one feed's checkpoint while holding its lease, so they refuse while a `sync` or
`serve` is pulling that feed (stop the feed with `paused = true`, or wait). They take `--out`, `--state` and the bucket
options, not `--config`.

- `reset --server S --resource R` deletes the checkpoint, so the next sync is a full load. It prints the checkpoint it
  removed; exit `1` when there was none.
- `replay --server S --resource R --after TIME` sets the checkpoint to an RFC 3339 time with no key: the next sync pulls
  again every record modified strictly after it. It refuses a time later than the saved checkpoint (that would skip
  records); with no checkpoint it sets where the first sync starts.
- To pull a closed window again without touching the checkpoint, use `sync --backfill --from --to` (see
  [Backfill](#backfill)).
- `history [--server S] [--resource R] [--limit N]` lists past runs from their manifests, newest first (default 10
  runs), one line per feed: records, deletes and `ok`, `interrupted` or `FAILED: error`, plus failed servers. With a
  bucket it reads the manifests in `<prefix>/data/`. `serve` keeps a manifest only for syncs that wrote or failed
  something, so quiet polls are not listed.

**Onboarding a server.** Write a draft with the new server's `name`, `base_url` and `[server.auth]` (and `filter` or
`dataset_id` if the vendor needs them), export its secrets, and run `discover --config draft.toml --starter`. It reads
`$metadata` and one record of each resource (`$top=1&$count=true`) with those credentials, then prints a starter block:
an `include` list of the resources that are readable and have a `ModificationTimestamp`, and a comment per resource with
its key and record count or why it was left out (for example a 403 for a resource the licence does not cover). Exit `1`
when `$metadata` cannot be read or no resource is readable. Servers do not publish their rate limits in a standard way,
so the block leaves `[server.limits]` for the vendor's numbers; copy the draft's `[server.auth]` table into it (secrets
are never printed).

Unconfirmed against a real server (#32): onboarding's `$top=1&$count=true` probe and the 403 a vendor sends for an
unlicensed resource are tested against mock servers only.

## Containers and deployment

The [`Dockerfile`](../Dockerfile) at the repository root builds one image with the `parquet` and `s3` features: a
static musl binary on `scratch`, about 30 MB, running as user 65532. It holds no libc, shell or package manager. The only
C code is two crypto libraries, compiled in the build stage and linked statically: `ring` (rustls for RESO servers and
the webhook) and `aws-lc` (rustls inside `object_store`, used by the `--s3-*` options).

TLS roots: requests to RESO servers and the webhook use roots built into the binary (webpki-roots). The S3 client checks
certificates against the system CA bundle, so the image carries one at `/etc/ssl/certs/ca-certificates.crt` (Alpine's).
For a store with a private CA, mount a bundle that includes it over that file. Without any bundle, every S3 request
fails with `Generic HTTP client error: builder error`. For a RESO server behind a private CA or a TLS-inspecting proxy,
mount its PEM and list it in `[server.network] ca_files`.

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
  (one writer per volume), liveness on `/healthz`, readiness on `/readyz`, and a Service for `/metrics` and `/status`; `--reload-every 1m` picks up an edited ConfigMap.
- [`systemd/reso-ingest.service`](../deploy/systemd/reso-ingest.service): `serve` on a single host, without containers,
  with `DynamicUser` and a state directory; `systemctl reload` sends SIGHUP to reload the configuration.

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
  first run is the baseline and reports no deletes). The key comparison keeps memory flat: keys are sorted in chunks
  of 100 000 that spill to temporary files in `<out>/.scratch/` (removed after each pass, and by the next pass after a
  day when a killed run left them; about 3 times the key set, some 40 MB per million keys), and the saved set is diffed by merging the two sorted streams. A million keys
  peak at 24 MiB, as do ten million (`LOADTEST.md`). With `deletes.flag_field` set, flagged records produce the events
  instead (see above).

## Media files

`reso-ingest media` turns the `Media` records that `sync` wrote into files:

1. It reads the NDJSON `Media` data files listed in the `manifest.*.json` files in `--out`, skipping the ones it has
   read before, and keeps the newest record per `MediaKey`. Parquet data files are not read (listed as skipped).
2. It downloads each new or changed `MediaURL` into `--media-dir` (default `<out>/media`) as
   `<server>/<sha256>.<ext>`, named by the SHA-256 of the content (#89). The index maps each `MediaKey` to its file, so
   the same photo under several keys is stored once, and a file whose content did not change is kept as it is. The
   server name is kept as is when it has only letters, digits, `-` and `_`; other characters are percent-encoded. The
   extension comes from the response's `Content-Type` (none when unknown); `MediaType` is not used, as it is often wrong.
   A file no key points at any more is deleted.
3. It writes `media.<run>.json` (per server: downloaded, unchanged, shared with another key, not modified, deleted,
   failed with the error, expired URLs, still pending, bytes) and prints a summary.

What counts as changed: a new `MediaKey`, or a new `MediaModificationTimestamp` (`ModificationTimestamp` when that is
absent). A new URL alone does **not** count, because signed CDN URLs change on every pull; the newest URL is kept for
the next download. A record with no timestamp at all is fetched again when its URL changes. A re-download sends the
saved `ETag` and `Last-Modified`, so an unchanged file costs a 304 and no transfer. A delete event removes the file.

A file that fails (404, an expired signed URL, a timeout after retries) is listed in the media manifest and stays
pending; the run carries on, exits `1`, and the next run tries it again. `--limit N` caps downloads per server per run;
the rest stay pending. Files are streamed to a `.part-*` name (the hash is known only at the end) and renamed when
complete, so a partial file never sits under a final name.

The index of what was read and saved is `state/media/<server>.json` plus small deltas in `state/media/<server>/`: each
save (after every page, every 50 downloads and at the end) writes only the entries that changed as a new delta, and once
the deltas hold more entries than the index (at least 10 000) the next save writes a new `<server>.json` and deletes
them, so saves cost what changed rather than the size of the index (#149). A load reads `<server>.json` and applies the
newer deltas in order; a delta that cannot be read is an error, not a fresh start. Downloads have their own limits (`[server.media]`),
so they do not use up the API's. The API token goes only to the API's own host by default (`send_token = "same_host"`):
media usually lives on a CDN that does not need it. Each server has `[server.media] workers` download workers (default
4); servers run in parallel.

### Media during sync (`sync --media`)

Some vendors (MLS Grid) have no top-level `Media` resource: media come as an array inside `Property`, `Member` and
`Office` records (`expand = ["Media"]`), and their URLs are signed, single-use and expire within the hour, so a later
`reso-ingest media` run cannot use them. `sync --media` queues the files of each page as soon as the page is written
and checkpointed, and a separate pool of download workers (`[server.media] workers`, default 4, with the
`[server.media]` limits) fetches them while the next pages are pulled. A slow file host does not slow the record sync;
the run ends once the queue is empty:

- For the `Media` resource each record is a media record; for any other resource the media records in each record's
  `Media` array are taken (`[server.media] nested_field = "..."` names another field). Each uses its own `MediaKey`,
  `MediaURL` and `MediaModificationTimestamp`, and the same change rule and index as above.
- Only files that are new or changed and whose record is in the page are fetched; a key seen on several pages before its
  download starts is fetched once. Files go to `--media-dir` (default `<out>/media`), or to the bucket (see
  "Running against a bucket" under [What it writes](#what-it-writes)), named by content as above. The run writes `media.<run>.json`, prints one line
  per server, and with `--metrics-file` adds the media metrics.
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

Columns are the four tags, `fetched_at` (UTC timestamp in milliseconds), `event`, `key` (delete events) and a `record` struct with one
field per `$metadata` property: strings, booleans, integers, doubles, `Date32`, UTC millisecond timestamps, lists for
collections. Decimals become doubles; enum, complex and unknown types become text. Properties missing from `$metadata`
are dropped, except navigation properties named in `expand`, which are kept as JSON text. A Parquet file is readable only once finished, so `sync` keeps one file open across pages and saves a checkpoint only
when the file holding its rows is finished (at `--max-file-mb` or the end of the resource). A run that dies mid-file leaves
a `.part` file and resumes from the last finished file, so those pages are fetched again (at-least-once).

### Partitioned Parquet (`--partition`)

`sync --format parquet --partition` (and `serve`) writes each file into a Hive-style folder:

```
source=<server>/resource=<resource>/date=<YYYY-MM-DD>/<server>.<resource>.<run>.<NNNN>.parquet
```

`date` is the UTC day the rows were fetched (from `fetched_at`): a run that goes past midnight starts a new file in the
next day's folder. Files are still rotated at `--max-file-mb`, the target file size (default 256 MiB). Folder values
keep letters, digits, `.`, `-` and `_` and escape every other byte as `%XX`, as Hive does, so they decode back to the
`source` and `resource` columns, which the files still hold. Manifests list the files with their folders. With
`--s3-bucket` or `--gcs-bucket` the folders become key prefixes under `<prefix>/data/`. `--partition` needs
`--format parquet`. Query engines that understand Hive partitions read the tree directly and skip folders a filter
rules out:

```sql
-- DuckDB
SELECT record.ListingKey, record.ListPrice
FROM read_parquet('reso-out/source=*/resource=Property/date=*/*.parquet', hive_partitioning = true)
WHERE source = 'mls-one' AND date >= DATE '2026-11-01';
```

### Compacting Parquet files

Many runs leave many small files, and repeats ([Delivery guarantee](#delivery-guarantee)). There is no `compact`
command; this DuckDB recipe merges one resource's files and keeps the latest version of each key, with the rule above.
Replace `ListingKey` with the resource's key field:

```sql
COPY (
  SELECT * EXCLUDE (k, ts)
  FROM (
    SELECT *, coalesce(record.ListingKey, key) AS k, record.ModificationTimestamp AS ts
    FROM read_parquet('reso-out/source=*/resource=Property/date=*/*.parquet', hive_partitioning = false)
  )
  QUALIFY row_number() OVER (PARTITION BY source, k ORDER BY fetched_at DESC, ts DESC NULLS LAST) = 1
      AND event IS NULL   -- drop keys whose latest event is a delete; remove this line to keep the delete events
) TO 'compacted' (FORMAT parquet, PARTITION_BY (source, resource), FILE_SIZE_BYTES '256MB');
```

It writes `compacted/source=…/resource=…/data_N.parquet`, a new tree: check it, then replace the old files and keep
the manifests. Compact while no `sync` writes to the resource, or only files older than the newest run, so no file is
removed before it is merged. The same query reads NDJSON output with `read_json('reso-out/*.Property.*.ndjson')`.
The recipe was checked with DuckDB 1.5 on files the writer made; it is not run in CI.

## Load test

`cargo run --release -p reso-ingest --example loadtest -- --records 1000000` syncs a million invented records from a
fake server with injected faults and kills, and checks that none is lost. How it works and the recorded numbers (25 MiB
peak memory at 1M records) are in [LOADTEST.md](LOADTEST.md).

## As a library

The same pieces are public:

- Configuration: `Config` (parse the TOML), `ServerConfig` (with `filter_for`, `expand_for`, `page_size_for`),
  `ResourceConfig`, `DeletesConfig`, `LimitsConfig`, `MediaConfig`.
- Running: `run_servers` (one task per server, capped), `sync_resource` and `pull_resource` (one resource into a
  `RecordWriter`), `sync_resource_media` (the same with `PageMedia` queueing each page's files), `reserve_run_id`.
- Output and state: `NdjsonWriter` / `ParquetWriter`, `RunManifest`, `CheckpointStore`, `StateStore` (the state
  backend interface) and `FileStateStore` (and `S3StateStore` with feature `s3`), `FileSink` and `LocalDirSink` (where finished data files go), `Lease`.
- Deletes: `reconcile_deletes` (with a `DeleteTarget`), `EventFeed`, `FlagDeleteWriter` (wraps a writer and adds an
  event after each flagged record).
- Media: `sync_media`, `page_pipeline` (a `PageMedia` that queues each page's files and the `MediaWorkers` that fetch
  them), `MediaIndex` (with `add_nested` for expanded arrays), `MediaManifest`, `MediaStore` with `LocalMediaStore` and
  `S3MediaStore` (feature `s3`), `MediaRoot`.

See the crate docs: `cargo doc -p reso-ingest --open`.
