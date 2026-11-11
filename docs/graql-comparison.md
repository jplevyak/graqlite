# GraQL compared with GQL, Cypher, SPARQL and Gremlin

GraQL is graqlite's query language. Its data model and Turtle-like syntax
are closest to **SPARQL**. Its feature set now covers what **GQL**,
**Cypher**, **SPARQL** and **Gremlin** users expect day to day: optional
patterns, aggregates, path expressions, parameters and expressions. Beyond
those, it has three things that none of the four has built in: **recursive
rules saved in the database**, **history** (every query can see the graph
as of an id or a time), and **statements about statements** as part of
the data model. **Datomic**, in the table's last column, is the nearest
relative: it has Datalog rules (passed with each query, not saved) and the
same three views of the past, but its facts are about entities and need a
schema, and a statement about a fact is an attribute of its transaction.

The GraQL queries below are in
[`examples/13-comparison.graql`](../examples/13-comparison.graql), which the
tests run. The other languages' versions are idiomatic sketches, so check
them against your engine's dialect. In the property-graph languages,
"knows" is a relationship and "age" a property; in SPARQL and GraQL both
are triples.

## At a glance

| | GraQL | GQL (ISO 2024) | Cypher | SPARQL 1.1 | Gremlin | Datomic |
|---|---|---|---|---|---|---|
| Model | triples; links are entries with ids, and links can be about links | property graph | property graph | RDF triples, named graphs | property graph | datoms: entity, attribute, value, transaction, added |
| Style | declarative, Turtle-like | declarative, ASCII-art patterns | declarative, ASCII-art | declarative, Turtle-like | traversal steps | declarative Datalog, in EDN; `pull` |
| Planner | cost-based, with constraint propagation | per engine | cost-based | per engine | mostly as written | clauses as written |
| Optional | `optional ( … )`, `-` | `OPTIONAL MATCH` | `OPTIONAL MATCH` | `OPTIONAL` | `optional()`, `coalesce()` | `get-else`, `pull` |
| Negation | `not` | `NOT EXISTS` | `WHERE NOT` | `FILTER NOT EXISTS`, `MINUS` | `not()` | `not`, `not-join` |
| Aggregates | `count sum avg min max sample`, implicit grouping, `having` | ✓ | ✓, implicit grouping | `GROUP BY`, `HAVING` | `group()`, `count()` | `count sum avg min max sample`, implicit grouping, `:with` |
| Paths | `^p`, `p / q`, `p \| q`, `* + {m,n}` | quantifiers, path modes, shortest | `*m..n`, `shortestPath` | `^p / \| * + ?` | `repeat()`, `path()` | recursive rules, recursive `pull` |
| Path as a value | ✗ | ✓ | ✓ | ✗ | ✓ | ✗ |
| Recursive rules | ✓ saved Datalog, stratified negation | ✗ | ✗ | ✗ (a reasoner) | ✗ (loops) | ✓ passed with each query |
| Expressions | arithmetic, text, time functions; `let` | ✓ | ✓ | `BIND`, functions | `math()`, steps | predicates, functions, host-language calls |
| Parameters | `$name` | ✓ | `$name` | `VALUES` | bindings | `:in` |
| Paging | `by … desc`, `limit`, `offset` | ✓ | `ORDER BY`, `SKIP`, `LIMIT` | `ORDER BY`, `LIMIT`, `OFFSET` | `order()`, `range()` | `:limit`, `:offset` (client API), no order |
| Statements about links | `<< s p o >>`, native | edge properties | edge properties | RDF-star (RDF 1.2) | edge properties | attributes of the transaction |
| History | `as of`, `since`, `history`, by id or time | ✗ | ✗ | ✗ | ✗ | `as-of`, `since`, `history` |
| Live queries | engine `StandingQuery` | ✗ | ✗ | ✗ | ✗ | `tx-report-queue`, of transactions |
| Literal types | untyped storage, typed computation | typed | typed | XSD datatypes, language tags | typed | typed attributes |
| Schema | ✗ | graph types | constraints | SHACL, OWL | ✗ | required, per attribute |

## Translations

### 1. A pattern with a filter: people Alice knows who are under 40

```text
GraQL   select ?who, ?age where /alice knows ?who . ?who age ?age . ?age < 40
SPARQL  SELECT ?who ?age WHERE { :alice :knows ?who . ?who :age ?age . FILTER(?age < 40) }
Cypher  MATCH ({key: 'alice'})-[:KNOWS]->(who) WHERE who.age < 40 RETURN who, who.age AS age
GQL     MATCH ({key: 'alice'})-[:KNOWS]->(who) WHERE who.age < 40 RETURN who, who.age AS age
Gremlin g.V().has('key','alice').out('knows').has('age', lt(40)).project('who','age').by().by('age')
```

