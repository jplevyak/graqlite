# Parameters, descending order, and offset

**Status:** closed · **Size:** small to medium · **Related:** [002](../002-multi-process-access.md) (a server needs parameters)

## Problem

Three small gaps:

- **Parameters.** An application has to paste values into GraQL text, which
  invites injection bugs and re-parses each time. Cypher has `$name`, GQL
  has parameters, SPARQL has `VALUES`, and Gremlin has bindings. A server
  (002) needs parameters for prepared statements.
- **Descending order.** `by` sorts ascending only, although the engine's
  `SortKey` has `descending`.
- **`offset`.** There is `limit` but no `offset` for paging. `offset` is a
  keyword.

## Design

### Parameters

```text
select ?f where $person knows ?f ; age ?a . ?a > $min_age
```

- **Lexing:** `$name` lexes as a parameter, a node of its own
  (`Node::Param`).
- **The API:**
  `Session::run_with(g, &statement, &Params)`, where
  `Params: HashMap<String, Param>` and
  `enum Param { Key(String), Value(Vec<u8>), Id(u64) }`.
  A parameter that is missing is an error naming it.
- **Binding** happens when the statement is lowered, like constants, so a
  parsed statement can run many times with different parameters. Caching
  plans is a separate step: a cached plan is only right while its estimates
  hold.
- **Saved rules** may not use parameters.
- **The shell:** `\set name value` (psql's style). The value is GraQL: a key,
  a literal, or `#id`. `\set` alone lists them, and `\unset name` removes
  one.

### Descending order

Done in [006](006-aggregates.md): `by ?age desc, ?name`, and `asc`
explicitly. What remains: Entries with no data (nodes, and
links) order by id, so that `by` works on any variable.

### Offset

`limit 10 offset 20`, in either order. In the engine it skips rows after
ordering. Without `by`, rows stream, so skipping still stops early.

## As implemented

- **Parameters bind by substitution.** `Statement::bind(&Params)` replaces
  each `$name` with its value's node before the statement runs.
  `Session::run_with` and `Session::execute_with` bind, and `run` binds with
  no parameters, so a statement that has some reports `$name is not given`.
  Parameters work everywhere nodes do: patterns, templates and `having`.
  Plans are not cached (as planned).
- **`Param` is `Key`, `Value` or `Id`.** `Param::parse` reads one GraQL
  token: `/key`, `#123`, or a word, number or quoted string. A value is
  never parsed as GraQL text, so a parameter can't change the query.
- **`#123`**, the entry with an id, was added as part of this work. It is
  phase 1 of [003](../003-queries-as-graph-data.md) (`Node::Id`). An
  `#id` in an `insert` or `delete` template must name an entry (checked
  with `Graph::check_entry`). A saved rule may use `#id`, and its reference
  link then points at the entry itself.
- **Rules** can't have parameters.
- **The shell:** `\set NAME VALUE`, `\set` to list them, `\unset NAME`, and
  `-v NAME=VALUE` / `--set NAME=VALUE` on the command line. `\explain` binds
  them.
- **Order:** sorting by data in the engine now uses `compare_cells`, the
  order of the aggregates: numbers by value (it used to compare bytes, so
  `10` came before `9`), then other data, then entries without data by id.
  `by` is the same with and without aggregates. `desc` was done in 006.
- **`offset n`**, before or after `limit`, each at most once. The engine's
  `Query::offset` skips rows after ordering, in the operator that limits,
  which still stops early when not ordering. Aggregate results skip after
  ordering too. Standing queries reject an offset, as they reject a limit.
- **Tests:** the random query test generates offsets, checked against
  brute force (the rows must be those at their places in order). There
  are also GraQL tests (parameters, `Param::parse`, ids, offset, numeric
  order), round trips, a CLI test for `-v`, and
  `examples/11-parameters.graql`.

## Tasks

- [x] `$name` parameters: lexer, `Node::Param`, `Session::run_with`, errors for missing parameters, and printing
- [x] Shell: `\set`, `\unset`, `-v`
- [x] `desc` and `asc` in `by`; ordering entries without data by id
- [x] `offset` in the engine and GraQL
- [x] Tests, an example, and docs
