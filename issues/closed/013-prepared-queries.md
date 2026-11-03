# 013: Prepared queries

**Status:** closed · **Size:** medium · **Related:** [012](012-reduce-only-when-it-pays.md), [008](008-parameters-order-offset.md)

## Problem

An embedded program usually runs a few query shapes over and over, with
different constants: "the city of person X", "the friends of X older than
N".  Each `Query::run` plans again.  With the bind join's buffers reused
(a2c672e), a two-pattern point query costs 17.6 µs in `benches/lookups.rs`,
of which the lookups are under 2 µs: the rest is planning.

GraQL's `$name` parameters (issue 008) don't help: `Statement::bind`
substitutes the values into the statement, which is then lowered to a
`Query` and planned on every run.

Making the planner faster helps every query a little.  Not planning at all
helps this case a lot.

## Design

**Parameters are variables bound before the query starts.**  A query
declares one with `Query::param("x") -> Var` and uses it like any variable,
in patterns, filters and paths.  The planner plans the top group as if the
parameters were already bound by the rows given, which it already knows how
to do for bind joins: a lookup with a bound subject is estimated from the
predicate's average links per subject, not the actual count for a value.
That estimate is what a plan made once for every value should use.

Executing starts from a row with the parameters' values in their slots,
rather than the empty unit row; every group planned alone (a hash join's
build side, a union's branches, a negation or optional group joined once)
starts from that row too.

- `Plan::execute_with(g, &[values])` runs a plan with the parameters'
  values, in the order declared.  `Plan::execute` of a query with parameters
  fails.
- `Query::prepare(g) -> Prepared` plans once.  `Prepared::execute(g,
  &values)` and `Prepared::run` run the cached plan, and plan again first if
  the graph's link count has changed by more than a factor of 2 since it was
  planned, so a plan made on an empty graph doesn't stay.
- Parameters are not selected by default (they are known), but can be.
  `explain` names them `$x`.

**What a parameter can't do:**

- A query with parameters is not reduced: the reduction reads the
  constants' index entries when planning, and there are none.  The
  reduction gate (012) already skips reducing most point queries.
- A rule body can't use one: rules are evaluated when planning, for every
  value.  `check` rejects it.
- A standing query can't have one.

## Phases

1. The `query` API: `param`, `execute_with`, `prepare`, `Prepared`; tests
   against the same query with constants, for random queries; the point query
   of `benches/lookups.rs` prepared.
2. GraQL: lower `$name` to a parameter instead of substituting it, and add
   `Session::prepare(g, text)` returning a prepared statement run with
   `Params`; the shell's `\prepare NAME STATEMENT` and `\run NAME`, or
   prepared statements cached by the shell's text.

## Phase 1, as implemented

As designed, with:

- `Prepared::execute` returns `Result<Rows>` (planning again can fail);
  `Plan::execute_with` returns `Rows` with the error first, as `execute`
  does.  It also checks the compaction horizon on each run.
- A union's branches may differ in the parameters they use: `check`
  compares the variables they bind apart from the parameters.
- The planner's "still the unit row" test, which stops it hash joining the
  first pattern for nothing, is now "bound only the parameters".

Tests: `random_prepared_queries` makes about half the constants of random
queries (patterns, paths, calls, filters, in every group but rule bodies)
parameters, and checks each plan, forced each way, and the prepared query
against brute force, with the original values and with other ones.  A
mutation starting the hash join's build side from a zero row fails it.

| `benches/lookups.rs`, 40 000 people | per query |
|---|---|
| two-pattern point query, planned each time | 17.0 µs |
| the same, prepared | 1.56 µs |

## Phase 2, as implemented

Lowering turns `$name` into a query parameter (`Query::param`), in triples,
paths, calls, comparisons and expressions, wherever the statement wasn't
bound first.  `Session::prepare(g, statement)` returns a
`PreparedStatement`, and `Session::run_prepared(g, &mut prepared, params)`
runs it:

- A `select` or `verify` whose parameters are all in its pattern is lowered
  and planned once.  Each run turns the `Params` into ids: a key by
  `find_by_key` (0, matching nothing, if absent), an id as it is.
- (Removed since, see below.) **A value parameter not in the graph falls back** to binding and running
  the statement as `run_with` does.  A value written in a comparison
  compares by its data, so `?age >= $min` must hold with `min` = `21` even
  if no entry has the data `21`; an id can't carry that.  Values in the
  graph, the usual case, take the planned path.
- Any other statement (parameters in the columns or `having`, `lookup`,
  `insert`, `delete`) is prepared trivially and runs as `run_with` does.
- **Lowered again** if the saved rules change (compared by pointer, then by
  value), or, when lowering named a key, value or path predicate not in the
  graph, or an `as of` a time, once the graph has changed.  This answers the
  open question about missing constants.

Not done: the shell's `\prepare` and `\run`.  A person typing a statement
doesn't notice 15 µs of planning; the shell can cache prepared statements by
text if scripts ever need it.

Tests: `prepared_statements` runs ten statements prepared and with
`run_with`, for six sets of values (in the graph, not in it, keys that
don't exist, ids), and the missing-constant and changed-rule cases.  A
mutation truncating the planned path's rows fails it.

| `benches/graql.rs`, 40 000 people | per query |
|---|---|
| `select ?c where #id a person ; city ?c`, as text | 19.3 µs |
| `$p`, parsed once, bound and run | 16.6 µs |
| `$p`, prepared | 1.55 µs |

## Open questions

- Should a plan be redone when a value's entry is far larger than the
  estimate, as a cached plan's worst case?  A bind join's lookup of a huge
  entry streams it, so the cost is the rows, not a blow-up; leave it until
  measured.
- ~~Expressions and aggregates have no builder in the `query` API.~~ Added
  after: `query::expr::Expr`, `Filter::Holds`, `Query::compute`,
  `aggregate`, `group_by`, `having` and `cells`, which GraQL's computed
  `select`s now lower to.
- ~~A GraQL statement with parameters in its columns or `having` is not
  planned once.~~ Done after, with parameters given values that are not
  entries (`Plan::execute_values`, `Prepared::run_values`/`cells_values`):
  a value is the entry with its data in a pattern, and the value itself in
  expressions, which `Filter::Holds` (now what GraQL's expression filters
  and data comparisons with a parameter lower to), computed values and
  `having` read.  This also removed the fallback for value parameters not in
  the graph: every `select` and `verify` is planned once.  It found that
  `run_with` didn't bind parameters in a `select`'s columns.
