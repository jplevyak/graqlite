# Graphs as results, with derived links

**Status:** closed · **Size:** large · **Related:** [014](014-subgraphs.md) (subgraphs, which take graphs as input), [016](016-graql-gaps.md), [007](007-path-expressions.md) (paths), [001](001-persistent-rules.md) (rules)

## Problem

In GraQL, `LOOKUP` returns a subgraph: "all triples with a subject … and
the corresponding nodes".  A result can hold
links that aren't stored but derived, and can then feed other statements.
The examples show three ways links are derived:

- **Asserted** ("condensed") with `!`: `?john_smith is_alive true!` adds
  the triple to the result for each match, and `@` gives the scope it's
  asserted per (`?john_smith //pet_lover true! @?pet`).  Rules assert the
  same way: `current_spouse` ends with `person //current_spouse ?other!`.
- **Aggregated:** `COUNT(?grandparent //number_of_grandkids ?grandchild)`
  derives a link from each grandparent to their count.  `ORDER(?marriage
  //marriage_order ?marriage_from) @person` derives each marriage's rank
  among a person's.  A later part of the same pattern can match the
  derived links: `?marriage //marriage_order (= 0)`.
- **By paths and rules:** `[name "John Smith"] /people/person/parent++
  ?ancestor` follows stored links to a derived ancestor edge.

Here, only part of that exists:

- `lookup pattern` returns the stored links its matches' positive triples
  use, as `Output::Links(Vec<u64>)`, without the nodes.  It finds them after
  the query, by looking up each triple of each row again: one more index
  lookup per triple per row.
- **Paths, path expressions and rule calls give nothing.**  With
  `/a knows /b . /b knows /c` and `rule reach(?x ?y) ?x knows ?y`, each of
  `lookup /a knows++ ?y`, `lookup /a (knows / knows) ?y` and
  `lookup reach(/a ?y)` returns no rows.  The stored links each match
  follows aren't found, and the derived edge `/a knows++ /c` can't be
  returned at all, since a result holds only stored links' ids.
- **No `!` or `@`** ("predicate modifiers is not supported", "'@' is not
  supported"), and `COUNT(…)` and `ORDER(…)` are taken for rule calls
  ("unknown rule 'COUNT'").
- **No graph inputs.**  GraQL has results as graphs: `( lookup … )` in
  `from`, `into` and templates, and `set name = graph …`.  The rest is in
  [014](014-subgraphs.md) and [016](016-graql-gaps.md) (`TEST`).
- From Rust, a query returns rows; links are only available by selecting
  link variables (`<< :?l s p o >>`).

## Design

### A graph result

```rust
/// A result graph: its links (stored first, in id order, then derived) and
/// its nodes.
pub struct ResultGraph {
    pub links: Vec<ResultLink>,
    /// The entries the matches bind that aren't in a link of the result,
    /// such as those of `LOOKUP ?subjects`.
    pub nodes: Vec<u64>,
}

pub enum ResultLink {
    /// A link in the graph: its id.
    Stored(u64),
    /// A link derived from stored ones.
    Derived {
        fields: [ResultNode; 3],
        by: Derivation,
        /// The stored links it was derived from: one derivation, not all.
        support: Vec<u64>,
    },
}

/// A field of a derived link, which needn't be in the graph yet.
pub enum ResultNode {
    Entry(u64),
    /// A value's data: a count, a rank, or a value not in the graph.
    Value(Cell),
    /// A key not in the graph, such as an author's `//key`.
    Key(String),
    /// A new node, from a blank node (`[ … ]` or `_:x`) in an asserted
    /// triple: the same number is the same new node, within the result.
    Blank(u32),
}

