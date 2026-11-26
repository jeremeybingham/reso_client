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
| 2026-11-26 | 6a04ded (rerun) | clean | 100000 | local (/dev/shm) | NDJSON | 0.73 | 137000 | 0.36 | 22.9 | 3.50 G | (same shape as ed70e57) |
| 2026-11-26 | 6a04ded + mimalloc (lead 3) | clean | 100000 | local (/dev/shm) | NDJSON | 0.53 | 189000 | 0.27 | 55.2 | 2.66 G | serde_json `format_escaped_str` 20.2%, `memcpy` 8.8%, `from_utf8` 5.7%, `memcmp` 5.4%, serde_json `skip_to_escape` 4.5% |
| 2026-11-26 | 5fbac1e (rerun, mimalloc default) | clean | 100000 | local (/dev/shm) | NDJSON | 0.52 | 192000 | 0.24 | 40.7 | 2.66 G | serde_json `format_escaped_str` 20.2%, `memcpy` 8.8%, `from_utf8` 5.7%, `memcmp` 5.4%, serde_json `skip_to_escape` 4.5% |
| 2026-11-26 | 5fbac1e + raw NDJSON (lead 4) | clean | 100000 | local (/dev/shm) | NDJSON | 0.50 | 200000 | 0.23 | 72.9 | 2.19 G | serde_json `skip_to_escape` 11.0%, `memcpy` 8.5%, `memcmp` 6.5%, `skip_to_escape` (slice) 3.8%, `memchr` 3.2% |
| 2026-11-26 | a56e62e, no `Value` (lead 4) | clean | 100000 | local (/dev/shm) | NDJSON | 0.43 | 233000 | 0.15 | 45.1 | 1.47 G | serde_json `skip_to_escape` 17.2%, `memcpy` 10.7%, `skip_to_escape` (slice) 6.0%, `ignore_str` 5.8%, `memchr` 4.8% |
| 2026-11-26 | 8b07928 | clean | 100000 | local (/dev/shm) | Parquet | 0.71 | 141000 | 0.43 | 121.5 | 4.56 G | `memcpy` 9.4%, serde_json `skip_to_escape` 8.2%, `memcmp` 7.5%, `skip_to_escape` (slice) 2.9%, serde_json `parse_str` 2.5% |

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

## Lead 3: mimalloc (2026-11-26)

The `mimalloc` feature makes mimalloc the global allocator of the `reso-ingest` binary (and of the load test, so the
sync child runs it). It is on by default (owner's decision, 2026-11-26), also in the container image; for the system
allocator run `PROFILE_NO_DEFAULT=1 scripts/profile.sh clean 100000`. Timed runs, five of each,
interleaved (100k) and one of each (1M), sync child only:

| records | allocator | wall s | child CPU s (user + sys) | peak RSS MiB |
|---|---|---|---|---|
| 100000 | glibc malloc | 0.69 to 0.71 | 0.33 to 0.34 | 22.9 to 24.0 |
| 100000 | mimalloc | 0.50 to 0.52 | 0.24 to 0.26 | 47.3 to 65.4 |
| 1000000 | glibc malloc | 7.01 | 3.48 | 24.3 |
| 1000000 | mimalloc | 5.06 | 2.44 | 53.3 |

- CPU falls by about 25% at 100k and 30% at 1M; instructions by 24% (3.50 G to 2.66 G). Allocator code fell from 1.06 G
  instructions (30% of the glibc run: `malloc`, `free`, `malloc_consolidate` and friends) to 0.24 G (9% of the
  mimalloc run), which is nearly all of the 0.84 G saved.
- Peak RSS roughly doubles, to about 50 to 65 MiB, and stays flat from 100k to 1M (mimalloc keeps freed pages per
  thread for reuse). Small next to a container's usual limit, but it is a cost.
- Wall time falls more than CPU here because the fake server runs in the load-test process, which also got mimalloc.
  Read the CPU column, not wall time, for the sync's own saving.
- Not measured: the container image. It links musl, whose malloc is known to be much slower than glibc's under many
  small allocations, so the saving there is likely larger. Building the musl binary needs the musl target and
  `musl-gcc` (not installed here); the image build is the place to measure it.
- Cost: a new dependency (`mimalloc` and `libmimalloc-sys`, MIT), and C code compiled by `cc` and linked statically,
  like `ring` and `aws-lc` already are. cargo-deny passes; MSRV 1.89 check passes.

Rows above dated before this change used the system allocator; rows after it use mimalloc unless marked.

## Lead 4: raw record text for NDJSON (2026-11-26)

`sync` now takes each page from `ReplicationRun::next_batch_raw`, which returns the parsed records and each record's
JSON text as the server sent it (`RawRecords`: spans into the page body, read with serde_json's `raw_value`, one scan
of the `value` array). `NdjsonWriter::write_raw_batch` serializes the four tags once per batch and copies each
record's text after them, instead of serializing every parsed record again. The records were still parsed (until the
parse half below): the replication order check, the checkpoint, the repeated-key check, flag deletes, media and
Parquet read them.

