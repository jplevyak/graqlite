# Fewer allocations per index lookup

**Status:** closed · **Size:** medium · **Related:** [011](011-adaptive-btree-index-entries.md) (streaming large entries did this for them), [012](012-reduce-only-when-it-pays.md), [013](013-prepared-queries.md)

## Problem

`IdSet` is a `BTreeSet<u64>`. A bind join's lookup of a small index entry
(`Matcher::candidates` in `src/query/exec.rs`, through
`tuple_index::find_links*` and `LocalTable::find`) collects the bucket's
ids into one; `Graph::live` then builds a `HashMap` keyed by subject,
predicate and object to keep each fact's latest link, and collects the
survivors into another `IdSet`. `LocalTable::count` on an inline entry
sorts and dedups a fresh vector on every call, and the planner calls
`count` for every constant it estimates. `Graph::find_links`, used by
`latest_link` and so by every `add_link`, does the `BTreeSet` and a log
read per id too.

Large entries avoid all this: `Matcher::begin` streams them newest first
and keeps a `seen` vector of facts, so liveness is decided in one pass.
Small entries, which are most lookups, take the slow shape.

`benches/lookups.rs` puts a prepared point query at a few microseconds, so
this is not what dominates today; it is what caps the row-at-a-time joins
and the write path once the syncs (021) are out of the way.

## Design

- **`IdSet` as a sorted `Vec<u64>`.** Trees give ids in order already;
  inline entries are at most `PROMOTE_AT` (64) ids, sorted and deduped on
  read. Set operations the queries use (`extend`, `range`, `last`,
  `contains`) are a merge or a binary search. `BTreeSet`'s only advantage,
  cheap insertion in the middle, is never needed: ids only grow.
- **Newest-first liveness for small entries.** `Lookups::ids` can give a
  small entry backward as cheaply as forward, so `live` and
  `Matcher::matches` can use the `seen` trick from `begin` and drop the
  `HashMap`.
- **Cached inline counts.** `LocalTable` already caches promoted keys'
  header pages in `promoted`; cache the count of recently counted inline
  keys the same way, invalidated by the key's next insert.
- **Reusable buffers.** `Matcher` keeps `new`, `live` and `seen` across
  rows for `begin`; let `matches` and `candidates` fill caller-provided
  vectors too.

Separately, every value lookup and every `node_by_key` pays a SHA-256
(`Uuid::of`). The lookup compares the stored data anyway, so the hash
only needs to be well distributed; a keyed 128 bit non-cryptographic hash
is several times cheaper on short data. That is a format change (the
value index is keyed by the hash), so leave it until profiles of point
queries show hashing, and note in the README that the hash is part of
the on-disk format either way.

## Tasks

- [x] `IdSet` as a sorted vector, with the set operations the code uses
- [x] `live` by newest-first scan without a map
- [x] ~~Inline count cache in `LocalTable`~~: counting allocates nothing
      instead (see below)
- [x] `benches/lookups.rs` before and after, per layer
- [x] Decide on the value hash after profiling; record the decision here

## As implemented

- **`IdSet`** (`src/local_table.rs`) is a newtype over a sorted vector of
  distinct ids, with the `BTreeSet` methods the code used (`insert`,
  `remove`, `contains`, `range`, `first`, `last`, `retain`, `extend`,
  `intersection`, iteration). `insert` and `extend` append when the ids
  come in order, as an index's do, and else insert or merge; sets built
  from ids in any order (`Graph::project`, the reduction's projections)
  collect them and sort once (`from_unsorted`), not an insert each.
  `LocalTable::find` hands over an inline entry's sorted vector as is.
- **Newest-first liveness:** `Graph::live` and `reduce::live_links` read
  the candidates backwards, so the first link seen of a fact is its
  latest; the facts seen are a short vector for a few links and a hash
  set past 32 (`graph::Facts`). No map, and the result is in order
  without sorting.
- **The latest link of a fact** (`latest_link`, `latest_link_as_of`, so
  every `add_link`, `find_link` and `delete_link`) reads the SPO entry
  newest first and stops at the first link whose record is that fact:
  usually one log read, where `find_links` read the record of every link
  of the fact into a set. The Matcher's subgraph and author accesses, and
  finding a result graph's stored link (`exec::stored`), use it too.
- **Counting** a key reads its bucket into a buffer of the thread's, and
  sorts the ids only if they aren't increasing (they are, but for a key
  given ids out of order). `Writer::get` no longer makes a `Reader` (an
  `Arc` clone and drop) per lookup.
