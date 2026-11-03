# Graph layering: `DATA`, `DEFAULT`, `base:graph`, and overlays

**Status:** closed · **Size:** large · **Related:** [014](014-subgraphs.md) (subgraphs, `ASSUME`), [015](015-graph-results.md) (result graphs), [016](016-graql-gaps.md) (`SET` of graphs)

## Problem


```
-- Set the data graph to be empty.  The data graph may be set by the
-- server for a client-server configuration.
DATA {}

-- Set the default graph which all graphs will extend.
DEFAULT {
  PROPERTY(/film/performance/actor 0 0 "").
  …
}

-- A base graph can be prepended via :
SET film_stuff = film_stuff:{ //ex_annie_hall /film/film/directed_by //ex_woody. }

-- Setting the default at the end is a no-op.
DEFAULT {}
```

So a graph can extend another, and a session has two of them:
- the **data graph**, which it reads and writes;
- the **default graph**, which every graph extends, such as a schema.

`ASSUME` (see [014](014-subgraphs.md)) adds links that are visible only
where the statement runs.  That's the same idea: links layered over a
graph without being written to it.

Here, `DATA`, `DEFAULT` and `ASSUME` are "not supported", and `base:graph`
doesn't parse in a `SET` (see 016).  There is one graph: the
database.

## Design: overlays

