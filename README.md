# reso-client

[![CI](https://github.com/jeremeybingham/reso_client/workflows/CI/badge.svg)](https://github.com/jeremeybingham/reso_client/actions)
[![codecov](https://codecov.io/gh/jeremeybingham/reso_client/branch/main/graph/badge.svg)](https://codecov.io/gh/jeremeybingham/reso_client)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Crates.io](https://img.shields.io/crates/v/reso-client.svg)](https://crates.io/crates/reso-client)
[![Documentation](https://docs.rs/reso-client/badge.svg)](https://docs.rs/reso-client)

A Rust library for reading real estate data from [RESO Web API](https://www.reso.org/reso-web-api/) servers
(OData 4.0), on its way to becoming an **ingest layer** for many RESO servers at once.

## Goal

reso-client is meant to sit between one or more RESO Web API servers and the applications that use their data.
It should:

- connect to many RESO-compliant servers, each with its own credentials and limits;
- discover the resources each server offers and pull every one the account is licensed for;
- follow the API's rules: OAuth2, server-driven paging, RESO-standard replication, rate limits and retries;
- keep each server's data current with incremental, resumable syncs, including deletes;
- hand the records over **raw**, tagged with where they came from, in a few file formats (NDJSON first, then Parquet).

A typical user is an aggregator pulling from many MLS vendors into its own store. Transforming records,
de-duplicating across MLSs, mapping local fields and enforcing data-licence rules are the consuming application's
job, not this crate's. The crate stays resource-agnostic: resource and field names are strings and records are raw
JSON, so it works with any server's schema, including [Data Dictionary 2.1](https://dd.reso.org/DD2.1/).

The plan is tracked in [#18](https://github.com/jeremeybingham/reso_client/issues/18) and the
[Ingest layer milestone](https://github.com/jeremeybingham/reso_client/milestone/1).

## Current state (v0.2.1)

Today the crate is a well-tested OData query client for **one server**, authenticated with a fixed bearer token
or OAuth2 client credentials.

What works:

- Fluent `QueryBuilder`: `$filter`, `$select`, `$orderby`, `$top`/`$skip`, `$count`, `$expand`, `$apply`, lookup
  by key.
- Count-only queries (`/$count`) and raw `$metadata` retrieval (returned as an XML string, not parsed).
- OAuth2 client credentials: the token is fetched, cached, renewed at 90% of its lifetime and once after a 401.
- Typed errors (`ResoError`) mapped from HTTP status codes and OData error bodies.
- Configuration from environment variables or code; several clients can run side by side.
- Async on tokio; about 180 unit, integration and doc tests, with HTTP mocked by wiremock.

Known limits, each covered by a milestone issue:

| Limit | Issue |
|---|---|
| No retry, backoff or rate limiting; `Retry-After` is ignored | [#22](https://github.com/jeremeybingham/reso_client/issues/22) |
| `$metadata` is not parsed, so resources, keys and fields cannot be discovered | [#23](https://github.com/jeremeybingham/reso_client/issues/23) |
| `$select`/`$expand` lists are not URL-encoded; `dataset_id` is a Bridge-specific path segment | [#31](https://github.com/jeremeybingham/reso_client/issues/31) |

## Roadmap

The milestone is split into phases. Phases A and B make the low-level client correct against real servers; phase C
builds the ingest layer on top of it, most likely as a second crate (`reso-ingest`) with a CLI.

**Phase A: correct against real servers**

1. [#19](https://github.com/jeremeybingham/reso_client/issues/19) Standard paging: follow `@odata.nextLink` and add a
   page iterator.
2. [#20](https://github.com/jeremeybingham/reso_client/issues/20) RESO-standard replication (`ModificationTimestamp`
   plus nextLink): done, `StandardReplication`; Bridge's `/replication` stays as a vendor mode.
3. [#21](https://github.com/jeremeybingham/reso_client/issues/21) OAuth2 client credentials with token caching and
   refresh.
4. [#22](https://github.com/jeremeybingham/reso_client/issues/22) Retry, backoff and client-side rate limiting.

**Phase B: knowing what a server has**

5. [#23](https://github.com/jeremeybingham/reso_client/issues/23) Parse `$metadata` into a model (entity sets, keys,
   fields, types, lookups).

**Phase C: the ingest layer**

6. [#24](https://github.com/jeremeybingham/reso_client/issues/24) Multi-server TOML configuration (secrets from
   environment variables).
7. [#25](https://github.com/jeremeybingham/reso_client/issues/25) NDJSON output with source tags and a run manifest.
8. [#26](https://github.com/jeremeybingham/reso_client/issues/26) Incremental sync with checkpoints and resume.
9. [#27](https://github.com/jeremeybingham/reso_client/issues/27) Deletes and reconciliation.
10. [#28](https://github.com/jeremeybingham/reso_client/issues/28) Ingest CLI: `sync`, `discover`, `status`.
11. [#29](https://github.com/jeremeybingham/reso_client/issues/29) Parquet output with a schema from `$metadata`.

**Later or optional**

- [#30](https://github.com/jeremeybingham/reso_client/issues/30) Media file download.
- [#31](https://github.com/jeremeybingham/reso_client/issues/31) Small fixes (any time).

**Test server.** Mock servers cover most of the work, but checking real-server behaviour needs read-only access to a
RESO Web API server certified for Data Dictionary 2.1
([#32](https://github.com/jeremeybingham/reso_client/issues/32), help wanted). Without one, #23 and the DD 2.1 plan
([#17](https://github.com/jeremeybingham/reso_client/issues/17)) are blocked, and #19-#21, #26, #27, #29 and #30 can
be built but not confirmed against a live server.

Phase A is planned as v0.3.0. Expect breaking changes before 1.0; they are recorded in the [changelog](CHANGELOG.md).

## Installation

```toml
[dependencies]
reso-client = "0.2.1"
```

Requires Rust 1.87 or later.

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
| `RESO_DATASET_ID` | No | Dataset segment inserted after the base URL (Bridge Interactive servers use it) |
| `RESO_TIMEOUT` | No | HTTP timeout in seconds (default 30) |

Or configure in code:

```rust
use std::time::Duration;
use reso_client::{ClientConfig, ResoClient};

let config = ClientConfig::new("https://api.mls.example/OData", "token")
    .with_dataset_id("actris_ref") // only for servers that use a dataset path segment
    .with_timeout(Duration::from_secs(60));
let client = ResoClient::with_config(config)?;

// OAuth2 client credentials instead of a fixed token
use reso_client::{ClientCredentials, CredentialTransport};
let credentials = ClientCredentials::new("https://auth.mls.example/token", "client-id", "client-secret")
    .with_scope("api")
    .with_transport(CredentialTransport::BasicHeader);
let client = ResoClient::with_config(ClientConfig::with_auth("https://api.mls.example/OData", credentials))?;
```

With client credentials the client fetches a token on first use, reuses it until 90% of `expires_in` has passed (a
token without `expires_in` is kept until the server answers 401), and on a 401 fetches a new token and retries the
request once. Concurrent requests on one client share its token and wait for a single fetch. Secrets and tokens are
redacted from `Debug` output and never logged. Unconfirmed against a real MLS server (#32).

## Quick start

```rust
use reso_client::{QueryBuilder, ResoClient};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ResoClient::from_env()?;

    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin' and ListPrice gt 500000")
        .select(&["ListingKey", "City", "ListPrice"])
        .order_by("ListPrice", "desc")
        .top(10)
        .build()?;

    // Records come back as raw OData JSON: { "value": [...], "@odata.nextLink": "..." }
    let results = client.execute(&query).await?;
    if let Some(records) = results["value"].as_array() {
        println!("{} records", records.len());
    }

    // Walk a whole result set: the server picks the page size and sends a next link
    let mut page = client.execute_page(&QueryBuilder::new("Member").build()?).await?;
    let mut total = page.records.len();
    while let Some(next) = client.next_page(&page).await? {
        total += next.records.len();
        page = next;
    }
    println!("{} members", total);

    // The service's $metadata document, as XML
    let metadata = client.fetch_metadata().await?;
    println!("{} bytes of metadata", metadata.len());

    Ok(())
}
```

Errors are `ResoError` values (`Config`, `Network`, `Unauthorized`, `RateLimited`, `ODataError` and others), so
callers can tell a bad token from a server fault or a rate limit.

The API reference is the rustdoc: `cargo doc --open`, or [docs.rs](https://docs.rs/reso-client). Agents and
contributors start from [AGENTS.md](AGENTS.md).

## Examples

With a `.env` in place, run any example with `cargo run --example <name>`. They were written against Bridge
Interactive's `actris_ref` reference dataset; the project has no working server credentials at the moment (see
[#32](https://github.com/jeremeybingham/reso_client/issues/32)).

| Example | Shows |
|---|---|
| `test_connectivity` | Connection and authentication check |
| `test_property`, `test_member` | Queries on the Property and Member resources |
| `test_filters`, `test_select` | Filter syntax and field selection |
| `test_core_queries` | The [RESO Web API core query examples](https://transport.reso.org/proposals/web-api-core.html#28-core-query-examples) |
| `test_metadata` | Fetching `$metadata` |
| `test_standard_paging` | Walking a whole result set by `@odata.nextLink` (`execute_page`, `next_page`); runs against the local reference server |
| `test_actris_ref` | A tour of the reference dataset |
| `analyze_property_fields` | Which Property fields are populated across 200 active listings |
| `analyze_active_listings` | Summary statistics for 200 active residential listings |
| `test_count_only`, `test_apply`, `test_expand`, `test_pagination_nextlink`, `test_replication` | Features that need server support `actris_ref` does not give (`/$count`, `$apply`, `$expand`, header paging, Bridge replication) |

## Development

```bash
cargo test                    # unit, integration and doc tests
cargo fmt && cargo clippy -- -D warnings
cargo doc --open
```

Live tests against a local copy of RESO's reference server are ignored by default (CI stays offline). Start the server
(see [#35](https://github.com/jeremeybingham/reso_client/issues/35)), then:

```bash
RESO_LIVE_URL=http://localhost:8080 cargo test --test live_reference_server -- --ignored
```

Unit tests live next to the code in `src/`; integration tests in `tests/` use wiremock servers. Contribution rules are
in [AGENTS.md](AGENTS.md).

## License

MIT; see [LICENSE-MIT](LICENSE-MIT).

## Resources

- [RESO Web API](https://www.reso.org/reso-web-api/)
- [RESO Data Dictionary](https://www.reso.org/data-dictionary/) and [DD 2.1](https://dd.reso.org/DD2.1/)
- [OData 4.0 URL conventions](https://docs.oasis-open.org/odata/odata/v4.0/odata-v4.0-part2-url-conventions.html)
- [Changelog](CHANGELOG.md)