Same scenario as lead 3 (mimalloc on), main (5fbac1e) and the change built with the `profiling` profile, runs
interleaved, sync child only:

| records | build | runs | wall s | child CPU s (user + sys) | peak RSS MiB |
|---|---|---|---|---|---|
| 100000 | main | 5 | 0.51 to 0.53 | 0.24 to 0.29 | 49 to 59 |
| 100000 | raw NDJSON | 5 | 0.48 to 0.50 | 0.22 to 0.23 | 45 to 73 |
| 1000000 | main | 3 | 5.09 to 5.11 | 2.46 to 2.47 | 55 to 65 |
| 1000000 | raw NDJSON | 3 | 4.76 to 4.85 | 2.13 to 2.18 | 47 to 89 |

- Instructions fell from 2.66 G to 2.19 G (18%); CPU about 8% at 100k and 13% at 1M. Writing NDJSON fell from 0.96 G
  instructions (36%, of which 0.86 G serializing) to 0.20 G (9%). Parsing is unchanged at about 1.5 G and is now 68%
  of the instructions.
- A first version scanned the body three times (the whole `value` member, then its elements, then each record) and
  checked each record for line breaks byte by byte: 3.07 G, more than before. One scan of the array and a `memchr`
  for `\n` fixed both.
- Peak RSS varies a lot between runs under mimalloc (45 to 89 MiB); no clear change. The page text is freed right after
  the batch is written.
- Output changes: `record` keeps the server's member order (serde_json without `preserve_order` sorted them) and number
  text (`1.50`, integers past 64 bits were turned into floats). Line breaks between tokens are dropped so a record stays
  on one line. Parquet output is unchanged.
- Parquet syncs also use `next_batch_raw` and ignore the text; parsing did not get dearer, so they pay about nothing.

### Parse half: no `Value` per record (2026-11-26)

