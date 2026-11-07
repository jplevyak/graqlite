# graqlite

A lightweight graph database written in Rust, with **GraQL**, a Turtle-like
query language and a `psql`-style shell.

- **Storage:** an append-only **record log** of nodes, values and links,
  where every entry is addressed by its byte position in the log, so ids
  are stable and ordered by time. Indexes are persistent hash tables whose
  large entries become append-only B+ trees.
- **Graph:** links of subject, predicate and object, with an author, a time
  and any number of **subgraphs**. Deleting a link appends a dead one, so the
  history stays: any query can see the graph **as of** an id or a time.
  A graph can be shared between threads: as an `RwLock<Graph>`, or through
  **readers** (`g.readers()`, then `latest()` on any thread), which see the
  last commit and run queries beside the writer, taking no locks.
- **Queries:** a cost-based planner and a streaming executor, with unions,
  negation, optional groups, filters, expressions, aggregates, paths and
  path expressions, recursive rules, parameters and prepared queries,
  standing queries, and queries returning **graphs** whose links can be
  derived by paths, rules and aggregates, or asserted; and **overlays**,
  links held in memory that queries read over the database (or alone).
- **GraQL** compiles to those queries, and the `graqlite` shell runs it.

## Examples

[`examples/`](examples/README.md) has GraQL scripts, each demonstrating an
area of the language or the shell, with the output each prints next to it
(and checked by `cargo test`): from the basics (01) and patterns (02),
through filters, paths and rules, templates, the shell, history, optional
patterns, aggregates, path expressions, parameters and expressions (03 to
12), to a comparison with other languages (13), the runnable parts of the
GraQL tour (14), lookups (15), derived links (16), subgraphs (17), asserted
links (18), aggregates as links (19), symbols and graphs (20), graph
layering (21) and the rest of the tour (22).
[`examples/pending/`](examples/pending/README.md) is for scripts of features
that open issues will add (there are none now).

```sh
cargo run -q -- -f examples/01-basics.graql     # run one in memory
cargo run -q -- mydb                            # or open a shell on a database
```

```text
mydb=> insert /alice knows /bob ; age 30 . /bob age 25 ; knows /carol
INSERT 4
mydb=> select ?who, ?age where /alice knows ?who . ?who age ?age
 who  | age
------+-----
 /bob | 25
(1 row)
mydb=> lookup /alice knows++ ?x
 link | subject | predicate | object | derived
------+---------+-----------+--------+--------------------
 #60  | /alice  | knows     | /bob   |
 #130 | /bob    | knows     | /carol |
      | /alice  | knows++   | /bob   | path from #60
      | /alice  | knows++   | /carol | path from #60 #130
(4 rows)
```

[`docs/graql-comparison.md`](docs/graql-comparison.md) compares GraQL with
GQL, Cypher, SPARQL and Gremlin, query by query.

## Data model

Every entry in the log has an **id**: its byte position in the log body plus the
log's `start_id`. Ids are therefore stable, ordered by insertion time, and need
no separate index to resolve.

| Entry     | Encoding                                          | Use                         |
| --------- | ------------------------------------------------- | --------------------------- |
| **Node**  | A zero-length value: a single `0x00` byte         | A vertex with no payload    |
| **Value** | Length followed by that many bytes                | Labels, properties, blobs   |
| **Tuple** | Count followed by that many ids, plus an extra bit | Edges / hyperedges linking entries |

By convention the fifth field of a tuple (index 4, `record_log::WHEN_FIELD`)
holds a timestamp.

## Graph

`graph::Graph` is the database: nodes, interned values, and links of
subject, predicate and object (plus author, creation time and subgraphs),
stored in memory mapped files and indexed by each of those.

```rust
use graqlite::graph::{Graph, Options};
use graqlite::Linkage;

// Pass Some(path_prefix) for a graph on disk, or None for one in memory.
let mut g = Graph::open(None, Options::default()).unwrap();
let knows = g.value(b"knows").unwrap();        // values are interned
let (alice, bob) = (g.node().unwrap(), g.node().unwrap());
let link = g.link(alice, knows, bob).unwrap();
assert_eq!(g.link(alice, knows, bob).unwrap(), link);  // already live
g.commit().unwrap();

assert_eq!(g.follow_forward(knows, [alice]), [bob].into());
assert_eq!(g.find_link(alice, knows, bob), Some(link));

// Deleting appends a dead link; the history stays queryable by id (time).
g.delete_link(alice, knows, bob).unwrap();
assert!(g.follow_forward(knows, [alice]).is_empty());
let all = g.links_at(Linkage::Subject, [alice]);
assert_eq!(g.live(&all, Some(link)), [link].into());
```

