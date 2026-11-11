# GraQL examples

Each script builds a small graph and demonstrates one area of GraQL or of
the `graqlite` shell. Next to each script is the output it prints, so you can
read them without running anything.

| Script | Shows |
| ------ | ----- |
| [01-basics](01-basics.graql) | keys, values, links; `insert`, `select`, `verify`, `lookup`, `delete`; `\show` |
| [02-patterns](02-patterns.graql) | Turtle syntax (`;` `,` `.`), `a`, blank nodes (`?`, `[ ... ]`, `_:x`), braces, `distinct` |
| [03-filters](03-filters.graql) | comparisons, `=` / `!=`, `and`, `or`, `not`, negated predicates, `by`, `limit` |
| [04-paths-and-rules](04-paths-and-rules.graql) | transitive paths (`p++`), recursive rules, negated rules, saved rules, `delete rule` |
| [05-templates](05-templates.graql) | `insert` and `delete` with templates and `where`, and new blank nodes per match |
| [06-shell](06-shell.graql) | `\x`, `\format`, `\d`, `\explain`, transactions |
| [07-history-and-links](07-history-and-links.graql) | link nodes `<< s p o >>` (statements about links, nested, named), `as of` an id or a time, for statements and groups |
| [08-optional](08-optional.graql) | `optional ( ... )` and the `-` modifier, nesting, unbound variables in comparisons, negations and templates |
| [09-aggregates](09-aggregates.graql) | `count`, `sum`, `avg`, `min`, `max`, grouping, `as`, `having`, `by ... desc`, `limit` over groups |
| [10-paths](10-paths.graql) | path expressions: `/` sequences, `^` inverse, `\|` alternatives, `*`, `+`, `{m,n}`, searched from either end; `as ?n` for a path's length; one search from all the rows of a `distinct` query |
| [11-parameters](11-parameters.graql) | `$name` parameters and `\set`, `#id`, `limit` and `offset` for paging, numeric order in `by` |
| [12-expressions](12-expressions.graql) | arithmetic, `let`, functions (text, numbers, `bound`, `when`, times), expressions in select, filters and aggregates |
| [13-comparison](13-comparison.graql) | the GraQL side of [the comparison with GQL, Cypher, SPARQL and Gremlin](../docs/graql-comparison.md) |
| [14-reference-tour](14-reference-tour.graql) | the runnable parts of the GraQL tour, corrected: anonymous and named blank nodes, a Freebase-style film graph with performance nodes, the Man Booker nominations with `optional`, lookups of joins, rules in braces, a schema as data |
| [15-lookups](15-lookups.graql) | predicates without objects, subjects alone, `lookup ... where`, `<<` / `>>` chains and `!`, `+`, comparisons as blank nodes, tests of objects (`(< 40)`, `(~"re")`, `(PREFIX("x"))`) |
| [16-derived-links](16-derived-links.graql) | lookups as graphs: links derived by `++`, path expressions and rules, with the stored links they come from |
| [17-subgraphs](17-subgraphs.graql) | subgraphs: `insert … into`, `delete … from`, `select … from`, `lookup` / `verify … in`, `( p ) in g`, author and subgraph positions, `author` and `subgraph`, `//keys`, `insert graph ( lookup … ) into`, `\subgraphs` |
| [18-asserted-links](18-asserted-links.graql) | lookups asserting links: `!`, a scope `@?x`, blank nodes as new nodes, stored triples, rules' bodies, `insert graph` of asserted links |
| [19-aggregate-links](19-aggregate-links.graql) | aggregates deriving links: `COUNT`, `distinct`, scopes `@?x`, the rankings (`ORDER`, `DENSE_RANK`, `ROW_NUMBER`, `REVERSE_ORDER`, `desc`, ties, missing keys), matching their links, `current_spouse`, inserting ranks |
| [20-symbols-and-graphs](20-symbols-and-graphs.graql) | `SET` of keys, numbers, strings, booleans and graphs; graphs of their own links, `database:g` layering, deleting with `not`, `from graph`, a lookup's graph named and inserted |
| [21-graph-layering](21-graph-layering.graql) | the session's graphs: `DEFAULT` (a schema under every graph), `DATA {}` (scratch data never written) and `DATA database`, `base:{ … }`, `ASSUME`, a default graph of template rule calls |
| [22-the-rest-of-the-tour](22-the-rest-of-the-tour.graql) | `- pattern` and `+ pattern` defaults, nested limits, `FUNCTION` (conditions, `+integer`), rule parameters without `?`, `\format tree`, template rules in `INSERT`, `TEST` |
| [24-loading-files](24-loading-files.graql) | `insert … from csv`, columns by name and position, a clause saying what a column holds, `delete … from csv`; `\copy` |
| [23-vector-search](23-vector-search.graql) | vectors as values (`vector`, `str`, `distance`), stored by a `let`, `index vector on`, `\di`, `nearest(P q k ?v ?d)` filtered by links, by a parameter and by a bound vector, `\explain` |
| [25-full-text-search](25-full-text-search.graql) | `(match "words")`, alternatives with `\|`, `match(?v, q)`, joined with the graph, `\explain` of the index, `\reindex` |

Scripts for features open issues will add go in [pending/](pending/README.md);
the golden tests don't run them.

To run one on a database in memory:

```sh
cargo run -q -- -f examples/01-basics.graql
```

Or load one into a database on disk, and then explore it interactively:

```sh
cargo run -q -- mydb -f examples/04-paths-and-rules.graql
cargo run -q -- mydb
```

`cargo test --test cli` checks that every script still prints its `.out`
file. After an intended change in output, run
`GRAQLITE_BLESS=1 cargo test --test cli` to rewrite the `.out` files, and
review the diff.
