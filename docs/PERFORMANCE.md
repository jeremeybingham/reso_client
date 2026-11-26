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
record's text after them, instead of serializing every parsed record again. The records are still parsed: the
replication order check, the checkpoint, the repeated-key check, flag deletes, media and Parquet read them.

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

Next for this lead: skip building `Value` trees for NDJSON. The consumers that read fields (order check and checkpoint:
timestamp and key; repeated keys; flag deletes; media) would read them from the record text with a small borrowed
deserializer instead. Parsing is 68% of the instructions now, and most of it is building and dropping the trees, so this
could save up to about half the remaining CPU. It touches reso-client's replication API and every consumer above.

## Leads

Ranked by expected saving for the cost of the change. Sub-issues of #162 are listed in the issue as they open.

1. **Done (#165).** `Page::from_response` cloned the record array (`src/paging.rs`); it now moves the array out of
   the body. Saved more than the 11% expected (see above).
2. **Done (#166).** The repeated-key check allocated a `String` per record (`reso-ingest/src/checks.rs`); it now
   hashes the borrowed string key. From about 6% to about 2%.
3. **Allocator. Measured (above).** mimalloc, default feature `mimalloc` (also in the image): about 25 to 30% less CPU,
   about twice the peak RSS.
4. **Parse and serialize round trip for NDJSON. Serialize half done (above).** The NDJSON writer copies each record's
   text (2.66 G to 2.19 G instructions). Left: parsing into `Value` (68% now), see "Next for this lead".
5. Not measured yet: Parquet conversion, bucket uploads (S3, GCS), many small incremental runs, `$metadata` and
   startup. Those need the bucket targets and per-phase timings listed in #162.
