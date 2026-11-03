# Optional patterns

**Status:** closed · **Size:** medium · **Related:** [006](006-aggregates.md) (computed values in rows)

## Problem

A query can't say "and, if there is one, its X". SPARQL's `OPTIONAL`, Cypher
and GQL's `OPTIONAL MATCH`, and Gremlin's `optional()` all do this, and in
practice it's the feature people miss most: "each person, and their email if
they have one". Today it needs two queries, or a union with a negation:

```text
select ?p, ?e where ?p a person ; email ?e
                 or ?p a person and not ( ?p email ? )   -- and ?e unbound: not allowed
```

That doesn't work either, because both branches of an `or` must bind the
same variables.

## Design

### Syntax

The grammar marks a predicate optional with a `-` modifier after its
objects. GraQL parses it, and reports it as unsupported:

```text
select ?p, ?e where ?p a person ; email ?e -
```

That is terse but hard to read, and it applies only to a single predicate.
Add a group form as the main syntax, and keep `-` as shorthand for an
optional group of one predicate:

```text
select ?p, ?e, ?n where ?p a person and optional ( ?p email ?e . ?e verified ?n )
```

### Meaning

An optional group is a left outer join: each row of the group so far is
kept, extended by every match of the optional pattern, or left with the
optional's new variables **unbound** if it has none. This is SPARQL's
semantics, including its well-known edge case: a filter inside the optional
group sees the outer variables.

### The engine

- **Unbound:** rows are `Vec<u64>` ids, and no entry has id 0, so 0 means
  unbound. The shell already prints id 0 as `-`.
- **`Group::optional(group)`:** joined after the required patterns, by a
  bind join (evaluate it per row) or a hash join (evaluate it once, then
  left-join), chosen by cost as for `not`.
- **Filters, comparisons and `not`** over an unbound variable are false, as
  in SPARQL.
- **`by`:** unbound sorts first.
- **Reduction:** an optional group must not narrow the required variables.
  Reduce only within it.
- **Standing queries:** a change can switch a row between bound and unbound.
  This works like the existing handling of `not` (`NotChange`), and is the
  hard part.
- **Rules:** an optional in a rule body makes the relation's tuples carry
  unbound values. Allow it, and test it with negation.
- **`lookup`:** report the links of the matches that are bound.

## As implemented

As designed, with these decisions:

- **Where optional groups go:** a group's optional groups are joined after
  all of its required parts (patterns, paths, calls and unions), in the order
  written, wherever they appear in the text. So `A and optional (B) and C`
  means `(A and C) ⟕ B`. SPARQL instead makes the position matter. This
  version is simpler, and it never joins a required pattern against an
  unbound variable.
- **Unbound means a bound 0, not a free variable.** This differs from the
  plan above, which said negations of unbound variables are false.
  - A filter or comparison of an unbound variable fails.
  - A `not` or a later optional group that is joined on one finds no match,
    because no link has id 0. So `not ( ?e verified yes )` *holds* for a row
    without `?e`. That is the intuitive reading ("has no verified email").
    SPARQL would leave `?e` free there.
- **Rules:** an optional group in a rule body may leave head variables
  unbound. For stratification, a call inside an optional group counts like
  one inside a negation, because a left join is not monotonic. A relation
  that depends on itself through an optional group is rejected.
- **Standing queries** reject optional groups with a clear error for now.
  Keeping them up to date incrementally is left for when it is needed.
- **Templates:** `insert` and `delete` leave out a triple with an unbound
  variable.
- **`select *`** and a query's default selection include the optional
  variables.
- **The shell** shows unbound values as empty cells, as psql shows `NULL`.
- **Planning:** `Step::Optional` is a bind join (the group evaluated for
  each batch of rows, then the rows with no match passed through) or a hash
  join (the group evaluated once, and probed), chosen by cost.
  `\explain` shows `optional (for the rows)` or `optional (once, on ...)`.
- **Syntax:** `optional ( ... )` after `and` or after `.`, and the `-`
  modifier after a predicate's objects. `optional` became a keyword.
- **Tests:** the random query test now generates optional groups (nested,
  with filters and negations over their variables) and checks every plan
  against brute force, with bind and hash joins forced. It asserts that
  optional groups were joined both ways, and that rows really had unbound
  values. There are also focused engine tests (rules, stratification,
  standing queries), GraQL tests and round trips, and
  `examples/08-optional.graql`.

## Tasks

- [x] `Group::optional`, with the planner (bind or hash, by cost) and execution, using 0 for unbound
- [x] Filters, negation and ordering with unbound variables
- [x] Reduction confined to the optional group (it reduces only the top group's own patterns)
- [x] Standing queries: a clear error until they are supported
- [x] GraQL: `optional ( ... )` and the `-` modifier, `Pattern::Optional`, printing
- [x] Tests, an example, and docs