### 2. Optional: everyone, with an email if they have one

```text
GraQL   select ?n, ?e by ?n where ?p a person ; name ?n and optional ( ?p email ?e )
SPARQL  SELECT ?n ?e WHERE { ?p a :person ; :name ?n . OPTIONAL { ?p :email ?e } } ORDER BY ?n
Cypher  MATCH (p:Person) RETURN p.name AS n, p.email AS e ORDER BY n
GQL     MATCH (p:Person) RETURN p.name AS n, p.email AS e ORDER BY n
Gremlin g.V().hasLabel('person').order().by('name')
          .project('n','e').by('name').by(coalesce(values('email'), constant('')))
```

In the property-graph languages, a missing property is already null. When
an email is a node of its own, Cypher and GQL use `OPTIONAL MATCH`.

### 3. Aggregates: people per city, most first

```text
GraQL   select ?c, count(?p) as ?n by ?n desc, ?c where ?p city ?c
SPARQL  SELECT ?c (COUNT(?p) AS ?n) WHERE { ?p :city ?c } GROUP BY ?c ORDER BY DESC(?n) ?c
Cypher  MATCH (p) WHERE p.city IS NOT NULL RETURN p.city AS c, count(p) AS n ORDER BY n DESC, c
GQL     MATCH (p) WHERE p.city IS NOT NULL RETURN p.city AS c, count(p) AS n ORDER BY n DESC, c
Gremlin g.V().has('city').groupCount().by('city').order(local).by(values, desc)
```

GraQL groups by the columns that are not aggregated, as Cypher does.

### 4. Paths: everyone above Dan, through parents or guardians

```text
GraQL   select ?a by ?a where /dan ((^parent | ^guardian)+) ?a
SPARQL  SELECT ?a WHERE { :dan (^:parent | ^:guardian)+ ?a } ORDER BY ?a
Cypher  MATCH (a)-[:PARENT|GUARDIAN*1..]->({key: 'dan'}) RETURN DISTINCT a
GQL     MATCH (a)-[:PARENT|GUARDIAN]->+({key: 'dan'}) RETURN DISTINCT a
Gremlin g.V().has('key','dan').repeat(__.in('parent','guardian')).emit().dedup()
```

A path written as a sequence is `knows/likes` (or `knows / likes`). In a
GraQL path, `/likes` is "then likes", not a key.

### 5. Recursive rules with negation: the roots of the family tree

```text
GraQL   rule ancestor(?x ?y) ?x parent ?y
        rule ancestor(?x ?y) ?x parent ?z . ancestor(?z ?y)
        rule root(?x) ?x parent ? . not ancestor(? ?x)
        select ?r where root(?r)
SPARQL  SELECT DISTINCT ?x WHERE { ?x :parent ?c . FILTER NOT EXISTS { ?a :parent+ ?x } }
Cypher  MATCH (x)-[:PARENT]->() WHERE NOT EXISTS { ()-[:PARENT]->(x) } RETURN DISTINCT x
GQL     MATCH (x)-[:PARENT]->() WHERE NOT EXISTS { MATCH ()-[:PARENT]->(x) } RETURN DISTINCT x
Gremlin g.V().where(out('parent')).not(__.in('parent')).dedup()
```

The others can express this one query, but not the rules. GraQL's rules are
saved in the database, and named, reusable and checked when defined. They
can be mutually recursive, and use stratified negation of each other.
That is Datalog, which the other four leave to separate reasoners or to
code.

### 6. Statements about links: when Alice met each friend

```text
GraQL   insert << /alice knows /bob >> since 2019
        select ?who, ?y by ?y where << /alice knows ?who >> since ?y
SPARQL  (RDF-star / SPARQL 1.2) SELECT ?who ?y WHERE { << :alice :knows ?who >> :since ?y } ORDER BY ?y
Cypher  MATCH ({key: 'alice'})-[k:KNOWS]->(who) RETURN who, k.since AS y ORDER BY y
GQL     MATCH ({key: 'alice'})-[k:KNOWS]->(who) RETURN who, k.since AS y ORDER BY y
Gremlin g.V().has('key','alice').outE('knows').order().by('since')
          .project('who','y').by(inV()).by('since')
```

In GraQL a link is an entry, so statements about it are ordinary links, and
statements about those nest: `<< << /a knows /b >> since 2019 >> source /census`.

