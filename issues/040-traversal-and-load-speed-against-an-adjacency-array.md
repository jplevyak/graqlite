# Traversal and load speed against an adjacency array

**Status:** open · **Size:** large · **Related:** [007](closed/007-path-expressions.md) (paths), [026](closed/026-index-lookup-allocations.md) (lookup allocations), [033](closed/033-bulk-loading.md) (bulk loading), [035](035-graph-algorithms-as-relations.md) (algorithms over a compact adjacency), [019](019-parallel-queries.md)

## Measurement

Scyth (a code database) has a `FactGraph` trait with two backends: adjacency
arrays in memory (CSR, ids interned to `u32`) and recursive SQL. On
2026-11-10 a third was written on graqlite at `c805e96`, in memory: each
definition and each dependence node a graqlite node, each edge kind a
predicate, reachability and slices as path expressions through
`Query::path_expr`, one-step neighbours through `follow_forward` and
`follow_backward`. It agrees with the other two on every answer of
Scyth's differential test, and a context-sensitive slice (two phases over
three edge classes) is one path expression,
`(^within | ^call_in)* / (^within | ^returns)*`, which is a good result
for the language.

It is slower by one to nearly three orders of magnitude. Medians, one
thread, release build, AMD Ryzen 9 3950X; the graph is a generated program
of 51,505 lines: 7,588 definitions, 275,836 dependence nodes, 1,763,719
links (the 10k-line program, 318,641 links, gives the same ratios).

| Operation | Median result | Adjacency arrays | graqlite | Ratio |
| --- | ---: | ---: | ---: | ---: |
| Build the graph | 1.76 M links | 392 ms | 9,122 ms (193,000 links/s) | 23× |
| Neighbours, one predicate, one node | 0 | under 1 µs | 1 µs | 2× |
| Reachable within 3 links, backward | 0 | 1 µs | 24 µs | 27× |
| Reachable, unbounded, forward | 6 | 3 µs | 35 µs | 12× |
| Reachable from 300 starts (one query per start) | 956 | 0.23 ms | 26 ms | 115× |
| Distance to every node, both directions (level by level with `follow_*`) | 5,155 | 1.7 ms | 1,149 ms | 690× |
| Closure over one predicate, backward | 275,296 | 54 ms | 1,376 ms | 26× |
| Closure over one predicate, forward | 234,846 | 45 ms | 1,209 ms | 27× |
| Two-phase path expression, backward | 3,332 | 0.70 ms | 14 ms | 21× |
| Two-phase path expression, forward | 3,809 | 0.64 ms | 17 ms | 27× |

One thing went the other way. Replacing one definition's outgoing links
(15 links: delete, commit, link, commit) took 0.15 ms, where the array
backend rebuilds the whole graph (392 ms). A graph that is kept and edited
beats one that is rebuilt, by more as the graph grows.

## Reproduce

**In this repository,** with no other project: `benches/traversal.rs`
builds a graph shaped like a program's (definitions in 20 layers, each
calling 3 of the next, and 36 dependence nodes per definition) and times
each operation beside the same work on adjacency arrays built from the
same edges. It asserts that both give the same answers.

```sh
cargo bench --bench traversal                             # 2,000 definitions, 148,704 links, about a minute
GRAQLITE_BENCH_DEFS=7500 cargo bench --bench traversal    # 557,947 links, the size of the table above
```

Medians on the same machine, at `c805e96` plus the bench:

| Benchmark | 2,000 definitions: graqlite | arrays | Ratio | 7,500 definitions: graqlite | arrays | Ratio |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| build | 444 ms | 0.60 ms | 745× | 1,659 ms | 2.4 ms | 702× |
| 100 × callers within 3 | 2.86 ms | 36 µs | 79× | 3.54 ms | 47 µs | 75× |
| dep closure (70,260 and 263,760 nodes reached) | 184 ms | 0.94 ms | 196× | 941 ms | 4.5 ms | 211× |
| levels over calls (1,993 and 7,486 definitions) | 5.2 ms | 35 µs | 148× | 69.8 ms | 167 µs | 416× |
| replace one definition's calls | 65 µs | | | 64 µs | | |

The ratios are larger than in Scyth's table because the arrays here do
nothing but the traversal, where Scyth's backend also maps ids back to
hashes and builds ordered sets. Three things to read from it:

- A path query costs about 29 µs before it reaches anything (100 queries
  with small results in 2.9 ms).