A link is identified by its subject, predicate and object, and the latest link
with them says whether it is live or dead. Ids increase as entries are
appended, so queries taking `as_of` see the graph as of that id. Appends are
visible at once and durable on `commit`; `batch` appends all or nothing.

Links may be about other links. `set_link` makes a property single-valued by
deleting its other live values. `node_by_key` finds or creates the node with a
unique key. `import` loads entries numbered elsewhere, mapping their ids to
local ones.

Index entries that grow large become append-only B+ trees, streamed newest
first. A graph keeps all its history unless **compacted** (`compact`, or
`tick` with `Options::history`), which drops from large entries what no query
as of the horizon or later can see; queries as of earlier then fail.

### Threads and readers

A `Graph` is `Send` and `Sync`: queries read `&Graph`, so threads can share
one as an `RwLock<Graph>`, queries running in parallel and writes waiting
for them. Or give them **readers**, which run beside the writer: a reader
is a read-only `Graph` as of the last commit, sharing the writer's memory
mapped files and index readers, and taking no locks. `reader()` makes one;
`readers()` gives a handle for other threads, whose `latest()` is a reader
of the latest commit, published by the writer at each commit. Compaction
never passes a live reader's commit.

```rust
use graqlite::graph::{Graph, Options};
use graqlite::query::Query;

let mut g = Graph::open(None, Options::default()).unwrap();
let knows = g.value(b"knows").unwrap();
let (a, b) = (g.node().unwrap(), g.node().unwrap());
g.link(a, knows, b).unwrap();
g.commit().unwrap();

let readers = g.readers();                     // Clone + Send + Sync
let counted = std::thread::spawn(move || {
    let r = readers.latest();                  // the last commit, read only
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, knows, y);
    q.run(&r).unwrap().len()
});
g.link(b, knows, a).unwrap();                  // the writer goes on
assert_eq!(counted.join().unwrap(), 1);        // not committed: not seen
```

### Subgraphs and authors

A **subgraph** is a set of tags over the live facts: a fact is in subgraph
`g` if its latest link is live and names `g`, so every subgraph is part of
the main graph, with history as liveness has. `add_to_subgraph` appends a
link with the fact's subgraphs and `g` (linking the fact if it isn't live),
`remove_from_subgraph` one without `g`, and deleting the fact takes it out of
every subgraph. A link's **author** is who last changed the fact.

```rust
use graqlite::graph::{Graph, Options};

let mut g = Graph::open(None, Options::default()).unwrap();
let (knows, friends) = (g.value(b"knows").unwrap(), g.value(b"friends").unwrap());
let (a, b) = (g.node().unwrap(), g.node().unwrap());
g.add_to_subgraph(a, knows, b, friends).unwrap();     // linked, and tagged
assert_eq!(g.subgraphs_of(a, knows, b, None), [friends]);
assert_eq!(g.subgraph_links(friends, None).len(), 1);
g.remove_from_subgraph(a, knows, b, friends).unwrap();
assert!(g.find_link(a, knows, b).is_some());           // still live
assert!(g.subgraphs_of(a, knows, b, None).is_empty());
```

## Queries

`query::Query` matches triple patterns over variables against the live
links, and `plan` makes a plan of it to `execute`, streaming the rows in
batches.

