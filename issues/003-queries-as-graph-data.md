# Queries as graph data

**Status:** open · **Size:** large · **Related:** [001](closed/001-persistent-rules.md) (rules), [002](002-multi-process-access.md) (`subscribe`)

## Problem

GraQL text suits people, who write and read queries. It suits programs less:
standing queries registered by services, generated subscriptions, and queries
tuned by tools. A program that changes a text query has to generate text and
re-parse it. Nothing can ask the database which queries depend on a
predicate or an entity. And text can only resolve a constant by name when the
query runs, never pin it to a particular entity.

Storing a query as nodes and links in the graph solves all three. The graph
form would be the main copy for queries that machines maintain, and the
format that tools work with. Text stays the main copy for queries that people
maintain (see 001), and the two convert into each other.

## What the graph form gives

- **Incremental edits:** adding a filter is adding a link, and swapping a
  subtree is relinking one node.
- **Queries about queries:** "which subscriptions read `knows`", or "what
  depends on `/alice`", is an ordinary GraQL query, served by the existing
  indexes.
- **Bound or deferred constants, per constant:** see below. Text can only
  express deferred ones.
- **Reacting to changes:** a `StandingQuery` over the stored queries notices
  when one changes, and its owner re-plans it.
- **Structural sharing:** versions of a query can share their unchanged
  subtrees, because subtrees are referenced rather than copied.

## Constants: bound or deferred

| Constant | Stored as | Resolved |
| -------- | --------- | -------- |
| A value (`knows`, `"Alice Smith"`, `30`) | a link to the value | Not needed: values are interned by their data, so the value *is* "whatever has this data", and can't change. |
| An entity, bound | a link to the entity | When stored. It survives the entity's key changing or being removed. |
| An entity, deferred | a link to a **key reference** node | When the query is lowered, by `find_by_key`, exactly as text does today. An unknown key gives id 0, which matches nothing. |

A key reference is a node with `graqlite:refers_to_key "/alice"`. It must
**not** use `graqlite:key`: a node with that key *is* the entity, since keys
are unique. Key references are shared, one per key, and are found through the
object index on `graqlite:refers_to_key`. So "what refers to `/alice`" is one
lookup, for 001's rule text and for stored queries alike.

Keyless nodes can only be bound.

For text to express everything the graph can, GraQL gains `#123` for a bound
id. The shell already prints entries that have no key as `#123`, so its
output can then be pasted back as input. With that, text converts to the
graph form and back without loss.

## What to encode: GraQL's parsed form

Encode `graql::Statement` and `graql::Pattern`, not `query::Query`:

- `query::Query` has already resolved names to ids, so it can't hold deferred
  constants.
- Comparisons with a literal lower to `Filter::Test`, an opaque closure
  (`src/graql.rs:989`), which can't be stored.
- `Relation` indices in a `Query` are positions in one query.

The parsed form has names, a tree of `and`, `or` and `not`, and comparisons
as data, and it lowers through the existing `lower` path. It needs one new
variant, `Node::Id(u64)`, for bound constants (and `#123`).

## Sketch of the encoding

Every structural node has a `graqlite:kind`. Each variable is one node per
query, and a blank node is a variable with `graqlite:blank true`.
`select ?w where /alice knows ?w . ?w age ?a . ?a < 30` becomes:

```text
Q   graqlite:kind graqlite:Select ; graqlite:select S0 ; graqlite:where A
S0  graqlite:kind graqlite:Slot ; graqlite:pos 0 ; graqlite:item Vw
A   graqlite:kind graqlite:And ; graqlite:part T1, T2, C1
T1  graqlite:kind graqlite:Triple ; graqlite:subject K ; graqlite:predicate knows ; graqlite:object Vw
T2  graqlite:kind graqlite:Triple ; graqlite:subject Vw ; graqlite:predicate age ; graqlite:object Va
C1  graqlite:kind graqlite:Compare ; graqlite:op "<" ; graqlite:left Va ; graqlite:right 30
K   graqlite:kind graqlite:KeyRef ; graqlite:refers_to_key "/alice"
Vw  graqlite:kind graqlite:Var ; graqlite:name w
Va  graqlite:kind graqlite:Var ; graqlite:name a
```

