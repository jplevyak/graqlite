# History and link variables in GraQL

**Status:** closed · **Size:** medium · **Related:** [003](../003-queries-as-graph-data.md) (`#id` syntax), [009](009-expressions-and-literal-types.md) (link fields)

## Problem

Two of graqlite's strengths are in the query engine but have no GraQL syntax:

- **History.** Ids increase as entries are appended, so an id is also a time.
  `query::Query::as_of` sees the whole graph as it was when an id was
  appended, and `Group::at` sees one pattern that way. GraQL can only query
  the present.
- **Statements about statements.** A link is an entry with an id, and links
  can have links as their subject or object. `Group::link_pattern` binds a
  variable to the id of the link a pattern matches. GraQL can't name a link,
  so it can neither query nor insert facts about links, such as when a
  friendship started or who asserted it.

## Design

### Link nodes: `<< s p o >>`

A link node is written as in RDF-star (the lexer already has `<<` and `>>`,
reported as unsupported today). It can go anywhere a node can, as a subject,
predicate or object:

```text
select ?who, ?year where << /alice knows ?who >> since ?year
insert << /alice knows /bob >> since 2019
select ?l where << :?l /alice knows ?x >>
```

- In a pattern, `<< s p o >>` matches the live link with that subject,
  predicate and object, and stands for that link's id. Unlike RDF-star's
  quoted triples, the link must be there: a link node asserts its triple.
- `<< :?l s p o >>` also binds `?l` to the link, in the same way that
  `[ :?x ... ]` names a blank node. `<< :_:x s p o >>` gives it a label.
- A link node needs no predicates after it: `<< :?l ?a knows ?b >>` on its
  own is a subject that just matches the link.
- Link nodes nest: `<< << /a knows /b >> since 2019 >> source /census`.
- **In templates:** `insert` links the triple (or finds it live already),
  then uses its id. `delete` deletes it like any other triple in the
  template. To delete only a statement about a link, bind the link in the
  `where` pattern:
  `delete ?l since 2019 where << :?l /a knows /b >>`.

The parsed form gets `Pattern::Link(name, s, p, o)`: a triple whose link is
bound to the variable or blank node `name`, just as `[ ... ]` becomes a blank
node plus triples. It lowers to `Group::link_pattern`, and it prints as the
subject `<< :?name s p o >>`, so printing stays canonical (see the printer in
`src/graql/print.rs`). Templates become lists of `Triple` and `Link`
patterns.

### `as of`

`as of N` takes an id: a number, or `#N` once 003 adds bound ids.

- **Statement:** the whole statement sees the graph as of `N`, rules
  included (relations are evaluated with the query's `as_of`).
  - `select ?x as of 120 where ...` (after `limit`, before `where`)
  - `verify as of 120 /a knows /b`, and the same for `lookup`
  - `as of` is followed by `of`, so it can't be confused with a future alias,
    `?expr as ?name` ([009](009-expressions-and-literal-types.md)).
- **Group:** `( P ) as of N` sees the patterns and paths of `P`, including
  those in its `or`s and `not`s, as of `N`, unless an inner group has its own
  `as of`. A call to a rule inside such a group is an error: a relation is
  evaluated once per query, so it can't be seen at another time.
- **Writes:** `insert` and `delete` change the present. Their `where`
  patterns may use group `as of`.
- **Printing:** the parsed form gets `Pattern::AsOf(Box<Pattern>, u64)` and
  an `as_of` on `Select`, `Verify` and `Lookup`.
- **The engine** needs `Group::path_at`, because paths have an `as_of` field
  but no way to set it.

### Finding ids

To use `as of`, people need ids. The shell's `\ids` and `#id` output already
show them. With link variables, a link's id is when it was made:
`select ?l where << :?l /alice knows /bob >>` gives the id to go back to.
`\info` shows the next id.

## Open questions

- **Wall-clock time** (done since, below): `as of "2026-10-01"` needs a map from time to id.
  Links carry a `when` time, but nodes and values don't, and the log has no
  index of where entries start for a binary search. That calls for a small
  time index: an entry each time a commit records its time and next id. This
  is left for later; ids come first.
- **Link fields:** a link's author and creation time, and dead links (the
  history of deletions), would be functions in
  [009](009-expressions-and-literal-types.md): `when(?l)`, `author(?l)`, and
  an `ever` modifier that matches dead links too.
- **Standing queries:** check how a link variable and an `as of` group work
  with them. `as of` in a standing query is probably an error, because the
  past doesn't change.

## As implemented

As designed, with these details:

- **Link nodes go in subject and object positions only.** In predicate
  position, `<<` keeps the grammar's meaning (a predicate modifier, still
  reported as unsupported). A link as a predicate would be rare.
- **`as of N` includes the entry with id `N`:** as of a link's own id, the
  link is there. An id after every entry sees the present.
- **`lookup as of N`** reports the links that were live as of `N`, not those
  live now.
- **`not << s p o >>`** parses, but like any pattern that is only a negation
  it needs a positive pattern beside it (the engine's rule for groups).
- **Precedence of errors:** a call in an `as of` group is reported before
  "unknown rule". The error suggests putting `as of` on the statement.
- **The parsed form:** `Pattern::Link` and `Pattern::AsOf`; `as_of` on
  `Select`, `Verify` and `Lookup` (which became struct variants); templates
  are `Vec<Pattern>` of `Triple` and `Link`. The engine gained
  `Group::path_at`.
- **Tests:** round trips for the new syntax, behaviour tests (`link_nodes`
  and `as_of`), `examples/07-history-and-links.graql`, and
  `tests/cli/as-of-rule-call.graql`.

## Follow-up: times

`as of` also takes an ISO 8601 time: `as of "2026-10-31T12:30Z"`, UTC
unless it gives an offset, which sees the graph as of the last commit at
or before then.

- **The index:** each commit records its time and next id in `Times`
  (`src/times.rs`), kept on disk in `{prefix}times`. Times never decrease,
  even if the clock does, so the file can be binary-searched.
- **Rebuilding:** the file is appended to without being flushed, since it
  can be rebuilt. When it is missing (older databases, a `--restore`),
  belongs to another log, or ends early (after a crash, or a torn record),
  the missing commits are rebuilt from the creation times of their links.
- **The API:** `Graph::id_at_time`, `Graph::time_of`, `Graph::times`, and
  `Graph::commit_at` (to give a commit's time, as when importing).
  `times::parse_time` and `times::format_time` handle ISO 8601.
- **The shell:** `\show` says when an entry was committed, and `\info`
  shows the span of the history. `\explain` shows a statement's `as of`.
  `GRAQLITE_TIME` fixes the time of every commit, so that the CLI tests'
  output is the same each run.

## Tasks

- [x] Lexer and parser: link nodes (named, labelled, nested, as subject or object), statement and group `as of`
- [x] `Pattern::Link` and `Pattern::AsOf`; templates as `Triple` and `Link` patterns; `as_of` on statements
- [x] Lowering: `link_pattern`, `as_of`, `at`, and a new `Group::path_at`; an error for calls in `as of` groups
- [x] `insert` and `delete` with link nodes in templates
- [x] Printing, with round-trip tests (saved rules use the same printer)
- [x] Tests: statements about links, `as of` before and after a delete, nesting, templates
- [x] An example script (`examples/07-history-and-links.graql`), and the README and module docs
