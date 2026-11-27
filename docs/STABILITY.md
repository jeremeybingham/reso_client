# Stability and the 1.0 plan

This page says what users of `reso-client` and `reso-ingest` can rely on now, what 1.0 will promise, and what has to
happen first. Releasing and publishing stay the owner's call.

## Now (0.x)

- `reso-client` follows Cargo's semver rules for 0.x: a **minor** bump (0.3 → 0.4) may break the API, a **patch** bump
  may not. Every breaking change is marked "breaking" in [`CHANGELOG.md`](../CHANGELOG.md).
- `reso-ingest` is first published at 0.4.0, with the same version as `reso-client` (#178); the two share version
  numbers from then on. It ships marked as not yet run against a vendor server; the #76 pass may change it in 0.5.0.
  Before 1.0 its command-line options, configuration file, output layout and metrics may change in a minor release;
  changes are recorded in the changelog, and renamed options and settings follow the deprecation policy below.
- `reso-ingest`'s **library API** is not covered by `cargo semver-checks` before 1.0 and may change in any minor
  release without a deprecation; it exists for the binary. Its command line and configuration file are what users
  rely on, and they follow the deprecation policy.
- `Cargo.lock` is committed, so `cargo install --locked reso-ingest` builds with the dependency versions CI tested.
- CI runs `cargo semver-checks` on `reso-client` against the last release on crates.io. While the version in
  `Cargo.toml` is still the released one, the job reports breaking changes without failing (they are expected before
  the next minor release); once `Cargo.toml` is bumped for a release, it fails on any change that version does not allow.

## What 1.0 will promise

**`reso-client`** (the published library):

- The public API in rustdoc: `ResoClient`, `ClientConfig`, `Auth`, `QueryBuilder`/`Query`, `StandardReplication` and
  `Checkpoint`, `Metadata`, `Downloader`, `RetryPolicy`/`RateLimit`, `NetworkOptions`, `ResoError`. No breaking change
  without a 2.0.
- `ResoError` and other enums stay `#[non_exhaustive]`: new variants are not breaking.
- Records stay raw `serde_json::Value` (see the rules in [`AGENTS.md`](../AGENTS.md)).
- `Checkpoint`'s serialized form: a checkpoint written by any 1.x reads in every later 1.x.
- Not covered: log message text, `Debug` output, and the exact URLs built (servers see the same query; spacing and
  parameter order may change).

**`reso-ingest`** (published from 0.4.0; whether it reaches 1.0 together with `reso-client` is decided then):

- The configuration file: a 1.x configuration loads in every later 1.x; a setting is deprecated with a warning for at
  least one minor release before it is removed in 2.0.
- The output: NDJSON line fields and Parquet columns (added columns are not breaking), file names, manifests, the state
  layout (checkpoints, key sets, media index) and the access log. A later 1.x reads the state of an earlier one.
- Exit codes, metric names and labels, the `serve` endpoints and the webhook summary fields (new fields are not
  breaking).
- Not covered: log text, progress lines, the library API of `reso-ingest` (it is for the binary; it may stay 0.x).

## The 1.0 surface of `reso-client`

Reviewed in [#157](https://github.com/jeremeybingham/reso-rs/issues/157). Every type below is reachable from the
crate root and from its module (`reso_client::Page` and `reso_client::paging::Page` are the same item).

| Module | Public items | Can grow without a break |
|---|---|---|
| `client` | `ResoClient`, `ClientConfig` | `ClientConfig` is `#[non_exhaustive]` (build with `new`, `with_auth`, `from_env`, `with_*`) |
| `auth` | `Auth`, `ClientCredentials`, `CredentialTransport` | all three `#[non_exhaustive]` |
| `queries` | `QueryBuilder`, `Query`, `ReplicationQueryBuilder`, `ReplicationQuery` | private fields; built by the builders |
| `paging` | `Page`, `parse_link_header` | `Page` `#[non_exhaustive]` |
| `replication` | `ReplicationResponse` (Bridge vendor mode) | `#[non_exhaustive]`; `ReplicationResponse::new` |
| `standard_replication` | `StandardReplication`, `ReplicationRun`, `Checkpoint` | `StandardReplication` and `ReplicationRun` have private fields; `Checkpoint` `#[non_exhaustive]` (`new`, `at`) |
| `metadata` | `Metadata`, `EntitySet`, `EntityType`, `Property`, `NavigationProperty`, `EnumType`, `EnumMember` | all `#[non_exhaustive]`; built by `Metadata::parse` |
| `download` | `Downloader`, `DownloadConfig`, `DownloadTarget` (trait), `SendToken`, `Validators`, `SavedFile`, `DownloadOutcome` | all but `DownloadOutcome` `#[non_exhaustive]`; `DownloadOutcome` stays exhaustive so callers can match both cases |
| `network` | `NetworkOptions`, `DEFAULT_USER_AGENT`, `redact_proxy` | `NetworkOptions` `#[non_exhaustive]` (`default`, `with_*`) |
| `retry` | `RetryPolicy`, `RateLimit`, `RateUsage`, `WindowUsage`, `ClientStats`, `count_requests` | all structs `#[non_exhaustive]` |
| `error` | `ResoError`, `Result` | `ResoError` `#[non_exhaustive]` (new variants); the fields of its struct variants (`message`, `status_code`) are fixed for 1.x |
| root | `JsonValue` (= `serde_json::Value`) | kept: records are `serde_json::Value`, so serde_json 1.x is part of the API either way |

Rules for adding to it: a new public struct with public fields that may grow is `#[non_exhaustive]` and comes with a
constructor or builder; a new public enum is `#[non_exhaustive]` unless callers must match every case; anything else
stays `pub(crate)`. `DownloadTarget` gets no new required methods in 1.x (a new method needs a default body).

## MSRV policy

- `reso-client`: Rust **1.87**, the `rust-version` in `Cargo.toml`. CI checks it with that toolchain, resolving
  dependencies with `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback` (the newest versions that still support it).
  Raising the MSRV is allowed in a minor release, is listed in the changelog, and keeps at least the last six months of
  stable Rust (about four releases) supported.
- `reso-ingest`: the `rust-version` in its `Cargo.toml` (1.89, set by its dependencies: `crc-fast` under
  `object_store`; arrow and parquet need 1.88). As a binary built from source or used as the container image, it follows its
  dependencies; CI checks the same number.
- The container image is built with the toolchain named in the `Dockerfile`.

## Deprecation policy

- Deprecate before removing: mark the item `#[deprecated(since = "x.y.z", note = "use … instead")]`, keep it working,
  and say so in the changelog's "Deprecated" section. Remove it in the next breaking release at the earliest (before
  1.0: the next minor; after: 2.0).
- For `reso-ingest` options and settings: keep the old name working with a `warn` log line naming the new one.

## Before 1.0 (to do)

The release plan is the tracker [#179](https://github.com/jeremeybingham/reso-rs/issues/179) (owner, 2026-11-26):

1. ~~API review ([#157](https://github.com/jeremeybingham/reso-rs/issues/157))~~: done; the result is
   [the 1.0 surface](#the-10-surface-of-reso-client) below. It ships in 0.4.0.
2. **0.4.0** (milestone `0.4.0`): both crates, released before any vendor server is available and marked as not yet
   checked against one. `reso-client` 0.4.0 and the first `reso-ingest` publish (#178), after the pre-release
   clean-ups (#177) and #162 lead 6. Unsigned tag; no container image on a registry.
3. **0.5.0** (milestone `0.5.0`): the #76 pass against at least one vendor server (#32), its fixes, and the
   compatibility table ([`COMPATIBILITY.md`](COMPATIBILITY.md)) filled in for it; realistic load fixtures (#163);
   release artifacts: SBOM, signed tags and a signed container image
   ([#158](https://github.com/jeremeybingham/reso-rs/issues/158)). The largest likely API change is a replication
   mode without the two-field `$orderby` (#76, section 3).
4. After 0.5.0, at least one more minor release used in production with no breaking change needed, before tagging
   1.0.

## Release checklist

1. `git fetch origin`; on an up-to-date `main`.
2. Bump `version` in `Cargo.toml` and in `reso-ingest/Cargo.toml`, together with its `reso-client` dependency's
   `version` (the crates share version numbers), and move the changelog's "Unreleased" section under the new version.
3. `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
   `cargo test --workspace --all-features`, `cargo doc --no-deps --all-features`.
4. `cargo semver-checks -p reso-client` passes for the new version; `cargo deny --workspace --all-features check`
   passes; the MSRV job is green.
5. `cargo package --workspace` builds both packages from their `.crate` files (it resolves `reso-client` from the
   workspace, so it works before the client is on crates.io); check `cargo package -p reso-ingest --list` ships no
   test data, `local-data/` or secrets.
6. Tag `vX.Y.Z` (signed once #158 is done) and push the tag.
7. Publish in order: `cargo publish -p reso-client` first; once that version shows on crates.io,
   `cargo publish --dry-run -p reso-ingest`, then `cargo publish -p reso-ingest`.
