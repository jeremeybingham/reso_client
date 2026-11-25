# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `reso-ingest purge --server NAME` (#93, third part): deletes one server's data files (found by their `source` tag and
  the run manifests), its media directory and its state keys. A dry run unless `--execute`, which requires `--by`.
  Refuses while another process holds a lease on one of the server's feeds and holds them itself while deleting. Writes
  `audit/purge.<id>.json` with its SHA-256 beside it (`.sha256`) and in a `purged` line of the access log, which purge
  never deletes. Local storage only (a bucket needs delete and list on `FileSink`, #107). New public items:
  `plan_purge`, `execute_purge`, `PurgePlan`, `PurgeRecord`, `PurgedFile`, `PurgeOutcome`, `sha256_hex`, `AUDIT_DIR`,
  `PurgeNote` and the `AccessEntry::purge` field. New dependency `sha2` in `reso-ingest`.
- `reso-ingest` access log (#93, second part): each resource step of `sync` and `serve` appends one line to
  `<out>/audit/access.ndjson` (run id, server, resource, credential id, replication window, records, deletes, bytes
  received and written, status and error), also for a failed pull. The credential id names the variable or file the
  secret is read from (`env:VAR`, `file:/path`), never the secret. The file is only appended to; a line that cannot be
  written fails the resource. With `--s3-bucket` each run's lines also go to the bucket as `access.<run>.ndjson`
  (`serve`: for runs that keep a manifest). New public items: `AccessLog`, `AccessEntry`, `credential_id`, `ACCESS_LOG`.
- Scope (#93): source purge and an access audit trail are in scope for `reso-ingest`. They help the consumer comply
  with licence terms (deletion when a licence ends, audit questions about what was pulled); display rules stay out of
  scope and records stay raw. `AGENTS.md` and `README.md` say so; the access log and `purge` follow in their own PRs.
- `reso-ingest` reads secrets from files as well as environment variables (#92): `token_file`, `client_id_file` and
  `client_secret_file` beside the `*_env` keys, one form per secret. `ClientHandle` reads the secrets again before
  each resource and builds a new client when one changed, so `sync` and `serve` use a rotated secret file without a
  restart. After a 401 or a failed token request, a resource is retried once when the secrets changed. A file that
  cannot be read is `IngestError::SecretFile`, which names the file and never its contents. `reso-client` is unchanged.

### Changed
- `reso-ingest` with a bucket resumes a killed run from its last delivered file (#107, fifth part): the NDJSON writer
  with a sink used to rotate in the middle of a page and open the next file at once, so a file was nearly always open,
  the checkpoint was held until the end of the resource, and a killed initial load started again from the beginning.
  With a sink it now closes a file at the first page end past `--max-file-mb` (as Parquet does; a file can pass the limit
  by up to one page), and the checkpoint moves at each delivery. Delete events still rotate at the limit. Tests: a sync
  dropped while waiting for a page, resumed by a fresh worker with an empty `--out` (no record lost or repeated), and two
  `serve` daemons on one bucket (a feed never runs in both at once, each record written once).
- S3 errors name their causes: a missing CA bundle read `Generic HTTP client error: builder error`, now followed by
  `unexpected error: No CA certificates were loaded from the system`.

### Added
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
  and a two-run `sync`. Manifests, metrics file and media files are still local.
- `reso-ingest` S3-compatible state store (#107, first part, feature `s3`): `S3StateStore` and `S3Options` keep state
  under a bucket prefix on AWS S3, R2, MinIO or GCS (interoperability API), with `create` as `If-None-Match` and
  `replace` as `If-Match` on the ETag, so leases work across workers. Uses the `object_store` crate (approved);
  credentials from the standard `AWS_*` environment. The output sink, CLI selection and routing of manifests,
  media index and metrics through the backends follow in later parts of #107. Unconfirmed against a real bucket
  (tests use an in-process S3 fake). The feature adds `aws-lc-rs` (C) and a second `reqwest` to the build.
- `reso-ingest` output sinks (#85, second part): `FileSink` (`put_file`, `exists`) and `LocalDirSink`, with
  `NdjsonWriter::with_sink` and `ParquetWriter::with_sink`. With a sink, the writer stages a file under its directory,
  hands it to the sink when it is full or at `flush`/`finish`, then removes the staged copy; `commit` returns `true`
  only when no file is open, so a checkpoint is saved only after the sink holds the rows (at-least-once). A name the sink
  already has is never overwritten. Without a sink nothing changes. No new dependencies; the CLI does not select a sink yet. The S3-compatible backend is #107.
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
- `ClientStats` counts responses by status (`ok`, `throttled` for 429, `client_errors`, `server_errors`) for both the
  API client and `Downloader`, and has `ClientStats::since` for the difference between two snapshots (#79). The new
  public fields break code that builds `ClientStats` with a struct literal.
- `reso-ingest` metrics (#79): `sync --metrics-file PATH` writes Prometheus text (freshness lag per feed, records,
  requests, responses by class, duration, success); the manifest gains `stats` per resource. Library:
  `render_prometheus`, `render_prometheus_at`, `write_metrics_file`, `parse_rfc3339`, `ResourceStats`; `ResourceEntry`
  gains a `stats` field. Example Grafana dashboard and alert rules in `reso-ingest/monitoring/`. No new dependencies.
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

- `reso-ingest` logs (#96): the binary installs a `tracing` subscriber writing to stderr, text by default or JSON lines
  with `--log-format json` (or `RESO_LOG_FORMAT=json`), level from `RUST_LOG` (default `info`). `run`, `server` and
  `resource` spans put `run_id`, `server` and `resource` on every event. Library: `cli::run_main` (installs the
  subscriber; `cli::run` still does not), `log_subscriber`, `LogFormat`, `env_filter`. New dependency in `reso-ingest`
  only: `tracing-subscriber` (`fmt`, `json`, `env-filter`).

### Changed
- **Breaking (`reso-ingest`, unpublished):** state is async and backend-neutral (#85). `CheckpointStore` methods are
  `async` and it gains `with_store`/`store`; `Lease::acquire` is `async` and takes `&Arc<dyn StateStore>`, and
  `Lease::release` (async) is the clean way to end a lease (dropping one still releases it in the background);
  `MediaIndex::path` is replaced by `MediaIndex::key`, `load`/`save` are `async` and take a `&dyn StateStore`;
  `MediaOptions.state` and `ServeOptions.state` are `Arc<dyn StateStore>`; `PageMedia::new` is `async`. `/readyz`
  answers `503 not ready: state cannot be saved` when the probe write fails.
- `ResoClient::execute_count` (and `QueryBuilder::count()`) now send `$count=true&$top=0` on the resource path and read
  `@odata.count` from the JSON body, instead of requesting `/Resource/$count`. Works on servers without a `/$count`
  route, such as the RESO reference server. `Query::to_odata_string()` for count-only queries changes accordingly (#53).

### Fixed
- `reso-ingest` with a bucket: two workers could get the same run id (#116). `reserve` checked the bucket for the id's
  manifest before taking the shared reservation, so another worker could deliver that manifest and release the
  reservation in between; the second run's manifest was then refused (`object already exists`) and stayed in `--out`.
  The reservation is now taken first and the bucket checked after. Seen about once in 20 loaded runs of the two-`serve`
  test, which also counted a worker's own back-to-back requests as overlap when a timer thread woke late.
- Download network errors no longer include the URL (#96): signed media URLs reached the debug log and the media
  manifest through reqwest's error text.
- `Metadata::parse` reads annotation values written as a `<String>` child element, resolves schema aliases in
  navigation property, property and entity set types, and keeps `RESO.OData.Metadata.StandardName` on enum members
  (new `EnumMember::standard_name`; code that builds `EnumMember` by struct literal must add the field) (#58).
- `run_servers` aborts its server tasks when its future is dropped before it completes (#116); they were detached
  and kept running (in-process callers of `cli::run` only; the binary always awaits the run).

### Documentation
- README, crate docs and AGENTS.md rewritten for the current state; new `reso-ingest/README.md` for the ingest
  command.
- README: Data Dictionary 2.1 notes (renamed unit fields, lookup values as strings, `any()` not `has`) (#17).
- README, AGENTS.md and crate docs describe file downloads (`Downloader`) and `reso-ingest media`; the
  `reso-ingest` README's configuration example no longer excludes `Media`, which `media` needs synced.
- README, `reso-ingest` README, AGENTS.md, crate docs and `servers.example.toml` brought up to date with #65-#69:
  vendor differences now supported, `sync --media`, flag deletes, run ids; "What's next" rewritten.

### reso-ingest (unpublished, in this repository)
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

[Unreleased]: https://github.com/jeremeybingham/reso_client/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/jeremeybingham/reso_client/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/jeremeybingham/reso_client/releases/tag/v0.2.1
[0.2.0]: https://github.com/jeremeybingham/reso_client/jeremeybingham/reso_client/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jeremeybingham/reso_client/releases/tag/v0.1.0