The other kinds:
- `Or` and `Not`;
- `Path` (`p++`);
- `Call`, with a relation name and argument slots;
- `Rule`, with a name, head slots and a body;
- `Verify` and `Lookup`;
- later, `Insert` and `Delete` templates.

`Select` also holds `distinct`, `by` slots and `limit`.

### Ordered lists need nodes for their items

A link is identified by its subject, predicate and object. So `reach(?x ?x)`
can't be two links `C graqlite:arg Vx`: the second would be the first again.
Ordered lists (head variables, arguments, `select` and `by` variables) use a
**slot** node per item, which has a `graqlite:pos` and a `graqlite:item`. The
parts of `and` and `or` are distinct nodes, so they need no slots. They come
out in the order their links were appended, which follows the text's order.

## Writing, editing and dropping

- **Storing:** a query is written in one `Graph::batch`, so a half-written
  query is never committed. A `batch` commits, and on failure rolls back
  everything since the last commit, so inside a shell `\begin` transaction
  the shell has to append the links directly instead.
- **Editing:** treat a stored query as immutable, and give it a **handle**: a
  node with a single-valued `graqlite:current` link (`set_link`) to the
  query's root. An edit builds a new root that shares the unchanged
  subtrees, then moves the handle, so readers switch atomically from one
  complete version to the next. `as_of` on the handle gives past versions,
  each of them complete. Editing in place, inside a batch, would also work,
  but readers could see a query that is half changed.
- **Dropping:** delete the handle's links, and the links of any subtree no
  other version uses. As with any deletion in graqlite, the nodes stay in the
  log, unlinked.
- **Loading:** `load(&Graph, root)` checks the structure (each kind's
  required links, one of each where one is expected, slot positions
  `0..n`). It reports where in the structure a check failed, because tools
  and `insert` or `delete` can leave a structure broken.

## Visibility

The query graph's links are ordinary links in the `graqlite:` namespace, like
key links today. So `select * where ?s ?p ?o` sees them, and the statistics
count them (`\d` lists them too). The choices:

1. Accept this, and have the shell's `\d` hide `graqlite:` predicates by
   default.
2. Put the links in a subgraph (`LinkFields::subgraphs`), and teach patterns
   to exclude or select subgraphs. GraQL reports subgraph positions as
   unsupported today, so this is a feature of its own.

Start with 1. The `graqlite:` namespace is already reserved by keys.

## API sketch

```rust
// graql
pub fn store(g: &mut Graph, statement: &Statement) -> Result<u64>;  // root id
pub fn load(g: &Graph, root: u64) -> Result<Statement>;
impl fmt::Display for Statement { ... }   // from 001: graph -> text

// handles
pub fn store_as(g: &mut Graph, handle: u64, statement: &Statement) -> Result<u64>;
pub fn current(g: &Graph, handle: u64) -> Option<u64>;
```

The shell: `\store NAME STMT` stores a query under the handle `/NAME`,
`\run NAME` runs it, and `\show` of a query's root prints its text.

## Phases

1. **Bound constants in text:** `Node::Id` and the `#123` syntax, with tests.
   (This is also useful by itself: the shell's output can be pasted back.)
   *Done, in [008](closed/008-parameters-order-offset.md).*
2. **The printer:** `Display` for the parsed form, shared with 001.
3. **Encoding:** `store` and `load` with validation, and round-trip tests:
   text → graph → text, over the statements in `src/graql/tests.rs`.
4. **Handles and versions:** `store_as`, `current`, structural sharing, and
   dropping.
5. **Users:** stored standing queries, which re-plan when their handle moves,
   and the server's `subscribe` (002). Rules can switch from text to this
   when tools start maintaining them (001).
6. **Shell:** `\store`, `\run`, and `\show` printing queries, and `\d`
   hiding `graqlite:` predicates.

## Open questions

- **Patterns with `as_of`:** `query::Pattern::as_of` isn't in the GraQL
  grammar. It should be stored as an attribute of `Triple` and `Path` once
  the grammar has it.
- **A program building queries without text:** should there be a builder
  that writes the graph form directly, or is building a `Statement` and
  calling `store` enough? Start with `store`.
- **Literal values:** storing `?a < 30` interns the value `30` if it isn't
  in the graph. That is harmless, but it is a write that text doesn't make.
