# Expressions and literal types

**Status:** closed (decision: A, untyped storage and typed computation) · **Size:** large · **Related:** [006](006-aggregates.md), [004](004-history-and-link-variables.md)

## Problem

GraQL can compare data, but can't compute with it:

- arithmetic (`?price * ?qty`);
- string functions (length, case, contains, regex);
- binding a computed value to a variable (SPARQL `BIND`, Cypher `WITH ... AS`);
- returning a computed value.

Values are also untyped: a value is bytes, and a comparison is numeric when
both sides parse as numbers. So `"10"` and `10` are the same value, and
there are no booleans, dates or language tags. SPARQL has XSD datatypes, and
Cypher and GQL have typed properties.

## Design

### Computed values

Expressions produce typed scalars. [006](006-aggregates.md) added
`query::aggregate::Cell` (`Id`, `Int`, `Float` and `Unbound`) and
`Output::Table`. Expressions add `Text` and `Bool`. An id's data
converts as needed: a number for arithmetic, text for string functions. A
conversion that fails is an error for that row, which drops the row, as
SPARQL does.

```text
select ?p, ?total where ?p bought ?item . ?item price ?price ; qty ?q
       . let ?total = ?price * ?q . ?total > 100
select ?p as ?who, upper(?n) as ?name where ?p name ?n
```

- **Statements:** `let ?v = expr` is a subject, and works like SPARQL `BIND`.
  `expr as ?name` goes in the `select` list.  Comparisons accept expressions on either side.
- **Functions:**
  - numbers: `+ - * / %`, `abs`, `round`;
  - strings: `str`, `length`, `upper`, `lower`, `contains`, `starts`,
    `ends`, `regex`, `concat`;
  - `bound(?x)` (with [005](005-optional.md));
  - for links: `when(?link)` and `author(?link)`
    ([004](004-history-and-link-variables.md)): a link's creation time and
    author;
  - `id(?x)`.
- **The engine:** a `Filter::Expr` evaluated per row, and a "compute" step
  that extends rows with computed cells, placed by the planner once its
  inputs are bound. Computed variables can't be used as join keys into the
  indexes, because they aren't ids, unless an expression's value is interned
  data found by `find_value`.

### Decision needed: literal types

| Option | Storage | Notes |
| ------ | ------- | ----- |
| **A. Untyped data, typed computation** | as now | Types come from syntax at the use site (`10` versus `"10"`, `int(?x)`). No format change. `"10"` and `10` are still one value. |
| **B. A type tag in the data** | a prefix byte, or a varint type before the data | Exact types, and dates, like RDF literals. Changes the record format, interning, and every comparison. `"10"` and `10` become distinct values. |
| **C. Types as links** | `?v graqlite:type integer` | Optional and gradual, and fits the graph model. But typing is per value, not per use, so one value can't be both a string and an integer. |

Recommendation: **A** now. Computation is typed, and storage stays untyped
bytes; add date and time functions that parse ISO 8601 text. Revisit B only
if exact types (`1` versus `1.0` versus `"1"`) are a real requirement, since
it is a format break.

## As implemented

Option **A**: storage is unchanged, and computation is typed.

- **Cells:** `Cell` gains `Text` and `Bool`, so it is no longer `Copy`.
  - Conversions: `Cell::number` reads numbers from text and data, and
    `Cell::text` gives an entry's data (or its key).
  - `compare_cells` ranks numbers by value, then booleans, then text and
    data by bytes, then entries without data.
  - `Aggregation::apply` now groups and folds rows of cells, so groups and
    aggregates can be computed. This changes its signature, which 006 had
    just added.
- **Expressions** (`graql::Expr`): `+ - * / %`, unary `-`, parentheses, and
  functions (`graql::Func`):
  - numbers: `abs`, `round`, `ceil`, `floor`, `number`;
  - text: `str`, `length`, `upper`, `lower`, `concat`, and the tests
    `contains`, `starts`, `ends`;
  - `bound`, `id`;
  - for links: `when` and `author`;
  - times: `time` (ISO 8601 to milliseconds) and `iso` (the reverse).

  Integer arithmetic stays integral until it overflows, and `/` gives a
  float. Anything that can't be computed is unbound, and no comparison
  holds for an unbound value.
- **Filters run in the engine:** a comparison with an expression, or a test
  subject (`contains(?n, "a")`), compiles to `query::Filter::Expr`, a
  closure over the row with the variables it reads. So it works inside `or`,
  `not`, `optional` and rules, and the planner applies it as soon as its
  variables are bound. A `not` of only comparisons and tests becomes a
  negated filter, since the engine needs a pattern in a negated group.
  `\explain` shows the expression.
- **`let ?v = expr`** is expanded wherever `?v` is used in an expression, so
  it needs no computed values in the engine's rows. Using `?v` in a triple,
  path, call or link is an error, as is defining it twice.
- **Select columns** may be expressions (`?p * ?q as ?total`). `as` can also
  name a plain variable. Selects that compute are evaluated after the query
  (`ComputePlan`). Aggregates take expressions (`sum(?p * ?q)`). `by` may
  use a column, or a variable that isn't selected.
- **The shell** shows text cells as literals (raw in CSV and TSV), and
  booleans as `true`/`false`. It completes function names too.
- **Function names** are reserved: a rule can't be named like one.
- **Not done:**
  - `regex`: it would need a dependency; `contains`, `starts` and `ends`
    cover the common cases.
  - Comparisons as select columns: comparisons belong in patterns.
  - A parenthesized expression at the start of a subject: `(` there starts
    a group, so write `2 * (?a + 1) > 3`.
  - `/` without spaces in an expression is a key, as elsewhere in GraQL.
- **Tests:** the aggregation and ordering of computed cells, GraQL
  expression tests (arithmetic, filters in negations, `let`, text, optional
  values, aggregates, aliases, errors, times, saved rules), round trips, and
  `examples/12-expressions.graql`.

## Tasks

- [x] `Cell` (shared with 006), and the shell rendering it
- [x] An expression parser, with precedence; `let`; `as` in `select`; expressions in comparisons
- [x] `Filter::Expr`, and computing select columns after the query
- [x] The functions above (except `regex`), including `when` and `author`
- [x] Printing, and saved rules that contain expressions
- [x] Tests, an example, and docs
