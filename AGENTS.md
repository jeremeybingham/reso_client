# AGENTS.md

Guidance for AI coding agents and people working in this repository (tool-neutral).

reso-client is a Rust library (crates.io `reso-client`, pre-1.0, v0.2.1) for RESO Web API servers (OData 4.0). Its goal
is to become the **ingest layer** between one or more RESO servers and the applications that use their data: connect to
many compliant servers, discover and pull every resource the account is licensed for, follow the API's rules (OAuth2,
server-driven paging, RESO-standard replication, rate limits, retries), keep data current with incremental resumable
syncs, and hand records over **raw**, tagged with their source, in a few file formats (NDJSON first, then Parquet). The
typical user is an aggregator pulling from many MLS vendors into its own store. It is **not** responsible for
de-duplication across MLSs, local field mapping, data-licence or display-rule enforcement, or choosing a database.
Today the crate is a one-server query client (bearer token or OAuth2 client credentials); the plan is tracker issue
#18 (milestone "Ingest layer", child issues #19-#31, test-server need #32), and README.md has the current limits and
roadmap.

## Finding things

| You want | Go to |
|---|---|
| Goal, current state and limits, roadmap, quick start, configuration | `README.md` |
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
  Property/Member structs. This keeps the crate working with any server schema, including Data Dictionary 2.1 (#17).
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
- **No new dependencies without discussion** (XML parser, OAuth2 crate, rate-limit crates, `arrow`/`parquet` are open
  decisions in #18). The ingest layer is likely a second crate (`reso-ingest`) so this client stays lean.
- Every bug fixed gets a regression test. Docs change in the same commit as the behaviour they describe.

## Where things are

```
src/lib.rs          public API, re-exports, crate docs
src/client.rs       ResoClient, ClientConfig, HTTP operations
src/auth.rs         Auth, ClientCredentials, token fetch, cache and renewal (OAuth2 client credentials)
src/queries.rs      QueryBuilder, Query, ReplicationQueryBuilder
src/metadata.rs     Metadata model parsed from $metadata (roxmltree): entity sets, types, keys, properties, enums
src/paging.rs       Page, next-link reading (body, `next` and `Link` headers), relative resolution
src/replication.rs  ReplicationResponse, ReplicationQuery (Bridge `/replication`, vendor mode)
src/standard_replication.rs  StandardReplication, ReplicationRun, Checkpoint (RESO-standard, keyset on timestamp+key)
src/retry.rs        RetryPolicy (backoff, Retry-After), RateLimit and its limiter, ClientStats counters
src/error.rs        ResoError and the Result alias
reso-ingest/        second workspace crate (unpublished): config.rs (multi-server TOML, secrets via `*_env`), runner.rs
                    (independent per-server tasks, concurrency cap), output.rs (RecordWriter, NdjsonWriter: tagged lines, size
                    rotation), manifest.rs (RunManifest, pull_resource, sync_resource), checkpoint.rs (CheckpointStore, JSON files: checkpoints, key sets, event sequences),
                    deletes.rs (reconcile_deletes, EventFeed: delete events by key diff or EntityEvent),
                    cli.rs + main.rs (`reso-ingest` binary: sync, discover, status; hand-rolled args, run() takes writers so tests run it in-process), servers.example.toml, tests/
tests/              queries_tests.rs (URL building), client_http_tests.rs (wiremock servers),
                    paging_tests.rs (wiremock multi-page walks),
                    standard_replication_tests.rs (fake RESO server: ties, caps, ignored $orderby), auth_tests.rs (token endpoint, 401 retry),
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
100). No `/replication`, no `/Resource/$count` (use `$count=true&$top=0`), no `has` on multi-value fields. Unknown
resources return an HTML 404; unknown `$select`/`$orderby` fields are ignored. Its mock `POST /oauth/token` accepts any
credentials. Never commit its `$metadata`, seed data or responses (RESO licence); tests use invented data. Live tests:
`RESO_LIVE_URL=http://localhost:8080 cargo test --test live_reference_server -- --ignored`.
