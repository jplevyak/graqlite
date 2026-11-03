# Graph algorithms as built-in relations

**Status:** open · **Size:** medium · **Related:** [007](closed/007-path-expressions.md) (reachability and shortest walks), [015](closed/015-graph-results.md) (result graphs as inputs), [017](closed/017-graph-layering.md) (overlays), [019](019-parallel-queries.md)

## Motivation

CozoDB's "fixed rules" (`ShortestPathDijkstra`, `PageRank`,
`ConnectedComponents`, `BetweennessCentrality`, `CommunityDetectionLouvain`
and a dozen more) are algorithms called like relations from Datalog,
taking a relation as their input graph. Neo4j sells the same as a
separate library. Users of a graph database expect weighted shortest
paths and PageRank to be there; without them they export to NetworkX.

graqlite has the pieces: paths find reachability and one shortest walk
by link count; rules are relations; a [`Call`](../src/query/plan.rs) can
be given its tuples (`Step::Call { given: true }`); and a lookup's
result graph (015) or an overlay (017) is a graph value that a
statement can name. An algorithm is then a relation whose tuples are
computed from a graph at plan time.

## Design

- **A built-in relation per algorithm,** called as rules are, its first
  argument the graph to run on (a predicate, or a graph symbol from
  `set g = graph (lookup ...)`, or `database`), the rest its inputs and
  outputs:
  - `shortest_path(G ?from ?to ?cost ?len)` with a cost predicate or
    expression (`weight(?l)` over the link `?l`); Dijkstra from the bound
    end, or both ends bound, or all pairs for a small graph.
  - `pagerank(G ?node ?score)`, `components(G ?node ?component)`
    (connected, and strongly), `degree(G ?node ?in ?out)`,
    `betweenness(G ?node ?score)`, `label_propagation(G ?node ?label)`.
  - Each runs over the live links of `G` as the query sees them (`as
    of`, subgraphs, overlays all apply, since `G` is read through the
    same lookups), producing tuples that the rest of the pattern joins.
    Outputs are values (numbers), ephemeral as computed values are
    (025), and can be inserted: `insert ?n rank ?s where pagerank(knows
    ?n ?s)` stores the ranks.
- **Where they run:** at plan time, as relations are evaluated before the
  query, with the graph loaded into a compact adjacency form (ids
  renumbered to `0..n`, CSR arrays) once per call. For a predicate over
  the whole database this is a scan of its index entry, which the
  streaming cursor makes cheap; the adjacency can be cached per
  (predicate, commit) for readers.
- **Explain** shows the algorithm, the graph's size, and the time.
- **Not rules:** these aren't expressible as Datalog rules efficiently
  (PageRank needs floating point iteration to a tolerance; Dijkstra
  needs a priority queue), which is why CozoDB made them fixed. Keep the
  set small and let 019 parallelize them, since each is embarrassingly
  parallel over nodes.

## Tasks

- [ ] A `Builtin` relation kind in `query::Relation`, with a trait for
      implementations and the adjacency loader
- [ ] `shortest_path` (weighted, from one end, both ends, or all pairs
      under a size limit), with provenance: the links of the path, so
      `lookup` returns them as derived links (015)
- [ ] `pagerank`, `components`, `degree`
- [ ] GraQL calls with a graph argument; `\explain`
- [ ] A benchmark on `benches/index.rs`'s graph with a `knows` predicate
      added
- [ ] Example: ranking and clustering a small social graph

## Open questions

- A graph argument that is a predicate reads one index entry; a graph
  symbol reads a result graph already in memory; `database` reads every
  predicate. Should an algorithm over several predicates take a path
  expression (`(knows | follows)`) as its graph? It falls out of
  `paths.rs` if the loader walks the automaton's single steps.
- Floating point results aren't reproducible across platforms bit for
  bit; the golden tests should round them.
