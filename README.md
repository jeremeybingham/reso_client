# reso-client

[![CI](https://github.com/jeremeybingham/reso_client/workflows/CI/badge.svg)](https://github.com/jeremeybingham/reso_client/actions)
[![codecov](https://codecov.io/gh/jeremeybingham/reso_client/branch/main/graph/badge.svg)](https://codecov.io/gh/jeremeybingham/reso_client)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Crates.io](https://img.shields.io/crates/v/reso-client.svg)](https://crates.io/crates/reso-client)
[![Documentation](https://docs.rs/reso-client/badge.svg)](https://docs.rs/reso-client)

Rust tools for pulling real estate data from [RESO Web API](https://www.reso.org/reso-web-api/) servers (OData 4.0).

This repository holds two crates:

| Crate | What it is | Status |
|---|---|---|
| [`reso-client`](https://crates.io/crates/reso-client) | Async client for one server: queries, standard paging and replication, OAuth2, retries and rate limits, a parsed `$metadata` model, file downloads | v0.3.0 on crates.io |
| [`reso-ingest`](reso-ingest/README.md) | Library and command that pull every resource from many servers into tagged NDJSON or Parquet files, incrementally, with delete detection, and download the `Media` files they point at | In this repository only, not published |

Together they form an **ingest layer**: they connect to many servers, follow the API's rules, keep each copy current,
and hand the records over **raw**, tagged with where they came from. Mapping fields, de-duplicating across MLSs,
enforcing data-licence rules and choosing a database are left to the application that uses the data. Resource and
field names are plain strings and records are raw JSON, so any server schema works, including
[Data Dictionary 2.1](https://dd.reso.org/DD2.1/).

## Status and limits

- **Tested against:** mock servers (every test, offline) and RESO's reference server run locally
  ([#35](https://github.com/jeremeybingham/reso_client/issues/35)): a full `reso-ingest sync`, a repeat run, an
  interrupted run resumed, a changed record and a deleted record all came out right.
- **Not yet tested against a vendor server** (Bridge, MLS Grid, Trestle and others): OAuth2, rate limits, replication
  and delete feeds behave as the standard says, but are unconfirmed in practice until
  [#32](https://github.com/jeremeybingham/reso_client/issues/32).
- `reso-ingest` syncs only resources with a single-field key and a `ModificationTimestamp`; others are skipped.
- `reso-ingest sync --format parquet` writes files rotated by `--max-file-mb`; a checkpoint is saved when its file is finished.
- Media files: `reso-ingest media` downloads them from NDJSON `Media` output (not from Parquet output); checked against the
  reference server's `Media` records, not yet against a vendor's file host (signed URLs, media limits).
- Pre-1.0: breaking changes can come in any minor release and are listed in the [changelog](CHANGELOG.md).

### Data Dictionary 2.1

The crates need no change for [DD 2.1](https://dd.reso.org/DD2.1/): they never name a resource or field, and records
stay raw. Code that hard-codes field names should note:

- **Renamed unit fields.** Twelve fields drop their plural `s`; the plural names are deprecated: `AirRightsUnits`,
  `AvailableAreaUnits`, `BuildingAreaUnits`, `CommercialAreaUnits`, `FloorAreaRatioUnits`, `HeightUnits`,
  `LeaseRateUnits`, `MaximumContiguousAreaUnits`, `MinimumDivisibleAreaUnits`, `ParkingRatioUnits`,
  `PricePerAreaUnits`, `TotalBuildingAreaUnits` (so `AirRightsUnit` and so on). Four expansion fields are also renamed;
  read a server's `$metadata` (`fetch_metadata_model()`) for the names it uses.
- **Lookup values arrive as strings,** whether the server declares lookups as strings with a `Lookup` resource or as
  enum types. Filter with string literals (`StandardStatus eq 'Active Under Contract'`). For multi-value fields use
  `any()`; `has` failed on RESO's reference server.

Checked on RESO's reference server with DD 2.1 metadata, not yet on a vendor server
([#17](https://github.com/jeremeybingham/reso_client/issues/17)).

## Installation

```toml
[dependencies]
reso-client = "0.3"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Rust 1.87 or later.

## Quick start

```rust
use reso_client::{QueryBuilder, ResoClient, StandardReplication};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ResoClient::from_env()?;

    // One query; the response is the raw OData JSON: { "value": [...], "@odata.nextLink": "..." }
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin' and ListPrice gt 500000")
        .select(&["ListingKey", "City", "ListPrice"])
        .order_by("ListPrice", "desc")
        .top(10)
        .build()?;
    let results = client.execute(&query).await?;
    println!("{} records", results["value"].as_array().map_or(0, Vec::len));

    // A whole result set: the server picks the page size and sends a next link
    let mut page = client.execute_page(&QueryBuilder::new("Member").build()?).await?;
    let mut total = page.records.len();
    while let Some(next) = client.next_page(&page).await? {
        total += next.records.len();
        page = next;
    }
    println!("{total} members");

    // RESO-standard replication on ModificationTimestamp; save run.checkpoint() to resume later
    let mut run = client.replicate(StandardReplication::new("Property", "ListingKey").page_size(1000));
    while let Some(batch) = run.next_batch().await? {
        println!("{} records, up to {:?}", batch.len(), run.checkpoint());
    }

    // What the server offers, from $metadata
    let model = client.fetch_metadata_model().await?;
    for name in model.resource_names() {
        println!("{name}");
    }

    Ok(())
}
```

What else the client does:

- `QueryBuilder`: `$filter`, `$select`, `$orderby`, `$top`, `$skip`, `$count`, `$expand`, `$apply`, lookup by key.
- Paging by `@odata.nextLink` in the body or by `next` and `Link` headers; relative links are resolved, and a link to
  another host is refused so the token is never sent elsewhere.
- OAuth2 client credentials: the token is fetched on first use, renewed at 90% of its lifetime and once after a 401.
- Retries with backoff and `Retry-After` for network errors and 429/502/503/504; client-side limits per second, per
  hour and bytes per day; request counters (`ResoClient::stats`). Off by default.
- `$metadata` as raw XML (`fetch_metadata`) or a model (`fetch_metadata_model`): entity sets, keys, properties,
  navigation properties, enum types.
- File downloads (`ResoClient::downloader`): a `Downloader` with its own retries, rate limit, timeout and counters,
  so `Media` files do not use up the API's budget. Each download is a conditional GET (`ETag`, `Last-Modified`) written
  to a `.part` file and renamed when complete; the API token goes only to the API's own host unless `SendToken` says
  otherwise.
- Bridge Interactive's `/replication` endpoint and `dataset_id` path segment, as a vendor mode.
- Typed errors (`ResoError`: `Unauthorized`, `RateLimited`, `ODataError`, `Network` and others); secrets are never
  logged and are redacted from `Debug` output.

The full API, with examples, is the rustdoc: [docs.rs/reso-client](https://docs.rs/reso-client) or `cargo doc --open`.

## Configuration

`ResoClient::from_env()` reads these variables (a `.env` file works too; see [example.env](example.env)):

| Variable | Required | Description |
|---|---|---|
| `RESO_BASE_URL` | Yes | Base URL of the server's OData service |
| `RESO_TOKEN` | One of the two | Static bearer token |
| `RESO_CLIENT_ID` | One of the two | OAuth2 client id; when set, client credentials are used instead of `RESO_TOKEN` |
| `RESO_CLIENT_SECRET` | With `RESO_CLIENT_ID` | OAuth2 client secret |
| `RESO_TOKEN_URI` | With `RESO_CLIENT_ID` | Token endpoint URL |
| `RESO_SCOPE` | No | Scope sent with the token request |
| `RESO_CREDENTIAL_TRANSPORT` | No | How the id and secret are sent: `body` (form fields, default), `header` (HTTP Basic) or `query` |
| `RESO_DATASET_ID` | No | Bridge Interactive's dataset path segment; leave unset for other servers |
| `RESO_TIMEOUT` | No | HTTP timeout in seconds (default 30) |
| `RESO_MAX_RETRIES` | No | Retries for network errors and 429/502/503/504 (default 0) |
| `RESO_RATE_PER_SECOND`, `RESO_RATE_PER_HOUR`, `RESO_BYTES_PER_DAY` | No | Client-side ceilings (default none) |

Or in code:

```rust
use std::time::Duration;
use reso_client::{ClientConfig, ClientCredentials, CredentialTransport, RateLimit, ResoClient, RetryPolicy};

let credentials = ClientCredentials::new("https://auth.mls.example/token", "client-id", "client-secret")
    .with_scope("api")
    .with_transport(CredentialTransport::BasicHeader);
let config = ClientConfig::with_auth("https://api.mls.example/odata", credentials)
    .with_timeout(Duration::from_secs(60))
    .with_retry(RetryPolicy::new(4))
    .with_rate_limit(RateLimit::default().per_second(2));
let client = ResoClient::with_config(config)?;
```

## Ingest command

`reso-ingest` reads a TOML file listing servers (secrets come from environment variables it names) and pulls each one:

```sh
cargo run -p reso-ingest -- sync --config servers.toml --out data/    # incremental pull of every server
cargo run -p reso-ingest -- media --config servers.toml --out data/   # download new and changed Media files
cargo run -p reso-ingest -- discover --config servers.toml            # each server's $metadata model as JSON
cargo run -p reso-ingest -- status --out data/                        # checkpoints and the last run
```

Configuration, output format, delete detection, media download and Parquet are described in [reso-ingest/README.md](reso-ingest/README.md).

## Examples

`examples/` holds runnable programs: `cargo run --example <name>` with a `.env` or the variables above set. Most were
written against Bridge Interactive's `actris_ref` dataset, and the project has no vendor credentials at the moment
(#32), so they are kept as usage samples rather than checked on every change.

| Example | Shows |
|---|---|
| `test_connectivity` | A first request: configuration, network and authentication |
| `test_standard_paging` | Walking a whole result set by `@odata.nextLink`; runs against the local reference server |
| `test_property`, `test_member`, `test_filters`, `test_select` | Queries, filters and field selection |
| `test_core_queries` | The [RESO Web API core query examples](https://transport.reso.org/proposals/web-api-core.html#28-core-query-examples) |
| `test_metadata` | Fetching `$metadata` |
| `analyze_property_fields`, `analyze_active_listings` | Field fill rates and summary statistics over 200 active listings |
| `test_actris_ref` | A tour of Bridge's reference dataset |
| `test_count_only`, `test_apply`, `test_expand` | `execute_count` (`$count=true&$top=0`), `$apply` and `$expand`, which need server support |
| `test_replication`, `test_pagination_nextlink` | Bridge's `/replication` endpoint (vendor mode) |

To run against RESO's reference server started locally (see
[#35](https://github.com/jeremeybingham/reso_client/issues/35)), set `RESO_BASE_URL=http://localhost:8080` and
`RESO_TOKEN=dummy`.

## Development

```sh
cargo fmt && cargo clippy -- -D warnings   # both crates
cargo test                                 # unit, wiremock integration and doc tests; offline
cargo test -p reso-ingest --features parquet
RESO_LIVE_URL=http://localhost:8080 cargo test --test live_reference_server -- --ignored   # local reference server
```

Rules for contributors and coding agents are in [AGENTS.md](AGENTS.md).

## What's next

**Can be done now, from vendors' public documentation** (built against mock servers like the rest; confirmed only by
#32). Read in November 2026, MLS Grid and Trestle differ from the standard in ways `reso-ingest` cannot yet handle:

- **Required filter and expand per server or resource.** MLS Grid requires `OriginatingSystemName eq '…'` in every
  request and serves `Media` only through `$expand` on `Property`, `Member` and `Office` (with `$expand`, at most 1000
  records per page).
- **Deletes signalled by a field.** MLS Grid marks removed records with `MlgCanView` false and drops them from the feed
  after 7 days; it has no `EntityEvent`. Expanded child records carry no flag and replace the earlier set.
- **Media from expanded records.** `reso-ingest media` reads only a top-level `Media` resource. MLS Grid's media URLs
  are signed, single-use and expire after an hour, so files must be fetched soon after the sync that saw them.
- **More rate limits.** MLS Grid also caps requests per day (40,000) and download volume per hour (4 GB); `RateLimit`
  has per second, per hour and bytes per day only.

**Small fixes:** run ids have one-second resolution, so two syncs in the same second would share file names.

**Waiting on others:**

- A vendor server ([#32](https://github.com/jeremeybingham/reso_client/issues/32)): run against it, fix what it shows,
  check media download against its file host, then publish `reso-ingest`.
- Moving off `chrono` ([#51](https://github.com/jeremeybingham/reso_client/issues/51)) waits on an arrow/parquet release.
- Replies from RESO on the reference server issues (reso-tools #453, #454, #455).

**Not planned without a need:** resources with a composite key or no `ModificationTimestamp`, `media` from Parquet
output.

The overall plan is tracker [#18](https://github.com/jeremeybingham/reso_client/issues/18).

## License

MIT; see [LICENSE-MIT](LICENSE-MIT).

## Resources

- [RESO Web API](https://www.reso.org/reso-web-api/)
- [RESO Data Dictionary](https://www.reso.org/data-dictionary/) and [DD 2.1](https://dd.reso.org/DD2.1/)
- [OData 4.0 URL conventions](https://docs.oasis-open.org/odata/odata/v4.0/odata-v4.0-part2-url-conventions.html)
- [Changelog](CHANGELOG.md)
