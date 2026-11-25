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
cargo run -p reso-ingest -- media --config servers.toml --out data/ --limit 1000
cargo run -p reso-ingest -- discover --config servers.toml --server mls-one
cargo run -p reso-ingest -- status --out data/
```

- **`sync`** pulls each resource the server's `$metadata` lists (after the config's `include`/`exclude` filters),
  continuing from the saved checkpoint; the first run is the full load. `--server` and `--resource` narrow it (each can
  be repeated). `--deletes` also writes delete events. `--start-over` drops the selected checkpoints first.
  `--max-file-mb` (default 256) starts a new data file after that size.
- **`media`** downloads the files that the `Media` records from `sync` point at (see [Media files](#media-files)).
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

[server.media]                      # optional; file downloads, separate from the API limits
per_second = 4                      # also per_hour, bytes_per_day, max_retries, *_backoff_ms
timeout_secs = 300                  # per file; default 120
send_token = "same_host"            # never, same_host (default) or always
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

## Media files

`reso-ingest media` turns the `Media` records that `sync` wrote into files:

1. It reads the NDJSON `Media` data files listed in the `manifest.*.json` files in `--out`, skipping the ones it has
   read before, and keeps the newest record per `MediaKey`. Parquet data files are not read (listed as skipped).
2. It downloads each new or changed `MediaURL` into `--media-dir` (default `<out>/media`) as `<server>/<MediaKey>.<ext>`.
   The key is kept as is when it has only letters, digits, `-` and `_`; other characters are percent-encoded. The
   extension comes from the response's `Content-Type` (none when unknown); `MediaType` is not used, as it is often wrong.
3. It writes `media.<run>.json` (per server: downloaded, not modified, deleted, failed with the error, still pending,
   bytes) and prints a summary.

What counts as changed: a new `MediaKey`, or a new `MediaModificationTimestamp` (`ModificationTimestamp` when that is
absent). A new URL alone does **not** count, because signed CDN URLs change on every pull; the newest URL is kept for
the next download. A record with no timestamp at all is fetched again when its URL changes. A re-download sends the
saved `ETag` and `Last-Modified`, so an unchanged file costs a 304 and no transfer. A delete event removes the file.

A file that fails (404, an expired signed URL, a timeout after retries) is listed in the media manifest and stays
pending; the run carries on, exits `1`, and the next run tries it again. `--limit N` caps downloads per server per run;
the rest stay pending. Files are written to `<name>.part` and renamed when complete, so a partial file never sits under
the final name.

The index of what was read and saved is `state/media/<server>.json`. Downloads have their own limits (`[server.media]`),
so they do not use up the API's. The API token goes only to the API's own host by default (`send_token = "same_host"`):
media usually lives on a CDN that does not need it. Downloads run one at a time per server; servers run in parallel.

Not yet checked against a vendor file host
([#32](https://github.com/jeremeybingham/reso_client/issues/32)): signed URLs that expire before the run reaches them
(they fail with 403 until the next `sync` brings a fresh URL), vendor media limits, and real volumes.

## Parquet

Build with the `parquet` feature and pass `--format parquet` for typed columns instead of NDJSON:

```sh
cargo run -p reso-ingest --features parquet -- sync --config servers.toml --format parquet
```

Columns are the four tags, `fetched_at` (timestamp), `event`, `key` (delete events) and a `record` struct with one
field per `$metadata` property: strings, booleans, integers, doubles, `Date32`, UTC millisecond timestamps, lists for
collections. Decimals become doubles; enum, complex and unknown types become text. Properties missing from `$metadata`
are dropped. A Parquet file is readable only once finished, so `sync` keeps one file open across pages and saves a checkpoint only
when the file holding its rows is finished (at `--max-file-mb` or the end of the resource). A run that dies mid-file leaves
a `.part` file and resumes from the last finished file, so those pages are fetched again (at-least-once).

## As a library

The same pieces are public: `Config` (parse the TOML), `run_servers` (one task per server, capped), `sync_resource`
and `pull_resource` (one resource into a `RecordWriter`), `NdjsonWriter` / `ParquetWriter`, `CheckpointStore`,
`RunManifest`, `reconcile_deletes`, `EventFeed`, and for media `sync_media`, `MediaIndex` and `MediaManifest`. See the crate docs: `cargo doc -p reso-ingest --open`.
