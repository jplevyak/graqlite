# Persistent rules

**Status:** closed (decision: A, always save) · **Size:** medium · **Related:** [003](../003-queries-as-graph-data.md) (queries as graph data)

## Problem

A GraQL `rule` lives only in the `graql::Session` that defined it (a
`Vec<RuleDef>` in memory). When the `graqlite` shell exits its rules are gone,
so a user has to reload a rules file with `-f` or `\i` each time, and a library
user has to keep the rule text and redefine it in every new session. Rules are
part of the schema, and should last as long as the data does.

## Proposal: store rules in the graph

Keep each rule's GraQL text as a value, linked from a well-known node:

```text
/graqlite/rules  graqlite:rule  "rule reach(?a ?b) ?a knows ?m . reach(?m ?b)"
```

- **Loading:** `Session::load(&Graph)` (or `Session::open`) reads the live
  `graqlite:rule` links and parses their text, in id order, so the rules are
  defined in the order they were added.
- **Defining:** a rule that is saved appends the link, through the usual
  write path.
- **Dropping:** a new `drop rule NAME` statement (and a `\drop NAME` shell
  command) deletes the links of every definition of `NAME`.

Compared with a sidecar file (`DIR/rules.graql` or `~/.graqliterc`), this gives:

- Rules commit and roll back with the data, inside `\begin` ... `\commit`.
- They travel in `\dump` snapshots and through `import`.
- `as_of` sees the rules as they were at that time.
- Library users and the shell see the same rules, with no file to manage.

The cost is two reserved names (`/graqlite/rules` and `graqlite:rule`), in the
same way that `graqlite:key` is reserved for keys today.

## Storage format: text, plus reference links

The alternative is to store a rule as graph data: its parsed form as nodes and
links, with links to the entities it names. [Issue 003](../003-queries-as-graph-data.md)
designs that encoding, for queries in general. For rules, text is the better
main copy, with reference links added to get the main benefit of the graph
form.

| | Text | Graph data (003) |
|---|---|---|
| Who maintains it | people, who write and read it as text | programs, which change it incrementally |
| Defining and dropping | one link, so a rule is never half there | many links in one `batch`. Dropping has to delete the whole structure, and loading has to validate it. |
| Constants | resolved by name when the query is built (`src/graql.rs:944`) | bound to the entity or deferred to a key reference, chosen per constant |
| What you can ask about rules | nothing, without parsing every rule (but see the reference links below) | everything: dependencies, impact, every use of a predicate |
| Code | storing the text, plus the printer, which `\dr` wants anyway | an encoding, encoder, decoder and validator for every pattern kind |
| Grammar changes | old text may stop parsing, so `load` skips it and reports it | the stored structure needs migrating when the parsed form changes |

People write rules, so the deciding factor is that the text is what they
wrote and can edit. A rule's constants also usually name schema: predicates
and types, which are values, so binding them to entities gains nothing.
Values are interned by their data, so a link to the value `knows` already
means "whatever has the data `knows`".

### Reference links

When a rule is saved, link the rule's link (a link about a link) to what its
text names:

```text
L  graqlite:uses   knows          # a value: a link to the value
L  graqlite:uses   K              # an entity named by key: a key reference
L  graqlite:calls  "reach"        # a relation it calls
K  graqlite:refers_to_key "/alice"
```

`L` is the `/graqlite/rules graqlite:rule "..."` link. `K` is 003's shared key
reference node, so "what uses `/alice`" is one query over rules and stored
queries alike. The reference links are derived from the text and written in
the same `batch`. Dropping a rule deletes them along with the text. If they
are ever out of step with the text, they can be rebuilt from it.

Once 003 adds `#123` for bound ids, rule text can bind a constant to an
entity too, and the reference link is then a link to the entity itself. If
tools take over maintaining rules, rules can move to the graph form, and the
reference links are a subset of it.

## Prerequisite: print GraQL back

The parser keeps no source text, and a `RuleDef` holds the parsed `Pattern`.
To store a rule we need its text, so:

1. Implement `Display` for `graql::Node`, `Pattern` and `RuleDef`. The output
   must parse back to the same value. Quote values as the shell already does
   (`render::literal_of`): bare when they are an identifier or number that
   isn't a keyword or `a`, and quoted otherwise. Add parentheses where `and`,
   `or` and `not` need them.