A layer is either the database or an **overlay**: a set of links held in
memory (a [015](015-graph-results.md) `ResultGraph`, whose derived links
are the overlay's own), over a base layer.

- **Reading** a layered graph reads its links and its base's, down to the
  database.  An overlay's links shadow the base's for the same SPO, so an
  overlay can delete a fact (a dead link in the overlay) as well as add one.
- **Matching:** each overlay has a small in-memory index by subject,
  predicate and object.  A lookup reads the database's index entry and the
  overlays', and liveness is computed over all of them, the overlay's links
  counting as later than any stored one.  Plans are unchanged; estimates add
  the overlays' counts.
- **Writing** goes to the top layer: to the database if it's the data
  graph, else to the overlay.  An overlay is never written to the database
  unless inserted (`insert graph G`, 015).

The tour's forms then mean:
- **`base:G`** is the graph `G` (an overlay) over `base`.  `film_stuff:{ … }`
  layers the new links over the earlier `film_stuff`.
- **`DEFAULT G`** sets an overlay that every graph the session reads is
  layered over: the database (or the data graph) and every named or literal
  graph.  It applies to the statements after it, so setting it last does
  nothing ("a no-op").
- **`DATA G`** sets the data graph: the database (the default), or an
  overlay, such as `{}`, to work on scratch data that is never written.  A
  server could set it for its clients.
- **`ASSUME T`** adds `T` to an overlay for the transaction, and
  `ASSUME T in g` to subgraph `g` within it.

### API

- `query::Overlay`: links (live or dead) with their own index, built from a
  `ResultGraph` or from templates.
- `Plan::execute_over(g, &[&Overlay], values)`, and the same for
  `execute_cells` and `execute_graph`: the overlays from the bottom up.
  Without overlays, execution is unchanged and costs nothing more.
- `graql::Session` keeps the default graph, the data graph, the session's
  `ASSUME`d links, and named graphs (`SET name = graph`, 016), and runs each
  statement over them.

## Phases

1. **Overlays in queries.**  `Overlay` with its ids, liveness over layers,
   `as of` over layers, and `execute_over`.  Tests: random queries over the database plus a random
   overlay, against brute force over their union, with the overlay's dead
   links deleting stored facts.
2. **GraQL.**  `DEFAULT`, `DATA`, `base:{ … }` (with 016's `SET`), and
   `ASSUME`.
3. **Rules in the default graph.**  The tour's `DEFAULT { PROPERTY(…) }`
   calls rules whose bodies assert links (015's `!` in rules), so the
   default graph is the links they assert.  Needs 015's phase 3.

## Phase 3, as implemented

With 016's template rules: a call of a rule that only asserts triples, in
a graph literal (`DEFAULT { PROPERTY(…) . }`, `SET g = { … }`), is that
rule's triples with the call's arguments, a new node for each blank node.
So the tour's default graph of properties is a graph of the links its
`PROPERTY` calls assert, once `PROPERTY` asserts them (the tour's own
matches, asserting nothing).  `examples/21-graph-layering` ends with it.

## Closed

All three phases are done; `examples/21-graph-layering` is the golden
test.  How it differs from the plan: overlays are merged into one before
a statement runs (a layered graph is one overlay over the database or
nothing), and `ASSUME` lasts the session outside a `\begin` transaction.

## Phases 1 and 2, as implemented

- **`query::Overlay`**: one link per fact (live, or dead: deleting the fact
  beneath), with its author and subgraphs, indexed by subject, predicate,
  object, subgraph and author; over the database (`Overlay::new`) or
  nothing (`Overlay::alone`).  `set`, `insert`, `delete`,
  `remove_from_subgraph`, `live` (a fact's live link beneath it), `over`
  (layering, taking the lower's base), `of_graph` (from a result graph) and
  `insert_into` (written, making its ephemeral values, keys and nodes).
- **Ids** are 015's ephemeral ids, extended: `Graph::ephemeral_node` gives
  anonymous ones, for an overlay's links and new nodes, and values and keys
  have theirs.  `#~n` in the shell.
- **Execution.**  `Plan::execute_over` (and `_cells_`, `_graph_`;
  `Query::run_over`, `cells_over`, `graph_over`) runs a plan beneath an
  overlay, which the executor's context hands to each pattern's matcher and
  path search.  A matcher reads the database's live links as before (none,
  over nothing), drops those of facts the overlay has, and adds the
  overlay's live links that may match; a path's steps and witnesses
  likewise (`overlay::step_links`).  So liveness is by layer: the
  overlay's state of a fact wins, whatever the ids.  Reduction is off, and
  a large index entry isn't streamed, beneath an overlay; rules (and link
  aggregates) are evaluated beneath it.  Without one, execution is as
  before.
- **History:** a pattern `as of` an id reads the database as of then and
  the overlay as it is.
- **GraQL** (`Session`): `data G` (`database` for the database), `default
  G` (`default {}` clears it), `assume T [in g]`, `set name = graph G`,
  `from graph G` / `in graph G`, and `insert graph G`, which writes into
  the data graph (the database, or its overlay).  A statement reads its
  graph, or the data graph, over the default graph, beneath the links
  assumed.  Inserts and deletes go to the data graph: with `data {}`, to its
  overlay, with ephemeral ids for what the database hasn't.  `maintain`
  refuses to run with any of these set; prepared statements run unprepared
  beneath them.
- **`ASSUME`'s extent, refined:** until the transaction ends, which is the
  shell's `\commit` or `\rollback` of a `\begin` (`Session::end_transaction`);
  outside one, statements commit one by one, so assumptions last the
  session.  They are over every graph read, `in graph database` too.

Tests: `random_queries_over_overlays` checks random queries beneath random
overlays (over the database or alone, with new nodes, live links and dead
ones deleting stored facts) against brute force over the layered links,
under each join, and their graphs' overlay links.  Mutations (overlay links
not added in path steps, dead overlay links kept, the database read under
an overlay alone) each fail it.  `overlays` checks the API;
`graphs_and_symbols` GraQL's, with `DATA`, `DEFAULT`, `ASSUME` and
prepared statements.  `examples/21-graph-layering` is the golden test.
Phase 3 (the tour's `DEFAULT { PROPERTY(…) }`) needs calls in graphs.

## Decisions

These were open questions; the answers are what the design above assumes.

- **The data graph governs reads and writes.**  It is what a session's
  statements read (over the default graph) and where its inserts and
  deletes go.  So `DATA {}` gives a session scratch data: the tour's inserts
  go to the empty overlay, and the database is neither read nor written.
  A server can set the data graph for its clients, as the tour says.
- **A named graph sees only its own links, over the default graph.**
  `from graph g` doesn't see the data graph's links; to see them, layer
  `g` over the database with `base:`.  So a graph's contents are what its
  definition says, wherever it is read.
- **`database` names the stored graph.**  It is reserved (`SET database
  = …` is an error) and goes wherever a graph name does: `DATA database`
  makes the database the data graph again, `database:{ … }` layers links
  over it, and `from graph database` or `lookup … in database` reads it
  even while `DATA` is set to something else.  It always means the stored
  graph, never the current data graph.  The grammar has no such name
  (`DATA` takes only a template, and a base is any identifier), so here
  `DATA` takes a graph: a name, `{ … }`, or `base:graph`.  The default graph
  needs no name, since every graph is layered over it; `default` (and
  `data`, for the current data graph) can be reserved later if needed.
- **The default graph is read-only through statements.**  Inserts go to
  the data graph, not the default graph.  Every graph extends the default
  graph, so a fact written there would appear in every graph: right for
  the shared definitions it holds, such as the tour's `PROPERTY`
  declarations, but not for ordinary data.  It changes only by `DEFAULT`.
- **`ASSUME` lasts for the transaction.**  The tour's "execution context"
  is closest to a transaction: the assumed links are seen by the statements
  after it until the commit or rollback, and then dropped.  They are never
  written.
- **Overlays have no history.**  `as of N` reads the database's layers as of
  `N` and the overlays as they are now: an assumption holds whenever you
  look.  (See overlay ids below for what this asks of the executor.)
- **Overlays are not subgraphs.**  A subgraph (014) is a tag on some of one
  graph's live links; an overlay adds and removes links without writing
  them.  An overlay's links can still be in subgraphs (`ASSUME T in g`):
  the tags are part of the overlay's links.

## Overlay ids

Rows, indexes and joins all work on `u64` ids, and overlays are full of
entries that aren't in the log: new nodes (blank nodes, new keys such as
the tour's `//ex_annie_hall`), new values, and the overlay's links
themselves, which link variables and link nodes (`<< … >>`) can bind.  So
overlay entries get ids too.

- **Existing entries keep their ids.**  Values are interned by their data
  and keys are unique, so an overlay first resolves its values and keys
  against the database.  Only entries that are really new get overlay ids.
- **New entries get ids with the top bit set**, which can never collide
  with log ids (byte positions).  An overlay keeps its own small tables
  from data to value and from key to node.  Lowering a query looks
  constants up in the overlays as well as the database, so a query can
  name a value that exists only in an overlay.
- **Resolving an id checks its range.**  Reading a record, its data or key,
  a link's fields, `\show`, and the shell's naming all ask the overlay for
  ids in that range.  This is most of phase 1's work.
- **Liveness goes by layer, not by id.**  An overlay link shadows the
  base's links of the same SPO because its layer is above, not because its
  id is larger: "latest by id" becomes "top layer first, then latest by id
  within the database".
- **`as of` skips overlay ids.**  Overlay ids are larger than any log id, so
  the code that cuts links off by id range must let them through, or they
  would vanish from every `as of` query.
- **Materializing maps them to real ids.**  `insert graph G` makes a real
  entry for each overlay id (new nodes, interned values, created keys) and
  rewrites the links, as `Graph::import` does for entries numbered
  elsewhere; it should reuse that.
- **Overlay ids last the session.**  They mean nothing to another session
  or after a restart, so the shell shows them differently, as `#~1`.

015's phase 4 started this: `Graph::ephemeral_value` and
`Graph::ephemeral_key` give values and keys not in the graph ids with the
top bit set, held by the graph in memory, which `data` and `key` read.
Overlays' values and keys can be those; their nodes and links need more.

015's derived links have the same problem: `ResultNode::Blank`, `Value`
and `Key` are new entries without ids.  When a result graph becomes an
overlay (`SET g = graph (lookup …)`, then `from graph g`), those become
overlay ids, so both issues share this mechanism.

Allowing overlays only between existing entries would avoid all this, but
it rules out the tour's `DATA {}` followed by inserts, and `ASSUME` of any
new fact, so it isn't an option.