- **Patterns** of groups: unions, negation (`not`, like SPARQL's `FILTER NOT
  EXISTS`), optional groups (`OPTIONAL`), filters, `distinct`, ordering,
  `offset` and `limit`, and `as_of` for the query or a pattern. A pattern can
  be restricted to a subgraph or an author (`in_subgraph`, `by_author`,
  `in_graph` for a group): a constant, or a variable binding each.
- **Paths and rules:** path patterns match transitive closures, path
  expressions (`PathExpr`) chains of links like regular expressions, and
  rules define recursive relations (Datalog-style, stratified negation),
  evaluated semi-naively.
- **Planning** orders the patterns by estimated cost, from index counts and
  the graph's statistics, choosing a bind join (an index lookup per row) or a
  hash join for each. It may first reduce the top group by constraint
  propagation, when it estimates that pays.
- **Parameters and prepared queries:** a query run many times with different
  constants declares them as parameters (`q.param("x")`) and is prepared once
  (`q.prepare(&g)`); a point query then costs its index lookups (a couple of
  µs) rather than its planning (tens). Values that aren't entries are
  given as cells (`run_values`): a number compares as a number.
- **Expressions and aggregates:** `query::expr::Expr` (arithmetic,
  comparisons, functions) filters rows (`Filter::Holds`) and computes values
  (`q.compute`), and `q.aggregate`, with `group_by` and `having`, folds groups;
  such a query returns cells (`q.cells`).
- **Graphs as results:** a query that records its links (`record_links`, or
  `q.graph(&g)`) returns a `ResultGraph`: the links its rows matched, and links
  derived from them, each with the stored links it came from: by paths (one
  shortest walk each), by rules (each tuple's first derivation), and
  asserted (`q.assert(Assertion::new().triple(s, p, o).scope(v))`, and
  `q.rule_asserts`), with new nodes for blank nodes, and by aggregates
  (`q.link_aggregate(LinkAggregate::new(LinkFunction::Rank, s, p, key))`),
  whose links the rest of the query can match.
- **Overlays** (`Overlay`): links held in memory, over the database or
  alone, live or deleting a stored fact, layered (`over`), made from a
  result graph (`Overlay::of_graph`) and written (`insert_into`).  Any plan
  runs beneath one (`execute_over`, `q.run_over`, `q.graph_over`), its rules
  and paths too.  Their nodes, values and keys not in the database have
  ephemeral ids (`Graph::ephemeral_node`, `ephemeral_value`).
- **Standing queries** (`StandingQuery`) keep a query's rows up to date as
  entries are committed, from the newly committed links rather than
  evaluating it again; `MaintainedSubgraph` keeps a subgraph as a template's
  facts for each of their rows.

```rust
use graqlite::graph::{Graph, Options};
use graqlite::query::Query;

let mut g = Graph::open(None, Options::default()).unwrap();
let knows = g.value(b"knows").unwrap();
let (a, b, c) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
g.link(a, knows, b).unwrap();
g.link(b, knows, c).unwrap();

let mut q = Query::new();
let (x, y) = (q.var("x"), q.var("y"));
q.pattern(a, knows, x).pattern(x, knows, y).select([y]);
let plan = q.plan(&g).unwrap();
println!("{}", plan.explain());
assert_eq!(plan.execute(&g).collect::<Result<Vec<_>, _>>().unwrap(), [[c]]);

// Planned once, run for each person.
let mut q = Query::new();
let (who, y) = (q.param("who"), q.var("y"));
q.pattern(who, knows, y);
let mut friends = q.prepare(&g).unwrap();
assert_eq!(friends.run(&g, &[a]).unwrap(), [[b]]);
assert_eq!(friends.run(&g, &[b]).unwrap(), [[c]]);
```

## GraQL

`graql` parses GraQL and runs it with the
query engine. The [`graql` module docs](src/graql.rs) list the syntax in
full; in short:

(GraQL is unrelated to Graql, the typed query language TypeDB used from 2016
to 2021 and has since renamed TypeQL. graqlite has no schema: any node can
have any predicate.)

- **Statements:** `select` (with expressions, aggregates, `having`, and `by`,
  `limit` and `offset` before `where` or, as in SQL, after the pattern),
  `verify`, `lookup` (returning a graph), `insert` and
  `delete` with templates and `where`, `rule` / `delete rule`, `insert graph
  G`, `maintain`, `author` and `subgraph`, `set`, `data`, `default`,
  `assume`, `function` and `test`, each query optionally `as of` an id or an
  ISO 8601 time.
- **Patterns** in Turtle's style (`;`, `,`, `.`), blank nodes (`[ … ]`,
  `_:x`), `/keys`, `#ids`, `$params`, `and`, `or`, `not`, `optional ( … )` and
  the `-` modifier, comparisons and expressions (`let ?t = ?p * ?q . ?t >
  100`, with string, number and time functions), link nodes (`<< /a knows /b
  >> since 2019`), transitive predicates (`p++`), path expressions (`?x (knows+
  / ^parent | guardian{1,3}) ?y`, with keys as steps), and calls of rules.
- **The tour's shorter forms:** a predicate without an object (`?s type`), a
  subject alone (`lookup ?subjects`), `lookup P where Q`, `<<` / `>>` to
  reverse and chain predicates and `!p` to reverse one, `+` after objects,
  comparisons as blank nodes (`[ :?x != /a ]`), and tests of objects (`?p mass
  (< 40)`, `?p name (~"Bob.*")`, `?p name (prefix("Bi"))`).
- **Subgraphs:** `insert T into g`, `delete T from g`, `select … from g`,
  `lookup` / `verify … in g`, `( p ) in g`, the positions after an object (`s
  p o author subgraph`), the session's `author` and `subgraph`, `//keys` (the
  author's own), and `maintain T into g where P`, kept as the rows change.
- **Lookups return graphs**, with derived links: `lookup /a knows++ ?x`
  gives the links followed, and a link from `/a` to each `?x` reached.
  `s p o!` asserts a triple for each match, and `@?x` gives its scope:
  `lookup ?p name ?n ; pet ?x - . ?p pet_lover true! @?x`.  Aggregates
  derive links, which the pattern can match: `COUNT(?g grandkids ?c)`, and
  `ORDER(?m order ?began) @?p . ?m order (= 0)`, ranking from 0.
- **Symbols and graphs:** `set me = /joe`, `set g = graph { … }`, `set h =
  database:g`, `set found = graph ( lookup … )`; `select … from graph g`,
  `lookup … in graph g`, `insert graph g`; and the session's graphs, `data
  {}` (scratch), `default { … }` (a schema under every graph) and `assume`.
- **The rest of the tour:** `lookup - pattern` defaults, nested limits
  (`lookup ?p a person limit 10 and ?p pet ?x limit 1`), `function`s with
  conditions and typed operators, bare rule parameters, template rules
  (`insert property(/p 0 "gloss")`), `test`, and the shell's `\format
  tree`.

Statements can be parsed once and run with parameters (`Session::run_with`),
or planned once (`Session::prepare`, `run_prepared`). A statement that only
reads runs on a reader too, on any thread (`Session::read`). Parsed
statements print back as canonical GraQL (`Display`).

```rust
use graqlite::graph::{Graph, Options};
use graqlite::graql::{self, Output};

let mut g = Graph::open(None, Options::default()).unwrap();
graql::execute(&mut g, "insert /alice knows /bob ; age 30 . /bob age 25").unwrap();
let out = graql::execute(&mut g, "select ?who where /alice knows ?who . ?who age ?a . ?a < 30")
    .unwrap();
let Output::Rows { rows, .. } = &out[0] else { panic!() };
assert_eq!(rows, &[[g.find_by_key(b"/bob").unwrap()]]);

// A lookup's graph: the links followed, and those derived by the path.
graql::execute(&mut g, "insert /bob knows /carol").unwrap();
let out = graql::execute(&mut g, "lookup /alice knows++ ?x").unwrap();
let Output::Graph(graph) = &out[0] else { panic!() };
assert_eq!((graph.stored().count(), graph.derived().count()), (2, 2));

// A subgraph: who knows whom among the people of Paris.
graql::execute(&mut g, "insert /alice city paris . /bob city paris").unwrap();
graql::execute(&mut g, "insert ?x knows ?y into parisians where ?x knows ?y ; city ?c . ?y city ?c")
    .unwrap();
let out = graql::execute(&mut g, "select ?y from parisians where /alice knows ?y").unwrap();
let Output::Rows { rows, .. } = &out[0] else { panic!() };
assert_eq!(rows, &[[g.find_by_key(b"/bob").unwrap()]]);
```

Rules are saved in the graph, as the canonical text of each definition
linked from the node `/graqlite/rules`, so they commit and roll back with the
data, travel in snapshots, and are seen by every `Session`. Defining a rule
saved already does nothing, and a rule is checked (that its unions bind the
same variables, its head is bound, and the rules it calls are stratified)
before it is saved. Each definition has reference links to the values and
keys it names and the rules it calls, so `graql::rules_using` finds the rules
that use something. A query includes only the rules it calls, directly or
not, and saved text that no longer parses is left out and reported by
`Session::skipped_rules`. Parsed statements print back as canonical GraQL
(`Display`).

```rust
use graqlite::graph::{Graph, Options};
use graqlite::graql;

let mut g = Graph::open(None, Options::default()).unwrap();
graql::execute(&mut g, "insert /a parent /b . /b parent /c").unwrap();
graql::execute(&mut g, "rule ancestor(?x ?y) ?x parent ?y").unwrap();
graql::execute(&mut g, "rule ancestor(?x ?y) ?x parent ?z . ancestor(?z ?y)").unwrap();
// Another session, or another process after a commit, sees the rules.
let out = graql::execute(&mut g, "verify ancestor(/a /c)").unwrap();
assert_eq!(out[0], graql::Output::Verified(true));
assert_eq!(graql::rules_using(&g, b"parent").unwrap().len(), 2);
```

## Command line shell

`graqlite` is an interactive shell for a graph, in the manner of `psql`. It
runs GraQL statements and prints their results as tables, with history,
line editing and completion (by [rustyline](https://crates.io/crates/rustyline)).

```text
$ cargo run --release -- mydb
graqlite 0.1.0 (mydb).  Type \? for help, \q to quit.
mydb=> insert /alice knows /bob ; age 30 .
  /bob age 25
INSERT 3
mydb=> select ?who, ?age where /alice knows ?who . ?who age ?age
 who  | age
------+-----
 /bob | 25
(1 row)
```

`DIR` is the directory of the database, created if it does not exist; with
none, the database is in memory. A statement runs when it is complete. A line
ending in `.`, `;`, `,`, `and`, `or` or an open parenthesis continues it, and
a blank line runs what has been entered. Each statement is committed as it
runs, unless in a `\begin` ... `\commit` (or `\rollback`) transaction. A
statement that fails is rolled back, and so is the transaction it is in.
After each commit, the subgraphs the session `maintain`s are brought up to
date, and what that writes committed.

Results show an entity by its key, a value by its data (quoted when it is
not an identifier or number), and anything else as `#id`. Backslash
commands:

| Command                     | Does                                              |
| --------------------------- | ------------------------------------------------- |
| `\d`, `\dS`                 | the predicates, with their live links and fan out (`S`: with graqlite's own) |
| `\dr`                       | the rules saved in the database, with their bodies |
| `\subgraphs`                | the subgraphs, with their links                   |
| `\drop NAME`                | delete every definition of a rule                 |
| `\info`                     | a summary of the database                         |
| `\show ENTRY`               | an entry (`#id`, `/key` or value) and its links   |
| `\explain STMT`             | the query plan of a `select`, `verify` or `lookup` |
| `\begin`, `\commit`, `\rollback` | transactions                              |
| `\x`, `\format aligned\|csv\|tsv\|tree`, `\ids`, `\timing` | output settings |
| `\set [NAME VALUE]`, `\unset NAME` | parameters (`$NAME`); also `-v NAME=VALUE` |
| `\i FILE`, `\o [FILE]`       | run a file; send results to a file                |
| `\dump FILE`                | write a snapshot (restore it with `--restore`)    |
| `\compact ID\|TIME`         | drop the history before an id or time from large index entries; `as of` earlier then fails |
| `\?`, `\q`                  | help; quit                                        |

`-c TEXT` and `-f FILE` run statements and exit (`-1` runs them in one
transaction), as does piping a script to standard input. A script runs each
group of statements when a blank line or a backslash command ends it, and
stops at the first error, exiting with status 1. `--csv`, `--tsv` and `-x`
set the output format; see `graqlite --help`.

See [Examples](#examples) for scripts to run in it.

The shell is the `cli` feature, on by default; a library user can leave out
rustyline with `default-features = false`. One process at a time may open a
database (see [issue 002](issues/002-multi-process-access.md)); within it,
any number of threads (see [Threads and readers](#threads-and-readers)).

## Record log usage

```rust
use graqlite::RecordLog;

// A growable log whose first entry has id 1.
let mut log = RecordLog::new(1);

let alice = log.push_value(b"alice");
let bob = log.push_value(b"bob");
let knows = log.push_value(b"knows");
let edge = log.push_tuple(false, &[alice, knows, bob]).unwrap();

// Read entries back by id.
assert_eq!(log.blob(alice).unwrap(), b"alice");
let (ids, _extra) = log.tuple(edge).unwrap().decode(edge);
assert_eq!(ids, vec![alice, knows, bob]);

// Walk the whole log in insertion order.
let mut id = log.start_id();
while id < log.end_id() {
    let entry = log.entry(id).unwrap();
    id += entry.encoded_len() as u64;
}

// Persist the bytes and reopen them later, e.g. from a file or mmap.
let bytes = log.into_inner();
let reopened = RecordLog::adopt(&bytes[..]).unwrap();
assert!(reopened.is_valid());
assert_eq!(reopened.blob(bob).unwrap(), b"bob");
```

`RecordLog<B>` works over any buffer:

- `B: AsRef<[u8]>` (such as `&[u8]` or a memory map) can be read.
- `B: AsMut<[u8]>` (such as `&mut [u8]`) can also be written in place with
  `make_node`, `make_value` and `make_tuple` at ids you choose.
- `RecordLog<Vec<u8>>` grows as you append with `push_node`, `push_value` and
  `push_tuple`.

All decoding is bounds checked. A truncated or corrupt log returns an
`Error` and never reads out of bounds. `Entry::complete_len` and
`Tuple::estimate_len` help when you consume a log that is still arriving.

## Dynamic table

`dynamic_table` is a persistent hash table from a key (`key`, plus an optional
`key2` for 128 bit keys) to an extra bit and a list of non-zero `u64` values.
It has one `Writer` and any number of `Reader`s, which take no locks and can
look up keys on other threads while the writer inserts.

```rust
use graqlite::dynamic_table::{Options, Writer};

// Pass Some(path_prefix) for a table on disk, or None for one in memory.
let mut table = Writer::open(None, Options::default()).unwrap();
table.insert(42, 0, false, &[7, 8]).unwrap();  // append values
table.insert(42, 0, true, &[9]).unwrap();
table.replace(43, 99, false, &[1]).unwrap();   // replace a 128 bit key's values
table.delete(43, 99).unwrap();

let reader = table.reader();                   // Clone + Send + Sync
let found = reader.get(42, 0).unwrap();
assert_eq!((found.bit, found.values), (true, vec![7, 8, 9]));
assert!(reader.get(43, 99).is_none());
```

Each level of the table is a file of 128 byte buckets. Each level has twice as
many buckets as the one before. A key's operations are appended to its bucket
as compact varint records, and each bucket links to an overflow bucket when it
fills. When too many buckets overflow, the table moves to the next level a few
buckets at a time. Each level is mapped at a fixed address with room reserved to
grow, so growing never moves memory that readers are using. An old level is
unmapped `Options::retire_delay` after the move finishes (5 minutes by default).

It runs on 64-bit Unix only, and one process at a time; a lock file stops a second
writer.

## Record log format

All multi-byte integers are little endian.

### Header (16 bytes)

| Offset | Size | Field         | Meaning                                         |
| -----: | ---: | ------------- | ----------------------------------------------- |
| 0      | 4    | `magic`       | `0xe3e0ba2f`                                    |
| 4      | 1    | `version`     | Record log version, currently `0`               |
| 5      | 1    | `compression` | `0` = none, `1` = V1                            |
| 6      | 1    | `written`     | `0` = unwritten, `1`+ = restorable              |
| 7      | 1    | `reserved`    | Must be zero                                    |
| 8      | 8    | `start_id`    | Id of the first entry; `1` for a root database  |

The body follows immediately, at offset 16.

### Tagged variable integer

Every entry begins with an unsigned integer whose top bit is a tag: `0` for a
value, `1` for a tuple. The first byte is `[tag:1][more:1][value:6]` and each
following byte is `[more:1][value:7]`, least significant group first. Seven
bytes hold any 48 bit length.

### Group variable integer

A tuple's ids are stored in groups of five. Any remaining one to four ids form a
last, shorter group. Each group is a length prefix followed by each integer in
its minimal number of little-endian bytes (1 to 8):

- Groups of 1 or 2 ids have a 1 byte prefix. Groups of 3 to 5 ids have a 2 byte prefix.
- Bit 7 of the first prefix byte is the tuple's **extra bit**. Only the first
  group carries it.
- The other prefix bits hold each integer's `byte length - 1` in 3 bits, in order.

### Id encoding

Each id is stored relative to a base, normally the tuple's own id, and zigzag
encoded. An earlier id may be stored as a negative offset from the base (odd
values). Otherwise it is stored as its absolute value (even values). The
encoder picks whichever is smaller, so ids must be at most `i64::MAX`.

## Project layout

```text
src/
  lib.rs              crate root and public re-exports
  record_log.rs       RecordLog, PersistentHeader, Entry, Value, Tuple
  record.rs           Record (an entry and its id), links, RecordSet
  coder.rs            tagged / group variable integers and id encoding
  error.rs            Error and Result
  mmap.rs             memory mapping and file helpers
  dynamic_table/      persistent hash table: Writer, Reader (mod.rs), record
                      format, tests
  local_record_log.rs record log in memory mapped files, with commits and
                      snapshots (tests in local_record_log/)
  local_table.rs      index from ids or uuids to id sets, on a dynamic table,
                      promoting large sets to trees and compacting them
  postings.rs         large index entries as append-only B+ trees
  local.rs            local database: a log plus index tables (tests in local/)
  tuple_index.rs      built-in graph indexes: links by linkage, values, SPO,
                      intersections (tests in tuple_index/)
  stats.rs            per-predicate link statistics for query planning
  times.rs            the time of each commit, mapping times to ids, and
                      ISO 8601 times
  graph.rs            Graph: the graph database API, subgraphs, compaction,
                      readers, ephemeral values (tests in graph/)
  query/              pattern queries: types and builders (mod.rs), the
                      cost-based planner and prepared queries (plan.rs),
                      reduction (reduce.rs), batched operators (exec.rs),
                      path expressions (paths.rs), rules (rules.rs),
                      expressions (expr.rs), computed values and aggregates
                      (compute.rs, aggregate.rs), result graphs and derived
                      links (result.rs, provenance.rs), aggregates as links
                      (link_aggregate.rs), overlays (overlay.rs), standing
                      queries (standing.rs), maintained subgraphs
                      (maintained.rs), tests
  graql.rs            GraQL parser, lowering and runner; graql/ has its
                      expressions, printing, saved rules, prepared
                      statements and tests
  bin/graqlite/       the command line shell: arguments and line editing
                      (main.rs), commands (shell.rs), tables (render.rs)
benches/              record log, index, lookups and GraQL benchmarks
examples/             GraQL scripts, the output each prints, and pending/
                      for ones of open issues
docs/                 the comparison with GQL, Cypher, SPARQL and Gremlin
tests/
  cli.rs              runs the shell on the examples and tests/cli/*.graql,
                      comparing with their .out files, and tests its options
issues/               design notes for open work (closed ones in closed/)
```

## Building and testing

```sh
cargo build
cargo test          # unit tests, the README's examples, and the shell's scripts
GRAQLITE_BLESS=1 cargo test --test cli   # rewrite the scripts' .out files
cargo clippy --all-targets
cargo bench         # criterion: record log, index, lookups, GraQL
cargo doc --open
python3 scripts/check-links.py   # the Markdown files' relative links
```

The library needs Rust 1.88 or later (`rust-version` in `Cargo.toml`). The
shell's line editor, rustyline 19, needs 1.95, so on an older toolchain build
the library alone with `--no-default-features`. CI (`.github/workflows/ci.yml`)
runs the formatting, clippy, docs and link checks, the tests on Linux and macOS, and
both minimum toolchains. [`BENCHMARKS.md`](BENCHMARKS.md) records the
benchmarks' numbers on one machine, to compare a change with.

graqlite's runtime dependencies are [libc](https://crates.io/crates/libc), for
memory mapping, [sha2](https://crates.io/crates/sha2), for value uuids, and
[regex-lite](https://crates.io/crates/regex-lite), for GraQL's `~`.
[rustyline](https://crates.io/crates/rustyline) is used by the shell only, and
[criterion](https://crates.io/crates/criterion) for benchmarks only.

## License

MIT; see [LICENSE](LICENSE).
