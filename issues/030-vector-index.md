# A vector index for nearest-neighbour search

**Status:** open · **Size:** large · **Related:** [031](031-full-text-search.md) (another index over values), [009](closed/009-expressions-and-literal-types.md) (untyped values), [011](closed/011-adaptive-btree-index-entries.md) (index files that grow)

## Motivation

CozoDB added an HNSW (hierarchical navigable small world) index in 0.6
and became, with that one feature, a graph database people reach for
when they want embeddings next to their relations: a query can walk the
graph from the nearest neighbours of a vector, or filter neighbours by
graph patterns, in one statement. The same combination is what retrieval
systems want and what vector-only stores can't do.

graqlite is a good host for it: values are untyped blobs, so a vector is
a value; links give each vector its entity and the entity its context;
`as of` and subgraphs apply to the links, so a search can be scoped the
way any pattern can. What it lacks is an index that finds values by
distance, and a way to ask.

## Design

- **Vectors are values.** A vector is a value holding `f32`s little
  endian (a header byte for the element type, then the elements), made
  by a function `vector("0.1, 0.2, ...")` or `vector(?text)` and shown by
  `str` as text. Dimension is the length. No other typing: a predicate's
  index declares the dimension and refuses values of another.
- **An index is declared per predicate**, and saved in the graph as rules
  are (001), so it commits, rolls back and travels in snapshots:
  `index vector on embedding dims 768 metric cosine [m 16] [ef 200]`.
  It indexes the object values of live links with that predicate. `\di`
  lists indexes; `delete index vector on embedding` drops one.
- **The index is derived, and rebuildable from the log,** like the
  statistics and the commit times: it is built in memory from the links
  appended (`index_records` feeds it), saved to `{prefix}vector.<p>` on
  `tick` and when the graph is dropped, and on open loaded and caught up
  from the saved id, or rebuilt from scratch if missing. So no new crash
  invariant: a commit is durable in the log; the index is a cache of it.
  That keeps the first version small and lets the on-disk layout change
  freely. A memory mapped, incrementally updated layout can come later if
  rebuilds get slow.
- **Deletions are tombstones.** A dead link marks its value's entry
  unreachable for new searches when the value has no other live link with
  the predicate; a rebuild drops them. HNSW handles this adequately
  until tombstones dominate, when `tick` rebuilds.
- **Querying** is a call of a built-in relation, so it lowers to a
  [`Step::Call`](../src/query/plan.rs) with given tuples computed at plan
  time, and joins like any relation: `nearest(embedding $q 10 ?v ?d)`
  binds the ten nearest values `?v` and their distances `?d`, and the
  rest of the pattern joins them: `select ?doc, ?d by ?d where
  nearest(embedding $q 10 ?v ?d) . ?doc embedding ?v ; author /ann`.
  When the pattern is selective and the neighbours are not (a filter
  that keeps one in a thousand), the planner can run it the other way
  round: candidates first, then distance to each, which is a scan the
  same relation offers with the candidates given. Start with the call;
  add the planner choice when a benchmark shows it matters.
- **Snapshot readers** (018) search the index as of their commit: the
  in-memory index keeps, per entry, the id that added it, and a search
  ignores entries after the reader's end; tombstones carry the id that
  deleted.

## Tasks

- [ ] The vector value encoding, `vector(...)`, `str` of a vector, and
      `distance(a, b, metric)` as an expression function (useful alone)
- [ ] An HNSW over values in memory, fed by `index_records`, with
      tombstones, saved and loaded like `stats`
- [ ] `index vector on P ...` and `delete index`, saved in the graph
- [ ] `nearest(P $q k ?v ?d)` as a built-in relation; `\explain` shows it
- [ ] A benchmark: 100 000 vectors of 384 dimensions, recall at 10
      against brute force, and queries per second
- [ ] Example 23: documents with embeddings, searched and then filtered
      by their links

## Open questions

- A dependency for the index (`hnsw_rs`, `usearch`) or a few hundred
  lines of our own? The project has three runtime dependencies on
  purpose; HNSW is small enough to own, and owning it makes the as-of
  and tombstone behaviour straightforward.
- Quantization (`f16`, `int8`) for memory: later, behind the same value
  header byte.
- Should the index also serve `?v (nearest $q 10)` as an object test, the
  way `(~"re")` does? It reads well but hides a relation with two outputs.
