# Ephemeral values grow without bound

**Status:** open · **Size:** medium · **Related:** [015](closed/015-graph-results.md) (computed values), [017](closed/017-graph-layering.md) (overlays' ephemeral nodes)

## Problem

A value a query computes that the graph doesn't have (a count, a rank,
the result of an expression, an overlay's node or key) gets an id from
`Graph::ephemeral_value`, `ephemeral_node` or `ephemeral_key`
(`src/graph.rs`). The data goes into a vector and a map shared by the
graph and all its readers, and is never removed: `ephemeral_data`
returns a `&[u8]` whose lifetime is extended, unsafely, on the promise
that an entry is "kept (never changed or dropped) until the graph is
dropped". A process that runs aggregate queries continuously accumulates
one entry per distinct result, for as long as the graph is open.

## Design

Two parts:

1. **Make the storage sound to evict from.** `ephemeral_data` returns an
   `Arc<[u8]>` (or the callers take a closure, as `with_link` does) so an
   entry can be dropped while a caller holds its data.
2. **Scope the ids.** Options, in order of preference:
   - *Per query.* Results already carry `Cell`s with their data
     (`query::aggregate::Cell`); the only reason a computed value needs a
     graph-wide id is to be compared by id with the graph's entries and
     with other computed values in the same query. A per-query arena,
     cleared when the query's `Rows` is dropped, serves that. Overlays
     (`Overlay`) and the session's `data` graph need ids that outlive one
     query: give each overlay its own arena, dropped with it.
   - *A bounded cache* on the graph, keyed by data, evicting least
     recently used entries once over a size, with the `Arc` from part 1
     keeping live data valid. Simpler, but an id can then come to name
     different data after eviction, which `Overlay::of_graph` and
     standing queries holding rows would have to cope with.

Start with part 1 and measure what part 2 needs: a counter of ephemeral
entries in `\info` would show how fast real sessions grow.

## Tasks

- [ ] `ephemeral_data` returns owned data; remove the unsafe lifetime
      extension
- [ ] Count ephemeral entries in `\info`
- [ ] Per-query (and per-overlay) arenas for computed values
- [ ] A test: a loop of aggregate queries on an open graph does not grow
      its ephemeral table
