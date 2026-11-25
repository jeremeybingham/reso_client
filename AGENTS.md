# AGENTS.md

Guidance for AI coding agents and people working in this repository (tool-neutral).

This repository is an **ingest layer** for RESO Web API servers (OData 4.0), in two Rust crates:

- `reso-client` (crates.io, pre-1.0, v0.3.0): a client for one server: queries, standard paging and replication,
  bearer token or OAuth2 client credentials, retries and rate limits, a parsed `$metadata` model.
- `reso-ingest` (unpublished, `reso-ingest/`): pulls every resource from many servers into tagged NDJSON or Parquet
  files, incrementally and resumably, with delete detection; library plus the `reso-ingest` command.

Records are handed over **raw**, tagged with their source. The typical user is an aggregator pulling from many MLS
vendors into its own store. The crates do **not** de-duplicate across MLSs, map local fields, enforce data-licence or
display rules, or choose a database. The plan is tracker issue #18 (milestone "Ingest layer"); the open work is a
vendor-server run (#32).

## Finding things

| You want | Go to |
|---|---|
| What the crates do, limits, quick start, configuration, what's next | `README.md` |
| The ingest command: configuration, output format, deletes, media files, Parquet | `reso-ingest/README.md` |
| The full plan, gap list with code references, open owner decisions | GitHub issue #18 and its children |
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
  limiting are hand-written); heavier ones belong in `reso-ingest` (`clap`, `toml`, and `arrow`/`parquet` behind the
  `parquet` feature).
- Every bug fixed gets a regression test. Docs change in the same commit as the behaviour they describe.

## Where things are

```
src/lib.rs          public API, re-exports, crate docs
src/client.rs       ResoClient, ClientConfig, HTTP operations
src/download.rs     Downloader, DownloadConfig, SendToken: file downloads with their own limits (Media files)
src/auth.rs         Auth, ClientCredentials, token fetch, cache and renewal (OAuth2 client credentials)
src/queries.rs      QueryBuilder, Query, ReplicationQueryBuilder
src/metadata.rs     Metadata model parsed from $metadata (roxmltree): entity sets, types, keys, properties, enums
src/paging.rs       Page, next-link reading (body, `next` and `Link` headers), relative resolution
src/replication.rs  ReplicationResponse, ReplicationQuery (Bridge `/replication`, vendor mode)
src/standard_replication.rs  StandardReplication, ReplicationRun, Checkpoint (RESO-standard, keyset on timestamp+key)
src/retry.rs        RetryPolicy (backoff, Retry-After), RateLimit and its limiter, ClientStats counters
src/error.rs        ResoError and the Result alias
reso-ingest/        second workspace crate (unpublished); README.md documents the command
  src/config.rs     Config: multi-server TOML, secrets read from the variables named by `*_env`
  src/runner.rs     run_servers: one independent task per server, concurrency cap
  src/output.rs     RecordWriter, NdjsonWriter: tagged lines, size rotation
  src/parquet_out.rs  ParquetWriter typed from $metadata (`parquet` feature)
  src/manifest.rs   RunManifest, pull_resource, sync_resource
  src/checkpoint.rs CheckpointStore: JSON files for checkpoints, key sets, event sequences
  src/deletes.rs    reconcile_deletes, EventFeed: delete events by key diff or EntityEvent
  src/media.rs      sync_media, MediaIndex, MediaManifest: Media files from sync output (`media` command)
  src/cli.rs, main.rs  the `reso-ingest` binary (sync, media, discover, status); run() takes writers so tests run in-process
  src/error.rs      IngestError
  servers.example.toml, tests/ (wiremock servers, CLI run in-process)
tests/              queries_tests.rs (URL building), client_http_tests.rs (wiremock servers),
                    paging_tests.rs (wiremock multi-page walks),
                    standard_replication_tests.rs (fake RESO server: ties, caps, ignored $orderby), auth_tests.rs (token endpoint, 401 retry),
                    download_tests.rs (conditional GET, .part files, retries, token scope),
                    retry_tests.rs (retries, Retry-After, counters; limiter timing is in src/retry.rs with a paused clock),
                    auth_logs.rs (no secrets in tracing output; own binary),
                    live_reference_server.rs (`#[ignore]`, needs RESO_LIVE_URL)
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
