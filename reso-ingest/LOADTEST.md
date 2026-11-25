# Load test (#90)

`examples/loadtest.rs` runs `reso-ingest sync` against a fake RESO server with many records and checks the output. It
is not part of `cargo test`.

```sh
cargo run --release -p reso-ingest --example loadtest -- --records 1000000
```

The fake server runs in the harness process on `127.0.0.1`. Its data is invented: `Property` records with ten fields
(about 750 bytes per output line with the default `--remarks-bytes 400`), five records per `ModificationTimestamp` so
the keyset has ties to break, `@odata.nextLink` paging, `$count`, and a `$metadata` document. The sync runs in a child
process (the same binary, `loadtest child sync ...`), so its peak memory (`VmHWM`, Linux only) is its own and it can be
killed. After each scenario the harness reads every output line and counts missing keys, repeated keys and lines that
are not a whole record.

| Scenario | What happens | Passes when |
|---|---|---|
| `clean` | One full sync, no faults | every record exactly once |
| `faults` | `--fault-percent` (default 5) of `/Property` requests get a fault, split evenly: 503, 429 with `Retry-After: 1`, a 300 ms slow page, the connection closed before the response, or the body cut off halfway. A failed sync is run again, as a scheduler would, up to `--max-runs` | no record missing, none repeated |
| `kill` | The sync gets SIGKILL `--kills` times (default 3) at even points of the pull; each time a new sync resumes | no record missing; at most one page repeated per kill |

`--server-rps N` makes the server answer 429 above N requests a second. The configuration it writes uses
`max_retries = 6`, `initial_backoff_ms = 100`, `max_backoff_ms = 2000`. Output goes to `target/loadtest/<scenario>`
and is deleted after the check unless `--keep` (about 760 MB per scenario at 1M records). Results print as JSON on
stderr and as table rows on stdout; add the rows below.

After a kill the harness deletes the feed's lease file. Without that the next sync skips the feed until the lease runs
out (up to 5 minutes, see "What it writes" in the README), which is the documented behaviour, not what is measured here.

## Results

Rows are added, never replaced, so later changes can be compared. Wall time includes the fake server, which shares the
machine (and in `faults`, the waits for `Retry-After` and backoff).

| date | commit | machine | scenario | records | runs | wall s | records/s | peak RSS MiB | requests | faults | repeats | torn lines | result |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-25 | 25b8145 | i9-9900K, WSL2 | clean | 1000000 | 1 | 11.6 | 85935 | 25 | 1002 | 0 | 0 | 0 | ok |
| 2026-11-25 | 25b8145 | i9-9900K, WSL2 | faults | 1000000 | 9 | 27.8 | 36013 | 24 | 1038 | 44 | 0 | 0 | ok |
| 2026-11-25 | 25b8145 | i9-9900K, WSL2 | kill | 1000000 | 4 | 11.4 | 87638 | 25 | 1004 | 0 | 2000 | 0 | ok |

Defaults: page size 1000, NDJSON on a local disk, no `--deletes`, no Parquet, no bucket.

## Findings from the first run

- **Memory does not grow with the pull:** 25 MiB peak for 1M records (759 MB of output in 3 files).
- **A response body cut off halfway is not retried.** The client retries a connection closed before the response
  (10 of them, all recovered), 503s, 429s and slow pages, but a body that ends early is a `Parse error: Failed to read
  response: error decoding response body`, which fails the resource. Each of the 8 cut bodies cost a failed run; the
  next run resumed from the checkpoint and nothing was lost or repeated. Tracked in #125.
- **Kills repeat less than the bound:** 2000 records repeated over 3 kills (bound 3000, one page per kill). No torn
  lines were seen, but these kills landed between writes; a kill in the middle of writing a page could leave a partial
  last line in an NDJSON file (#86 documents the delivery guarantee).

## Not covered yet

- `--deletes` at a million keys (#87), Parquet output, the S3 sink, `sync --media` (#89), and several servers at once.
- A real network: the fake server is on loopback, so the numbers are an upper bound for the client side.