### 7. Expressions: revenue per city

```text
GraQL   select ?c, sum(?p * ?q) as ?revenue by ?revenue desc where ?x city ?c ; price ?p ; qty ?q
SPARQL  SELECT ?c (SUM(?p * ?q) AS ?revenue) WHERE { ?x :city ?c ; :price ?p ; :qty ?q }
          GROUP BY ?c ORDER BY DESC(?revenue)
Cypher  MATCH (x) WHERE x.price IS NOT NULL
          RETURN x.city AS c, sum(x.price * x.qty) AS revenue ORDER BY revenue DESC
GQL     MATCH (x) WHERE x.price IS NOT NULL
          RETURN x.city AS c, sum(x.price * x.qty) AS revenue ORDER BY revenue DESC
Gremlin g.V().has('price').group().by('city')
          .by(project('p','q').by('price').by('qty').math('p * q').sum())
```

### 8. Parameters: the same query for any person

```text
GraQL   select ?f where $who knows ?f          (Session::run_with, or \set who /bob)
SPARQL  SELECT ?f WHERE { VALUES ?who { :bob } ?who :knows ?f }
Cypher  MATCH ({key: $who})-[:KNOWS]->(f) RETURN f
GQL     MATCH ({key: $who})-[:KNOWS]->(f) RETURN f
Gremlin g.V().has('key', who).out('knows')     (with who bound by the client)
```

### 9. History: whom Alice knew then, and doesn't now

```text
GraQL   select ?who where ( /alice knows ?who ) as of "2026-01-01" and not ( /alice knows ?who )
SPARQL  — (modeled by hand: versioned named graphs, or validity intervals on reified triples)
Cypher  MATCH ({key: 'alice'})-[k:KNOWS]->(who)
          WHERE k.from <= $t AND $t < k.to AND k.to <= datetime() RETURN who
GQL     — (as Cypher: validity properties kept by the application)
Gremlin — (as Cypher)
```

graqlite's log is append-only and its ids increase, so every query can see
the graph as of an id or a time. A deletion is a link of its own, so the
history is kept without the application recording it.

## Each language, against GraQL

- **SPARQL** is the closest relative: the same triples, `;` `,` `.`, blank
  nodes, `a`, `?x`, `OPTIONAL`, `ASK` (`verify`), `INSERT/DELETE … WHERE`,
  and property paths. GraQL identifies entities by `/keys` instead of IRIs,
  and stores values untyped, so `"10"` and `10` are one value, though
  computation is typed. GraQL adds rules, `as of`, and native statements
  about links. SPARQL has named graphs, IRIs and datatypes, `CONSTRUCT` and
  `DESCRIBE`, and federation.
- **Cypher** and **GQL** model a property graph: relationships have
  properties of their own, which GraQL writes as statements about links.
  - What they have that GraQL lacks: paths as values, weighted shortest
    paths and path modes (GraQL gives a path's least number of links, `(p+
    as ?n)`, and `lookup` the links of one such chain); `WITH` pipelines; `MERGE`; and, in GQL, graph types.
  - What GraQL has: recursive rules, with negation, beyond what quantified
    paths can express; `as of`; and its upserts are implicit (linking a
    live link returns it, and keys are unique).
- **Gremlin** is traversal steps embedded in a host language. It is very
  flexible, but the plan is mostly the order you write.
  - GraQL is declarative, and its planner orders the joins by estimated cost
    (with constraint propagation first, as the C++ GraQL did).
  - Recursion in Gremlin is `repeat()`, a loop, not a fixpoint with negation
    semantics.

## What only GraQL has, of these five

1. **Rules saved in the database:** Datalog with recursion and stratified
   negation, versioned and rolled back with the data, and checked when
   defined.
2. **History:** `as of` and `since` an id or an ISO 8601 time, and every
   link of a fact with `history`, for a whole statement or one group of
   patterns, from an append-only log.
3. **Statements about statements:** `<< s p o >>` in the data model, nesting
   freely, with each link's author and time recorded.
4. **Standing queries** in the engine, kept up to date from the commits.
   GraQL has no syntax for them yet; see issue 002's `subscribe`.

## What GraQL still lacks

- Paths as values, and weighted shortest paths (issue 035); the least
  number of links is `(p+ as ?n)`.
- `regex`, and typed literals (dates, language tags); values are untyped
  bytes (issue 009's option B would change that).
- Named graphs in queries (the engine has subgraph fields).
- `CONSTRUCT`-like results, `WITH`-style pipelines, and subqueries.
- A schema language.
- Several processes, or a server, sharing a database (issue 002).
