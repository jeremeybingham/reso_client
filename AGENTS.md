# AGENTS.md

Guidance for AI coding agents and people working in this repository (tool-neutral).

This repository is an **ingest layer** for RESO Web API servers (OData 4.0), in two Rust crates:

- `reso-client` (crates.io, pre-1.0, v0.3.0): a client for one server: queries, standard paging and replication,
  bearer token or OAuth2 client credentials, retries and rate limits, a parsed `$metadata` model, file downloads
  (`Downloader`) with their own limits.
- `reso-ingest` (unpublished, `reso-ingest/`): pulls every resource from many servers into tagged NDJSON or Parquet
  files, incrementally and resumably, with delete detection, and downloads the files of `Media` records; library plus
  the `reso-ingest` command.

Records are handed over **raw**, tagged with their source. The typical user is an aggregator pulling from many MLS
vendors into its own store. The crates do **not** de-duplicate across MLSs, map local fields, enforce data-licence or
display rules, or choose a database. They do help the consumer comply with licence terms: `reso-ingest` keeps an
append-only access log of what was pulled, when and with which credential, and can purge one source's files, media and
state when its licence ends (#93); records stay raw. The ingest-layer plan was tracker issue #18 (milestone "Ingest
layer", closed); production-scale work is tracker #78 (milestone "Production scale", work order in #116). Open work is
listed under "What's next" in `README.md`.

## Finding things

| You want | Go to |
|---|---|
| What the crates do, limits, quick start, configuration, what's next | `README.md` |
| The ingest command: configuration, output format, deletes, media files, Parquet | `reso-ingest/README.md` |
| The plan and open owner decisions | Production scale: issue #78 and its children, work order #116; ingest layer (done): #18 |
| What to check on a real server, and what may change | GitHub issue #76 (access: #32) |
| API reference (every method, type and example) | rustdoc: `cargo doc --open`, or docs.rs/reso-client |
| Runnable examples (written against Bridge's `actris_ref`) | `examples/` |
| Version history | `CHANGELOG.md` |
| How the project was built with Claude (background only) | `dev_summary.md` |
| Current state and next step (local, git-ignored) | `SESSION.md` |

## Build and test

```sh
cargo fmt && cargo clippy -- -D warnings   # required before every commit
cargo test                                 # unit, integration (wiremock) and doc tests; no network or credentials needed
cargo doc --no-deps                        # public items must have doc comments
cargo run --example test_connectivity      # examples need a .env (see example.env); none exists on this machine
```

## Rules that must hold

- **Resource-agnostic.** Resource and field names are strings and records are raw `serde_json::Value`; do not add typed
  Property/Member structs. This keeps the crate working with any server schema, including Data Dictionary 2.1 (see the
  README section of that name).
- **Raw in, raw out.** The crate never transforms, maps or drops record fields; it may only add source tags (server,
  resource, fetch time, run id) around a record.
- **Standard first, vendor second.** RESO-standard behaviour (`ModificationTimestamp` replication, `@odata.nextLink` in
  the body) is the default; vendor behaviour (Bridge `/replication`, `dataset_id` path segment) is a clearly named mode.
- **Tests are offline.** Mock servers (wiremock) only; CI never touches a network. No real server credentials are
  available, so behaviour against real servers is unconfirmed until #32 is resolved: say so in PRs.
- **Secrets.** Bearer tokens and client secrets are never logged, printed or committed (`.env` is git-ignored); debug
  output redacts them.
- **Library code** returns `Result<T, ResoError>` (all errors in `src/error.rs`), never `unwrap()`/`expect()`, logs
  through `tracing` rather than `println!`, and avoids glob imports. Examples may unwrap.
- **Public API.** Every public item has a `///` doc comment, with an example where useful. Breaking changes are allowed
  before 1.0 but are recorded in `CHANGELOG.md` with a version bump.
- **No new dependencies without discussion.** Keep `reso-client` lean (its XML parser is `roxmltree`; OAuth2 and rate
  limiting are hand-written; its only test-only TLS crate is `rustls`, already in the tree via reqwest); heavier ones belong in `reso-ingest` (`clap`, `toml`, `tracing-subscriber`, `reqwest` for the webhook, `axum` for `serve`, `sha2` for purge records, `object_store` (`aws` and `gcp`) behind the `s3` feature, and
  `arrow`/`parquet` behind the `parquet` feature).
- Every bug fixed gets a regression test. Docs change in the same commit as the behaviour they describe.

## Where things are

```
src/lib.rs          public API, re-exports, crate docs
src/client.rs       ResoClient, ClientConfig, HTTP operations
src/download.rs     Downloader, DownloadConfig, SendToken, DownloadTarget: file downloads with their own limits (Media files), to a path or streamed to a target
src/auth.rs         Auth, ClientCredentials, token fetch, cache and renewal (OAuth2 client credentials)
src/queries.rs      QueryBuilder, Query, ReplicationQueryBuilder
src/metadata.rs     Metadata model parsed from $metadata (roxmltree): entity sets, types, keys, properties, enums
src/paging.rs       Page, next-link reading (body, `next` and `Link` headers), relative resolution
src/replication.rs  ReplicationResponse, ReplicationQuery (Bridge `/replication`, vendor mode)
src/standard_replication.rs  StandardReplication, ReplicationRun, Checkpoint (RESO-standard, keyset on timestamp+key)
src/network.rs      NetworkOptions: proxy, extra CA certificates, User-Agent, connection pool (#91); used by the client and Downloader
src/retry.rs        RetryPolicy (backoff, Retry-After), RateLimit and its limiter (RateUsage), ClientStats counters, count_requests scopes
src/error.rs        ResoError and the Result alias
reso-ingest/        second workspace crate (unpublished); README.md documents the command
  src/config.rs     Config: multi-server TOML, secrets read from the variables (`*_env`) or files (`*_file`) named; server filter,
                    [server.resources.<Name>] (ResourceConfig), flag-field deletes (DeletesConfig), limits, media, network (NetworkConfig)
  src/secrets.rs    ClientHandle: a server's client, rebuilt when its secrets change; retry once after a 401 (#92)
  src/runner.rs     run_servers: one independent task per server, concurrency cap
  src/output.rs     RecordWriter, NdjsonWriter: tagged lines, size rotation, never overwrite; reserve_run_id (.run.<id>)
  src/parquet_out.rs  ParquetWriter typed from $metadata (`parquet` feature); `partitioned` = Hive folders source=/resource=/date= (#86)
  src/manifest.rs   RunManifest, pull_resource, sync_resource, sync_resource_media
  src/state.rs      StateStore: async key-value trait with conditional writes (state backends, #85); FileStateStore
  src/sink.rs       FileSink (put_file, exists; list, get_file, delete for purge, #139), LocalDirSink: where finished data files go; list includes the source=… partition tree; writers take one with `with_sink` (#85, S3 in #107)
  src/s3.rs         S3Options, Provider, S3StateStore, S3FileSink, S3MediaStore: state, data and media files in an S3-compatible or GCS bucket via `object_store` (feature `s3`; `--s3-*` and `--gcs-bucket` options, #107, #130)
  src/audit.rs      AccessLog, AccessEntry: append-only `audit/access.ndjson`, one line per resource pull (#93); per-run copy to the sink
  src/purge.rs      plan_purge, execute_purge, PurgeRecord: `purge` of one server's files, media and state; record + SHA-256 in audit/ (#93); in a bucket too (#139)
  src/runs.rs       run id reservation in the state store (stale ones swept after 7 days), manifest delivery to the sink, `runs/last.json` for `status` (with a sink only)
  src/checkpoint.rs CheckpointStore: checkpoints, key sets (sorted lines, `.keys.ndjson`), event sequences, schema snapshots in a StateStore
  src/keyset.rs     ExternalSorter, diff: key sets at millions of keys, sorted in spilled chunks and diffed by merge (#87); stale scratch sweep (#134)
  src/serve.rs      Daemon, ServeOptions: `reso-ingest serve`, one task per feed (interval, jitter, backoff), axum endpoints
                    (/healthz, /readyz, /metrics, /status); each sync is `cli::sync_server_with` for one resource;
                    Daemon::reload applies a new Config (ReloadSummary: feeds added, removed, restarted, paused, resumed)
  src/shutdown.rs   Shutdown: a flag `sync` checks between pages; the binary sets it on SIGTERM/Ctrl-C (cli::watch_signals), exit code 3
  src/backfill.rs   BackfillWindow, BackfillEntry, ProgressWriter: `sync --backfill` (#88): progress lines, rate share, windows
  src/lease.rs      Lease: lock key per (server, resource) in the StateStore, expiry and renewal by conditional write;
                    `sync` skips a feed another process holds
  src/deletes.rs    reconcile_deletes, EventFeed, FlagDeleteWriter: delete events by key diff, EntityEvent or flag field;
                    DeleteLimits holds a mass delete back (`--allow-mass-deletes` releases it)
  src/media.rs      sync_media, page_pipeline, MediaIndex, MediaManifest, PageMedia, MediaWorkers: Media files from sync output (`media`) or per page (`sync --media`) with a worker pool; files named by SHA-256 (#89); index = snapshot + deltas (#149)
  src/media_store.rs  MediaStore, MediaUpload, LocalMediaStore, MediaRoot: where media files go (a directory; S3MediaStore in s3.rs)
  src/cli.rs, main.rs  the `reso-ingest` binary (sync, serve, media, discover [--starter], status, reset, replay, history, purge); run() takes writers so tests run in-process,
                    run_main() also installs the log subscriber
  src/logging.rs    LogFormat, log_subscriber: text or JSON log lines, RUST_LOG filter; run/server/resource spans
  src/metrics.rs    render_prometheus: freshness lag and per-run metrics as Prometheus text; render_media_prometheus (media counts); write_metrics_file; parse_rfc3339
  src/checks.rs     SchemaSnapshot ($metadata diff), CheckingWriter (repeated keys), count_warning; findings are warnings
  src/webhook.rs    run_summary, post_summary: the run-summary webhook (URL from `webhook_url_env`)
  src/error.rs      IngestError
  examples/loadtest.rs  load-test harness (#90): fake server with 1M records, faults, kill and resume; results in LOADTEST.md
  monitoring/       example Grafana dashboard and Prometheus alert rules (a test checks they use only exported metrics)
  servers.example.toml, tests/ (s3_tests.rs for the S3 state store, a `sync`/`status` run, a `purge` in the bucket, a killed sync resumed by a fresh worker and two `serve` workers sharing a bucket and `sync --media` into a bucket with its purge, against an in-process fake; s3_live.rs against a real server when RESO_S3_TEST_* or RESO_GCS_TEST_BUCKET is set; wiremock servers, CLI run in-process; sync_media_tests.rs for sync --media; media_scale_tests.rs for many media records, dedupe and a slow file host (#89);
                    log_json.rs for JSON log lines, own binary; run_main.rs for the log format and global subscriber of run_main, own binary; metrics_tests.rs for metrics files; checks_tests.rs for run checks and the webhook; audit_tests.rs for the access log; purge_tests.rs for purge; lease_tests.rs for held feeds; backfill_tests.rs for sync --backfill; state_tests.rs for state on a non-file backend; shutdown_tests.rs for a stop in the middle of a run; serve_tests.rs for the daemon and its reload; ops_tests.rs for config directories, paused feeds, reset/replay/history and onboarding (#94);
                    secret_rotation.rs for rotated secret files and no secrets in logs, own binary)
tests/              queries_tests.rs (URL building), client_http_tests.rs (wiremock servers),
                    paging_tests.rs (wiremock multi-page walks),
                    standard_replication_tests.rs (fake RESO server: ties, caps, ignored $orderby), auth_tests.rs (token endpoint, 401 retry),
                    download_tests.rs (conditional GET, .part files, retries, token scope),
                    retry_tests.rs (retries, Retry-After, counters; limiter timing is in src/retry.rs with a paused clock),
                    network_tests.rs (User-Agent, a small forward proxy, HTTPS with the test CA in fixtures/tls, pool; #[serial]),
                    auth_logs.rs (no secrets in tracing output; own binary),
                    live_reference_server.rs (`#[ignore]`, needs RESO_LIVE_URL)
Dockerfile          reso-ingest image: static musl binary on scratch, `parquet` and `s3` features, CA bundle for S3 (CI: .github/workflows/container.yml)
deploy/             kubernetes/ (ConfigMap+Secret+PVC, CronJob for sync, Deployment+Service for serve), systemd/ unit
examples/           test_*.rs feature demos, analyze_*.rs real-data analysis
```

Conventions: builders return `Self` for chaining; types PascalCase, functions snake_case; imports grouped std, external,
internal; unit tests in the same file (`#[cfg(test)]`), integration tests in `tests/`.

## Common changes

- **New query option:** method on `QueryBuilder` and a field on `Query` (`src/queries.rs`); emit it in
  `to_odata_string()`; unit tests there, integration tests in `tests/queries_tests.rs`; doc example; an example file if
  it is a major feature.
- **New error variant:** add to `ResoError` (`src/error.rs`), handle it in `src/client.rs` where needed, test it.
- **Release:** bump `Cargo.toml`, update `CHANGELOG.md`, run the full checks, `cargo doc --no-deps`, tag `vX.Y.Z`, then
  `cargo publish`. Releasing and publishing are the owner's call.

## Server notes

The RESO reference dataset `actris_ref` does not support `$count`, `$apply`, `$expand` or replication (authorization).
Mark such features as server-dependent in docs. OData 4.0 is the source of truth for query syntax; the RESO Web API
builds on it (https://www.reso.org/reso-web-api/, https://www.odata.org/documentation/).

RESO's reference server (tools.reso.org, run locally with Docker; findings in issue #35): Data Dictionary 2.0 by default
(2.1 via `METADATA_PATH`), 948 seed records, reads need no token. Supports `$filter`, `$orderby`, `$select`, `$top`
(cap 2000), `$skip`, `$count=true`, `$expand`, and an absolute `@odata.nextLink` in the body (`$skip` paging, default page
100). No `/replication`, no `/Resource/$count` (`execute_count` uses `$count=true&$top=0`), no `has` on multi-value fields. Unknown
resources return an HTML 404; unknown `$select`/`$orderby` fields are ignored. Its mock `POST /oauth/token` accepts any
credentials. Never commit its `$metadata`, seed data or responses (RESO licence); tests use invented data. Live tests:
`RESO_LIVE_URL=http://localhost:8080 cargo test --test live_reference_server -- --ignored`.
