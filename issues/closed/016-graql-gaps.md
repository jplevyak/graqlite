# The rest of the tour

**Status:** closed · **Size:** large · **Related:** [014](014-subgraphs.md) (subgraphs, authors), [015](015-graph-results.md) (graph results, `!`, aggregates as links), [010](010-language-comparison.md) (the comparison)

## Problem

Each statement run alone in the shell: 16 of 68 run, and 52 fail.  Those that run are inserts
with `;`, `,` and `[ … ]`, `DELETE`, full-triple `LOOKUP`s, `++`,
`SELECT … BY`, `VERIFY`, and rules.  Issues 014 and 015 cover subgraphs,
authors, result graphs, `!`/`@` and aggregates as links.  This issue is the
rest.

| Example | Error here | Section |
|---|---|---|
| `LOOKUP ?subjects`, `LOOKUP ?subject ?`, `LOOKUP ? type`, `LOOKUP [type;name]` | "expected a node, found the end" / "';'" | short lookups |
| `LOOKUP ?subject WHERE …` | "expected a node, found 'where'" | `WHERE` |
| `?person name (< "Joe")`, `(~"Bob.*")`, `(> ?population)`, `(PREFIX("foobar_"))` | "an expression object is not supported" | expression objects |
| `name "John Smith" +`, `LOOKUP + […]`, `LOOKUP - {…}` | "predicate modifiers" / "default modifiers is not supported" | modifiers |
| `LIMIT 10` after a predicate, `AND ?film LIMIT 10` | "predicate modifiers is not supported" | nested limits |
| `?director <</film/film/directed_by ?film >>…` | "'<<' and '>>' is not supported" | direction |
| `?director !/film/film/directed_by ?film` | "a group needs a pattern or a union" (misparsed) | direction |
| `SET me = /author/John_Plevyak`, `SET anInteger = 100`, … | "'set' is not supported" | symbols |
| `TEST LOOKUP … LOOKUP …` | "'test' is not supported" | `TEST` |
| `FUNCTION foo(x, y) x + y / 3`, `x +integer y` | "'function' is not supported" | functions |
| `RULE current_spouse(person) {…}` | "expected a variable, found person" | rule parameters |
| `/people/marriage/spouse [:?other != person]` | "expected a node, found '!='" | comparisons as blank nodes |
| `INSERT SimpleProperty(…)` | "a template other than triples is not supported" | calls in templates |
| `DATA {}`, `DEFAULT {…}`, `film_stuff:{…}` | "'data'" / "'default' is not supported" | not here: [017](017-graph-layering.md) |
| `ASSUME me name …` | "'assume' is not supported" | not here: 014 |

`SELECT`, too, differs where it runs: the examples say it returns "a forest
of result trees … 'dangled' by the result order (i.e. the first result is
the root)", and here it returns a table.

## Design

### Short lookups

A subject needn't have a predicate or object, and a predicate needn't have
an object:

- `LOOKUP ?subjects` is every node (with its links: see 015's
  `ResultGraph::nodes`).  `LOOKUP ?s ?` and `LOOKUP ?s ? ?` are every link
  and its nodes.
- `LOOKUP ? type` is every link with predicate `type`: a missing object is
  a fresh variable.
- `[type; name]` is a blank node with both predicates.  `LOOKUP [type;name]`
  is the nodes with a type and a name, and those links.
- In a `select` or `where`, the same forms are patterns: `?s type` holds if
  `?s` has a type.

### `WHERE`

`LOOKUP pattern WHERE P` returns the links and nodes `pattern` matches in
the rows of `P`.  "Things in the WHERE clause constrain the results … but
need not themselves be returned", so `P`'s links are not in the result.
It lowers to one query, recording only `pattern`'s links (015's hidden link
variables, for `pattern` only).

### Expression objects

An object in parentheses is a test of the object, whose left side is
elided: `?person name (< "Joe")` is `?person name ?n . ?n < "Joe"`.

