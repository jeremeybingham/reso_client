# reso-ingest

Pulls every resource from one or more [RESO Web API](https://www.reso.org/reso-web-api/) servers into files, keeps
them current with incremental runs, and records deletes. Built on [`reso-client`](../README.md).

Status: in this repository only (`publish = false`, version 0.0.1). Checked end to end against RESO's reference server
run locally; not yet run against a vendor server
([#32](https://github.com/jeremeybingham/reso_client/issues/32)).

## Commands

```sh
cargo run -p reso-ingest -- sync --config servers.toml --out data/
cargo run -p reso-ingest -- sync --config servers.toml --server mls-one --resource Property --deletes
cargo run -p reso-ingest -- discover --config servers.toml --server mls-one
cargo run -p reso-ingest -- status --out data/
```

- **`sync`** pulls each resource the server's `$metadata` lists (after the config's `include`/`exclude` filters),
  continuing from the saved checkpoint; the first run is the full load. `--server` and `--resource` narrow it (each can
  be repeated). `--deletes` also writes delete events. `--start-over` drops the selected checkpoints first.
  `--max-file-mb` (default 256) starts a new data file after that size.
- **`discover`** prints each server's `$metadata` model as JSON: resources, keys, properties, enums.
- **`status`** shows the saved checkpoints and the last run's manifest.

Progress lines (`##progress step=N/T label=server/resource`) go to stderr and the summary to stdout. Exit code `0` on
success, `1` if a server or resource failed (the summary names it), `2` for bad usage or configuration. `--help` on
any command lists every option.

## Configuration

One TOML file lists the servers; [servers.example.toml](servers.example.toml) shows every setting. Secrets are never
written in it: `token_env`, `client_id_env` and `client_secret_env` name the environment variables that hold them, and
a file with a literal secret is rejected.

```toml
max_concurrent_servers = 2          # default 4

[[server]]
name = "mls-one"
base_url = "https://api.mls-one.example/odata"
include = ["Property", "Member", "Office"]   # empty or absent: every resource
exclude = ["Media"]

[server.auth]
type = "client_credentials"         # or "bearer" with token_env
token_url = "https://auth.mls-one.example/oauth/token"
client_id_env = "MLS_ONE_CLIENT_ID"
client_secret_env = "MLS_ONE_CLIENT_SECRET"

[server.limits]                     # optional; default 5 retries, no rate limit
per_second = 2
```

Each server runs as its own task: one server failing does not stop the others.

## What it writes

In `--out` (default `reso-out`):

- `<server>.<resource>.<run>.<NNNN>.ndjson`: one line per record, the record untouched inside four source tags:

  ```json
  {"source":"mls-one","resource":"Property","fetched_at":"2026-11-24T12:00:00Z","run_id":"...","record":{...}}
  ```

  A delete event has the same tags, then `"event":"delete"` and the record's `key` instead of `record`.
- `manifest.<run>.json`: counts, files, checkpoints and errors for the run.
- `state/` (or `--state`): one JSON checkpoint per server and resource, saved after each batch is written, so a rerun
  after a crash continues from there. Delivery is at least once: a record can appear twice, never zero times.

## How it syncs

- Replication is RESO-standard: filter on `ModificationTimestamp`, order by timestamp then key, follow
  `@odata.nextLink`, resume strictly after the last (timestamp, key). Resources without a single-field key or a
  `ModificationTimestamp` are skipped (an error if named with `--resource`).
- Deletes (`--deletes`): when `$metadata` declares an `EntityEvent` resource, its feed is read from the last sequence
  number and each named record is looked up; otherwise every key is pulled and compared with the previous run's (the
  first run is the baseline and reports no deletes).

## Parquet

Build with the `parquet` feature and pass `--format parquet` for typed columns instead of NDJSON:

```sh
cargo run -p reso-ingest --features parquet -- sync --config servers.toml --format parquet
```

Columns are the four tags, `fetched_at` (timestamp), `event`, `key` (delete events) and a `record` struct with one
field per `$metadata` property: strings, booleans, integers, doubles, `Date32`, UTC millisecond timestamps, lists for
collections. Decimals become doubles; enum, complex and unknown types become text. Properties missing from `$metadata`
are dropped. A Parquet file is readable only once finished, so `sync` finishes one file per page.

## As a library

The same pieces are public: `Config` (parse the TOML), `run_servers` (one task per server, capped), `sync_resource`
and `pull_resource` (one resource into a `RecordWriter`), `NdjsonWriter` / `ParquetWriter`, `CheckpointStore`,
`RunManifest`, `reconcile_deletes` and `EventFeed`. See the crate docs: `cargo doc -p reso-ingest --open`.
