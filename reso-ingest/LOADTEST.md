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
| `backfill` | `sync --backfill --resource Property` (#88) with `per_second = --limit-rps` (default 100) and `rate_percent = --rate-percent` (default 50), killed once halfway and resumed, while `sync --resource Member` runs every second against a Member feed that grows by `--members-per-second` (default 50). Not in `all` | every record, at most one page repeated; a `##progress` line with `sub=` and `eta=` per page, ending complete; the resumed run counts only the records left; Property requests within the backfill's share; every Member sync succeeds, using the rest of the limits |
| `deletes` | `sync --deletes` key comparison only (a checkpoint at the last record is seeded, so no records are pulled): a baseline run, a run after every `--gone-every`-th record (default 100, so 1%) left the server, and a run with nothing new. Not in `all` | each gone key exactly once as a delete event, no other key |

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
| 2026-11-25 | 9d0e62f + #125 fix | i9-9900K, WSL2 | faults | 1000000 | 1 | 28.8 | 34768 | 26 | 1039 | 44 | 0 | 0 | ok |
| 2026-11-25 | 00cfe45 (before #87) | i9-9900K, WSL2 | deletes | 1000000 | 3 | 7.1 | 422063 | 363 | 3003 | 0 | 0 | 0 | ok |
| 2026-11-25 | 00cfe45 (before #87) | i9-9900K, WSL2 | deletes | 10000000 | 3 | 78.7 | 380991 | 3360 | 30003 | 0 | 0 | 0 | ok |
| 2026-11-25 | 00cfe45 + #87 | i9-9900K, WSL2 | deletes | 1000000 | 3 | 4.6 | 652295 | 24 | 3003 | 0 | 0 | 0 | ok |
| 2026-11-25 | 00cfe45 + #87 | i9-9900K, WSL2 | deletes | 10000000 | 3 | 48.7 | 616283 | 24 | 30003 | 0 | 0 | 0 | ok |
| 2026-11-26 | 2364279 (#88) | i9-9900K, WSL2 | backfill | 1000000 | 2 | 20.1 | 49676 | 25 | 1004 | 0 | 0 | 0 | ok |

For `backfill` wall time is the backfill's two runs, and requests are Property requests (the Member syncs are counted
apart in the JSON). For `deletes` the records column is keys and records/s is keys per second over the three runs. The fake server set
`TCP_NODELAY` from the `deletes` rows on; earlier rows ran without it (large pages hardly wait, small ones waited
about 40 ms each).

Defaults: page size 1000, NDJSON on a local disk, no `--deletes`, no Parquet, no bucket.

## Findings from the first run

- **Memory does not grow with the pull:** 25 MiB peak for 1M records (759 MB of output in 3 files).
- **A response body cut off halfway is not retried.** The client retries a connection closed before the response
  (10 of them, all recovered), 503s, 429s and slow pages, but a body that ends early is a `Parse error: Failed to read
  response: error decoding response body`, which fails the resource. Each of the 8 cut bodies cost a failed run; the
  next run resumed from the checkpoint and nothing was lost or repeated. Tracked in #125.
  Fixed in #125: the cut body is now retried like a network error, and the same run (8 cut bodies among 44 faults)
  finishes in one sync.
- **Kills repeat less than the bound:** 2000 records repeated over 3 kills (bound 3000, one page per kill). No torn
  lines were seen, but these kills landed between writes; a kill in the middle of writing a page could leave a partial
  last line in an NDJSON file (#86 documents the delivery guarantee).

## Findings from the deletes runs (#87)

- **Before:** the key comparison held both key sets in memory: 363 MiB peak at 1M keys and 3.3 GiB at 10M, with an
  18 MB (1M) or 178 MB (10M) single-JSON key set rewritten on each pass.
- **After:** 24 MiB peak at 1M and at 10M keys (chunks of 100 000 keys spill to sorted run files, merged; the saved
  set is streamed). A pass takes 1.5 s per million keys here, down from 1.9 to 2.9 s; the key set is 12.9 MB per
  million keys. Each gone key came out exactly once.

## Findings from the backfill run (#88)

- **The share holds:** 1004 Property requests in 20.1 s, 49.9 a second against a share of 50 (limit 100, `rate_percent`
  50). Unlimited, the same pull takes about 12 s (`clean`), so at 1M records here the share, not the client, sets the
  pace: about 20 s per million records per 50 requests a second at 1000 records a page.
- **Incremental syncs kept running:** 20 Member syncs ran during the backfill, each in 0.03 s on average (0.04 s at
  most), all successful; 19 reported running on the other 50% (the first started before the backfill held its lease).
- **Progress and resume:** 1000 progress lines, each with `sub=` and `eta=`. The backfill was killed at 500 000 records;
  the resumed run counted 500 000 left and ended at `sub=500000/500000`. No record was missing or repeated (the kill
  came right after a page was saved).
- **Memory** stayed at 25 MiB peak, as for a plain sync.
- A short backfill is not slowed: each process's limiter starts with a full window, so a 50 000-record run (two
  processes, 54 requests) finished in 0.6 s.

## Not covered yet

- Parquet output, the S3 sink, `sync --media` (#89), and several servers at once.
- A real network: the fake server is on loopback, so the numbers are an upper bound for the client side.
