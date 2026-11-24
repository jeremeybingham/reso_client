# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
- **Breaking:** `ResoError` gains the variants `TokenRequest` and `OutOfOrder`; a `match` without a wildcard arm
  no longer compiles.

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
