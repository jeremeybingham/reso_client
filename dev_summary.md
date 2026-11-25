# Development Summary: reso-client

> Written in November 2025, when the project reached 0.2.1. It is a history of how the project began; files it
> mentions such as `USAGE.md` and `llms.txt` have since been removed. Current documentation: [README.md](README.md).

## Overview

`reso-client` is a Rust library for the [RESO Web API](https://www.reso.org/reso-web-api/), the standardized REST interface used by Multiple Listing Service (MLS) providers in real estate. It wraps OData 4.0 query semantics in an ergonomic, type-safe Rust API, and is published to crates.io as `reso-client`. The project is a good example of a developer using Claude Code to accelerate the evolution of a working prototype into a production-quality open-source library.

---

## Development Timeline

### Phase 1: Initial Prototype (October 20, 2025)

The project began as a personal Rust application — not a library — with files like `src/main.rs`, `src/metadata_parser.rs`, and `src/reso_client.rs`. The initial commit (`initial upload`) established the core HTTP client logic, OData query building, bearer token auth, and environment variable configuration. The early README and `.env-example` were created by the user directly.

**User's contribution here:** The foundational architecture — the decision to target RESO/OData, the choice of Rust, the async approach with Tokio, and the domain knowledge about how MLS APIs work (dataset IDs, replication endpoints, metadata fetching) — all came from the user in this phase.

### Phase 2: Library Extraction and Infrastructure (November 2, 2025)

The user performed a substantial manual refactor: deleted the application-specific files (`main.rs`, `metadata_parser.rs`, etc.), restructured the codebase as a proper Rust library, added a CI/CD workflow, an MIT license, a CHANGELOG, and initial unit tests for the query builder. URL encoding for filter expressions was fixed. This was all done through direct commits, not Claude PRs.

**User's contribution here:** The decision to publish as a library rather than keep it as an internal tool was entirely the user's. The project structure, module layout (`client.rs`, `queries.rs`, `replication.rs`, `error.rs`), and the builder pattern API design reflect user-driven architectural choices made at this stage.

### Phase 3: Feature Development via Claude (November 3, 2025)

The first Claude-authored pull requests appeared on November 3. The branch naming convention (`claude/rust-app-property-counts-011CUm...`) makes it clear which PRs were Claude-driven.

- **PR #1** — Added count-only query support: a new `.count()` method on `QueryBuilder` and an `execute_count()` method on the client that hits the `/$count` OData endpoint and returns a `u64`. The user prompted Claude to implement efficient record counting without fetching all records.
- **PR #2** — Added OData `$apply` aggregation support: a `.apply()` builder method for groupby/aggregate expressions. The user had a concrete use case in mind (getting counts grouped by field) and directed Claude to add the feature.
- **PR #3** — Documentation update: Claude documented both new features in README and USAGE.md, including important server compatibility warnings (the RESO reference server `actris_ref` does not support `$apply`).

**Division of responsibility here:** The user identified what features were needed (count queries, aggregation) based on domain knowledge of RESO/OData. Claude implemented the Rust code, wrote the tests, and updated the documentation.

### Phase 4: Examples and v0.2.0 Release (November 4-5, 2025)

The user added an `examples/` directory and a comprehensive suite of 13+ runnable example files (`test_property.rs`, `test_filters.rs`, `test_replication.rs`, etc.) through a combination of direct commits and uploads. The v0.2.0 release also included a significant refactor of `client.rs` for DRY principles (extracting shared HTTP helpers) and reorganization of tests from inline to a dedicated `tests/` directory.

**User's contribution here:** The example suite appears to have been authored or directed by the user in close collaboration with Claude. The CHANGELOG entry for v0.2.0 describes a `~67 line` reduction from DRY refactoring — a specific, targeted code quality decision. The user also added several "analysis" examples (`analyze_property_fields.rs`, `analyze_active_listings.rs`) that reflect real-world use cases the user wanted to demonstrate.

### Phase 5: Publication Preparation via Claude (November 7, 2025)

With a working library in hand, the user directed Claude to prepare the project for publication to crates.io. This produced PRs #4-7 in rapid succession:

- **PR #4** — Fixed Clippy warnings in example files (needless borrows, manual range comparisons)
- **PR #5** — Updated `Cargo.toml` with MSRV (`rust-version = "1.70"`), a 5th keyword for discoverability, an `exclude` list to reduce package size, and removed `Cargo.lock` from version control
- **PR #6** — Added comprehensive rustdoc coverage across all public APIs, with doc examples on every method
- **PR #7** — Added code coverage reporting: a `cargo-tarpaulin` job in CI, Codecov.io integration, and status badges in the README

Between Claude PRs, the user also made several direct commits: uploading files via the GitHub web UI, fixing Rust version in `Cargo.toml` to 1.87.0 to address Clippy errors, manually running `cargo fmt`, and bumping the version to `0.2.1`.

**Division of responsibility here:** The user set the publication goal and made judgment calls (e.g., the specific Rust version, which badges to add, removing `Cargo.lock`). Claude handled the mechanical work: finding and fixing all lint warnings, writing comprehensive doc comments, and wiring up CI tooling.

### Phase 6: Comprehensive Test Coverage via Claude (November 8, 2025)

The final major development push was a systematic test coverage campaign, entirely Claude-driven, structured as a series of eight sequential PRs (PRs #8-15) on a single branch:

| PR | Focus | New Tests | Running Total |
|----|-------|-----------|---------------|
| #8 | Error module unit tests | 27 | 114 |
| #9 | Replication module unit tests | 17 | 131 |
| #10 | Client module unit tests | 30 | 161 |
| #11 | HTTP integration tests (wiremock) | 22 | 183 |
| #12 | Fix test race condition (serial_test) | 0 | 183 |
| #13 | Update documentation to reflect coverage | 0 | 183 |
| #14 | Add llms.txt and CLAUDE.md | 0 | 183 |
| #15 | Final 6 tests for 100% line coverage | 6 | 189 |

A race condition was discovered after PR #11 — environment variable tests were interfering with each other when run in parallel. Claude diagnosed the root cause (process-global env vars), added `serial_test` as a dev dependency, and fixed the issue in PR #12.

PR #14 is notable: Claude authored both `llms.txt` (following the llmstxt.org standard to help other LLMs navigate the codebase) and `CLAUDE.md` (project-specific instructions for future Claude Code sessions), essentially documenting the human-AI collaboration pattern for future use.

**Division of responsibility here:** The user directed Claude to improve test coverage; Claude designed and implemented the phased approach, chose the testing tools (`wiremock` for HTTP mocking, `serial_test` for isolation), and diagnosed the race condition independently.

---

## How the User Interacted with Claude Code

Several patterns characterize how this user worked with Claude:

**Goal-oriented prompting.** The user consistently provided concrete, domain-informed goals rather than implementation instructions. "Add efficient record counting," "prepare for crates.io publication," "improve test coverage" — Claude determined the implementation approach while the user set the requirements.

**Hands-on integration.** The user didn't delegate everything. Direct commits appear throughout the history — manual `cargo fmt` runs, version bumps, GitHub web UI file uploads, and README edits. The user maintained active ownership of the codebase while using Claude for acceleration.

**Rapid iteration.** Multiple PRs in a single session (PRs #8-15 all on November 8) shows the user was reviewing and merging Claude's work quickly and directing follow-up tasks immediately. The sequential PR structure for test coverage (phases 1-4) suggests the user understood Claude's outputs well enough to validate each phase before proceeding.

**Domain expertise driving decisions.** The most important product decisions — what OData features to support, how to handle the `actris_ref` server quirks, the replication endpoint design, what examples to include — consistently originated with the user. Claude contributed implementation quality, documentation completeness, and test coverage.

---

## Project Outcome

In approximately 3 weeks (Oct 20 – Nov 8, 2025), the project went from an initial Rust application prototype to a published crates.io library (`reso-client = "0.2.1"`) with:

- 189 passing tests (unit, integration, and doc tests)
- ~100% line coverage via Codecov
- Comprehensive rustdoc documentation on all public APIs
- A CI/CD pipeline (GitHub Actions)
- 13+ runnable examples
- 1,500+ line USAGE.md reference guide
- LLM-friendly documentation (`llms.txt`, `CLAUDE.md`)

The commit and PR history shows a clear division: the user supplied domain knowledge, product direction, and code review; Claude supplied implementation velocity, testing rigor, and documentation completeness.
