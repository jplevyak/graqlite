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

- [x] The vector value encoding, `vector(...)`, `str` of a vector, and
      `distance(a, b, metric)` as an expression function (useful alone)
- [x] An HNSW over values in memory, fed by `index_records`, with
      tombstones (fed at commits; saving and loading is step 5)
- [ ] Saved and loaded like `stats`
- [x] `index vector on P ...` and `delete index`, saved in the graph
- [x] `nearest(P $q k ?v ?d)` as a built-in relation; `\explain` shows it
      (exact, by a scan, for now)
- [ ] A benchmark: 100 000 vectors of 384 dimensions, recall at 10
      against brute force, and queries per second
- [ ] Example 23: documents with embeddings, searched and then filtered
      by their links

## Plan

1. Vector values and functions (done).
2. `nearest` as an exact scan (done) of a predicate's live links, so the query
   surface, `as of` and readers are settled first, and the benchmark has
   brute force to measure recall against. It can't be a relation with
   tuples computed at plan time (`Step::Call`): `$q` and `as of` are
   known only when the query runs, so it is a step of its own, with the
   predicate, the query vector (a constant or parameter, at first) and
   `k` given.
3. `index vector on P …` and `delete index` (done), saved in the graph
   as rules are, and `\di`.
4. (Done.) An HNSW of our own (a seeded RNG, so tests are deterministic), fed from
   committed records only, so a rollback has nothing to undo; links not
   yet committed are a small list scanned exactly, cleared on rollback.
   Entries carry the ids that added and deleted them, for readers.
5. Saved as `{prefix}vector.<p>` beside the statistics, loaded and caught
   up from the log, or rebuilt.
6. The benchmark, and example 23.

## Progress

Step 1 is done:

- **`src/vector.rs`:** the encoding (header byte `0xF8`, which UTF-8
  never has, then `f32`s little endian), `parse` and `format` of the text
  form `[0.1, 0.2]`, and `distance` by `Metric` (cosine, l2, dot), each
  smaller for nearer.