pub enum Derivation {
    /// A path pattern: its predicate (`p++`), or its expression's text.
    Path(PathLabel),
    /// A rule's head, or a link a rule's body asserts.
    Rule(Relation),
    /// A triple asserted with `!`.
    Asserted,
    /// An aggregate's link: `COUNT`, `ORDER`, …
    Aggregate(Function),
}
```

- `Plan::execute_graph(g, values) -> Result<ResultGraph>` (and `Query::graph`,
  `Prepared::graph`): the links of the query's matches, deduplicated, and
  their nodes.
- GraQL's `Output::Links(Vec<u64>)` becomes `Output::Graph(ResultGraph)`.
  The shell prints a derived link with no id and its derivation in a column,
  and `\show` of one lists its support.
- **Materializing:** inserting a result graph (014's
  `insert graph G into g`) stores its derived links.  A `Value` field
  becomes the value with that data, a `Key` field the node with that key,
  created if needed, and each `Blank` number one new node.
- **As an overlay:** a result graph read as a graph (`from graph g`, 017)
  needs ids for its `Blank`, `Value` and `Key` fields; 017's overlay ids
  give them.

### Links recorded during matching

Rather than looking each triple up again after the query, the executor
records the links it matches:

- **Patterns** get hidden link variables, as `<< :?l s p o >>` has, so each
  row carries the ids of the links it matched.  Patterns already find the
  link before binding, so this costs a slot per pattern, not a lookup.
  Optional groups' links are kept when matched; negations' never are.
- **Paths:** the breadth-first search records, for each (entry, state) pair
  it reaches, the link it was reached by.  Each pair found then has a
  witness path: the links walked back from it.  That's one path per pair,
  the shortest, not every path, which can be exponentially many.  `p++`
  gives a derived link `s p o` with that support; a path expression gives
  one labelled by the expression.
- **Rules:** semi-naive evaluation records, with each tuple's first
  derivation, the stored links and tuples it came from.  A call's tuple is
  a derived link if the relation has arity 2 (subject, object), labelled by
  the relation; its support is the stored links reached through the tuples
  it came from.  Other arities give support only.

Recording costs memory per row and per tuple, so it happens only when a
graph is asked for (`execute_graph`, `lookup`); rows don't pay for it.

### Asserted links: `!` and `@`

A triple whose object is followed by `!` is not matched but asserted.  It's
instantiated from each row of the rest of the pattern and added to the
result as `Derivation::Asserted`, with the row's matched links as support.
A triple that's already stored and live comes back as its stored link.

- See [The scope: `@`](#the-scope-) for `@`.
- A rule whose body asserts links gives them, as `Derivation::Rule`, when
  it's called; the rule's head binds the variables they use.
- `!` writes nothing.  `insert T where P` writes the same triples, and
  inserting the result graph materializes them.
- `//key` in an asserted triple (an author-scoped key, see 014) that isn't
  in the graph is a `ResultNode::Key`.
- **Blank nodes make new nodes**, as in `insert` templates (which create
  one per match).  In an asserted triple, `[ … ]` and `_:x` are each a
  `ResultNode::Blank`, new for each instantiation (so, with a scope, for
  each of its values), and shared by the triples of that instantiation that
  name them.
  A rule asserting `?prop subject_meta [ cardinality ?in ]`, as the tour's
  `PROPERTY` would to define a property, gives a new meta node per
  property.  That's how the tour's "new nodes are created with the node
  keyword" is done: there's no `node` keyword (see 016).

### The scope: `@`

`@` follows an asserted triple (`… ! @?x`) or an aggregate's call
(`COUNT(…) @?x`), and names its *scope*: what the tour calls its "foreach
point", the variable the work is done separately for each value of.

- **A variable.**  `@?pet` is `@` and the variable `?pet`: the `?` is the
  variable's, not the operator's.  The tour's `@person` names the rule
  parameter `person`, which its rule declares without a `?` (016's rule
  parameters); its expanded version, whose parameter is `?person`, writes
  `@?person` for the same scope.  A constant after `@` (a value, key or id)
  is an error.
- **Instantiations.**  An asserted triple, or an aggregate, is instantiated
  once per distinct binding of its own variables and the scope's.  Without
  `@`, its own variables alone decide.
- **An unbound scope gives nothing.**  Rows where the scope is unbound (from
  an optional group, or `-`) instantiate nothing.  So `@` is a condition as
  well as a scope: in `?p name "John Smith" ; pet ?pet - . ?p pet_lover true!
  @?pet` only the John Smiths with a pet are pet lovers, and without `@?pet`
  every John Smith is.
- **Links are a set.**  Instantiations that give the same subject,
  predicate and object are one derived link, supported by the first one's
  links (one derivation, as for paths and rules).  So Dan, with three pets,
  is one `pet_lover` link, whose support is one pet's link.  The scope is
  seen in the result where an instantiation is new: a blank node in the
  triple is a new node for each (`?p pet_record [ of ?pet ]! @?pet` gives a
  record per pet), and an aggregate's value is computed for each.
