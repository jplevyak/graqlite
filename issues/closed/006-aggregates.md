# Aggregates

**Status:** closed · **Size:** large · **Related:** [009](009-expressions-and-literal-types.md) (shares computed values), [005](005-optional.md)

## Problem

GraQL can't count, sum or group: "how many people live in each city", "the
oldest person", "people with more than 10 friends". All four languages it is
compared with can: SPARQL with `GROUP BY` and `HAVING`, Cypher with implicit
grouping, GQL, and Gremlin's `group()` and `count()`.

## Prerequisite: computed values in rows

Today a row is a list of ids (`Output::Rows { rows: Vec<Vec<u64>> }`), so
everything a query returns is an entry in the graph. A count is a number that
need not be in the graph, and a query must not append values just to answer.
Rows need cells that can be computed:

```rust
pub enum Cell { Id(u64), Unbound, Int(i64), Float(f64), Text(String), Bool(bool) }
```

This is the same foundation that expressions need
([009](009-expressions-and-literal-types.md)), so whichever comes first
builds it. It changes `Output::Rows`, the shell's rendering, and `query::Rows`.
The query engine's internal rows can stay as ids, with aggregates computed in
a final stage. `min` and `max` over data can still return the id of the
value.

## Design

```text
select ?city, count(?p) as ?n by ?n desc where ?p city ?city
select count(distinct ?c) as ?cities where ? city ?c
select ?p, count(?f) as ?friends where ?p knows ?f having ?friends > 10
```

- **Functions:** `count(?x)`, `count(*)`, `count(distinct ?x)`, `min`,
  `max`, `sum`, `avg`, and later `sample` and `group_concat`. `sum` and
  `avg` read numbers from data, and skip what isn't a number.
- **Grouping:** like Cypher, by the selected variables that are not
  aggregated, so there is no `group by` to forget. An explicit `group by`
  can come later if a grouping variable must not be returned.
- **`as ?name`:** names a result, which `by` and `having` can use. The 
  grammar's `as` in a select list is reported as unsupported today. This is
  that.
- **`having`:** a filter over groups.
- **Unbound** values (from [005](005-optional.md)) are skipped, except by
  `count(*)`.

### The engine

- **An aggregation stage after the joins:** hash the rows by their grouping
  key and fold. It runs before `by` and `limit`, which then apply to groups.
- **Streaming:** `limit` without `by` can stop early only without
  aggregation.
- **Standing queries:** incremental counts are natural (+1 and −1 per row
  change), but `min` and `max` need multisets. Support `count` and `sum`
  first, and report the others as unsupported in standing queries.
- **Rules:** aggregates in rule bodies interact with recursion, which needs
  stratification as with negation. Disallow them at first.

## As implemented

- **Aggregation is a stage after the query, not inside the engine:**
  `query::aggregate`. The engine's rows stay ids, and are kept with
  duplicates (no `distinct`), so that counts are right.
  `Aggregation::apply` groups them by some columns, in the order each
  group first appears, and folds the others with `Function`s into `Cell`s:
  `Id`, `Int`, `Float` or `Unbound`. This keeps the engine and its standing
  queries unchanged. Library users can apply an `Aggregation` to any
  query's rows.
- **The output:** `Output::Table { vars, rows: Vec<Vec<Cell>> }` is used only
  for a `select` with aggregates. Other selects keep `Output::Rows`, so
  existing callers are unaffected. `Output` is no longer `Eq`, since a cell
  can be a float.
- **Order of operations:** group, then `having`, then `by`, then `distinct`,
  then `limit`. `by` and `having` refer to the results' columns: the
  grouping variables, and the aggregates' `as` names. Anything else is an
  error.
- **The functions:**
  - `count(?x)` counts the bound values, `count(distinct ?x)` the distinct
    ones, and `count(*)` the rows (unbound values included).
  - `sum` stays an integer until it overflows or meets a non-integer; `avg`
    is a float, or unbound if there are no numbers.
  - `min` and `max` return the entry itself, so it shows by its name.
  - `sample` returns some value of the group.
  - With no grouping variables there is one row, even of no matches, as in
    SQL.
- **Ordering cells:** `compare_cells` is a total order. Unbound first; then
  numbers, and entries whose data is a number, by value; then other data by
  bytes; then entries without data, by id. (The first version mixed numeric
  and byte comparison, and that isn't transitive.) Floats print to six
  places without trailing zeros.
- **Also done:** `by ?x desc` (and `asc`), with or without aggregates, from
  [008](008-parameters-order-offset.md).
- **Syntax:** `having c1 and c2` (comparisons of columns and constants)
  comes after the select list. `having` became a keyword. `as` after a
  plain variable is reported as unsupported, until expressions
  ([009](009-expressions-and-literal-types.md)).
- **Not done:**
  - Incremental aggregates over standing queries. GraQL doesn't expose
    standing queries, and the server's `subscribe` (002) can add them.
  - `group_concat`, which needs text cells (009).
  - An explicit `group by`.
- **Tests:** the aggregation module (grouping, each function, unbound
  values, empty input, the order of cells), GraQL behaviour and errors,
  round trips, and `examples/09-aggregates.graql`.

## Tasks

- [x] `Cell` in GraQL output (`Output::Table`), with the shell rendering it (shared with 009)
- [x] An aggregation stage: grouping, the functions, `distinct`, `having`
- [x] GraQL: aggregate calls in `select`, `as ?name`, `having`; printing
- [x] Ordering and `limit` over groups
- [ ] Standing queries for `count` and `sum` (not applicable yet: GraQL has no standing queries)
- [x] Tests, an example, and docs