2. Test round trips: `parse(&def.to_string())` equals `def`, over the
   statements in `src/graql/tests.rs`, plus a property-style test over
   generated patterns if one is cheap.
3. Use it in `\dr`, which can then show rule bodies and not only the heads.

A by-product is that `render::literal_of` moves into `graql`, so there is one
quoting function.

## Decision needed: what `rule` means

| Option | `rule` | Saving | Notes |
| ------ | ------ | ------ | ----- |
| **A. Always save** | saves in the graph | — | Simplest model. A scratch rule needs `drop rule`. Changes the meaning of an existing statement. |
| **B. Session by default** | session only, as now | `save rule NAME`, or `rule ... save` | Backward compatible, and scratch rules are free. Two kinds of rule to explain. |
| **C. Shell decides** | session only in the library | the shell saves by default, and `\set saverules off` turns it off | The library stays simple, but the behaviour differs between the shell and the library. |

Recommendation: **A**. Rules are schema, and every other statement that
changes the graph (`insert`, `delete`) is durable. A session-only rule can
still be had with `\begin` ... `\rollback`.

## Open questions

- **Redefinition:** today several `rule` statements with one name add
  branches to one relation (that is how recursion is written). Saving keeps
  that. Should `rule` with an existing name and identical text be a no-op?
  It probably should, so that reloading a rules file is idempotent.
- **Validation at load:** if a saved rule no longer parses (after a grammar
  change), `load` should report it and skip it, not fail the session.
- **Ordering:** rules that refer to rules defined later are fine at query
  time, since relations are resolved when a query is lowered. Check this
  once loading defines them in bulk.

## As implemented

Option A, with the recommendations above for the open questions, and these
differences from the plan:

- **No `Session::load`:** the graph is the only store of rules. A `Session`
  caches the parsed rules, keyed by the set of live rule links, and reads
  them again whenever that set changes. So a rollback, another session, or a
  fresh `graql::execute` always sees the saved rules, with nothing to keep in
  step.
- **`delete rule NAME`, not `drop rule NAME`:** `delete` and `rule` are already
  keywords, so no new keyword was needed. A new keyword would have stopped a
  value named `drop` from being written bare. The shell's `\drop NAME` runs
  it. Saved text that doesn't parse can be deleted by name too, if it starts
  `rule NAME(`.
- **Rules are checked before they are saved:** lowering alone rejects only
  calls to unknown rules. So `Query::validate` was added, which runs what
  planning checks before it reads the graph (unions bind the same variables,
  heads are bound, rules are stratified) without evaluating anything. A rule
  is checked once every rule it calls is defined. Until then it is saved
  unchecked, so that forward references work.
- **A query includes only the rules it reaches** through its calls. A saved
  rule that can't be evaluated (one calling a rule never defined) affects
  only its callers, and queries don't grow as rules accumulate.
- **A rule's variables no longer leak into the query:** `select ?y where
  r(?x)` used to find `?y` among a rule's variables.
- **The canonical text is what is saved,** so redefining a rule written
  differently is a no-op (the same text is the same interned value, so the
  same link).
- **The shell:** `\d` hides `graqlite:` predicates and `\dS` shows them. It
  warns about skipped rules at startup and in `\dr`.
- **The API:** `graql::{RULES_KEY, RULE_PREDICATE, USES_PREDICATE,
  CALLS_PREDICATE, REFERS_TO_KEY_PREDICATE, key_reference,
  key_reference_or_new, rebuild_references, rules_using, literal}`,
  `Session::{rules, saved_rules, skipped_rules}`, `Statement::DeleteRule`,
  `Output::DeletedRule` and `Error::Invalid`.

## Tasks

- [x] `Display` for `Pattern`, `RuleDef` and `Statement`, with round-trip tests (shared with 003)
- [x] Move the quoting function into `graql`, and use it from the shell
- [x] Reserve `/graqlite/rules`, `graqlite:rule`, `graqlite:uses`, `graqlite:calls` and `graqlite:refers_to_key` (shared with 003), and document them in `graql`
- [x] Read saved rules in `Session`, kept up to date (in place of `Session::load`)
- [x] Save on `rule`, and `delete rule NAME`
- [x] Reference links, written and deleted with the text, and `rebuild_references`
- [x] Shell: warn about skipped rules at startup, `\dr` shows bodies, `\drop`, `\dS`
- [x] README: the GraQL and shell sections
