# Shopping Sessions

A Buildathon entry for the Shopping Sessions track: one Rust binary, no crate
dependencies, that turns the dirty event and click CSVs into the two daily
Parquet tables.

## How it works

Three phases, each a pool of `NCPU` threads pulling work from a shared counter.

1. **Parse.** The workers read 512 KiB blocks into reusable buffers on Linux. Set
   `SS_MAP=1` to use mapped input. For each
   block a SIMD pass finds every comma, newline and quote, then each line is
   split, unquoted and parsed into a 32-byte record. IDs of up to 9 ASCII bytes
   are packed into a `u64` that sorts like the string; anything else is
   interned. Events are bucketed by a hash of `event_id`, clicks by a hash of
   their person. One thread builds the links table (union-find) first.
2. **Dedup.** Each event bucket fits in cache. The earliest copy of every
   `event_id` wins, invalid and late events are dropped, the person is looked
   up, and the survivors are bucketed by person.
3. **Sessions.** Each person bucket is grouped by person, sorted by time,
   sessionized and attributed against that person's clicks with a sliding
   7-day window. Each thread writes its own `user_daily` Parquet file;
   campaign totals are merged at the end.

Clicks are not deduplicated: copies are identical, and neither the credited
click nor the set of assisting campaigns changes when a click appears twice.

Before exiting, every thread hands its memory and mapped pages back to the
kernel, which would otherwise free them on a single thread inside the timed run.

## Layout

- `native/src/main.rs`: the pipeline.
- `native/src/scan.rs`: block loading and the CSV structural index.
- `native/src/keys.rs`: IDs as sortable 64-bit keys.
- `native/src/ts.rs`: timestamps and prices.
- `native/src/pq.rs`: a small Parquet writer (delta and dictionary encodings, one write per file).
- `native/src/diag.rs`: a diagnostic mode (`SS_DIAG`) that benchmarks variants on the machine it runs on.
- `tools/reference.py`: a slow, literal implementation of the rules, for checking.
- `tools/compare.py`: exact diff of an output folder against expected CSVs.

## Build and check

```sh
docker buildx build --platform linux/amd64 --load -t shopping-sessions:latest .

# Without Docker (any platform with Rust 1.85+):
cd native && cargo test --release && cargo build --release
DATA_DIR=../dev OUTPUT_DIR=../out NCPU=8 target/release/shopping-sessions

# Against the published dev answer, or against the reference on any generated set:
python3 tools/compare.py out dev-answer
python3 tools/reference.py dev ref && python3 tools/compare.py out ref
```

`tools/compare.py` needs `pip install duckdb`. `tools/reference.py` is plain
Python and holds everything in memory: fine up to about 10 million events.
