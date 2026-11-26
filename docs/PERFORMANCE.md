# Performance (#162)

How to profile `reso-ingest`, the results so far, and the leads they point at. Rows are added, never replaced, so
later changes can be compared with earlier ones (as in `reso-ingest/LOADTEST.md`).

## How to run it

```sh
scripts/profile.sh clean 100000          # SCENARIO RECORDS [-- extra loadtest options]
```

The script builds the `profiling` Cargo profile (release, with line tables and symbols kept) and runs one
`examples/loadtest.rs` scenario twice:

1. **Timed run.** The `reso-ingest sync` child runs under `/usr/bin/time`, giving wall time, CPU time and peak RSS
   of the sync alone (`time.txt`).
2. **Callgrind run.** The child runs under `valgrind --tool=callgrind --separate-threads=no --dump-instr=no`. This
   counts CPU instructions per function, inclusive (with callees) and self (`annotate-inclusive.txt`,
   `annotate-self.txt`). It runs 30 to 40 times slower and does not see time spent waiting on the server, network or a
   bucket, so wall-time questions use the timed run.

Only the child is wrapped (`LOADTEST_WRAP`); the fake server runs natively in the harness process. Results go to
`target/profile/<date>-<commit>-<scenario>-<records>/`, load-test output to `/dev/shm/reso-profile` (deleted after).
With a wrapper set, the load test's own peak RSS column measures the wrapper, so peak RSS comes from `time.txt`.

`callgrind.out.<pid>` opens in kcachegrind or qcachegrind if a GUI is wanted (not installed here).

## Machine

Intel i9-9900K, WSL2 (Ubuntu 24.04), 14 CPUs and 49 GB visible, rustc 1.100, valgrind 3.22. The fake server shares
the machine.

## Results

Instructions are the sync child's, from callgrind. Top 5 by self cost name the function and its share of instructions.

| date | commit | scenario | records | sink | format | wall s | records/s | CPU s | peak RSS MiB | instructions | top 5 by self cost |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-26 | ac78902 + tooling | clean | 100000 | local (/dev/shm) | NDJSON | 0.78 | 128000 | 0.42 | 24.5 | 4.61 G | serde_json `format_escaped_str` 11.6%, `_int_free` 7.6%, `memcpy` 5.6%, `malloc` 5.1%, `malloc_consolidate` 4.0% |
| 2026-11-26 | ed70e57 (#165, #166) | clean | 100000 | local (/dev/shm) | NDJSON | 0.72 | 139000 | 0.34 | 22.8 | 3.52 G | serde_json `format_escaped_str` 15.2%, `_int_malloc` 9.6%, `memcpy` 6.6%, `_int_free` 5.6%, `from_utf8` 4.3% |

## Baseline findings (2026-11-26, clean, 100k records, NDJSON, local)

The sync spends 0.42 s of CPU in 0.78 s of wall time, so on this fake server about half the wall time is waiting on the
server. With a real vendor the network will weigh more (not measured until #32). Where the instructions go, inclusive:

| Share | Where |
|---|---|
| 32% | Parsing each page body into `serde_json::Value` (`ResoClient::parse_json_response`) |
| 31% | `CheckingWriter::write_batch`: 25% in `NdjsonWriter` (17% serializing records again), about 6% turning each key into a `String` for the repeated-key check |
| 22% | Dropping `Value` trees |
| 11% | `Page::from_response` cloning the whole `value` array out of the body (`.cloned()`) |
| about 35% | `malloc`/`free` and friends, spread over all of the above |

## After leads 1 and 2 (2026-11-26, #165, #166)

Same scenario, one run each. Instructions fell from 4.61 G to 3.52 G (24% fewer) and CPU time from 0.42 s to 0.34 s;
more than the 17% the two leads showed, because the copied array was also dropped. `Page::from_response` no longer
shows in the profile, and the repeated-key check fell from about 6% to about 2% inclusive (hashing the borrowed key and
the set itself). Parsing the page body (1.50 G instructions, now 43%) and writing NDJSON (1.17 G, 33%, of which 0.97 G
serializing) are unchanged in absolute terms and are now the two big items: leads 3 and 4. Wall time moved only from
0.78 s to 0.72 s, since about half of it is waiting on the fake server; a single run's wall time is noisy.

## Leads

Ranked by expected saving for the cost of the change. Sub-issues of #162 are listed in the issue as they open.

1. **Done (#165).** `Page::from_response` cloned the record array (`src/paging.rs`); it now moves the array out of
   the body. Saved more than the 11% expected (see above).
2. **Done (#166).** The repeated-key check allocated a `String` per record (`reso-ingest/src/checks.rs`); it now
   hashes the borrowed string key. From about 6% to about 2%.
3. **Allocator.** About a third of all instructions are in glibc `malloc`/`free`. A faster global allocator in the
   `reso-ingest` binary (mimalloc or jemalloc) is a new dependency, the owner's call; measure it first.
4. **Parse and serialize round trip for NDJSON.** Parsing into `Value` (32%) and serializing again (17%) could become a
   pass-through of each record's raw bytes (`serde_json` `raw_value`). A larger change: the writers, checks and
   checkpoint code read fields from `Value`. Parquet needs the parsed values anyway.
5. Not measured yet: Parquet conversion, bucket uploads (S3, GCS), many small incremental runs, `$metadata` and
   startup. Those need the bucket targets and per-phase timings listed in #162.
