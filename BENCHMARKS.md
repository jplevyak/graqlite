# Benchmarks

The medians of `cargo bench` (criterion) on one reference machine, to
compare a change with. Rerun on the same machine, and update this file in
the change, when a change affects them. A number from another machine says
little; compare it with a run of the base commit there instead.

Each name is the bench's group and id as criterion prints it; the number
after the last `/` is the size of the graph or log it runs on. The sources
are in [`benches/`](benches/).

**Reference machine:** AMD Ryzen 9 3950X (16 cores, 32 threads), 128 GiB,
Linux 6.8, XFS; Rust 1.99.1, release profile. Recorded 2026-11-07, at
commit `7020f43`; the graql, index, join, lookups and writes tables again
on 2026-11-08, with issue 026's change (the best of two runs).

### graql lookup

| Benchmark | Median |
|---|---:|
| 1000 people's links/40000 | 22.9 ms |
| a city's people/40000 | 3.88 ms |

### graql point query of 1000

| Benchmark | Median |
|---|---:|
| bound/40000 | 19.4 ms |
| prepared/40000 | 1.69 ms |
| text/40000 | 21.7 ms |

### index

| Benchmark | Median |
|---|---:|
| as of a tenth, type person/200000 | 3.81 ms |
| count predicate type/200000 | 1.45 µs |
| count type person/200000 | 1.45 µs |
| find predicate type/200000 | 7.92 ms |
| limit 10 of type person/200000 | 14.9 µs |
| plan as of a tenth, type person/200000 | 7.8 µs |
| plan type person and age/200000 | 17.5 µs |
| plan type person and city paris/200000 | 13.7 ms |
| type person and city paris/200000 | 11.1 ms |

### join

| Benchmark | Median |
|---|---:|
| 4000 rows, not reduced/120000 | 2.34 ms |
| 4000 rows/120000 | 2.36 ms |
| planning alone, not reduced/120000 | 16.6 µs |
| planning alone/120000 | 21 µs |

### lookups of 1000

| Benchmark | Median |
|---|---:|
| find_link/120000 | 330 µs |
| live links of entry/120000 | 390 µs |
| local table count/120000 | 208 µs |
| local table find/120000 | 239 µs |
| local table seek_rev/120000 | 244 µs |
| log record/120000 | 75.2 µs |
| prepared query per person/120000 | 1.61 ms |
| query per person/120000 | 18 ms |

### traversal

The first column was recorded 2026-11-10, at commit `c805e96` with the
bench added (issue 040); the second 2026-11-11, after the first work on
that issue: one sort in `query_property`, a path search that visits fewer
states, hashes ids cheaply and allocates nothing per step, one search from
all the rows of a `distinct` query, and a path's length as a value. Each
`arrays` line is the same work on adjacency arrays built from the same
edges. The `one query` lines are new: a query a start would be the line
above them (or, for the closure from 100, 100 times `dep closure`).

| Benchmark | Median at `c805e96` | Median after |
|---|---:|---:|
| build, graqlite/148704 | 444 ms | 454 ms |
| build, arrays/148704 | 596 µs | 439 µs |
| 100 x callers within 3, graqlite/148704 | 2.86 ms | 2.16 ms |
| 100 x callers within 3, arrays/148704 | 36 µs | 36.5 µs |
| callers of 100 within 3, one query, graqlite/148704 | | 857 µs |
| callers of 100 within 3, arrays/148704 | | 7.36 µs |
| dep closure, graqlite/148704 | 184 ms | 71 ms |
| dep closure, arrays/148704 | 939 µs | 983 µs |
| dep closure from 100, one query, graqlite/148704 | | 79.1 ms |
| dep closure from 100, arrays/148704 | | 1.09 ms |
| levels over calls, graqlite/148704 | 5.17 ms | 2.84 ms |
| levels over calls, arrays/148704 | 34.9 µs | 35.3 µs |
| levels over calls, one query, graqlite/148704 | | 3.16 ms |

With `GRAQLITE_BENCH_DEFS=7500` (557,947 links):

| Benchmark | Median at `c805e96` | Median after |
|---|---:|---:|
| build, graqlite/557947 | 1.65 s | 1.70 s |
| 100 x callers within 3, graqlite/557947 | 3.51 ms | 2.61 ms |
| callers of 100 within 3, one query, graqlite/557947 | | 1.47 ms |
| dep closure, graqlite/557947 | 921 ms | 340 ms |
| dep closure from 100, one query, graqlite/557947 | | 420 ms |
| levels over calls, graqlite/557947 | 71.7 ms | 12.1 ms |
| levels over calls, one query, graqlite/557947 | | 13.4 ms |

### traversal edit

| Benchmark | Median |
|---|---:|
| replace one definition's calls/148704 | 65.3 µs |

### writes of 1000

| Benchmark | Median |
|---|---:|
| find_by_key of 24 bytes/120000 | 637 µs |
| find_value of 24 bytes/120000 | 157 µs |
| link a live fact/120000 | 468 µs |
| link a new fact/120000 | 28.2 ms |

### record_log

| Benchmark | Median |
|---|---:|
| decode | 1.46 ms |
| encode | 1.6 ms |