An NDJSON sync no longer builds a `serde_json::Value` for any record. `next_batch_raw` returns only `RawRecords` (the
page body and each record's span); the readers that need a field read it from the record's text with
`record_fields`, a deserializer that borrows the named top-level members as `&RawValue` and skips the rest:

- the order check and checkpoint read the timestamp and key; only an escaped string, or a key whose timestamp ties with
  the checkpoint's, is parsed;
- the repeated-key check reuses the key spans the replication already found (`RawRecords::field`), so it does not
  scan the records again;
- flag deletes read the flag and key, only when configured.

Each record is now scanned twice (the body split and the timestamp and key read) instead of being parsed into a tree
and dropped. Parquet and `sync --media` still parse (`RawRecords::values`). A first version scanned a third time for
the repeated-key check (1.93 G instructions, `record_fields` 26% of them); keeping the key spans removed it.

Same scenario, main (e3832ad, with the serialize half) and the change (a56e62e), `profiling` profile, runs
interleaved, sync child only:

| records | build | runs | wall s | child CPU s (user + sys) | peak RSS MiB |
|---|---|---|---|---|---|
| 100000 | main | 5 | 0.81 to 0.93 | 0.24 to 0.30 | 47 to 77 |
| 100000 | no `Value` | 5 | 0.72 to 0.80 | 0.16 to 0.20 | 47 to 49 |
| 1000000 | main | 3 | 8.32 to 8.47 | 2.53 to 2.59 | 45 to 49 |
| 1000000 | no `Value` | 3 | 7.93 to 8.12 | 1.92 to 1.93 | 45 to 51 |

- Instructions fell from 2.19 G to 1.47 G (33%); child CPU about 30% at 100k and 25% at 1M. Since the baseline
  (4.61 G, 0.42 s) that is 68% fewer instructions and about 60% less CPU.
- Wall times in this set are higher than in the tables above for both builds (8.3 s against 5.1 s for main at 1M):
  the machine was busier. The runs were interleaved, so compare the two builds within this table only.
- Peak RSS stays in mimalloc's range; no `Value` trees means less to hold per page, but the page text dominates.
- Left in the profile: serde_json's string scanning (`skip_to_escape`, `ignore_str`, about 30%) in the two scans,
  copying text (`memcpy`, 11%) and the line-break check (`memchr`, 5%). Fewer scans would need a deserializer that
  splits records and reads their timestamp and key in one pass; small gain for a hand-rolled JSON reader, not planned.
- Output is unchanged from the serialize half. Unconfirmed against a real server (#32).

## Parquet, buckets and quiet runs (2026-11-26)

The load test can now pass `sync` options (`--sync-args`): `--format parquet`, and an S3 or GCS bucket, with the
output read back from the bucket for the check; and an `incremental` scenario times syncs that find nothing new
(`reso-ingest/LOADTEST.md`). Same machine, main at 8b07928, `profiling` profile, mimalloc, sync child only.

### Parquet

| records | format | wall s | child CPU s (user + sys) | peak RSS MiB | output |
|---|---|---|---|---|---|
| 100000 | NDJSON | 0.41 to 0.43 | 0.15 to 0.16 | 49 to 51 | 76 MB, 1 file |
| 100000 | Parquet | 0.69 to 0.71 | 0.42 to 0.44 | 121 to 149 | 2.9 MB, 1 file |
| 1000000 | NDJSON | 4.07 | 1.45 | 57 | 759 MB, 3 files |
| 1000000 | Parquet | 6.59 | 3.89 | 133 | 20 MB, 1 file |

- A Parquet sync uses about 2.7 times the CPU of an NDJSON one (4.56 G instructions against 1.47 G at 100k). Where
  they go, inclusive: 24% parsing each record into a `Value` (`RawRecords::values`; NDJSON skips this since lead 4),
  10% serializing the rows into arrow-json's tape (`Row::serialize`), 10% turning the tape into columns
  (`Decoder::flush`), and about 23% Parquet encoding (dictionary and plain byte arrays, Snappy). The rest is the copy
  of each record in `record_value`, dropping the `Value` trees and the two scans the sync shares with NDJSON (not
  broken down further).
- Peak RSS is about 2.5 times NDJSON's: a row group is held until it is written.
- The invented records repeat the same remarks, so Parquet compresses them 38 times; real records will compress far
  less (#163 would make the data realistic). The NDJSON size is realistic for about 750-byte records.

### Buckets

100000 records, two runs of each, AWS S3 `us-east-1` and GCS `us-east1` test buckets, from this machine over a home
connection. Every record was read back from the bucket and checked.

| sink | format | wall s | child CPU s | peak RSS MiB |
|---|---|---|---|---|
| local (/dev/shm) | NDJSON | 0.41 to 0.43 | 0.15 to 0.16 | 49 to 51 |
| AWS S3 | NDJSON | 18.98 to 19.20 | 0.58 | 127 to 139 |
| GCS | NDJSON | 18.84 to 19.27 | 0.41 to 0.45 | 146 to 154 |
| local (/dev/shm) | Parquet | 0.69 to 0.70 | 0.42 to 0.44 | 123 to 149 |
| AWS S3 | Parquet | 3.18 to 3.20 | 0.54 to 0.55 | 148 to 161 |
| GCS | Parquet | 4.12 to 4.26 | 0.54 | 163 to 167 |

- The upload is all of the difference: 76 MB in about 18.5 s is about 4 MB/s, this connection's upload speed, not
  the code's. The 2.9 MB Parquet runs take 2.5 to 3.5 s more than local, mostly the per-run round trips below. A
  cloud VM near the bucket should upload much faster; not measured.
- The pull waits while a file goes up (`FileSink::put_file` is synchronous, through `block_in_place`). With one file
  per run that costs nothing that could be overlapped; at 1M records (three 256 MB NDJSON files) each rotation stalls
  the pull for its upload. Overlapping them would save up to the shorter of pull and upload time, which matters only
  when both are long; not done.
- Peak RSS with a bucket is about 80 to 100 MiB above local for NDJSON, likely the multipart upload's 16 MiB parts in
  flight (not checked).
- NDJSON repeats every field name on every line, so compressed NDJSON (`.ndjson.gz` or zstd) would cut upload
  time and bucket storage several times on a slow link. It needs a compression crate and changes the file names
  readers expect: an owner decision, not done.

### Quiet runs (incremental)

`--scenario incremental --records 2000 --quiet-runs 10`: a sync that finds nothing new, as a whole process.

| state and data | median s | least s | most s | server requests |
|---|---|---|---|---|
| local (/dev/shm) | 0.021 | 0.021 | 0.022 | 1 |
| AWS S3 `us-east-1` | 1.76 | 1.72 | 2.73 | 1 |
| GCS `us-east1` | 2.29 | 2.16 | 2.57 | 1 |

Locally a quiet run is nothing. With a bucket it is round trips, one after another. Counted through a logging proxy
in front of the local RustFS server, a quiet `sync` of one resource makes 24 bucket requests:

| requests | what |
|---|---|
| 6 | the conditional-write check at start-up (`check_conditional_writes`: create, create again, replace with a wrong version, delete with a wrong version, head, delete) |
| 4 | run id: reserve, head `manifest.<id>.json` and `media.<id>.json` in the sink, and at the end head + delete the reservation |
| 2 | stale-reservation sweep: list all of `state/`, read each reservation (`runs::sweep_stale`; once an hour per process, so every run of a `sync` CronJob) |
| 1 | list all of `state/` again to look for a running backfill's lease (`apply_rate_share`) |
| 2 | schema snapshot: read, then written back even when unchanged |
| 3 | lease: create, then read + conditional delete at release |
| 2 | checkpoint read twice |
| 3 | `runs/last.json`, the manifest and the access-log copy |
| 1 | (first run only) the data file, after a head |

1.76 s over 24 requests is about 70 ms each, sequential. In `serve` the start-up check runs once per process, so a
quiet sync there is about 18 requests. Lead 6 below lists what could go.

## Leads

Ranked by expected saving for the cost of the change. Sub-issues of #162 are listed in the issue as they open.

1. **Done (#165).** `Page::from_response` cloned the record array (`src/paging.rs`); it now moves the array out of
   the body. Saved more than the 11% expected (see above).
2. **Done (#166).** The repeated-key check allocated a `String` per record (`reso-ingest/src/checks.rs`); it now
   hashes the borrowed string key. From about 6% to about 2%.
3. **Allocator. Measured (above).** mimalloc, default feature `mimalloc` (also in the image): about 25 to 30% less CPU,
   about twice the peak RSS.
4. **Done: parse and serialize round trip for NDJSON (above).** The NDJSON writer copies each record's text (2.66 G
   to 2.19 G instructions), and no `Value` is built per record (2.19 G to 1.47 G). Parquet still parses.
5. **Measured (above): Parquet.** 2.7 times NDJSON's CPU. The arrow-json decoder can read JSON text itself
   (`Decoder::decode`), so each row could be handed over as the NDJSON line already built from the record's text,
   skipping the `Value` parse, the copy, the drop and the tape serialization (about 45% of a Parquet sync's
   instructions). Records whose values do not fit the declared column type (an object or array in a `Utf8` column,
   which `conform` turns into JSON text today) would fall back to the current path, so the output stays the same.
6. **Measured (above): bucket round trips per run.** 24 requests for a quiet sync. Could go without changing what is
   checked: the second read of the checkpoint, the schema snapshot write when it has not changed, and the lease read
   before its release (the version from the create is enough); the two listings of all of `state/` could list
   `state/runs/` and the lease keys only, which matters once a bucket holds many feeds' key sets. Skipping the
   start-up conditional-write check (6 requests) for a bucket that passed it recently would trade a safety check for
   time: an owner decision.
7. Bucket uploads: bound by the connection here, not the code (above). Overlapping uploads with the pull and
   compressed NDJSON are possible later; not planned without a need from a real deployment.
8. Not measured: `sync --media` (needs a fake file server in the load test), backfill client CPU against its pacing,
   the musl image, and per-phase timings from log spans (the round-trip count above answers the incremental case).
