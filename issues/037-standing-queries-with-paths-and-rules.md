# Standing queries with paths and rules

**Status:** open · **Size:** large · **Related:** [001](closed/001-persistent-rules.md) (rules), [007](closed/007-path-expressions.md) (paths), [003](003-queries-as-graph-data.md) (`subscribe`), [019](019-parallel-queries.md)

## Problem

A `StandingQuery` keeps a query's rows up to date from the links a commit
added, without evaluating it again. It refuses the queries that cost the
most to evaluate again: `StandingQuery::new` returns "a standing query can
not have paths or calls" (`has_paths_or_calls` in
[`standing.rs`](../src/query/standing.rs)). So a transitive closure, a
path expression or a recursive rule is computed from nothing each time it
is asked, however little changed.

The use case that raised this is a code database (Scyth), which keeps
derived facts over a call graph: which functions transitively perform an
effect, which definitions are reachable from an entry point, which types
contain a type. One edit changes a handful of links and the answer changes
for the callers above them only. On a program of 6,500 definitions,
recomputing after adding one effect takes seconds where the change touches
a few thousand rows. A `MaintainedSubgraph` over a rule would make this
graqlite's job.

## Design

- **Insertions** are semi-naive evaluation started from the commit's new
  links as the delta, which `rules.rs` already does round by round from an
  empty relation. The standing query keeps each relation's tuples between
  commits and seeds the first round with the delta variants of the rules
  whose bodies match a new link.
- **Deletions** are the hard part, since a tuple may have other
  derivations. Two known ways:
  - *Counting:* keep a count of derivations per tuple. Exact for
    non-recursive rules; wrong for recursive ones (a cycle supports
    itself).
  - *Delete and re-derive (DRed):* over-delete every tuple with a
    derivation through a deleted link, then re-derive those that still
    have another. Correct for recursion, and the cost is in proportion to
    what the deletion could have affected.

  Start with DRed for recursive strata and counting for the rest.
- **Paths** are rules: `p+` is a two-rule relation, and a path expression
  is one relation per automaton state. A standing query with a path lowers
  it to rules and maintains those. A path from a bound start keeps only
  the tuples of that start.
- **Negation** is stratified already; a stratum is maintained after the
  strata it reads, treating their changes as its delta.
- **Result:** the same `Changes` (rows added and removed) a standing query
  gives now, so `MaintainedSubgraph` and `maintain T into g where P` work
  over rules and paths unchanged.

## Tasks

- [ ] Keep relations between commits in a standing query; insertions by
      semi-naive rounds from the commit's links
- [ ] Deletions: counting for non-recursive strata, DRed for recursive
- [ ] Lower `p+`, `p*` and path expressions to maintained relations
- [ ] `maintain ... where` over rule calls and paths in GraQL
- [ ] A test that, for random insertions and deletions, the maintained
      rows equal the query evaluated again
- [ ] A benchmark: a closure over a call graph of 10,000 nodes, one link
      added and one removed, against evaluating again

## Open questions

- Memory: a maintained closure holds its tuples. A closure over the whole
  graph can be quadratic. Should a standing query require a bound start,
  or a limit on tuples above which it falls back to evaluating again?
- Should maintained relations be stored (as links in a subgraph, which
  `MaintainedSubgraph` already does for a template) so they survive a
  restart, or rebuilt on open?