- **Not done:** a cache of inline counts. Counting is now one bucket read
  with nothing allocated (208 ns), and a cache would need invalidating on
  every insert and couldn't serve readers; the planner counts a few keys
  a plan, a few percent of planning. The Matcher's `matches` still makes
  a vector per row; the bind joins' hot path, `begin`, reuses its
  buffers already.
- **Tests:** `id_sets_are_sorted_and_distinct`,
  `counts_ids_inserted_out_of_order_once`,
  `live_links_of_many_facts_newest_first`; and three benchmarks added to
  `benches/lookups.rs` (`writes of 1000`): linking a live fact, linking a
  new one, and finding values and keys of 24 bytes.

### Measured

Against the base commit (`80de0c6`), alternating, the best of two runs
each on the reference machine (`BENCHMARKS.md`):

| Benchmark | Before | After | |
| --- | ---: | ---: | ---: |
| lookups of 1000 / find_link | 478 µs | 330 µs | −31% |
| lookups of 1000 / live links of entry | 501 µs | 390 µs | −22% |
| lookups of 1000 / local table find | 288 µs | 239 µs | −17% |
| lookups of 1000 / local table count | 242 µs | 208 µs | −14% |
| writes of 1000 / link a live fact | 607 µs | 468 µs | −23% |
| writes of 1000 / find_by_key of 24 bytes | 781 µs | 637 µs | −18% |
| writes of 1000 / link a new fact | 30.1 ms | 28.2 ms | −6% (noisy) |
| index / find predicate type (200 000 ids) | 21 ms | 7.9 ms | −62% |
| index / plan type person and city paris | 22.9 ms | 13.7 ms | −40% |
| index / count type person | 1.65 µs | 1.45 µs | −12% |

Point queries (`prepared query per person`, GraQL's), joins and
planning moved by less than 3%: the bind joins already streamed small
entries newest first. `index / type person and city paris` and `as of a
tenth` measured +2.7% and +2.4%, unevenly between runs (one +0%, one
+3% to +5%), with the machine's load near 4: within noise, but not
shown to be.

Linking a new fact costs 28 µs, nearly all of it outside the lookups
this issue is about (appending the record and its index entries).

### The value hash: SHA-256 stays

Measured on the reference machine, which has SHA instructions:

| Bytes | SHA-256 (`sha2`) | BLAKE3 | XXH3-128 |
| ---: | ---: | ---: | ---: |
| 8 | 56 ns | 69 ns | 3.4 ns |
| 24 | 50 ns | 68 ns | 5.5 ns |
| 64 | 75 ns | 57 ns | 7.0 ns |
| 200 | 139 ns | 233 ns | 18 ns |
| 1540 | 759 ns | 1422 ns | 71 ns |

- **BLAKE3** is no faster than SHA-256 with hardware support at these
  sizes: its SIMD pays off over several KiB. Not worth a format change.
- **XXH3** would save about 45 ns where a value is hashed: a third of
  `find_value` of 24 bytes (157 ns), but nothing a query does per row (a
  statement's constants are found once, when it is lowered, and the SPO
  keys of graphs up to 32 GiB are their own keys, unhashed), and nothing
  measurable of a write (28 µs a new link). A new value is hashed once,
  when appended.
- **And it isn't collision resistant,** which matters here: the value
  index holds one id per hash, and appending a value whose hash is
  another value's fails (`Error::DuplicateValue`), so anyone who chooses
  values stored could keep others from being stored. A non-cryptographic
  hash would need a secret seed per database, a value index that holds a
  set per hash and checks data, and, as the indexes have no format
  version, a version and a rebuild of the value index for existing
  databases.

So the hash stays, and the README says it is part of the on-disk
format. Revisit if a profile of real use shows hashing, on machines
without SHA instructions above all (where SHA-256 is several times
slower), together with the collision-tolerant value index.

## Closed

Closed by the commit making `IdSet` a sorted vector.

