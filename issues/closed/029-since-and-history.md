# `since` and `history`: the other two views of the past

**Status:** closed · **Size:** medium · **Related:** [004](004-history-and-link-variables.md) (`as of`, link variables), [018](018-snapshot-readers.md) (readers as of a commit)

## Motivation

Datomic gives a database value three filters over its immutable log:
`as-of t` (the database as it was), `since t` (only what was added
after `t`) and `history` (every assertion and retraction, with whether
each added or retracted). graqlite has the first, and keeps everything
the other two need: deleting appends a dead link, nothing is rewritten,
and every link carries its id (its place in time), its author and its
creation time. What is missing is a way to ask for it.

The questions these answer come up in every audited system: what changed
since the last report; who deleted this and when; what did this property
go through. Today each needs a pair of `as of` queries and a `not`, or
`\show` on one entry.

## Design

Two additions to [`Pattern`](../../src/query/mod.rs) and the grammar, both
orthogonal to `as of`:

- **`since ID|TIME`**, on a statement or a group as `as of` is: a pattern
  matches only links appended after the id (a time maps to an id as for
  `as of`). With `as of` it is a window: `select ?who as of 300 since
  200 where /ann knows ?who` is who Ann came to know between 200 and 300.
  The executor already filters candidate ids by `as_of` (`range(..=as_of)`)
  and streams large entries newest first from it; `since` is the other
  bound of the same range. A link that was live before `since` and is
  still live is not matched, as in Datomic; a link deleted and re-added
  after is. Standing queries (`standing.rs`) compute exactly "the links
  committed since" already, so `since` can share `live_links` with them.
- **`history`**, on a statement or a group: a pattern matches every link
  of its fact, live or dead, latest or not, instead of the latest live
  one. A link node binds the link: `select ?l, ?who, state(?l),
  iso(when(?l)), author(?l) history where << :?l /ann knows ?who >>` is
  the full story of Ann's acquaintances, in id order. `state` is a new
  expression function (`live`, `dead`); `when` and `author` exist. In the
  executor, `history` skips the liveness pass (`Graph::live`, the `seen`
  check in `Matcher::begin`), which is less work, not more. A `history`
  pattern can't be reduced (the reduction keeps latest links) and can't
  be a standing query.

Both are per pattern in `query::Query` (`Pattern::since`,
`Pattern::history`) so a group can mix them, as `as of` can.

The shell's `\show` already prints an entry's history; with `history` it
can be one query, and `\dr` could show when each rule was defined.

## Tasks

- [x] `Pattern::since` and `Query::since`, in the executor's candidate
      ranges and the reduction's
- [x] `Pattern::history` and `Query::history`; skip liveness; refuse in
      reductions and standing queries with a clear error
- [x] Grammar: `since ID|TIME` and `history` wherever `as of` goes;
      `Display` prints them; `state(?l)`
- [x] Example 07 gains a `since` window and a `history` listing; the
      comparison (010/022) gains the Datomic column
- [x] Tests: the random query tests check `history` against the links
      enumerated from the log, and `since` against `as of` pairs

## Open questions

- Should `history` rows carry the subgraphs each link had, so that
  subgraph membership over time is queryable too? The link's record has
  them; a `subgraphs(?l)` function returning a list needs list values,
  which expressions don't have yet.
- Datomic also exposes the transaction as an entity. graqlite's commit
  has a time (`times.rs`) but no id of its own; a `commit(?l)` function
  giving the first id of the commit that wrote `?l` would be enough for
  "everything in the same commit as this".

## Outcome

`since ID|TIME` and `history` work on a `select` and on any `( group )`,
with `as of` and in any order, and `Pattern::since`, `Pattern::history`,
`Query::since` and `Query::history` are the library's. `state(?l)` says
`live` or `dead`. Example 07 has a window and a listing, and the comparison
has a Datomic column. A random test checks every plan shape against links
enumerated from the log, and `since` against the links live at its ends.

`since` is a lower bound on the same range as `as of`, so a large entry
streamed newest first stops at it. A fact's latest link is the newest in the
range, if it has one there, so nothing older is read. `history` takes the
links of the range with no liveness pass, oldest first, so it reads a large
entry whole rather than streaming it.

### How it differs from the plan

- **Neither word is a keyword.** Example 07 uses `since` as a predicate, so
  they are recognized only after a group's `)` and before a select's
  `where`, where no node can be.
- **`verify` and `lookup` take them on a group,** as `verify ( p ) since 5`.
  Their `as of` is a prefix, and there `since 5 /x` is already a triple.
- **A statement's `since` and `history` print on a group.** The parser wraps
  the pattern, so `select ?w since 2 where p` prints as `select ?w where
  ( p ) since 2`, which means the same.
- **`history` skips reduction rather than failing.** Reduction is the
  planner's choice, so a query reads every link without it, even given a
  budget. A standing query does refuse, for `since` as well: its deltas
  compute what changed between commits and have no lower bound.
- **More is refused than the issue names,** each with its own error: paths
  (they follow live links), rule calls and `nearest` in a `since` or `history`
  group or query, aggregates' links, overlays (result graphs and assumptions,
  whose links have no place in the log), and `history` when recording links,
  as `lookup` does, since what it returns is live.
- **Compaction limits `history`, not `since`.** Compaction keeps what `since`
  matches but drops superseded and dead links, so a `history` pattern needing
  links at or before the horizon fails with `Error::Compacted`, reporting `as
  of 0` if it has no `since`.

`\show` and `\dr` are unchanged. The open questions above remain open.
