# RESO Client - Claude Code Instructions

## Project Overview

This is a Rust client library for RESO Web API servers using OData 4.0. The library provides a type-safe, ergonomic interface for querying real estate MLS data through standardized REST APIs. It's published to crates.io as `reso-client` and is currently at version 0.2.1 (pre-1.0).

**Key Purpose**: Enable developers to build Rust applications that interact with Multiple Listing Service (MLS) data through RESO-compliant APIs.

## Tech Stack

- **Language**: Rust 1.87.0+
- **Async Runtime**: tokio 1.x with full features
- **HTTP Client**: reqwest 0.12 with rustls-tls
- **Serialization**: serde 1.x + serde_json 1.x
- **Error Handling**: thiserror 1.x
- **Logging**: tracing 0.1
- **Testing**: tokio-test, wiremock, serial_test

## Project Structure

```
reso_client/
├── src/
│   ├── lib.rs          # Public API, re-exports, crate docs
│   ├── client.rs       # ResoClient, ClientConfig, HTTP operations
│   ├── queries.rs      # QueryBuilder, Query, ReplicationQueryBuilder
│   ├── replication.rs  # ReplicationResponse, ReplicationQuery
│   └── error.rs        # ResoError, Result type definitions
├── tests/
│   ├── queries_tests.rs      # Query building and URL generation tests
│   └── client_http_tests.rs  # HTTP operations with mock servers
├── examples/           # 15+ working examples demonstrating all features
│   ├── test_*.rs      # Basic feature demonstrations
│   └── analyze_*.rs   # Real-world analysis examples
├── README.md          # Quick start and overview
├── USAGE.md           # Comprehensive API reference (1500+ lines)
├── CHANGELOG.md       # Version history
├── llms.txt           # LLM-friendly documentation index
└── CLAUDE.md          # This file
```

## Development Setup

### Environment Variables (for examples and tests)

Create a `.env` file (gitignored) with:
```bash
RESO_BASE_URL=https://api.bridgedataoutput.com/api/v2/OData
RESO_TOKEN=your-oauth-token
RESO_DATASET_ID=actris_ref  # Optional
RESO_TIMEOUT=30              # Optional, seconds
```

### Common Commands

```bash
# Build the library
cargo build

# Run all tests (unit + integration + doc)
cargo test

# Run only unit tests (in src/ modules)
cargo test --lib

# Run only integration tests (tests/ directory)
cargo test --test '*'

# Run specific example
cargo run --example test_connectivity

# Format code (REQUIRED before commits)
cargo fmt

# Run lints
cargo clippy -- -D warnings

# Build documentation
cargo doc --open

# Check without building
cargo check
```

## Code Conventions

### Style Guide

1. **Formatting**: ALWAYS run `cargo fmt` before committing. The project uses standard rustfmt settings.

2. **Documentation**:
   - All public items MUST have doc comments (`///`)
   - Module-level docs use `//!` at the top of files
   - Include examples in doc comments when possible
   - Doc comments should explain WHY, not just WHAT

3. **Error Handling**:
   - Use `thiserror` for error definitions
   - All errors are in `src/error.rs`
   - Return `Result<T, ResoError>` for fallible operations
   - Use the re-exported `Result` type alias from `error` module

4. **Naming**:
   - Types: PascalCase (e.g., `ResoClient`, `QueryBuilder`)
   - Functions/methods: snake_case (e.g., `from_env`, `execute_count`)
   - Constants: SCREAMING_SNAKE_CASE
   - Private items: prefix with underscore only when needed to avoid warnings

5. **Imports**:
   - Group imports: std, external crates, internal modules
   - Use explicit imports, avoid glob imports (`use foo::*`)
   - Re-export commonly used types in `lib.rs`

6. **Testing**:
   - Unit tests go in the same file as the code (`#[cfg(test)] mod tests`)
   - Integration tests go in `tests/` directory
   - Use wiremock for HTTP mocking in integration tests
   - Use serial_test for tests that need sequential execution
   - All examples in `examples/` should be runnable and well-documented

### Code Patterns

**Builder Pattern**: Used extensively for `ClientConfig`, `QueryBuilder`, `ReplicationQueryBuilder`
```rust
let query = QueryBuilder::new("Resource")
    .filter("expression")
    .select(&["field1", "field2"])
    .top(100)
    .build()?;
```