- **`Cell::Vector`**, a computed vector. A cell's `data` is what a value
  holding it stores (a vector's encoding), and `Cell::of_data` reads a
  value's data back as a vector or text. Computed values are made from
  their data, not their text, so a vector stays a vector.
- **GraQL:** `vector(x)` (from text, or the vector `x` is), `str` of one,
  and `distance(a, b[, metric])`. The shell shows a stored vector as its
  text.
- **`insert … where` templates may use a `let`'s variable**, written as
  a value: `insert ?d embedding ?v where ?d text ?t . let ?v =
  vector(?t)`. Without it GraQL had no way to store a vector.
- Tests: `vector::tests`, `vectors_are_stored_encoded_and_measured`, and
  `tests/cli/vectors.graql`.

Step 2 is done:

- **`query::Nearest`**, added by `Group::nearest(search, subject)`: the
  predicate, the query vector (a constant, a parameter, or a variable
  bound earlier), `k`, the metric, and the variables of the value and
  distance. Its scan, `?subject P ?value`, is a group of its own, planned
  alone (`Step::Nearest`), so it sees `as of`, subgraphs, overlays and a
  reader's snapshot as any pattern does, and runs once per execution.
  `NearestLookup` decodes the vectors once, then, for each row, keeps the
  `k` nearest (ties by id) and binds each distance as an ephemeral value
  held by the query.
- **Planning:** an item like a call, ready once its vector is bound; it
  costs the scan plus a distance per vector per row. The planner can run
  it first, binding values that later patterns join, or after them, as a
  filter of values bound already. Either way, it is the `k` nearest of
  all, then joined.
- **GraQL:** `nearest(P q k ?v ?d [metric])` in a pattern, unless a rule
  has that name; its mistakes are reported by name. `\explain` shows the
  search and its scan.
- Standing queries refuse it: the nearest values aren't maintained
  incrementally.
- Tests: `nearest_matches_a_brute_force_search` (300 random vectors, each
  metric), `nearest_sees_overlays_and_history`,
  `nearest_finds_the_k_nearest_values`,
  `nearest_searches_a_reader_as_of_its_commit`, and
  `tests/cli/nearest.graql`.

Step 3 is done:

- **Declarations are the graph's** (`src/graph/indexes.rs`), not GraQL's,
  so that the index of step 4, in `Graph`, reads them: a link from
  `/graqlite/indexes` by `graqlite:vector_index` to the predicate, and
  from that link to its `graqlite:dims`, `metric`, `m` and `ef`. They
  commit, roll back, and travel in snapshots with the entries around
  them. `Graph::create_vector_index`, `drop_vector_index`,
  `vector_indexes` and `vector_index`; the writer keeps them in memory,
  read again on open, restore and rollback, and a reader reads them as of
  its commit.
- **Checked:** declaring one fails if a live link with the predicate
  hasn't a vector of its dimension, and afterwards `check_link` refuses
  such a live link (`Error::NotAVector`); dead links aren't checked.
- **GraQL:** `index vector on P dims N [metric M] [m X] [ef Y]` (cosine,
  16 and 200 if not given), `delete index vector on P`, and `\di`.
  `nearest` searches by the index's metric when not given one. `index` is
  a keyword now, as a statement starting with a plain word would read as
  a triple after a pattern: a value `index` must be quoted.
- Tests: `vector_indexes_are_declared_checked_and_kept` (refusals,
  replacing, rollback, a reader, reopening, a snapshot), the round trips
  of the statements' text, and `tests/cli/vector-index.graql`.

Step 4 is done:

- **`src/hnsw.rs`:** an HNSW of our own, with the paper's heuristic for
  choosing links, levels from a seeded generator, and each item's ranges
  of ids over which it is live, so that a search is as of an id: items
  no longer live are tombstones it passes through, and the beam widens
  if too few it finds are live. Distances are measured fast in `f32`
  (`vector::dot`, `l2_squared`, a norm kept per item), and the results
  measured again exactly, as the scan does, so they are the same digits.
- **`src/graph/vectors.rs`:** a `VectorSearch` per declared index,
  shared by the graph and its readers. Fed at each commit from the
  records committed since: each link with an indexed predicate makes its
  object live from its id, or ends its range there, by whether a live
  link with the predicate has it then. So a search as of any id since it
  was built (its floor) finds what a query as of that id would. Built
  from the live links on open and when a declaration is committed, and
  again at `tick` once tombstones are a quarter of it, keeping what the
  oldest reader may see.
- **Used when it answers as the scan would:** the writer has appended no
  links of the predicate since its last commit, no overlay has them, no
  subgraph is asked for, the metric is the index's, and the search is as
  of its floor or later. Else the scan. `\explain` says when the index
  is used, and the planner costs it as about `ef` per layer.
- **Measured** (release, one core, 20 000 vectors of 384 dimensions):
  clustered, recall at 10 is 1.0 with `ef` 200, a search 0.37 ms against
  6.7 ms for the exact scan, and building 375 µs a vector; uniformly
  random, recall is 0.57, as HNSWs are on data whose distances are all
  alike. The build is the cost, most of it measuring distances, in
  baseline x86-64 SIMD (no AVX); not measured with wider SIMD. Step 5,
  saving the index, keeps opening from rebuilding it.
- **A commit copies a search a reader is using** (`Arc::make_mut`), so
  that the reader's stays as it was: a cost proportional to the index,
  only while a search runs during a commit.
- Tests: `hnsw::tests` (recall for each metric, searches as of ids,
  rebuilding), `nearest_uses_the_vector_index_as_the_scan_would` (recall
  against the scan, links not yet committed, deletes seen by readers and
  `as of`, an overlay, rebuilding at `tick`, dropping), and a search after
  reopening.

## Open questions

- A dependency for the index (`hnsw_rs`, `usearch`) or a few hundred
  lines of our own? The project has three runtime dependencies on
  purpose; HNSW is small enough to own, and owning it makes the as-of
  and tombstone behaviour straightforward.
- Quantization (`f16`, `int8`) for memory: later, behind the same value
  header byte.
- Should the index also serve `?v (nearest $q 10)` as an object test, the
  way `(~"re")` does? It reads well but hides a relation with two outputs.