- **On an aggregate**, the scope is added to the grouping, as SQL's
  `PARTITION BY` adds to a window's: `COUNT(s p o) @?x` groups by `s` and
  `?x`.  `COUNT(?parent //number_of_parents_grandkids
  ?grandchild) @?grandparent` counts each parent's grandchildren separately
  for each grandparent, rather than counting a parent's grandchildren once
  for each of the parent's own parents, as the join's rows would without
  it.  A scope the derived triple leaves out can give a subject and
  predicate several values: `(s n 3)` and `(s n 5)` are two links, both
  kept.  In the tour's example they're equal, and so one.
- **On `ORDER`** (and the other rankings), the scope is the group ranked
  within, SQL's `PARTITION BY`: `ORDER(?marriage //marriage_order ?from)
  @?person` ranks each person's marriages apart, so each person has a
  marriage of rank 0.  Without the scope, every marriage in the graph is
  ranked together, and one has rank 0.

### Aggregates as links

The built-in aggregators are calls whose arguments are a triple:
`COUNT(s p o)`, the rankings (`ORDER(s p key)` and its kin), and the rest
of `Function` (`SUM`, `AVG`, `MIN`, `MAX`, `SAMPLE`).

- They group the rows by `s` and the `@` scope, if any, and derive `s p v`:
  `v` is the aggregate of `o` over the group, as a `ResultNode::Value`.
- **Rows, as SQL and select's aggregates count:** `COUNT(s p o)` counts the
  group's rows binding `o`, and `COUNT(distinct s p o)` (as `count(distinct
  ?x)` in a select) its distinct values of `o`; likewise `SUM`, `AVG` and the
  others, with `distinct`.
- The rankings instead derive each subject's rank: see
  [`ORDER`: ranking](#order-ranking).
- **Derived links can be matched** by the rest of the pattern, as
  `?marriage //marriage_order (= 0)` does.  So the pattern is evaluated in
  strata.  An aggregate reads the rows of the pattern *without* the
  patterns that match its own derived links (by their predicate) and what
  depends on them; negations and filters are part of what it reads.  Then
  the aggregate; then the patterns matching its links.  As with negation in
  rules, a pattern whose aggregate depends on its own result is an error.
- Select's aggregates (`select ?x, count(?y) …`) are unchanged: those are
  columns, these are links.

### `ORDER`: ranking

Ranking follows SQL's window functions, the most familiar model (GQL,
Cypher and SPARQL have none: they rank with `ORDER BY` and `LIMIT` over a
whole result, or a subquery per group): `ORDER(?m //order ?from) @?person`
is SQL's `RANK() OVER (PARTITION BY person ORDER BY from)`, but for one
thing, ranks start at 0.

- **The functions:** `RANK(s p key [desc])`, of which `ORDER` is another
  name; `DENSE_RANK`; and `ROW_NUMBER`.  `REVERSE_ORDER(s p key)` is
  `ORDER(s p key desc)`.  Each derives, for each
  subject `s` in each scope group, `s p r`: `s`'s rank `r` among the group's
  subjects ordered by `key`.
- **Ranks start at 0**, as array indexes do in C, Rust and Python, which is
  who GraQL is for (SQL ranks from 1): the first is rank 0, as the tour's
  `marriage_order (= 0)` has it.
- **The scope is SQL's `PARTITION BY`:** ranks are within each scope
  group, and without a scope, the subjects are ranked together.
- **The key is SQL's `ORDER BY`:** ascending; a trailing `desc`
  (`ORDER(?m //order ?from desc)`), as in `by ?x desc`, for descending.
  Keys compare as `select … by` and `min` / `max` compare them: numbers by
  value, other values by their data.
- **Ties**, as SQL's three functions treat them: `RANK` gives equal keys
  equal ranks and skips after them (`0, 0, 2`); `DENSE_RANK` doesn't skip
  (`0, 0, 1`); `ROW_NUMBER` gives each subject its own (`0, 1, 2`), equal keys
  ordered by the subjects' ids, so that it's deterministic (SQL leaves that
  order unspecified).  `ROW_NUMBER` is for "exactly one first", as SQL's
  "top one per group" uses it; `RANK`, the default, never hides a tie.
- **A subject with no key** (unbound, from an optional pattern) is ranked
  last, after those with keys, in either direction: SQL's `NULLS LAST`.  SQL
  never drops a row for a null key, and nor does this: a person whose only
  marriage has no start date still has that marriage at rank 0.