**Fluent API**: All builders return `Self` for chaining

**From/Into Traits**: Prefer implementing `From` when conversion is infallible

**Async**: All I/O operations are async, use `tokio::main` in examples

## Testing Strategy

**Test Coverage**: Currently 183 total tests
- 74 unit tests in `src/` modules
- 58 integration tests in `tests/` directory
- 51 doc tests in documentation

**Testing Philosophy**:
- Unit tests for internal logic and validation
- Integration tests for HTTP operations (with mocks)
- Doc tests for public API examples
- Examples for end-to-end demonstrations

**When Adding Features**:
1. Write unit tests for internal logic
2. Write integration tests for public API
3. Add doc tests for common usage
4. Create or update an example if it's a major feature
5. Update USAGE.md with detailed documentation
6. Update README.md if it's a user-facing feature

## Important Rules

### DO

- ✅ Run `cargo fmt` before every commit
- ✅ Run `cargo clippy` and fix all warnings
- ✅ Run `cargo test` to ensure all tests pass
- ✅ Update documentation when changing public APIs
- ✅ Add examples for new features
- ✅ Use the existing error types in `ResoError`
- ✅ Follow the builder pattern for complex types
- ✅ Include doc tests in public API documentation
- ✅ Use `tracing` for logging, not `println!` in library code
- ✅ Keep backward compatibility where possible (pre-1.0)

### DO NOT

- ❌ Commit code that doesn't pass `cargo fmt`
- ❌ Commit code with clippy warnings
- ❌ Use `unwrap()` or `expect()` in library code (examples are OK)
- ❌ Add dependencies without discussion
- ❌ Break backward compatibility without updating version
- ❌ Use `println!` in library code (use `tracing` instead)
- ❌ Ignore failing tests or doc tests
- ❌ Make breaking changes to public API without major version bump
- ❌ Commit `.env` files or secrets
- ❌ Use glob imports (`use foo::*`)

## Documentation Files

- **README.md**: First point of contact, keep concise, quick start focused
- **USAGE.md**: Comprehensive reference, detailed examples, troubleshooting
- **CHANGELOG.md**: Track all changes, follow Keep a Changelog format
- **llms.txt**: LLM navigation aid, keep synchronized with major changes
- **CLAUDE.md**: This file, instructions for Claude Code

## Server Compatibility Notes

Some features are NOT supported by the RESO Web API reference server (`actris_ref`):
- `$count` endpoint (returns 404)
- `$apply` aggregation (returns 400)
- `$expand` navigation properties (returns error)
- Replication endpoint (requires authorization)

When working on these features, note they are server-dependent in documentation.

## Common Patterns When Contributing

### Adding a New Query Feature

1. Add the method to `QueryBuilder` in `src/queries.rs`
2. Add corresponding field to `Query` struct
3. Update `to_odata_string()` to include new parameter
4. Add unit tests in `queries.rs`
5. Add integration tests in `tests/queries_tests.rs`
6. Add example usage in doc comment
7. Update USAGE.md with detailed documentation
8. Create or update an example file if significant

### Adding a New Error Type

1. Add variant to `ResoError` enum in `src/error.rs`
2. Implement `Display` formatting (thiserror handles this)
3. Update error handling in `src/client.rs` if needed
4. Add tests for error scenario
5. Update USAGE.md error handling section

### Release Process

1. Update version in `Cargo.toml`
2. Update CHANGELOG.md with changes
3. Run full test suite: `cargo test`
4. Run `cargo fmt` and `cargo clippy`
5. Build docs: `cargo doc --no-deps`
6. Create git tag: `git tag -a v0.x.x -m "Release 0.x.x"`
7. Push to crates.io: `cargo publish`

## Helpful Context

- This library is designed for production use but is pre-1.0 (API may change)
- It's used by real estate developers to access MLS data
- Performance matters: some users query millions of records via replication
- Security matters: bearer tokens must be protected, never logged
- The OData 4.0 spec is the source of truth for query syntax
- RESO Web API builds on OData with real estate specific resources

## Resources

- RESO Web API: https://www.reso.org/reso-web-api/
- OData 4.0: https://www.odata.org/documentation/
- Project repo: https://github.com/jeremeybingham/reso_client