- **Comparisons:** `(< 40)`, `(> ?population)`, `(= 0)`, `(!= x)`.
- **Matching:** `(~"Bob.*")` is a regular expression match of the object's
  text, as a new function (`matches`, or `~` as a binary operator in
  expressions).  This needs a regex crate, `regex` or `regex-lite`; it's a
  new dependency.
- **Functions:** `(PREFIX("foobar_"))` calls a function with the object as
  its first argument: `prefix(?o, "foobar_")`, a test of whether `?o`'s
  text starts with it (GraQL's `starts`).
- **Naming:** `( :?x expr )` (the grammar's `opt_variable`) binds the object
  to `?x` as well.

### Modifiers

- **`+` and `-`** after a predicate's objects make it mandatory or optional.
  `-` works already (it lowers to `optional`); `+`, the default, is
  rejected and should be accepted.
- **Defaults:** `LOOKUP + pattern` and `LOOKUP - pattern` set the default
  for the pattern's predicates, so `LOOKUP - { … name+ … }` is everything
  optional but `name`.
- **`*` and `/`** (the grammar's "return" and "/") have no examples; leave
  them unsupported until their meaning is known.

### Nested limits

`LIMIT n` and `OFFSET n` after a predicate, or `AND ?var LIMIT n`, limit
the values of a variable within each value of the variables before it:
"find at most 10 people and at most 10 types per person".  That's a limit
per group, not on the rows:

- Lowering keeps, for each limited variable, its position in the pattern's
  order and the variables before it.
- Execution applies them after matching, as `compute` does: rows are grouped
  by the earlier variables, and within each group only the first `n` values
  of the limited variable (in the order they come, or `BY`) are kept.  A
  plan whose join order matches can stop early instead.  Leave that for
  later.

### Direction

- `<<` before a predicate reverses it: `?director << /film/film/directed_by
  ?film` is `?film /film/film/directed_by ?director`.  `>>` makes it
  forward again.  Either can follow a predicate's objects, and then
  continues from them (the grammar's `more_predicates`): `?director <<
  /film/film/directed_by ?film >> /film/film/starring ?p` is `?film
  directed_by ?director . ?film starring ?p`.  That's the "chaining" the
  tour describes.
- `!` before a predicate (MCL's convention) reverses it, like `<<`, but
  "does not allow chaining": what follows is the same subject's.  It's
  already lexed (it's `T_LNOT`), and the parser misreads it now; it's the
  inverse path `^p` for one step.
- `;` after a chain goes back to the chain's first subject.  (The tour has
  no example of it; it's a guess to confirm.)

### Symbols: `SET`

- `SET name = thing` binds `name` for the session to a key, an id, a
  constant (`100`, `1.1`, `"one"`, `true`), an expression, a table, or a
  graph (015).
- **Resolution:** an identifier that's a symbol is its value.  Otherwise
  it's the value with that data, as now.  So after `SET me =
  /author/John_Plevyak`, `INSERT me name "John Plevyak"` writes the node
  with that key, not the value `me`.  This changes what scripts mean, but
  only scripts that `SET` the name.
- Keywords stay case-insensitive (the lexer's `(?i:…)`): `SET` and `set`
  are the same.
- **Grammar:** `SET film_stuff = { … }` and `SET film_stuff =
  film_stuff:{ … }` don't parse with `graql.y`, whose `graph_expr` has no
  `{ subjects }` form.  Accept `{ subjects }` (and `name:` before it) as a
  graph here.
- Symbols are the shell's `\set` parameters' natural home: `$name` and a
  `SET` name could share a namespace.  Leave them separate for now.

### `TEST`

`TEST ["description"] R1 R2` evaluates two results (lookups, selects or
graphs) and reports an error if they differ: tables as multisets of rows,
graphs as sets of links (015's equality, derived links by their fields).
It's a check for scripts, so a failed `TEST` makes the CLI exit non-zero
like any error.

### Functions

`FUNCTION name(x, y) expr` defines a function of values for the session (or
saved, as rules are), callable in expressions.

- **Parameters** are names (`x`), not variables; the body is an expression
  over them.
- **Types:** "All expressions are typed, as integer, real, string or
  boolean", and an operator may carry a type tag (`+integer`) where its
  operands' types aren't known.  Expressions are dynamically
  typed (a value is converted as an operation needs).  Accept the tags as
  conversions (`x +integer y` is `number(x) + number(y)` rounded to an
  integer), rather than adding static types.
- The grammar's other expression forms: `if … then … else`, `||`, `&&`,
  bit operators and shifts, `list(…)`, sequences `[e, … where P]`, and
  `f of [ … ]`.  Take `if`, `||` and `&&` with functions.  Leave the rest
  until there are examples.

### Rule parameters

`RULE current_spouse(person)` names a parameter without `?`, and uses it
bare in the body (`?marriage /people/marriage/spouse person`).  Accept an
identifier in a rule's head as a variable local to the rule, shadowing
symbols and values of that name in the body.  So a parameter can't share a
name with a predicate the body uses as a value (`RULE owner(pet) { ?p pet
pet }` would make both `pet`s the parameter).

### Comparisons as blank nodes

A blank node's content is a `predicate_phrase`, which is either predicates
or a comparison with a node: `[:?other != person]` is a new variable
`?other`, named by `:?other`, that isn't `person`.  So `?m spouse
[:?other != person]` is `?m spouse ?other . ?other != person`.  The same
goes for `<`, `>`, `<=`, `>=` and `=`, comparing as the expression
comparisons do.  Without a name (`[!= /a]`) the variable is fresh.

### Calls in templates

`INSERT SimpleProperty(/film/performance/actor 0 0 "")` inserts what a
rule's body asserts (015's `!` in rules), with the arguments bound.  It
needs 015's phase 3.

### `SELECT` as a forest

"A forest of result trees dangled by the result order": the first column's
values are roots, each with the second column's values under it, and so on.
Keep the table as the default output.  Add the forest as a display (the
shell's `\pset format tree`?) and as `Output::Forest`, built from the
table's rows grouped by each column in turn.

## Not in this issue

- **`DATA` and `DEFAULT`**, and `base:graph`: a data graph and a default
  graph that "all graphs will extend".  That's graph layering, in
  [017](017-graph-layering.md).
- **`ASSUME`**: hypothetical links, in [014](014-subgraphs.md)'s open
  questions.
- **The examples' own bugs**: `current_spouse` uses an unbound `?spouse`
  (for `?other`), the expanded rule doesn't match the first (it passes
  `?order`, which it then tests, as `ORDER`'s key, where the first passes
  `?marriage_from`), `PROPERTY` leaves `?obj` unconnected, the film examples skip the
  performance node the schema declares, and there are mistyped keys.
  `PROPERTY`'s comment says "new nodes are created with the `node`
  keyword", but neither `graql.lex` nor `graql.y` has one and nothing uses
  it.  The comment is stale: blank nodes in templates (`[ … ]`, `_:x`)
  create new nodes, here already, and in asserted links with 015's
  `ResultNode::Blank`.  Fix them in the tour before using it as a test.

## Phases

1. **Lookups:** short lookups, `WHERE`, `<<`/`>>`/`!` direction, `+` after
   objects, and comparisons as blank nodes.  With 015's phase 1, this covers most of the examples'
   `LOOKUP`s.
2. **Expression objects**, including `~` (with the regex dependency).
3. **Symbols and `TEST`**: `SET` of constants and keys (graphs with 015),
   and `TEST`.
4. **Defaults and nested limits.**
5. **Functions and rule parameters.**
6. **`SELECT` as a forest**, and calls in templates once 015's phase 3 is
   in.

## Phases 1 and 2, as implemented

- **Short forms:** `Pattern::Node` is a subject alone.  It lowers to
  nothing if the statement binds it by matching, and else to a call of a
  hidden relation, `_node(?x)`, defined by two rules: the subject or object
  of some link.  So `lookup ?subjects` is every entry in a link, values
  included.  A predicate's first object can be left out, a fresh variable;
  after `,`, an object is needed.  Neither is allowed in templates, so a
  half-typed `insert /a knows` is an error, not a new blank node.
- **`lookup … where P`:** `Statement::Lookup` has `where_`.  The lowering
  gives each triple of the lookup's own pattern a hidden link variable (not
  in negations, not in `P`), and the query records exactly those
  (`Query::record_links_of`), so `P`'s links aren't returned.
- **Direction:** `<<` before a predicate reverses it, and `>>` keeps it
  forward; after objects, either continues from the last object.  `;`
  returns to the first subject (the guess the tour leaves open), and `!p`
  reverses one predicate without chaining.  In predicate position `<<` is
  now a direction, not a link node; link nodes still work as subjects and
  objects.  `!` was misread as `not` before.
- **`+`** after objects is accepted.
- **Comparisons as blank nodes:** `[ :?x != y ]`, `[ < 32 ]`.
- **Tests of objects:** `( cmp e )`, `(~"re")` (the new function
  `matches`, with `regex-lite` as a dependency, compiled expressions cached
  per thread), a test function with the object as its implied first
  argument (`(prefix("Bil"))`, `prefix` being `starts`), `(e)` for equality,
  and `( :?x … )` naming the object.
- **Blank nodes' variables are hidden** in the query (the union check
  ignores them), so `or` branches may each have their own: the tour's
  `( ?p name (< "Joe") or ?p name (> "Will") )` needed this.
- **The shell** waits for a blank line to run a statement that ends with a
  subject alone or a predicate with no object, as it does for incomplete
  ones: `graql::ends_short`.  Scripts are unchanged: a blank line ends a
  statement.
- **One error message changed:** `/a age ) ?x` now reads `/a age` as a
  predicate with no object, and reports "expected a statement, found ')'"
  at the same place (`tests/cli/syntax-error.out`).

`examples/15-lookups.graql` is the golden test, from the pending script's
phases 1 and 2.  Of the tour's 68 statements, 35 now run (16 before).

## Phase 3: `SET`, as implemented

`TEST` remains; `SET` is done, with 015's graphs as inputs and 017's
overlays.

- **Symbols.**  `set name = value` names, for the session, a constant (a
  key, id, number, string, boolean, or another symbol's value) or a graph
  (`set g = graph G`, or `{ … }`, `base:G`, `database`, `( lookup … )`
  without `graph`).  Expressions and tables aren't supported.
- **Resolution is by the parser**, which reads an identifier as a symbol
  (`Node::Symbol`) if a `set` before it in the text named it, or the
  session did (`parse_with_symbols`, with `Session::symbols`; the shell
  passes them).  So a string is never a symbol, as the design wants, and
  a script `set`ting a name and using it parses at once.  The session
  replaces symbols by their values before a statement runs (a rule's
  definition too, so saved rules don't depend on the session); one naming
  a graph where a node goes is an error.  A prepared statement resolves
  them when prepared.
- **Graphs** are 017's overlays: `select … from graph G where …`, and
  `lookup` / `verify … in graph G`, read one (`Statement::From`); `insert
  graph G` writes one.  The grammar accepts `{ triples }` and `name:G` as
  graphs, with `not` before a triple deleting it.
- Symbols and the shell's `\set` parameters stay separate.

`graphs_and_symbols` checks resolution (a string isn't a symbol), constants
in patterns and comparisons, and errors; `print_round_trips` the syntax.
`examples/20-symbols-and-graphs` is the golden test.  Of the tour, its
`SET`s of keys and constants, `DATA {}`, `ASSUME me name …` and the
closing `DEFAULT {}` now run: 50 of 68 statements.  Its `SET film_stuff =
{ PROPERTY(…) }` needs calls in templates (phase 6).

## Phases 3 (`TEST`) to 6, as implemented

- **`TEST ["description"] R1 R2`**, each a `lookup`, `select`, `verify`
  or `graph G`: tables compare as multisets of rows, graphs as sets of
  links and nodes, each by its facts (stored, derived or an overlay's, a
  value or key by its data), so a lookup's graph equals a graph literal
  of the same links.  A pass prints `TEST … passed`; a failure is an
  error, so a script exits non-zero.
- **Defaults:** `- pattern` makes a pattern's predicates optional unless
  marked `+`, `+ pattern` mandatory unless marked `-`; asserted triples
  are neither.  The printer writes the expanded form (`optional ( … )`).
- **Nested limits:** `Query::limit_values(v, n, offset)`, applied by an
  operator after matching (before `distinct`, `order_by` and `limit`):
  for each limit in turn, the rows whose value of `v` is among the first
  `n` (after `offset`) of its group's values, least first (as `by` sorts),
  a group being the rows sharing the values of the variables limited
  before; rows with `v` unbound are kept.  GraQL's `limit n [offset m]`
  after a predicate's objects limits the object variable (or the subject,
  if the object is a constant); `?v limit n` limits `?v`; both are
  `Pattern::LimitValues`, at the top of a statement's pattern.  The
  limits are of the rows of the whole pattern: `?p a person limit 2 and
  ?p pet ?x limit 1` is two people with pets.
- **Functions:** `function name(x, y) expr`, for the session (not saved,
  unlike rules).  Its parameters are bare names, which the parser reads as
  symbols in its body; a call (`Expr::Apply`) is replaced by the body, with
  the arguments for the parameters, before the statement runs, so
  functions cost nothing at execution, and a cycle of calls is an error.
  A column computed by one is named as written (`kg(?m)`).  Expressions
  gained conditions: comparisons, `&&`, `||` and `if c then a else b`
  (`query::expr::Expr::If`); in a pattern, a call followed by a comparison
  is a function's, not a rule's.  Type tags convert, as decided: `x
  +integer y` is `round(number(x) + number(y))`, `+real` converts to
  numbers, `+string` concatenates; `boolean` is ignored.
- **Rule parameters:** a bare name in a rule's head is a variable of the
  rule, which the body (and `@`) names bare, shadowing symbols and values
  of that name.
- **`SELECT` as a forest:** the shell's `\format tree` writes rows as a
  forest, each value under the one before it, rows sharing their leading
  values sharing them; `Output` is unchanged.
- **Calls in templates:** a *template rule*, whose body only asserts
  triples, defines no relation (calling one in a pattern says so); its
  triples may use only its head's variables.  A call of it in a template
  (`insert`, `delete`, `assume`) or a graph literal is expanded, when the
  statement runs, to its asserted triples with the head the call's
  arguments, its blank nodes new for each call (and each definition).  A
  rule that asserts and also matches can be called in `insert … where`,
  whose pattern gains its patterns.  Calls of other rules in templates are
  errors.

Tests: `random_value_limits` checks nested limits against an oracle
filtering brute force's rows (mutations ignoring the earlier groups, or
the offset, fail it); `the_rest_of_the_tour` the GraQL of every phase;
`print_round_trips` the syntax.  `examples/22-the-rest-of-the-tour` is
the golden test.  The ignored test `count_tour_statements` runs the tour
statement by statement: 62 of its 68 statements run as written.

## Closed

Every phase is done.  The tour's other six statements are its own
mistakes, whose corrected forms run in the examples: `DEFAULT { PROPERTY(…)
}`, `SET film_stuff = { PROPERTY(…) }` and its layering, and `INSERT
SimpleProperty(…)` call rules the tour never defines as asserting
triples (its `PROPERTY` rule matches, and asserts nothing); and its two
`current_spouse` rules assert `?other`, which their heads haven't (015
decided a rule's assertions use its head's variables, so that its
assertions are its tuples': `current_spouse(person ?other)` runs).  `*`
and `/` as modifiers, lists, sequences and the bit operators stay
unsupported, without examples to say what they mean.

## Decisions

These were open questions.

- **Short lookups in `select`:** yes: a predicate with no object (`?s
  type`) is `?s type ?` in any statement's pattern, one grammar for all.
- **Nested limits and order:** without `BY`, "the first `n`" values of a
  limited variable within each group are the least by value (ids, numbers
  by value, as `by` sorts), so results don't depend on the plan; `BY`
  orders them otherwise.
- **Symbols and parameters** stay separate: `$name` is a parameter, a bare
  `name` a symbol.
- **Typed operators** are conversions: `x +integer y` converts its operands
  to numbers and rounds the result to an integer; values stay dynamically
  typed, with no static checking.
