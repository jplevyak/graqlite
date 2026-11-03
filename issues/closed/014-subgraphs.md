# Subgraphs: materializing query results into tagged links

**Status:** closed · **Size:** large · **Related:** [015](015-graph-results.md) (graph results, which phase 3 takes as input), [016](016-graql-gaps.md), [003](../003-queries-as-graph-data.md) (queries as data), [013](013-prepared-queries.md) (prepared queries), [010](010-language-comparison.md) (SPARQL's named graphs)

## Problem

GraQL can materialize subgraphs (`AUTHOR`, `SUBGRAPH`, `LOOKUP ? ? ? me` and `LOOKUP ? ? ? ? me`
are examples). `insert T into g where P`
wrote a query's results into a subgraph `g`, `select … from g` and
`lookup … in g` queried within one, and `insert graph (lookup …) into g2`
copied one.

| GraQL | What it does | Here |
|---|---|---|
| `insert T into g [where P]` | writes `T`'s triples into subgraph `g` | "a named graph is not supported" |
| `delete T from g [where P]` | removes them from `g` | same |
| `select … from g`, `lookup`/`verify … in g` | matches within `g` | same |
| `( lookup … )`, `{ triples }`, `name: graph` as a graph | a query's links, or literal triples, as a graph | "a graph template is not supported" |
| `insert graph G into g2` | copies a graph | same |
| `s p o author subgraph` | a triple's author and subgraph fields | "author and subgraph positions" |
| `subgraph g`, `author a` | defaults for later inserts | "'subgraph' is not supported" |
| `LOOKUP ? ? ? me`, `LOOKUP ? ? ? ? me` | links by author, and by subgraph | "author and subgraph positions" |
| `//key` | a key scoped to the author: `/u/<author>/key` | taken as the key `//key` |
| `assume T in g` | hypothetical additions | not covered here |

The storage layer already has what this needs.  Every link record has any
number of subgraph fields (`LinkFields::subgraphs`), and the Subgraph index
indexes each one.  What's missing is everything above it.

The obstacle is the data model.  A link's identity is its subject,
predicate and object (its SPO), and `add_link` returns the existing link
when the SPO's latest link is already live.  So a fact that's already
live can't be put into a subgraph: the subgraph fields are those of
whichever record is latest.

## Design: subgraphs as tags on links

There is one notion of a fact's liveness, and a subgraph is a set of tags over the live facts.

- **The main graph** is every live link, as now.  Queries that don't name a
  subgraph see it and are unchanged.
- **Membership:** a fact is in subgraph `g` (an entry's id) if the latest
  link of its SPO is live and lists `g` among its subgraphs.  So `g` is
  always a subset of the main graph, and membership has history like
  liveness: as of an id, it is that of the latest link at or before it.
- **Adding to `g`:** append a live link with the SPO and the union of the
  latest link's subgraphs and `g`, unless the latest is live and already
  has `g` (then return it, as `add_link` does).  If the fact isn't live,
  this adds it to the main graph too.
- **Removing from `g`:** append a live link with the latest's subgraphs less
  `g`, if it has `g`.  The fact stays in the main graph and its other
  subgraphs.
- **Deleting** a fact (`delete_link`) appends a dead link, which removes it
  from every subgraph.  Adding it again starts with no subgraphs, unless
  added to one.
- **Plain adds keep tags.**  `add_link` with no subgraphs, of a live fact,
  returns the latest link as now, so it doesn't drop the fact's tags.
  `add_link` with subgraphs takes the union, like adding to each.
- **Names:** a subgraph is any entry.  GraQL names one as a value (`into
  friends` is the value with data `friends`, as a predicate is), or by
  `/key` or `#id`.

A tag change appends a new link for the SPO, so the fact's link id changes
whenever its subgraphs do, as it does today when a fact is deleted and
added again.

### Authors

The examples treat the author like the subgraph: a position in a triple,
and a session default.  Unlike subgraphs, a link has one author, and it
isn't a tag: it's who wrote the latest link of the fact.

- **Matching:** `Pattern` gains `author: Option<Term>`, matched against the
  latest live link's author, the way `graph` is matched against its tags.
  `LOOKUP ? ? ? me` is all links whose latest record `me` wrote.  The Author
  index can find them when the author is bound and its entry small, as the
  Subgraph index does for graphs (with the same check that each candidate is
  the latest of its SPO).
- **Writing:** `author a` sets the session's author, written in each link it
  inserts.  Adding to or removing from a subgraph appends a link, which
  takes the session's author, so "the author" is whoever changed the fact
  last.
- **Author-scoped keys:** `//foo` is the key `/u/<author>/foo`, where
  `<author>` is the session author's id.  So each author has a namespace for
  keys (`//pet_lover`, `//number_of_grandkids` in the examples).  With no
  session author, `//foo` is an error.  Lowering resolves it like any key.
- **Policy:** the examples say the author and subgraph are "presumably one
  of a set permitted through authentication and policy", and set the
  default subgraph to the author.  Authentication and permissions are out of
  scope here; the session takes what it's given.

### Graph API

- `Graph::add_to_subgraph(s, p, o, g) -> Result<u64>`, and
  `remove_from_subgraph(s, p, o, g) -> Result<Option<u64>>`, as above.
- `Graph::in_subgraph(link, g, as_of) -> bool`.
- `Graph::subgraph_links(g, as_of) -> IdSet`: the Subgraph index's entry
  for `g`, keeping each link that is still the latest live link of its SPO.
  A removal's link doesn't list `g`, so it isn't in `g`'s entry, and the
  entry doesn't hold every link of each SPO.  Each candidate therefore needs
  a lookup of its SPO's latest link, as compaction's keep rule does.  It's
  one SPO index lookup per candidate, about 0.5 µs (`find_link` in
  `benches/lookups.rs`).
- `Graph::subgraphs_of(s, p, o, as_of) -> Vec<u64>`.

### Queries

- `Pattern` gains `graph: Option<Term>`: the link must be in that subgraph.
  A constant or parameter restricts the pattern to one subgraph; a variable
  binds each subgraph the matched link is in (SPARQL's `GRAPH ?g`).
- `Group::in_graph(term)`, and `Query::in_graph`, set the graph of every
  pattern of the group that doesn't name its own.  That's what GraQL's
  `from g` and `in g` lower to.
- **Matching:** the indexes still find candidate links by subject,
  predicate and object, and liveness is computed as now.  Membership is then
  a test of the live link's record, which the executor has already read.
  When the graph is bound and its entry is small, the planner can instead
  read the Subgraph index (a new `Access::Graph`), checking each candidate is
  the latest of its SPO and matches the pattern.  The estimate comes from the
  entry's count.
- **Not at first:**
  - Reduction skips patterns with a graph.
  - Standing queries reject them.  A tag change appends a live link
    superseding a live link, which the delta computation has never seen.
  - Paths take no graph.

### GraQL

- `insert T into g [where P]`, `delete T from g [where P]`.
- `select … from g`, `lookup … in g`, `verify … in g`.
- **Graphs as templates and patterns:** `( lookup … )` is the links a lookup
  finds, `{ triples }` literal ones, and `name: graph` names one for later in
  the statement.  So `insert graph (lookup ?p knows ?q where ?p city paris)
  into paris_friends` copies a query's links into a subgraph.
- **Positions:** `s p o author subgraph` sets the fields of one triple of a
  template; in a pattern, they match them (`LOOKUP ? ? ? me`).
- **Defaults:** `subgraph g` and `author a` set the session's defaults for
  later inserts; `//key`s resolve against the session's author.
- **Shell:** `\info` counts the subgraphs, and `\subgraphs` lists them with
  their sizes.

## Phases

1. **Write semantics and the Graph API.**  Union on add, removal,
   `delete_link` clearing tags, membership as of an id.  Tests: random adds,
   removes and deletes against a model of each subgraph's members, across
   commits, rollback, snapshots and compaction.
2. **Queries.**  `Pattern::graph` and `Pattern::author`, `in_graph`,
   membership and authorship checked on matching, and `Access::Graph` (and
   the Author index's equivalent).  Tests: the random query test with graphs,
   against brute force.  Benchmark: a query in a subgraph by each access.
3. **GraQL.**  `into`/`from`/`in`, positions, defaults, `//key`s, and an
   `examples/14-subgraphs.graql` golden test.  Graph templates and patterns
   (`( lookup … )`, `{ triples }`, `name: graph`) take
   [015](015-graph-results.md)'s `ResultGraph` and wait on its phase 1;
   inserting its derived links materializes them.
4. **Maintained subgraphs**, perhaps a later issue: a standing query whose
   changes are written into `g` on each update, so `g` stays the
   materialized result of a query rather than a snapshot.  This needs
   standing queries to handle tag changes (see "Not at first").

## Phases 1 and 2, as implemented

**Phase 1** is as designed, in `Graph`:
- `add_link` of a live fact takes the union of the latest link's subgraphs
  and the new ones (an author or time alone changes nothing);
- `add_to_subgraph`, `remove_from_subgraph`, `in_subgraph`,
  `subgraph_links` and `subgraphs_of`, as of an id; and a `# Subgraphs`
  section in `graph`'s docs.

Also:
- **Statistics:** they count, per predicate, the live links that replace a
  live one (`PredicateStats::replaced`), so `live()` stays the facts live.
  The stats file is version 2; a version 1 file is ignored and the
  statistics counted again from the log, which finds each replacement by
  looking up its fact.
- **Standing queries** needed no change: a tag change is the link's
  replacement, so a query without link variables sees no change, and one
  with them sees the old link's row go and the new one's come.

**Phase 2**, in `query`:
- `Pattern::graph` and `Pattern::author` (`Group::in_subgraph`, `by_author`,
  `in_graph` for a group and the groups in it, and patterns added to it
  after), matched in the executor against the link's record: a constant, a
  bound variable, or an unbound one, which binds each subgraph the link is
  in (a row each) or its author (if it has one).
- `Access::Graph` and `Access::Author` read the Subgraph or Author index for
  a constant, checking that each link is its fact's latest and live; the
  planner weighs them (a lookup per link) against the other accesses.
- `explain` writes `(s p o) in g by a`.
- As planned: such queries aren't reduced, and standing queries reject them.

Tests: `random_subgraphs` (phase 1) checks random adds, removes, deletes and
re-links against a model of each fact's subgraphs, after each change,
through a rollback, as of each commit, after reopening with the statistics
recounted, in a snapshot, and after compaction, with `stats().live()` the
facts live; mutations skipping the union or the recounting fail it.
`random_queries_in_subgraphs` (phase 2) restricts random queries' patterns
to random subgraphs and authors over a graph with random tags, against
brute force under each join, and again with every constant one read
through its index (the planner chooses that rarely on small graphs);
mutations skipping the latest-link check or the subgraph test fail it.

## Phase 3, as implemented

- **Syntax:** `Pattern::Tagged(s, p, o, author, subgraph)` for the positions,
  and `Pattern::InGraph(p, g)` for `( p ) in g`, which is also what `select
  … from g`, `lookup … in g` and `verify … in g` parse to.  A statement's
  subgraph applies, in lowering, to each of its triples (and link nodes)
  that names none of its own, as `as of` does; a path searched within one
  is unsupported.  `Insert` and `Delete` have a `graph` (`into` / `from`).
- **Writes:** an insert gives each link its triple's author and subgraph,
  else the statement's `into g`, else the session's (`author`, `subgraph`),
  and counts the links it appended (a fact newly live or newly tagged).  A
  delete with a subgraph, its triple's or `from g`, removes from it; else
  it deletes.  A `?` author is none asked for.
- **`insert graph ( lookup … ) [into g]`** links the lookup's stored links and
  the links its rules derive (the predicate being the rule's name, as a
  value).  Links derived by paths aren't: their predicate is the path; the
  links they follow are.  Copying from a named graph waits on 016's `SET`.
- **Session:** `author a` and `subgraph g` (`Output::Default`), shown by
  `\info`; `//key` is rewritten to `/u/<author's id>/key` before a
  statement runs, in its nodes and path steps, an error with no author.
- **Shell:** `\subgraphs` lists the subgraphs with their facts, by reading
  every live link, since the Subgraph index's keys can't be listed; not in
  `\info`, which would read them every time.
- `Statement::bind` now binds a lookup's `where`, which it had missed.
- `tests/cli/unsupported.graql` used author positions as its unsupported
  example; it uses `implies` now.

Tests: `print_round_trips` covers the new statements and positions;
`subgraphs_and_authors_in_graql` checks a triple's author and subgraph over
the statement's and the session's, deleting from a subgraph not in the
graph, `//keys` with and without an author, a path within a subgraph, a
subgraph as a prepared parameter, and `insert graph` of a rule's links.
`examples/17-subgraphs.graql` is the golden test, from the pending script (its
`ASSUME` part is 017's).

## Phase 4, as implemented

- **`query::MaintainedSubgraph`** keeps a subgraph as the facts of a template
  (triples of constants and the query's selected variables) for each row of
  a `StandingQuery`.  `new` writes it so, removing what else is in it;
  `update` brings the standing query up to date, nets the rows' changes
  fact by fact (each fact counted by the rows giving it), and adds the facts
  that gained their first row and removes those that lost their last.
  Netting is what keeps it from feeding on itself: its own tags change the
  links of facts the query may match, a row change that gives the same
  facts, so nothing more is written.
- **GraQL:** `maintain T into g where P`, for the rest of the session
  (`Output::Maintained`, printed `MAINTAIN g (+n -m)`); `Session::update`
  brings the session's maintained subgraphs up to date, and the shell runs
  it after each commit, committing what it writes.  A template is triples:
  no blank nodes, links or positions.  `maintain` is a keyword: as an
  identifier starting a statement, a template's last triple would have taken
  it for an author (the positions make a triple's end open).
- **Not done:** maintainers aren't saved with the graph; they last the
  session.  A maintained fact that is deleted stays out until its rows
  change.

Tests: `random_maintained_subgraphs` maintains `?a q ?b` for random standing
queries through random changes and commits, against brute force each time
(a mutation removing a fact whenever its count drops fails it);
`maintained_subgraphs_settle` checks a template giving the very facts its
query matches through a link variable settles, and the subgraph's facts
not given are removed; `maintaining_subgraphs_in_graql` the statement and
`Session::update`.  `examples/17-subgraphs.graql` maintains one through the
shell's commits.

## Examples

[`examples/17-subgraphs.graql`](../../examples/17-subgraphs.graql) has this issue's
examples (phases 1 to 3).

## Open questions

- **Statements about links:** a link about a fact's link (`<< :?l s p o >>
  source ?src`) names one record.  When a tag change supersedes that
  record, the statement is about a link that's no longer the latest.
  Deleting and re-adding has the same effect today, but tag changes make it
  routine.  Should statements about links follow the fact (its SPO) rather
  than the record, or should a tag change carry them forward?
- **Subgraph relations:** the grammar's `graph <= g`, `= g` and `!= g`
  look like relations between subgraphs (inclusion, equality).
- **`assume T in g`:** hypothetical additions visible only to the
  statement, or to a session.  This needs an overlay the graph doesn't
  have; out of scope here.  [017](017-graph-layering.md)'s overlays are
  the same mechanism, so `ASSUME` may belong there.  The pending script has
  an example of each reading.
- **Compaction** keeps the latest link at or before the horizon of each
  SPO, so membership as of the horizon or later is kept.  The Subgraph
  index's entries are compacted like the others (`LINK_INDEXES` includes
  it), but a tag change at every commit grows the entries of busy facts.
  Measure in phase 1.