- **A subject with several keys** (two `from` dates, or rows the joins
  repeat with different keys) is ranked once, by its best key in the
  order's direction: as SQL's `GROUP BY s` with `MIN(key)` (or `MAX`, for
  `desc`) and then the rank.  SQL ranks rows; this ranks subjects, since `s p
  r` is a property of `s`, of which a subject has one per scope group.
- **Which rows are ranked** (the strata above): the pattern's but those
  matching the ranking's own links, so its negations are included.  In
  `current_spouse`, the `NOT ?marriage /people/marriage/to` comes before the
  ranking, so only ongoing marriages are ranked: a person normally has one,
  of rank 0 whichever way the order runs, and the tour's ascending rule is
  right.  (Ranking all the marriages and then dropping the ended ones would
  make rank 0 a person's first marriage, ended for anyone remarried, who
  would then have no current spouse.)

They could be ranked differently: the foreach ids themselves, across all of them,
by their least key, with equal keys overwriting each other (one ranked, the rest not),
keyless ones tied at the end, and a `reverse_order` that, starting every key at the
maximum, ranks every subject equal.  Nothing here depends on it.

### Graphs as inputs

- `set name = graph G` names a result graph for the session, and `graph
  name` uses it.  `SET`'s other forms, and the grammar's lack of
  `{ subjects }` in a `SET`, are in [016](016-graql-gaps.md).
- `TEST` compares two results, tables or graphs: in
  [016](016-graql-gaps.md), using result graphs' equality from here.
- Graphs as `from`/`into` sources and templates (`insert graph G into g`)
  are [014](014-subgraphs.md)'s phase 3, which takes `ResultGraph`.

## Phases

1. **Recording.**  Hidden link variables for patterns; `execute_graph` and
   `ResultGraph` with stored links and nodes; `lookup` on top of them,
   replacing the per-row lookups.  Tests: `lookup`'s links against those of
   each brute-force row.  Benchmark: `lookup` before and after.
2. **Derived links by paths and rules.**  Witness paths for path patterns
   and expressions, and tuples' derivations for rules; `Derivation`, and the
   shell's display.  Tests: each derived link's support is a path or
   derivation of stored, live links that gives it.
3. **Asserted links.**  `!` and `@`, in lookups and in rule bodies.
4. **Aggregates as links.**  `COUNT`, `ORDER` and the others, with strata so
   that later patterns can match their links; then `current_spouse` (fixed,
   see 016) as a golden test.
5. **Graphs as inputs.**  `set name = graph`, `graph name`, then 014's
   phase 3.

## Phase 1, as implemented

- `Query::record_links` marks a query to record its links.  Planning it
  plans a copy whose patterns (in the top group, unions' branches and
  optional groups, not negations) each bind a link variable: their own, or
  a hidden one (`?_link0`, …), which `explain` shows.  Rows are unchanged.
- Unions' branches may now differ in their hidden variables: `check`
  ignores them, as it does parameters, and a union's variables are those of
  all its branches, so a branch not taken leaves the others' unbound (0).