- The closure costs 2.6 to 3.6 µs a node reached; the arrays 13 to 17 ns.
- The level-by-level walk grows 13 times for 3.75 times the definitions,
  so it is quadratic in the frontier. The edit is flat at 64 µs.

**Through Scyth,** which gave the first table: in a checkout of
`github.com/scyth-org/scyth` at commit `68d4b57` or later, which adds
`src/fact_graph/graq.rs` (the graqlite revision is pinned in its
`Cargo.toml`; change `rev` to test a fix):

```sh
# agreement with the other two backends, on every operation
cargo test --release --features graqlite --test fact_graph_backends

# the timings, on the committed 10k-line program
SCALE_PROGRAM=eval/scale/programs/scale-10k \
  cargo test --release --features graqlite --test graq_spike -- --ignored --nocapture

# the 51,505-line program of the table (generating it takes seconds, committing it two minutes)
cargo build --release --bin scyth-gen
./target/release/scyth-gen --size 50k --out /tmp/scale-50k
SCALE_PROGRAM=/tmp/scale-50k \
  cargo test --release --features graqlite --test graq_spike -- --ignored --nocapture
```

Scyth's report, with the 10k table, is
`eval/scale/results/2026-11-10-graqlite-spike/report.md` there.

## Where the time probably goes

Not profiled; from reading the code.

- **Small results cost a fixed 20 to 35 µs.** A path query that reaches
  nothing takes 24 µs: building the `Query`, planning, compiling the
  automaton. A `Prepared` path query with the start as a parameter should
  remove most of it; the builder could also cache the automaton per
  expression.
- **`Automaton::reach` works an entry at a time**, with a `HashSet<(u64,
  usize)>` of seen pairs, a `HashMap<(Step, u64), Vec<u64>>` of cached
  steps, and a `Vec` cloned per step. At 275,000 reached nodes that is
  5 µs a node. A dense bitmap per automaton state over a renumbered id
  space, and steps that append into a reused buffer, are the usual
  remedy.
- **Each step is an index lookup, a liveness check and a projection**
  (`query_property`: `find_links_by_intersection`, `live`, `project`),
  which reads the link's tuple from the log to find its other end. An
  adjacency array reads the other end directly.
- **`query_property` over many ids** extends one `IdSet` per id
  (`links.extend(...)`), which looks quadratic in the frontier if `extend`
  merges sorted sets. The bench confirms the growth is quadratic
  (`levels over calls`), though not that this line is the cause.
- **Loading** is 5 µs a link in memory with nothing flushed: three index
  tables and the statistics per link.

## Design

- **A compact adjacency per (predicate, commit),** built on first use and
  kept for readers of that commit: ids renumbered to `0..n`, CSR arrays in
  both directions, live links only. [035](035-graph-algorithms-as-relations.md)
  already plans this loader for its algorithms; path search and
  `follow_*` should use the same one when the predicate's adjacency is
  built or the query will touch enough of it to pay for building. With
  `as of` or an overlay, fall back to the index.
- **Keeping it up to date:** a commit's new and dead links of the
  predicate are applied as a small delta list over the arrays, merged when
  it grows. This keeps the one result that favoured graqlite.
- **Multi-source reach:** a path pattern whose start is a set (a variable
  bound by earlier rows) should search once from all starts when only the
  reached set is asked for, and per start only when the pairs are.
- **Path length as a value:** `?x (p+) ?y` binding the least number of
  links, so that "distance to every node" is one query. 035's
  `shortest_path` covers the weighted case; this is the unweighted one,
  which the search computes anyway.
- **Bulk load:** build the index entries for a batch sorted by key, once,
  where `begin_bulk` now defers only the flush.

## Tasks

- [x] A bench on a graph of this shape, beside adjacency arrays
      (`benches/traversal.rs`)
- [ ] Profile the closure and the level-by-level walk in it, and confirm
      or correct the list above
- [ ] Fix `query_property`'s per-id extend
- [ ] Prepared path queries; cache the automaton
- [ ] The adjacency cache per predicate and commit, shared with 035, with
      a delta for commits; path search and `follow_*` on it
- [ ] Multi-source reach; path length as a value
- [ ] Bulk load building sorted index entries per batch
- [x] The bench's numbers in `BENCHMARKS.md`, to compare each fix with

## Target

Within 3× of an adjacency array on reads once a predicate's adjacency is
built, and a build under 1 µs a link, while keeping the edit of one node's
links under a millisecond.
