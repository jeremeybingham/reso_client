# Stability and the 1.0 plan

This page says what users of `reso-client` and `reso-ingest` can rely on now, what 1.0 will promise, and what has to
happen first. Releasing and publishing stay the owner's call.

## Now (0.x)

- `reso-client` follows Cargo's semver rules for 0.x: a **minor** bump (0.3 → 0.4) may break the API, a **patch** bump
  may not. Every breaking change is marked "breaking" in [`CHANGELOG.md`](../CHANGELOG.md).
- `reso-ingest` is unpublished (version 0.0.1). Its command-line options, configuration file, output layout and
  metrics can change in any commit; changes are recorded in the changelog.
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

**`reso-ingest`** (when published; the owner decides whether it gets its own 1.0):

- The configuration file: a 1.x configuration loads in every later 1.x; a setting is deprecated with a warning for at
  least one minor release before it is removed in 2.0.
- The output: NDJSON line fields and Parquet columns (added columns are not breaking), file names, manifests, the state
  layout (checkpoints, key sets, media index) and the access log. A later 1.x reads the state of an earlier one.
- Exit codes, metric names and labels, the `serve` endpoints and the webhook summary fields (new fields are not
  breaking).
- Not covered: log text, progress lines, the library API of `reso-ingest` (it is for the binary; it may stay 0.x).

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

1. The #76 pass against at least one vendor server (#32), and the compatibility table
   ([`COMPATIBILITY.md`](COMPATIBILITY.md)) filled in for it. The largest likely API change is a replication mode
   without the two-field `$orderby` (#76, section 3).
2. API review ([#157](https://github.com/jeremeybingham/reso_client/issues/157)): `cargo semver-checks` shows that
   `ClientConfig`, `RateLimit`, `ClientStats` and `EnumMember` can be built with struct literals, so every new field
   breaks callers (0.4.0 already breaks them that way). Make such structs `#[non_exhaustive]` with constructors or
   builders, check every public type the same way, and decide which re-exports stay.
3. Release 0.4.0, then at least one more minor release used in production with no breaking change needed, before
   tagging 1.0.
4. Release artifacts: SBOM and signed tags ([#158](https://github.com/jeremeybingham/reso_client/issues/158)).

## Release checklist

1. `git fetch origin`; on an up-to-date `main`.
2. Bump `version` in `Cargo.toml` (and `reso-ingest/Cargo.toml` if it is released), move the changelog's "Unreleased"
   section under the new version.
3. `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
   `cargo test --workspace --all-features`, `cargo doc --no-deps --all-features`.
4. `cargo semver-checks -p reso-client` passes for the new version; `cargo deny --workspace --all-features check`
   passes; the MSRV job is green.
5. Tag `vX.Y.Z` (signed once #158 is done) and push the tag; `cargo publish -p reso-client`.
