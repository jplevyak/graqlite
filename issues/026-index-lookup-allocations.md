# Fewer allocations per index lookup

**Status:** open · **Size:** medium · **Related:** [011](closed/011-adaptive-btree-index-entries.md) (streaming large entries did this for them), [012](closed/012-reduce-only-when-it-pays.md), [013](closed/013-prepared-queries.md)

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

- [ ] `IdSet` as a sorted vector, with the set operations the code uses
- [ ] `live` by newest-first scan without a map
- [ ] Inline count cache in `LocalTable`
- [ ] `benches/lookups.rs` before and after, per layer
- [ ] Decide on the value hash after profiling; record the decision here