- `ResultGraph { links, nodes }` and `ResultLink::Stored` (non-exhaustive,
  for phase 2's derived links).  `nodes` are the selected entries, other
  than link variables' values, that aren't a subject, predicate or object of
  a result link.
- `Plan::execute_graph` (and `_values`), `Query::graph`, `Prepared::graph`
  (and `_values`).  A standing query can't record links.
- GraQL's `Output::Links` is `Output::Graph(ResultGraph)`.  `lookup` lowers
  to a recording query, so it no longer looks each triple up again, and a
  prepared `lookup` is planned once like `select` and `verify`.  The shell
  prints a node as a row with only its subject.
- **Differences from before:** a chain path (`/ann (knows / knows) ?z`)
  lowers to patterns, so its links are now found (before, none); and a
  group `( p ) as of id` gives the links it matched then (before, the links
  live as of the statement's `as of`, or now).  Paths with repetition and
  rule calls still give none: phase 2.

Tests: `random_queries_recording_links` checks, for random queries, that
recording leaves the rows as brute force gives them (under each join), and
that the links are those brute force binds to link variables the test adds
itself (a mutation skipping optional groups fails it); and the nodes.
`lookup_records_its_links` checks GraQL's `lookup` against the old
triple-by-triple algorithm on nine statements, and the two differences.

| `benches/graql.rs`, 40 000 people | before | after |
|---|---|---|
| `lookup ?p a person ; city c0` (4 000 rows) | 8.46 ms | 3.85 ms |
| `lookup #id a person ; city ?c`, 1 000 times | 21.2 ms | 20.7 ms |

## Phase 2, as implemented

- **Marks.**  `PathPattern` and `Call` have a `mark`: a hidden variable the
  executor sets to 1 in each row the path or call matches (a `Marked`
  wrapper around their lookups), so a row says which of a union's or an
  optional group's paths and calls it followed.  `record_links` marks the
  paths and calls of the groups it records; GraQL's lowering marks those of
  a lookup's own pattern (not `_node`'s call, nor `where`'s).
- **Witnesses, after matching.**  For each row's marked path, a
  breadth-first search from its subject to its object over the path's
  automaton (`Automaton::witness`, the search `reach` does, with each pair's
  parent link) gives a shortest walk; `p++` is the automaton of `p+`.  The
  executor's own searches are unchanged.  One witness per derived link.
- **Rules.**  When the query records links, `rules::evaluate` plans each
  rule's body recording its links (rewritten before the calls' addresses
  are taken, so semi-naive evaluation's overrides still apply), and keeps
  each tuple's first derivation: the row's links, paths and calls
  (`Relations::derivations`).  A tuple's support is its derivation's links,
  paths' witnesses, and calls' tuples' support, memoized; the first
  derivation uses only tuples found before it, so this ends.
- **Results.**  `ResultLink::Derived { fields, by, support }`, with
  `ResultNode::{Entry, Value, Path}` and `Derivation::{Path, Rule(name)}`.
  A `p++` link's predicate is `p`; a path expression's, the expression; a
  rule's, its name.  Relations of two columns give links; others give only
  their support.  Support links are stored links of the graph too, and
  derived links come after them, deduplicated.
- **The shell** prints a derived link with no id, `p++` as its predicate for
  a transitive path, and a `derived` column, `path from #53 #83` or `rule
  reach from #53`, only when there are derived links.
- A chain path (`knows / likes`) is still lowered to patterns: stored links,
  no derived link.

Tests: `random_queries_recording_links` now also checks each derived link
(a path's support is a walk of live links from its subject to its object,
forward `p` links for `p++`; a rule's link is a tuple of the relation as
brute force evaluates it, with non-empty, live support), that every row's
paths and two-column calls of the top group have derived links, and that
the extra stored links are derived links' support.  Mutations dropping a
witness's last link, leaving calls unmarked, or giving tuples no support
each fail it.  `lookups_derive_links` checks GraQL's on fixed cases: `p++`,
a recursive rule, a path expression with `^`, a `where`'s path, and an `or`
branch not taken.  `examples/16-derived-links.graql` is the golden test.

The 4 000-row lookup of `benches/graql.rs`: 3.97 ms (3.85 after phase 1,
on a loaded machine).

## Phase 3, as implemented

- **Query API.**  `Assertion { triples, scope }`, built with
  `Assertion::new().triple(s, p, o).scope(v)`, of `TemplateNode`s: a
  `Term`, a `Value(Cell)` or `Key` that needn't be in the graph, or a
  `Blank(n)`.  `Query::assert` adds one to the query's graph, and
  `Query::rule_asserts` to the last rule added.  The rows are unchanged:
  assertions only show in `graph`.  `check` requires a query's assertions'
  variables to be bound by its patterns (optional ones included), and a
  rule's to be its head's.
- **Instantiating.**  For each row, each assertion is instantiated once per
  distinct binding of its variables and scope (none if the scope is
  unbound); a triple with a variable unbound is left out.  A value or key
  in the graph is its entry; a triple of entries that is live (as of the
  query's `as of`) comes back as its stored link, and the others as
  `Derived { by: Derivation::Asserted }`, supported by the first
  instantiating row's links, paths' witnesses and calls' support.  Each
  blank number is a new `ResultNode::Blank` per instantiation, numbered
  across the result.  Derived links are deduplicated, so the links are a
  set.
- **Rules.**  Assertion variables are the head's, so a rule's assertions
  depend only on its tuples: semi-naive evaluation records, for each tuple,
  each asserting rule that derives it (`Relations::asserting`), not only
  the first derivation's, so a tuple derived by two rules gets the
  assertions of both.  A call of the query's rows instantiates the
  assertions of the rules deriving its tuple, as `Derivation::Rule(name)`,
  supported by the tuple's support.  Only the calls of the query's own
  rows: a rule called by another rule asserts nothing.
- **GraQL.**  `s p o!` after an object (with its author and subgraph left
  out) parses to `Pattern::Assert(triples, scope)`: the triple, then a
  blank node object's own triples (`?p record [ animal ?pet ]!`), and the
  scope after `@`, a variable.  Lowering collects them at the top of a
  lookup's pattern or a rule's body; elsewhere (`or`, `not`, `optional`,
  `as of`, `in`, `where`, `select`, `verify`, templates) is an error, as is
  a variable not bound by the pattern.  A blank node the pattern doesn't
  bind is a new node.  The printer writes the blank node object back
  nested.  `@` after a call is still unsupported (phase 4).
- **Materializing.**  `insert graph ( lookup … )` now writes asserted links
  too, and any derived link but a path's: a value or key made if needed, a
  new node for each blank number.
- **The shell** prints an asserted link's derivation as `asserted from
  #…`, a blank as `_:n`, a key as itself, and a value as a literal; and
  `true` and `false` are now printed bare, as GraQL reads them.

Tests: `random_queries_asserting` checks, for random queries with a random
assertion (variables, constants, a value in the graph and one not, the
query's own patterns, a blank node through two triples, and a scope),
that the stored and derived links are what brute force's rows give: the
distinct bindings with the scope bound, stored triples as their links,
each other triple derived once, and a blank node for each instantiation.
Mutations ignoring the scope, keeping triples with unbound variables,
deduplicating by fewer variables, or not finding stored links each fail it.
`asserting` checks the API's cases, rules' assertions with keys and
blanks, and a tuple derived by two rules; `lookups_assert_links` GraQL's,
with materializing and the errors.  `examples/18-asserted-links.graql` is
the golden test.  Of the tour's statements, the two lookups asserting
links now run (37 of 68).

## Phase 4, as implemented

- **Query API.**  `LinkAggregate { function, subject, predicate, object,
  distinct, descending, scope }`, with `LinkFunction::{Fold(Function),
  Rank, DenseRank, RowNumber}`; `Query::link_aggregate` and
  `Query::rule_link_aggregate`.  A query's rows are those of its patterns:
  an aggregate whose links nothing matches leaves them as they are, and
  shows in its graph.
- **Planned as rules** (`query/link_aggregate.rs`).  Planning rewrites each
  group with aggregates (the query's top group, a rule's body): each
  aggregate gets a relation `(s, v[, scope])`, defined by a rule that
  folds (`Rule::aggregate`) the rows of its *base*, and the group's
  patterns with the aggregate's predicate (in the groups within too)
  become calls of it; with a scope, of a relation `(s, v)` projecting it,
  so that each link is matched once.  The base is the group without the
  calls of its own links' relation, nor of those of the aggregates after
  it, and, until no more are, the parts using variables only those bind.
  Strata: an aggregate rule's dependencies count as negated, so it is
  evaluated after what it reads, alone.
- **Order of strata: as written.**  The design left open how aggregates
  reading each other are ordered.  Working it out from the dependencies
  doesn't do: an aggregate's base, before it is known what reads what,
  contains the patterns matching the links of those that read it, which
  makes a cycle of every chain.  So an aggregate reads the links of the
  aggregates before it, never its own or later ones': written order is the
  strata, and there are no cycles.
- **Folds** reuse `aggregate::fold` per subject and scope, over the rows
  of `(s, o, scope)` (rows with `s` or the scope unbound fold nothing).
  **Rankings** group by scope, take each subject's best key (`compare_cells`,
  least, or greatest for `desc`), sort with keyless subjects last, and
  number as `RANK`, `DENSE_RANK` or `ROW_NUMBER` (ties by subject id).
- **Values.**  A count or rank is a value: the graph's, if it has one, or
  else an *ephemeral value* (`Graph::ephemeral_value`): an id with the top
  bit set, held in memory by the graph and never written, whose data
  `Graph::data` reads, so it compares, sorts and prints as a value.
  Lowering gives constants missing from the graph ephemeral ids too
  (values and keys, `Graph::ephemeral_key`), so that `(= 0)` matches a
  computed rank by id, and an aggregate's predicate, as a `//key`, needn't
  be in the graph; prepared parameters likewise.  `insert … where` writes
  an ephemeral value as a value, and `Graph::add_link` refuses its id
  (it is past the log).  This is the start of 017's overlay ids.
- **Results.**  `Derivation::Aggregate(name)`; each row gets the links of
  the top group's aggregates for its subject and scope, supported by the
  links of the rows folded (merged provenance, as the tuple's
  derivation).  Ephemeral fields are `ResultNode::Value` or `Key`, and
  `insert graph` writes them.
- **GraQL.**  `COUNT([distinct] s p o [desc]) [@?x]` and the other names
  parse to `Pattern::LinkAggregate`; in any statement's pattern or rule
  body, at its top.  Standing queries (`maintain`) refuse aggregates.
- **Fixed along the way:** a comparison with a parameter or constant not in
  the graph (`?a != $x`) was false for every row; it is now true, as for
  any other value it differs from.  And the query tests' random links are
  in id order, so that their random queries are the same each run (they
  depended on a hash map's order, and one test's coverage count sometimes
  fell short).

Tests: `random_link_aggregates` checks, for random queries with a random
aggregate (each function, `distinct`, `desc`, a scope) and a pattern
matching its links, that the rows are brute force's rows joined with the
aggregate's tuples as an oracle works them out independently (a rank as
the number of subjects before), and that its graph's aggregate links are
of those tuples.  Seven mutations (skipping ranks, keyless first, `desc`
ignored, the scope ignored, `distinct` ignored, ties by id reversed, the
best key the worst) each fail it.  `link_aggregates` checks the base's
exclusions (a comparison of the rank, a negation ranked), an aggregate
reading an earlier one's links, a later one's not, errors, the graph's
links and support, and a rule's.  `aggregates_as_links` checks GraQL's,
`current_spouse`, inserting ranks and the errors;
`ephemeral_values_and_keys` the graph's.  `examples/19-aggregate-links`
is the golden test.  The tour's two `COUNT` lookups now run (39 of 68);
its `current_spouse` rules need 016's rule parameters (`person`).

## Phase 5, as implemented

Graphs as inputs were done with 016's `SET` and 017's overlays: a graph
read as input is an `Overlay` (see 017), and a result graph becomes one
with `Overlay::of_graph`: its stored links' facts, and its derived links
(but paths', whose links are there) with values, keys and blank nodes not
in the graph as ephemeral ids.

- `set name = graph ( lookup … )` names a lookup's graph, alone (not over
  the database); `select … from graph name`, `lookup` / `verify … in graph
  name` read it; `insert graph name` (and `insert graph ( lookup … )`, as
  before) writes it, now through `Overlay::insert_into`, which makes its
  ephemeral values, keys and nodes.
- Reading a graph, the links of the overlay a lookup matches are
  `Derivation::Overlay` links (they aren't stored, so have no ids the log
  knows); the shell labels them `overlay`, and shows ephemeral nodes as
  `#~n`.

`graphs_and_symbols` checks a lookup's graph named, read and inserted;
`examples/20-symbols-and-graphs` is the golden test.

## Closed

All five phases are done, and the issue's examples are golden tests:
`16-derived-links` (phases 1 and 2), `18-asserted-links` (3, with the
tour's Man Booker query), `19-aggregate-links` (4) and
`20-symbols-and-graphs` (5).  How it differs from the plan:

- Aggregate strata are in written order: an aggregate reads the links of
  those before it (see phase 4).
- Values a query computes, and constants not in the graph, are ephemeral
  values held by the graph, which became the start of 017's overlay ids.
- The tour's Man Booker query runs with the winners optional (`-`), not in
  the tour's `lookup - { … }` form, whose default modifiers are 016's
  phase 4; and its `current_spouse` rules need 016's rule parameters.

The open questions below remain open, for later issues if they matter.

## Open questions

- **All paths, or one?**  A witness per matched pair is cheap; every path
  isn't.  A bound (`lookup … paths 10`) could give more for small cases.
- **Materialized support.**  When a derived link is inserted, should its
  support be stored too, as links about the new link (`<< :?l … >>
  derived_from ?s`)?  That makes materialized links explainable, at a cost
  of links per support.
- **Rules of other arities** give support but no link.  Should a ternary
  relation be a link with a subgraph, or an author, in its third position?
