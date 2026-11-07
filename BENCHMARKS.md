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
commit `7020f43`.

### graql lookup

| Benchmark | Median |
|---|---:|
| 1000 people's links/40000 | 24 ms |
| a city's people/40000 | 4.24 ms |

### graql point query of 1000

| Benchmark | Median |
|---|---:|
| bound/40000 | 20 ms |
| prepared/40000 | 1.71 ms |
| text/40000 | 23 ms |

### index

| Benchmark | Median |
|---|---:|
| as of a tenth, type person/200000 | 3.94 ms |
| count predicate type/200000 | 1.6 µs |
| count type person/200000 | 1.7 µs |
| find predicate type/200000 | 21.7 ms |
| limit 10 of type person/200000 | 15 µs |
| plan as of a tenth, type person/200000 | 7.91 µs |
| plan type person and age/200000 | 17.1 µs |
| plan type person and city paris/200000 | 22.4 ms |
| type person and city paris/200000 | 10.8 ms |

### join

| Benchmark | Median |
|---|---:|
| 4000 rows, not reduced/120000 | 2.37 ms |
| 4000 rows/120000 | 2.36 ms |
| planning alone, not reduced/120000 | 16.7 µs |
| planning alone/120000 | 21.1 µs |

### lookups of 1000

| Benchmark | Median |
|---|---:|
| find_link/120000 | 479 µs |
| live links of entry/120000 | 483 µs |
| local table count/120000 | 235 µs |
| local table find/120000 | 289 µs |
| local table seek_rev/120000 | 247 µs |
| log record/120000 | 77.1 µs |
| prepared query per person/120000 | 1.56 ms |
| query per person/120000 | 17.4 ms |

### record_log

| Benchmark | Median |
|---|---:|
| decode | 1.46 ms |
| encode | 1.6 ms |
