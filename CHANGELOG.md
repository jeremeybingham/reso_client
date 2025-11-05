# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **Refactored client.rs for DRY principle** - Extracted common HTTP request handling logic into reusable helper methods (`send_authenticated_request`, `parse_json_response`, `parse_text_response`), reducing code duplication by ~67 lines across 6 public methods
- **Improved code documentation** - Added inline comments throughout `src/` explaining:
  - Complex URL building logic with dataset_id path insertion
  - Error handling flow and OData error parsing
  - Header extraction logic for replication next links
  - Business rule explanations (e.g., why replication max is 2000 records)
  - OData v4.0 specification compliance details
- **Reorganized test structure** - Moved 438 lines of query builder tests from `src/queries.rs` to `tests/queries_tests.rs` for better separation of concerns and more idiomatic Rust project structure

### Documentation
- Updated README.md with comprehensive test running instructions
- Added documentation for both unit tests and integration tests
- No breaking changes to public API

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
- Replaced `property_test.rs` with more comprehensive `test_property.rs` example
- Formatting improvements across example files
- Updated query examples to demonstrate real-world usage patterns

### Documentation
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

[Unreleased]: https://github.com/jeremeybingham/reso_client/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jeremeybingham/reso_client/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jeremeybingham/reso_client/releases/tag/v0.1.0
