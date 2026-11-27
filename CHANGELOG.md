# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Planned as 0.4.0. Both crates are released together and share version numbers from now on. `reso-ingest` is
published for the first time; its entries below cover everything since 0.3.0, while it was in the repository but
unpublished. Neither crate has been run against a vendor server yet
([#32](https://github.com/jeremeybingham/reso-rs/issues/32), [#76](https://github.com/jeremeybingham/reso-rs/issues/76)):
the checks against one, and the changes they bring, are planned for 0.5.0.

### Upgrading `reso-client` from 0.3

- Configuration and result types and the public enums are `#[non_exhaustive]` (#157): build structs with their
  constructors and builders (or `Default` and field assignment) instead of struct literals, and give a `match` on
  `Auth`, `CredentialTransport` or `SendToken` a `_` arm. This also covers the new fields `ClientConfig::network`,
  `RateLimit::per_day`/`bytes_per_hour`, the `ClientStats` response counts and `EnumMember::standard_name`.
- `Page::body` no longer holds the `value` array; read the records from `Page::records` (#165).
- `ODataErrorResponse` and `ODataErrorDetail` are no longer public.
- Behaviour: `execute_count` sends `$count=true&$top=0` instead of requesting `/Resource/$count` (#53); every request
  sends a `User-Agent` (`reso-client/<version>`, #91); a response body cut off partway is a retried
  `ResoError::Network` instead of a `ResoError::Parse` (#125).
- The minimum Rust version stays 1.87.

### reso-client

#### Added

- `ReplicationRun::next_batch_raw` returns `RawRecords`, each record's JSON text as the server sent
  it, without parsing the records into `serde_json::Value`s (#162, lead 4); the order check and checkpoint read only
  the timestamp and key members. `RawRecords::values` parses them when needed, `RawRecords::from_texts` builds one,
  and `record_fields` reads a few top-level members of a record's text without parsing the rest.
  `RawRecords::field` reads one member; for the key field it reuses the spans the replication already found, without
  another scan. `next_batch` is unchanged. Turns on `serde_json`'s `raw_value` feature (no new crate).
- `Downloader::download_into` streams a file into any `DownloadTarget` (`start`, `write` per chunk,
  `finish`, `abort` on failure) with the same retries, validators and limits as `download_to`; a target error is not
  retried. `download_to` is now `download_into` with a `.part` file target.
- Rate-limit budget and per-task request counts (#109). `reso-client`: `ResoClient::rate_usage` and
  `Downloader::rate_usage` return a `RateUsage`: for each window of the `RateLimit`, the requests or bytes counted in it
  now and the ceiling (`WindowUsage`, `fraction()`); it never waits, also while a request is held back by the limit.
  `count_requests(future)` returns the `ClientStats` of the requests `future` makes with any `ResoClient`, separate from
  other tasks sharing the client; scopes nest. `reso-ingest`: per-resource `stats` now count only the resource's own
  requests (under `serve`, feeds of one server syncing at once no longer count each other's, and a resource retried
  with changed secrets keeps the first try's requests); the manifest has `rate_budget` (per server, unit and window) and
  the metrics have `reso_ingest_rate_limit_used` and `reso_ingest_rate_limit`, read live from each server's client at
  `serve`'s `/metrics`; the example dashboard has a rate-limit panel.
- Network options (#91): `NetworkOptions` (`ClientConfig::with_network`) sets an HTTP(S) proxy (with basic proxy
  credentials and a `NO_PROXY` list; the proxy variables are honoured by default, `without_system_proxy` turns them
  off), extra trusted CA certificates (PEM bytes or a file), the `User-Agent`, and the connection pool's idle cap and
  timeout. They apply to API, token and `Downloader` requests. `ClientConfig::from_env` reads `RESO_PROXY`,
  `RESO_NO_PROXY`, `RESO_CA_FILE` and `RESO_USER_AGENT`. `reso-ingest` reads them from `[server.network]`; a proxy URL
  with credentials comes from `proxy_env` or `proxy_file`, never the file. Proxy passwords are left out of `Debug`
  output and errors. The README covers fixed egress IPs (NAT gateway or proxy) for vendors that allow-list addresses.
- `ClientStats` counts responses by status (`ok`, `throttled` for 429, `client_errors`, `server_errors`) for both the
  API client and `Downloader`, and has `ClientStats::since` for the difference between two snapshots (#79). The new
  public fields break code that builds `ClientStats` with a struct literal.
- File downloads (#30): `ResoClient::downloader(DownloadConfig)` returns a `Downloader` with its own `RetryPolicy`,
  `RateLimit`, timeout and counters. `Downloader::download_to(url, dest, &Validators)` sends a conditional GET
  (`If-None-Match`, `If-Modified-Since`), streams the body to `<dest>.part` and renames it when complete, and returns
  `DownloadOutcome::NotModified` or `Saved(SavedFile)` (bytes, `ETag`, `Last-Modified`, `Content-Type`). `SendToken`
  decides where the API token goes (default: the API's own scheme, host and port only). New error `ResoError::Io`
  (non-breaking: `ResoError` is `#[non_exhaustive]`). The `tokio` dependency gains the `fs` and `io-util` features; no
  new crates.
- `RateLimit::per_day` (requests per rolling 24 hours) and `RateLimit::bytes_per_hour` (#68), with `RESO_RATE_PER_DAY` and
  `RESO_BYTES_PER_HOUR` in `from_env` and `per_day`, `bytes_per_hour` in `[server.limits]` and `[server.media]`.
  MLS Grid's published limits (2/s, 7,200/h, 40,000/day, 4 GB/h) can now all be set; unconfirmed against it (#32).
  The new public fields break code that builds `RateLimit` with a struct literal.
- `StandardReplication::expand(&[&str])` sends `$expand` on every replication page, for servers that serve related
  records only inline (MLS Grid's `Media`) (#65).

#### Changed

- Breaking: `Page::body` no longer holds the `value` array; the records are moved out of the body into
  `Page::records` instead of copied, so each page's records are held once (#165, about 11% of a sync's CPU
  instructions in the 100k profile). Every other member of the response (`@odata.context`, `@odata.count`,
  `@odata.nextLink`, vendor annotations) stays in `body`. Read records from `records`.
- Breaking: API review for 1.0 (#157). These types are now `#[non_exhaustive]`, so new fields or
  variants are no longer breaking changes; outside the crate, structs are built with their constructors and builders
  (or `Default` plus field assignment) instead of struct literals, and a `match` on the enums needs a `_` arm:
  - configuration: `ClientConfig`, `ClientCredentials`, `NetworkOptions`, `RetryPolicy`, `RateLimit`,
    `DownloadConfig`, `Validators`, `Checkpoint` (build with `Checkpoint::new` or `Checkpoint::at`);
  - results: `Page`, `ReplicationResponse`, `SavedFile`, `ClientStats`, `RateUsage`, `WindowUsage`, and the metadata
    model (`Metadata`, `EntitySet`, `EntityType`, `Property`, `NavigationProperty`, `EnumType`, `EnumMember`);
  - enums: `Auth`, `CredentialTransport`, `SendToken`.

  `DownloadOutcome` stays exhaustive (304 or saved are the only outcomes of a conditional GET). `ODataErrorResponse` and
  `ODataErrorDetail` in `reso_client::error` are no longer public (they were undocumented helpers for parsing error
  bodies). New `Validators::with_etag` and `Validators::with_last_modified`. The public surface for 1.0 is listed in
  `docs/STABILITY.md`.
- requests now send a `User-Agent` header, `reso-client/<version>` by default (none was sent before);
  `reso-ingest` sends `reso-ingest/<version>` (#91).
- Breaking: `ClientConfig` has a new public field `network`; code that builds `ClientConfig` with a
  struct literal must set it (`NetworkOptions::default()`) (#91).
- A response body cut off partway (the connection closed before `Content-Length` bytes arrived) is now a
  `ResoError::Network` error and retried under the `RetryPolicy`, counted in `ClientStats.retries` (#125). It was a
  `ResoError::Parse` error ("Failed to read response"), never retried, so one cut body failed the whole resource. A
  complete body that is not valid JSON is still a `Parse` error and not retried. Found by the load test (#90).
- `ResoClient::execute_count` (and `QueryBuilder::count()`) now send `$count=true&$top=0` on the resource path and read
  `@odata.count` from the JSON body, instead of requesting `/Resource/$count`. Works on servers without a `/$count`
  route, such as the RESO reference server. `Query::to_odata_string()` for count-only queries changes accordingly (#53).

#### Fixed

- Download network errors no longer include the URL (#96): signed media URLs reached the debug log and the media
  manifest through reqwest's error text.
- `Metadata::parse` reads annotation values written as a `<String>` child element, resolves schema aliases in
  navigation property, property and entity set types, and keeps `RESO.OData.Metadata.StandardName` on enum members
  (new `EnumMember::standard_name`; code that builds `EnumMember` by struct literal must add the field) (#58).

### reso-ingest (first published release)

`reso-ingest` is first published at the same version as `reso-client` (#178). It is built and tested against mock
servers and RESO's reference server, but not yet run against a vendor server (#32, #76), so 0.5.0 may change its
options, configuration and output; renamed options and settings follow the deprecation policy in
`docs/STABILITY.md`. `Cargo.lock` is now committed, so `cargo install --locked reso-ingest` uses the versions CI
tested. The package leaves out the integration tests (they read files elsewhere in the repository) and the
load-test results. Its library API exists for the binary and is not covered by semver checks before 1.0.

What it does: `sync` pulls every resource of many servers into tagged NDJSON or Parquet files, incrementally and
resumably, with delete detection; `serve` runs the same as a daemon with health, metrics and status endpoints;
`media` and `sync --media` download the files `Media` records point at; `purge` and the access log help with
licence terms. State and output can live in a directory, an S3-compatible bucket or GCS (feature `s3`). The entries
below list each part as it was added; [`reso-ingest/README.md`](reso-ingest/README.md) describes the whole.

#### Added

- `serve --ui` (`ServeOptions::ui`) adds read-only HTML status pages: `/` (one row per feed, failing
  and paused rows highlighted), `/feed/<server>/<resource>` (last error, checkpoint, last 20 runs) and `/runs` (latest
  100 runs). Hand-written HTML, no scripts, reloads every 15 s, every value escaped. Feature `ui`, on by default, adds
  no dependency; without it `--ui` is a start-up error. A warning is logged when `--ui` listens on a public address
  (#161).
- `deploy/observability/`: a Prometheus + Grafana kit for one `serve` daemon (#161). It includes a scrape config with
  30-day retention, four `serve` alert rules (`ResoServeDown`, `ResoFeedFailing`, `ResoFeedNoSuccess`,
  `ResoRateLimitHigh`, with runbooks in `docs/OPERATIONS.md`) plus the existing rules, and a provisioned dashboard
  filtered by server and resource. It runs natively (`setup.sh install`: pinned, checksum-verified releases as
  systemd user units) or with `docker compose`.
- `serve --ui --grafana-url URL` (`ServeOptions::grafana_url`) links each feed on `/` and its feed page
  to the kit's dashboard filtered to that feed (`/d/reso-ingest-serve?var-server=…&var-resource=…`). Without `--ui`, or
  with a URL that is not `http://` or `https://`, it is a start-up error (#161).
- `serve` exports `reso_ingest_feed_last_success_timestamp_seconds` per feed. Under `serve`,
  `reso_ingest_last_success_timestamp_seconds` is never set, because the daemon has no run end time.
- `RecordWriter::write_raw_batch(raw, fetched_at)` (default: parse, then `write_batch`), and `sync`
  uses it. `NdjsonWriter` copies each record's text; the repeated-key check and flag deletes read only their members
  from it, so an NDJSON sync builds no `Value` per record. `sync --media` parses the records as before (Parquet:
  lead 5, below). In the 100k-record load test: see `docs/PERFORMANCE.md`.
- feature `mimalloc`, on by default, makes mimalloc the global allocator of the binary and the
  container image (#162, lead 3). In the 100k-record load test the sync uses about 25% less CPU and about twice the
  peak RSS (about 55 MiB instead of 24); see `docs/PERFORMANCE.md`. It adds C code (built by `cc`, linked statically).
  Build with `--no-default-features` for the system allocator.
- delivery guarantee documented (#86): at least once, ordered by `ModificationTimestamp` then key within
  a resource's run, idempotency key `(source, resource, key, ModificationTimestamp)`; a repeat after an interrupted run
  is the same record with new `fetched_at` and `run_id` (tested). `sync --partition` and `serve --partition` (Parquet
  only) write files to Hive-style folders `source=<server>/resource=<name>/date=<UTC fetch day>/`, still rotated at
  `--max-file-mb`; `purge` removes a server's partition folder. A DuckDB recipe for compacting files to the latest
  version of each key is in `reso-ingest/README.md`. New `ParquetWriter::partitioned`, `hive_value` and
  `partition_prefix`; `ServeOptions` gains `partition`.
- media at scale (#89). `sync --media` (and `serve --media`) queues each page's new and changed files
  for a separate pool of download workers (`[server.media] workers`, default 4, with the `[server.media]` rate limits),
  which fetch them while the record sync goes on, so a slow file host no longer holds up the pages; the run ends once
  the queue is empty. A key seen on several pages before its download starts is fetched once. Files are streamed with
  no local copy and named by the SHA-256 of their content (`<sha256>.<ext>`): the same photo under several `MediaKey`s
  is stored once, and a re-sent file with the same bytes is counted `unchanged` and kept. A file no key points at any
  more is deleted. With `--s3-bucket` or `--gcs-bucket`, files go to `<prefix>/media/<server>/` (one conditional
  `PUT` up to 8 MiB, multipart to a staging name and a copy above), and `purge` deletes them there. New `MediaStore`
  and `MediaUpload` traits, `LocalMediaStore`, `S3MediaStore` (feature `s3`), `MediaRoot`, `page_pipeline`,
  `PipelineOptions`, `MediaWorkers` and `DEFAULT_WORKERS`. Metrics: `reso_ingest_last_run_media_files` (by
  `outcome`), `_media_bytes`, `reso_ingest_media_pending` and `reso_ingest_media_index_ok` in the metrics file, and
  running totals (`reso_ingest_media_files_total`, `reso_ingest_media_bytes_total`) per feed on `serve`'s `/metrics`.
  Breaking: `MediaReport` gains `queued`, `unchanged`, `shared` and `expired` (401, 403 or 410 from the file host);
  `MediaEntry` gains `sha256`; `PageMedia` has no lifetime, `new` or `finish` (use `page_pipeline`); `sync_media`
  takes the worker count; `ServeOptions.media_dir` is now `media: Option<MediaRoot>`; `plan_purge` and
  `execute_purge` take a `&dyn MediaStore` instead of the media directory, and `PurgePlan.media_dir` is now `media`
  (a location string); `write_metrics_file` takes the media reports; `MediaConfig` gains `workers`. Files downloaded
  by earlier versions under `<MediaKey>.<ext>` are not renamed: they are fetched again under their hash when their
  record changes. Checked offline only (wiremock and an in-process S3 fake), not against a vendor file host (#32).
- `sync --backfill` for the first pull of a large feed (#88). It counts the records left with `$count`,
  prints `##progress` lines with the time left after every page, records `backfill` in the manifest and
  `reso_ingest_backfill_remaining_records`/`_rate_percent` in the metrics, and uses `[server.backfill] rate_percent`
  (default 50) of the server's rate limits while other syncs of the server use the rest. `--from`/`--to` load one
  `ModificationTimestamp` window with its own checkpoint. `ServerConfig` gains a `backfill` field and `ResourceEntry` a
  `backfill` field (struct literals need them).
- Google Cloud Storage through its own API (#130). `S3Options` gains `provider` (`Provider::S3`, the
  default, or `Provider::Gcs`); the commands take `--gcs-bucket` (env `RESO_GCS_BUCKET`, prefix from `--s3-prefix` or
  its alias `--gcs-prefix`), with credentials from a service-account key (`GOOGLE_SERVICE_ACCOUNT`). Versions on GCS
  are object generations, and a versioned delete is a conditional `DELETE` (`x-goog-if-generation-match`). The `s3`
  feature now enables `object_store`'s `gcp` feature as well (no new C code: the same rustls and aws-lc-rs stack).
  Checked against a real bucket.
- `sync` and `serve` check at start-up that a bucket enforces conditional writes (#131):
  `S3StateStore::check_conditional_writes` creates a throwaway object, tries to create it again and to replace it with
  a stale version, and deletes it; if either second write succeeds the command stops with exit code 2.
  `--s3-unsafe-no-conditional-writes` skips the check.
- run id reservations in the state store (`runs/<run>.reserved`) now record when they were made, and
  `sync`/`serve` remove ones older than 7 days (a run killed before delivering its manifest left its key for good), at
  most once an hour per process; each delete is conditional on the version read (#132).
- a `--deletes` key comparison first removes `keys-<pid>-<n>` directories in `<out>/.scratch` that
  nothing has changed for a day, which runs killed mid-pass (`kill -9`, OOM, eviction) left behind at about 40 MB per
  million keys (#134). Age decides rather than a pid check, so an output volume shared by several hosts is safe; a
  library caller's `DeleteTarget::scratch_dir` is swept the same way, the system temporary directory never.
- `purge` works with a bucket (`--s3-bucket`, `--gcs-bucket`, #139). The server's data files are found
  by the run manifests in the bucket (plus unlisted NDJSON objects named like the server's, read for their tag) and
  deleted there with its state keys; the purge record, its `.sha256` and the access line are copied to `<prefix>/data/`
  besides the local `audit/`. `FileSink` gains `list`, `get_file`, `delete` and `local_path` (defaults refuse, so
  existing sinks still compile), implemented by `LocalDirSink` and `S3FileSink`; `LocalDirSink::existing` opens a
  directory without creating it. Breaking: `plan_purge` and `execute_purge` take `files: Option<&dyn FileSink>`, and
  `PurgeOutcome::Done` gains `delivered` and `delivery_error`.
- `reso-ingest purge --server NAME` (#93, third part): deletes one server's data files (found by their `source` tag and
  the run manifests), its media directory and its state keys. A dry run unless `--execute`, which requires `--by`.
  Refuses while another process holds a lease on one of the server's feeds and holds them itself while deleting. Writes
  `audit/purge.<id>.json` with its SHA-256 beside it (`.sha256`) and in a `purged` line of the access log, which purge
  never deletes. With a bucket since #139 (above). New public items:
  `plan_purge`, `execute_purge`, `PurgePlan`, `PurgeRecord`, `PurgedFile`, `PurgeOutcome`, `sha256_hex`, `AUDIT_DIR`,
  `PurgeNote` and the `AccessEntry::purge` field. New dependency `sha2` in `reso-ingest`.
- `reso-ingest` access log (#93, second part): each resource step of `sync` and `serve` appends one line to
  `<out>/audit/access.ndjson` (run id, server, resource, credential id, replication window, records, deletes, bytes
  received and written, status and error), also for a failed pull. The credential id names the variable or file the
  secret is read from (`env:VAR`, `file:/path`), never the secret. The file is only appended to; a line that cannot be
  written fails the resource. With `--s3-bucket` each run's lines also go to the bucket as `access.<run>.ndjson`
  (`serve`: for runs that keep a manifest). New public items: `AccessLog`, `AccessEntry`, `credential_id`, `ACCESS_LOG`.
- `reso-ingest` reads secrets from files as well as environment variables (#92): `token_file`, `client_id_file` and
  `client_secret_file` beside the `*_env` keys, one form per secret. `ClientHandle` reads the secrets again before
  each resource and builds a new client when one changed, so `sync` and `serve` use a rotated secret file without a
  restart. After a 401 or a failed token request, a resource is retried once when the secrets changed. A file that
  cannot be read is `IngestError::SecretFile`, which names the file and never its contents. `reso-client` is unchanged.
- `reso-ingest` container image with the `s3` feature (#107, fourth part): the `Dockerfile` builds `parquet,s3`, so the
  image holds `aws-lc` (C, from `object_store`'s rustls) beside `ring`, both static on musl. The S3 client verifies
  certificates against the system CA bundle (rustls-platform-verifier), which `scratch` lacks, so every S3 request failed
  with `builder error`; the image now carries Alpine's `/etc/ssl/certs/ca-certificates.crt`. Checked from the image
  against AWS S3 and the GCS interoperability endpoint with dummy keys (TLS completes, the store answers 403). CI checks
  the bundle is in the image and that `sync` has `--s3-bucket`. About 30 MB, was 20 MB.
- `reso-ingest` run records in the bucket (#107, third part, feature `s3`): with `--s3-bucket`, run manifests and
  `sync --media` manifests are uploaded to `<prefix>/data/` (the local copy is removed), a copy of the newest run manifest
  is kept at `<prefix>/state/runs/last.json` and `status` reads it, and run ids are also reserved in the state store
  (`runs/<run>.reserved`, removed once the manifest is delivered) so workers with separate `--out` directories never share
  one; `serve` does the same per sync. `media` refuses `--s3-bucket` (its input files are in the bucket; use
  `sync --media`). The metrics file and downloaded media files stay local. `FileSink` gains a provided `location` method.
  Without a bucket nothing changes.
- `reso-ingest` S3 output sink and CLI selection (#107, second part, feature `s3`): `S3FileSink` uploads finished data
  files (one `PUT` with `If-None-Match: *` up to 32 MiB, multipart above, never replaces a name). `sync`, `serve`, `media`
  and `status` take `--s3-bucket`, `--s3-prefix`, `--s3-endpoint`, `--s3-region`, `--s3-allow-http`, `--s3-single-delete`
  (env `RESO_S3_*` for the first four); state goes to `<prefix>/state/`, data files to `<prefix>/data/`. `ServeOptions`
  gains `sink`. clap's `env` feature and tokio's `fs` and `io-util` features are enabled. Checked against a real
  S3-compatible server (RustFS; MinIO no longer ships binaries or images): conditional writes, a lock race, multipart,
  and a two-run `sync`.
- `reso-ingest` S3-compatible state store (#107, first part, feature `s3`): `S3StateStore` and `S3Options` keep state
  under a bucket prefix on AWS S3 or an S3-compatible store (R2, MinIO, RustFS), with `create` as `If-None-Match` and
  `replace` as `If-Match` on the ETag, so leases work across workers (GCS has its own backend, #130, above). Uses the
  `object_store` crate (approved); credentials from the standard `AWS_*` environment. Checked against a real AWS S3
  bucket (#115). The feature adds `aws-lc-rs` (C) and a second `reqwest` to the build.
- `reso-ingest` output sinks (#85, second part): `FileSink` (`put_file`, `exists`) and `LocalDirSink`, with
  `NdjsonWriter::with_sink` and `ParquetWriter::with_sink`. With a sink, the writer stages a file under its directory,
  hands it to the sink when it is full or at `flush`/`finish`, then removes the staged copy; `commit` returns `true`
  only when no file is open, so a checkpoint is saved only after the sink holds the rows (at-least-once). A name the sink
  already has is never overwritten. Without a sink nothing changes. No new dependencies.
- `reso-ingest` state backends (#85, first part): `StateStore`, an async key-value trait with conditional writes
  (`get` with a version, `put`, `create`, `replace`, `delete`, `list`; boxed futures, usable as `dyn StateStore`), and
  `FileStateStore`, today's files behind it (same names and contents). Checkpoints, key sets, event sequences, schema
  snapshots, leases and the media index all go through it. No new dependencies.
- Container image and deployment examples (#84): a `Dockerfile` building `reso-ingest` (with `parquet`) as a static
  musl binary on `scratch`, about 20 MB, non-root, read-only root filesystem friendly; `deploy/kubernetes/` (ConfigMap,
  Secret, PVC, CronJob for `sync`, Deployment and Service for `serve` with probes) and `deploy/systemd/` (a hardened
  unit for `serve`); a `Container` CI workflow that builds the image on tags and on PRs touching it, without pushing.
- `reso-ingest serve` (#83): daemon mode. Each resource with an `interval` (on the resource, or on the server for
  its `include` list) syncs on its own schedule with start-up and per-run jitter, a per-server `concurrency` cap, one
  shared client per server (so rate limits cover all its feeds) and doubling backoff for failing feeds (up to 15 minutes).
  Endpoints `/healthz`, `/readyz`, `/metrics` and `/status` over plain HTTP. Library: `Daemon`, `ServeOptions`,
  `parse_interval`, `cli::Format` is public; config keys `interval` (server, resource) and `concurrency` (server).
  New dependency in `reso-ingest`: `axum` (HTTP/1 only); tokio gains the `net` feature.
- `reso-ingest` shutdown (#82): on SIGTERM or Ctrl-C `sync` stops after the last page written, flushes and checkpoints,
  marks the manifest (and each cut resource) `interrupted`, and exits with code 3; a second signal exits at once with 130.
  Library: `Shutdown`, `cli::run_with_shutdown`, `cli::EXIT_INTERRUPTED`; `RunManifest` and `ResourceEntry` gain an
  `interrupted` field and `sync_resource_media` takes a `&Shutdown`. New feature of tokio (`signal`), no new crates.
- `reso-ingest` leases (#81): `sync` takes a lock file `state/<server>.<resource>.lock` per feed (process id, host, expiry,
  renewed while the run lives) and skips a feed another process holds, with a warning in the manifest; a crashed holder's
  lease expires after 5 minutes. Library: `Lease`, `LeaseOutcome`, `LeaseHolder`. No new dependencies.
- `reso-ingest` metrics (#79): `sync --metrics-file PATH` writes Prometheus text (freshness lag per feed, records,
  requests, responses by class, duration, success); the manifest gains `stats` per resource. Library:
  `render_prometheus`, `render_prometheus_at`, `write_metrics_file`, `parse_rfc3339`, `ResourceStats`; `ResourceEntry`
  gains a `stats` field. Example Grafana dashboard and alert rules in `reso-ingest/monitoring/`. No new dependencies.
- `reso-ingest` logs (#96): the binary installs a `tracing` subscriber writing to stderr, text by default or JSON lines
  with `--log-format json` (or `RESO_LOG_FORMAT=json`), level from `RUST_LOG` (default `info`). `run`, `server` and
  `resource` spans put `run_id`, `server` and `resource` on every event. Library: `cli::run_main` (installs the
  subscriber; `cli::run` still does not), `log_subscriber`, `LogFormat`, `env_filter`. New dependency in `reso-ingest`
  only: `tracing-subscriber` (`fmt`, `json`, `env-filter`).
- Server `filter` and per-resource settings `[server.resources.<Name>]` (`filter`, `expand`, `page_size`) (#65). The
  server filter goes on sync pulls and on the key pulls and lookups of `--deletes`. New `ResourceConfig`,
  `ServerConfig::filter_for`, `expand_for`, `page_size_for`; `reconcile_deletes` takes a `DeleteTarget`
  (resource, key field, filter) instead of the resource and key field (breaking for library callers); `ParquetWriter::with_expanded` keeps expanded navigation properties as JSON text.
- `reso-ingest media` (#30): downloads the files of the `Media` records that `sync` wrote (NDJSON), into
  `<out>/media/<server>/<MediaKey>.<ext>`, with `--limit` and `--media-dir`. Re-downloads only when the media timestamp
  changes (not when only a signed URL changes), sends saved validators, removes files on delete events, keeps failures
  pending for the next run, and writes `media.<run>.json`. Per-server `[server.media]` settings (`MediaConfig`);
  library API `sync_media`, `MediaIndex`, `MediaOptions`, `MediaReport`, `MediaManifest`.
- `sync --media` (#67): downloads the files of each page's media records right after the page is written,
  so signed single-use URLs are fresh. Takes top-level `Media` records or the `Media` arrays nested in other records
  (`[server.media] nested_field`); library API `PageMedia`, `sync_resource_media`, `MediaIndex::add_nested`.
- Deletes signalled by a flag field (#66): `deletes = { flag_field = "MlgCanView" }` on a server
  (`DeletesConfig`); `FlagDeleteWriter` writes a delete event for each record whose flag is `false`, after the record
  itself, which stays raw. `sync` does this with or without `--deletes` and skips the key comparison for such servers.
- Run ids never collide (#69): `reserve_run_id(dir)` reserves an id with a `.run.<id>` marker file in `--out`, adding
  `-02`, `-03`, … when a run started in the same second already holds it; `sync` and `media` use it. `NdjsonWriter`
  and `ParquetWriter` create their data files with `create_new`, so an existing file is never overwritten (the write
  fails instead).

#### Changed

- fewer state requests per `sync`, which matters with a bucket, where each one is a round trip (#162,
  lead 6). A quiet sync of one resource makes 19 bucket requests instead of 21 (20 instead of 23 in a run that sweeps stale
  run ids). The checkpoint is read once, not
  twice. The schema snapshot is written only when it changed. A lease is released with one conditional delete (the
  version from its create or last renewal) instead of a read and a delete. The lease lookup for a running backfill
  and the stale run-id sweep list one level of `state/` (`StateStore::list_dir`) instead of every key, and the sweep
  no longer reads the reservation it just made. The start-up check of conditional writes (6 requests) stays.
- `StateStore` has three new provided methods: `create_versioned` and `replace_versioned` (they also
  report the version written, as `Written`) and `list_dir` (keys one level down). `FileStateStore` and
  `S3StateStore` override them. Other backends keep working unchanged and can override them to save the same
  requests.
- NDJSON `record` values keep the server's member order and number text (`1.50` stays `1.50`, an
  integer past 64 bits is no longer turned into a float) instead of being sorted and normalised by a parse and
  serialize round trip (#162). Line breaks between tokens are dropped. A consumer that compared lines byte for byte
  with earlier output sees the difference; the parsed records are the same apart from those numbers.
- a Parquet `sync` hands each record's JSON text to the Arrow decoder as sent
  (`ParquetWriter::write_raw_batch`) instead of parsing it into a `Value` and serializing it again (#162, lead 5).
  A batch with a record that does not fit the column types as sent (an object or array in a text column, a value of
  the wrong type) takes the old path, so the rows and errors are the same, with one difference: a number in a text
  column keeps the server's text (`1.50`, not `1.5`). In the 100k-record load test: see `docs/PERFORMANCE.md`.
- the repeated-key check (`CheckingWriter`) hashes string keys without copying them into a `String`
  per record (#166).
- `rust-version` is now 1.89, what its dependencies need (`crc-fast` under `object_store`; arrow and
  parquet need 1.88); it said 1.87, which no longer built. `reso-client` stays at 1.87 (#95).
- the Parquet `fetched_at` column is now a UTC timestamp in milliseconds
  (`Timestamp(Millisecond, "+00:00")`); it was seconds with no time zone, which DuckDB read as an integer (#86).
- `FileSink::list` (`LocalDirSink`, `S3FileSink`) also returns the files in the `source=…` partition
  tree, as `/`-joined names; `LocalDirSink::put_file` creates parent folders and `delete` removes partition folders it
  leaves empty (#86).
- media index saves cost what changed, not the size of the index (#149). The index is now a snapshot
  (`media/<server>.json`, as before) plus deltas (`media/<server>/<seq>.json`) holding the entries changed or removed
  since the save before; a save writes one delta, or a new snapshot once the deltas hold more entries than the index
  (at least 10 000), and then deletes the deltas. A save with no change writes nothing. At 1M entries a page's save
  takes 1.8 ms and 0.6 MiB, where the whole rewrite took 0.7 s and 384 MiB (`loadtest --scenario media-index`, rows in
  `LOADTEST.md`). An index written by earlier versions loads unchanged. `purge` deletes the deltas too. New
  `MediaIndex::save_changes`, `delta_prefix`, `is_index_key`, `mark_read` and `entry_mut`; `MediaIndex::save` still
  writes a whole snapshot. Breaking: `MediaIndex` equality compares `read_files` and `entries` only. The index is still
  held whole in memory during a run.
- an S3 endpoint at `storage.googleapis.com` is refused, since GCS's interoperability API ignores the
  S3 precondition headers (#115); use `--gcs-bucket`. Breaking for code that builds `S3Options` with a struct
  literal: add `provider` (or use `..S3Options::default()`).
- on S3, a versioned `S3StateStore::delete` (lease release, stale reservations) is one `DELETE` with
  `If-Match` on the ETag instead of a `HEAD` then an unconditional `DELETE`, so an object replaced in between is no
  longer deleted (#133). `object_store` has no conditional delete, so it is a presigned request from its signer with
  `If-Match` in the signature; no new dependency. The start-up probe (#131) now also checks that a delete with a stale
  version leaves the object in place.
- `reso-ingest --deletes` key comparison at millions of keys (#87): key sets are saved as sorted lines
  (`<server>.<resource>.keys.ndjson`: a header line, then one key per line) instead of one JSON document, pulled keys
  are sorted in chunks spilled to temporary files and the two sets are diffed by merging, so memory stays flat:
  24 MiB peak for 1M and for 10M keys, against 363 MiB and 3.3 GiB before. An old `.keys.json` key set is read once
  and converted. Delete events now come out sorted by key. `DeleteTarget` gains `scratch_dir` and `chunk_keys`
  (`with_scratch_dir`, `with_chunk_keys`); the command uses `<out>/.scratch`. `StateStore` gains provided methods
  `get_to_file` and `put_from_file` (overridden to stream by `FileStateStore` and `S3StateStore`, multipart above
  32 MiB). `CheckpointStore::load_keys` returns keys sorted and without repeats. The load harness has a `deletes`
  scenario, and its fake server sets `TCP_NODELAY` (small pages had waited ~40 ms each).
- `reso-ingest` with a bucket resumes a killed run from its last delivered file (#107, fifth part): the NDJSON writer
  with a sink used to rotate in the middle of a page and open the next file at once, so a file was nearly always open,
  the checkpoint was held until the end of the resource, and a killed initial load started again from the beginning.
  With a sink it now closes a file at the first page end past `--max-file-mb` (as Parquet does; a file can pass the limit
  by up to one page), and the checkpoint moves at each delivery. Delete events still rotate at the limit. Tests: a sync
  dropped while waiting for a page, resumed by a fresh worker with an empty `--out` (no record lost or repeated), and two
  `serve` daemons on one bucket (a feed never runs in both at once, each record written once).
- S3 errors name their causes: a missing CA bundle read `Generic HTTP client error: builder error`, now followed by
  `unexpected error: No CA certificates were loaded from the system`.
- Breaking (library): state is async and backend-neutral (#85). `CheckpointStore` methods are
  `async` and it gains `with_store`/`store`; `Lease::acquire` is `async` and takes `&Arc<dyn StateStore>`, and
  `Lease::release` (async) is the clean way to end a lease (dropping one still releases it in the background);
  `MediaIndex::path` is replaced by `MediaIndex::key`, `load`/`save` are `async` and take a `&dyn StateStore`;
  `MediaOptions.state` and `ServeOptions.state` are `Arc<dyn StateStore>`; `PageMedia::new` is `async`. `/readyz`
  answers `503 not ready: state cannot be saved` when the probe write fails.

#### Fixed

- `reso-ingest` with a bucket: two workers could get the same run id (#116). `reserve` checked the bucket for the id's
  manifest before taking the shared reservation, so another worker could deliver that manifest and release the
  reservation in between; the second run's manifest was then refused (`object already exists`) and stayed in `--out`.
  The reservation is now taken first and the bucket checked after. Seen about once in 20 loaded runs of the two-`serve`
  test, which also counted a worker's own back-to-back requests as overlap when a timer thread woke late.
- `run_servers` aborts its server tasks when its future is dropped before it completes (#116); they were detached
  and kept running (in-process callers of `cli::run` only; the binary always awaits the run).

### Repository and documentation

- Product readiness (#95): `docs/COMPATIBILITY.md` (which servers are checked), `docs/STABILITY.md` (1.0 plan, MSRV and
  deprecation policy, release checklist), `docs/OPERATIONS.md` (deployment guide, a runbook per shipped alert, support
  statement), `SECURITY.md`. The alert rules carry `runbook_url` annotations. CI runs `cargo deny` (`deny.toml`:
  permissive licences, crates.io only, advisories), `cargo semver-checks` on `reso-client` and an MSRV check of both
  crates.
- Scope (#93): source purge and an access audit trail are in scope for `reso-ingest`. They help the consumer comply
  with licence terms (deletion when a licence ends, audit questions about what was pulled); display rules stay out of
  scope and records stay raw. `AGENTS.md` and `README.md` say so.
- README, crate docs and AGENTS.md rewritten for the current state; new `reso-ingest/README.md` for the ingest
  command.
- README: Data Dictionary 2.1 notes (renamed unit fields, lookup values as strings, `any()` not `has`) (#17).
- README, AGENTS.md and crate docs describe file downloads (`Downloader`) and `reso-ingest media`; the
  `reso-ingest` README's configuration example no longer excludes `Media`, which `media` needs synced.
- README, `reso-ingest` README, AGENTS.md, crate docs and `servers.example.toml` brought up to date with #65-#69:
  vendor differences now supported, `sync --media`, flag deletes, run ids; "What's next" rewritten.

## [0.3.0] - 2026-11-24

The low-level client now follows the RESO standard (paging, replication, OAuth2, retries, `$metadata` model). The
repository also gains `reso-ingest`, a second crate with a multi-server sync command; it is **not** published with this
release and its entries are listed separately below.

### Added
- `$metadata` model (#23): `Metadata::parse(xml)` and `ResoClient::fetch_metadata_model()` return entity sets, entity
  types (key, properties with type, nullability, `MaxLength` and lookup name, navigation properties) and enum types
  with members. Unknown elements (annotations, vendor extensions, actions, complex types) are skipped.
  `resource_names()` falls back to entity type names for servers with no entity container (the RESO reference
  server). New dependency: `roxmltree` (no dependencies of its own). `fetch_metadata` still returns the raw string.
- Retry, backoff and client-side rate limiting (#22): `RetryPolicy` (network errors and 429/502/503/504 are retried
  with exponential backoff and jitter; `Retry-After` in seconds or as an HTTP date replaces the wait, and one longer
  than `max_retry_after` ends the retrying; other 4xx and 500 are never retried), `RateLimit` (requests per second,
  per hour and response bytes per day; requests wait instead of failing), `ClientConfig::with_retry`,
  `with_rate_limit` and the fields `retry` and `rate_limit`, `ResoClient::stats` (`ClientStats`: requests, retries,
  bytes), and `RESO_MAX_RETRIES`, `RESO_RATE_PER_SECOND`, `RESO_RATE_PER_HOUR`, `RESO_BYTES_PER_DAY` in `from_env`.
  Defaults retry nothing and limit nothing, so behaviour is unchanged until configured. Adding public fields to
  `ClientConfig` breaks code that builds it with a struct literal. The `tokio` dependency gains the `time` feature
  (and `test-util` as a dev feature); no new crates.
- RESO-standard replication (#20): `StandardReplication` (resource, key field, optional timestamp field, page size,
  filter, select, starting `Checkpoint`), `ResoClient::replicate` and `ReplicationRun::next_batch`. It filters on
  `ModificationTimestamp`, orders by timestamp then key, follows `@odata.nextLink`, and resumes strictly after the last
  (timestamp, key) so records that share a timestamp are neither skipped nor repeated. With no next link it re-queries
  from its checkpoint until an empty page (one extra request), so a server that caps `$top` still completes. A record
  older than the one before it, or the checkpoint repeated, is the new error `ResoError::OutOfOrder`. Bridge's
  `/replication` endpoint is unchanged and documented as a vendor mode.
- OAuth2 client credentials (#21): `Auth` (`Bearer` or `ClientCredentials`), `ClientCredentials` (token URL, id,
  secret, optional scope, `CredentialTransport` body/Basic header/query) and `ClientConfig::with_auth`. The token is
  fetched on first use, cached, renewed after 90% of `expires_in`, and renewed once after a 401 before the request is
  retried; concurrent requests wait for one fetch. `from_env` reads `RESO_CLIENT_ID`, `RESO_CLIENT_SECRET`,
  `RESO_TOKEN_URI`, `RESO_SCOPE` and `RESO_CREDENTIAL_TRANSPORT`. New error `ResoError::TokenRequest`.
- Standard server-driven paging (#19): `Page` (records, absolute `next_link`, `count`, request `url`, raw `body`),
  `ResoClient::execute_page`, `ResoClient::next_page` and `ResoClient::fetch_page` (resume from a saved link), and
  `paging::parse_link_header` for RFC 8288 `Link: <url>; rel="next"` headers. Example `test_standard_paging`.
- Test suite: unit, wiremock integration and doc tests for every module (from about 44% line coverage in
  0.2.0); `serial_test` keeps environment-variable tests apart.

### Changed
- `dataset_id` / `with_dataset_id` / `RESO_DATASET_ID` are documented as Bridge Interactive-specific; the crate docs
  describe `$metadata` retrieval as raw XML plus a parsed model (#31).
- **Breaking:** `ClientConfig::token: String` is replaced by `ClientConfig::auth: Auth`. `ClientConfig::new(url, token)`
  and `RESO_TOKEN` work as before.
- **Breaking (behaviour):** `execute_next_link` and `execute_replication` now read the next link from the body's
  `@odata.nextLink` first, then the `next` header, then a parsed `Link` header with `rel="next"`. Before, only headers
  were read, so a standard result set stopped after the second page, and a `Link` header was returned as its raw
  `<url>; rel="next"` text. Relative links are resolved to absolute URLs (against `@odata.context`, else the request URL).
- **Breaking (behaviour):** `execute_next_link` refuses a link on another scheme, host or port (`ResoError::Parse`),
  so the bearer token is never sent to a server other than the configured one.
- **Breaking:** `ResoError` gains the variants `TokenRequest` and `OutOfOrder` and is now `#[non_exhaustive]`: a
  `match` on it needs a wildcard arm, and later releases can add variants without a breaking change.

### Fixed
- `$select` and `$expand` items are now percent-encoded (spaces, slashes, non-ASCII, nested options) in key-access,
  normal and replication queries; the commas between items stay literal (#31).

### reso-ingest (unpublished, in this repository)
- Ingest crate skeleton (#24): the repository is now a cargo workspace; `reso-ingest` (unpublished, not part of the
  `reso-client` package) parses a multi-server TOML file (`Config`, `servers.example.toml`), resolves secrets from named
  environment variables, rejects literal secrets, and runs one task per server independently with a concurrency cap
  (`run_servers`). New dependency in `reso-ingest` only: `toml`.
- NDJSON output (#25): `NdjsonWriter` writes one line per record with `source`, `resource`, `fetched_at` and `run_id`
  tags around the raw record, rotating files by size; `RunManifest` and `pull_resource` record each run.
- Incremental sync (#26): `sync_resource` and `CheckpointStore` (one JSON file per server and resource, written
  atomically after each flushed batch; at-least-once).
- Deletes and reconciliation (#27), in `reso-ingest`: `reconcile_deletes` writes delete events
  (`"event":"delete"` and `key`, same source/resource/run tags as records) for records that left a feed. It uses the
  server's `EntityEvent` feed when `$metadata` declares it (`EventFeed::detect`; field names configurable), looking up each
  named record, and otherwise compares a `$select=<key>` pull with the key set saved by the previous run. The first run
  only records a baseline; a failed or partial key pull emits nothing; an empty key pull over a non-empty set is
  refused. `RecordWriter::write_deletes` (new required method), `CheckpointStore` key-set and event-sequence files,
  `ResourceEntry::deletes`.
- The `reso-ingest` command (#28): `sync` (`--server`, `--resource`, `--deletes`, `--start-over`, `--max-file-mb`,
  `--format`), `discover` and `status`. New dependency in `reso-ingest` only: `clap`.
- Parquet output (#29), in `reso-ingest` behind the optional `parquet` feature: `ParquetWriter` (a `RecordWriter`) writes
  the NDJSON rows as typed Parquet, with the schema taken from `$metadata` (`schema_for`, `arrow_type`; unknown types fall
  back to text), and `reso-ingest sync --format parquet`. New optional dependencies in `reso-ingest` only: `parquet`,
  `arrow-array`, `arrow-json`, `arrow-schema`.

## [0.2.1] - 2025-11-07

### Added
- Comprehensive documentation with examples to all public APIs
- Implemented code coverage tracking Codecov.io

### Documentation
- Updated README.md with badges to prep for crates.io release


## [0.2.0] - 2025-11-05

### Added
- Comprehensive example suite demonstrating all RESO client functionality:
  - `test_property.rs` - Property resource queries with filtering and field selection
  - `test_connectivity.rs` - Basic API connectivity and authentication testing
  - `test_filters.rs` - OData filter syntax examples (comparison, logical, string functions)
  - `test_select.rs` - Field selection and projection examples
  - `test_replication.rs` - Replication endpoint usage with pagination
  - `test_core_queries.rs` - Core RESO queries demonstrating various patterns
  - `test_apply.rs` - OData aggregation with `$apply` parameter (requires server support)
  - `test_count_only.rs` - Efficient count-only queries using `/$count` endpoint
  - `test_expand.rs` - Navigation property expansion examples (requires server support)
  - `test_member.rs` - Member resource queries for agent/broker data
  - `test_metadata.rs` - Metadata retrieval and parsing examples
  - `test_pagination_nextlink.rs` - Server-side pagination using `@odata.nextLink`
  - Enhanced query builder functionality with additional methods and parameters
  - Improved error handling for API responses and network failures
  - CoreQueries.md documentation file

### Changed
- **Refactored client.rs for DRY principle** - Extracted common HTTP request handling logic into reusable helper methods (`send_authenticated_request`, `parse_json_response`, `parse_text_response`), reducing code duplication by ~67 lines across 6 public methods
- **Improved code documentation** - Added inline comments throughout `src/` explaining:
  - Complex URL building logic with dataset_id path insertion
  - Error handling flow and OData error parsing
  - Header extraction logic for replication next links
  - Business rule explanations (e.g., why replication max is 2000 records)
  - OData v4.0 specification compliance details
  - **Reorganized test structure** - Moved 438 lines of query builder tests from `src/queries.rs` to `tests/queries_tests.rs` for better separation of concerns and more idiomatic Rust project structure
  - Replaced `property_test.rs` with more comprehensive `test_property.rs` example
  - Formatting improvements across example files
  - Updated query examples to demonstrate real-world usage patterns

### Documentation
- Updated README.md with comprehensive test running instructions
- Added documentation for both unit tests and integration tests
- No breaking changes to public API
- All examples include detailed comments and usage instructions
- Examples demonstrate both successful queries and error handling patterns
- Clear indication of server-specific requirements (e.g., `$apply`, `$expand`)

## [0.1.0] - 2025-10-19

### Added
- Initial release of reso-client
- OData 4.0 query builder with fluent API
- Support for `$filter`, `$select`, `$orderby`, `$top`, `$skip`, and `$count` query parameters
- OAuth bearer token authentication
- Environment variable configuration support
- Metadata fetching via `$metadata` endpoint
- Optional dataset ID path support for RESO providers that require it
- Comprehensive error handling with specific error types
- Async/await support using Tokio runtime
- URL encoding for filter expressions and order-by clauses
- Complete unit test coverage for query building

### Security
- Token redaction in debug output to prevent accidental exposure in logs

[Unreleased]: https://github.com/jeremeybingham/reso-rs/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/jeremeybingham/reso-rs/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/jeremeybingham/reso-rs/releases/tag/v0.2.1
[0.2.0]: https://github.com/jeremeybingham/reso-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jeremeybingham/reso-rs/releases/tag/v0.1.0
