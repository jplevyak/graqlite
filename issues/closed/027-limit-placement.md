# A trailing `limit` silently becomes a nested limit

**Status:** closed · **Size:** small · **Related:** [016](016-graql-gaps.md) (nested limits), [008](008-parameters-order-offset.md) (`limit` and `offset`)

## Problem

The statement's `limit` goes before `where`: `select ?x limit 3 where
...`. Written after the pattern, where SQL, SPARQL and Cypher put it, it
parses as a nested limit on the last predicate's object (016's `?p pet ?x
limit 1`). Both are valid, so nothing warns:

```text
select ?c, count(?p) as ?n by ?n desc where ?p city ?c limit 3
```

returned all ten groups: the limit applied to `?c` within each `?p`, and
each person has one city. With `limit 3` before `where` it returned
three. A user coming from any other query language will write the first
form and get the wrong answer without an error.

## Design

Options:

1. **Accept the statement clause after the pattern too.** A `limit` (or
   `offset`, `by`) that ends the statement, after the last pattern, is
   the statement's; a nested limit is only one followed by more pattern
   (`. `, `;`, `and`), or written in parentheses: `( ?p pet ?x limit 1 )`.
   This changes the meaning of a trailing nested limit, which the
   examples would show (22 uses it at the top level: check them).
2. **A distinct keyword for nested limits:** `?p pet ?x each limit 1`, or
   `first 1`. No ambiguity, one more keyword to learn.
3. **Keep the grammar and warn.** `\explain` prints which variable a
   nested limit binds, and the shell warns when a statement's only
   `limit` is nested at the end.

1 matches what people write; 3 is cheap and could go in first.

## Tasks

- [x] Decide between 1 and 2: 1
- [x] Do 3: `\explain` shows nested limits; the warning was dropped with 1
- [x] Update the grammar in `graql.rs` and its module docs
- [x] Examples 11 and 22, and the comparison (010), show both forms
- [x] Tests for the trailing form with and without `by`

## As implemented

Option 1, with SQL's trailing `by` too:

- **A `select`'s `limit` and `offset` ending the pattern are the
  statement's.** The parser records where the last nested limit it read
  ended; if the pattern ends there, the `select` takes it off the pattern
  (through `and`s), with the node `?v limit n` writes alone if the pattern
  binds `?v` elsewhere, so `... ?p city ?c limit 3` and `limit 3 where ...
  ?p city ?c` parse to the same statement. Both spellings of a nested limit
  count, `?p city ?c limit 3` and `. ?c limit 3`: the rule is about the
  position, not the form.
- **`by`, then `limit` and `offset`, may follow the pattern**, as in SQL:
  `select ?c, count(?p) as ?n where ?p city ?c by ?n desc limit 3`. Each
  goes before `where` or after the pattern, not both: given twice, it is a
  syntax error at the second, which says to write a nested limit ending the
  pattern in parentheses.
- **A nested limit is one followed by more pattern, or in parentheses**:
  `select ?c where ( ?p city ?c limit 3 )`. Parentheses don't nest a group
  in the lowering's sense, so the limit stays at the top of the pattern,
  where nested limits are allowed. `lookup` and `verify`, which have no
  limit of their own, are unchanged.
- **Printing** puts a `select`'s pattern in parentheses when it ends with
  a nested limit, so that it reads back the same.
- **`\explain`** prints each nested limit after the match tree: `nested
  limit ?c 3`, with `offset m`, and `for each ?p, ...` for the limits
  before it.
- **No warning.** Option 3's warning (`Statement::warnings`, printed by the
  shell) was written first, but with option 1 the case it warned of no
  longer exists, so it was removed rather than kept empty.
- **A bug found on the way:** nested limits were ignored by queries that
  compute values. `cells`, the path for aggregates and expressions,
  drained the matches into the computation without the `ValueLimits`
  operator that `rows_of` adds, so `select count(?p) as ?n where ( ?p a
  person limit 2 )` counted every person. The Problem's example returned
  all ten groups because of this, not because the limit applied within
  each `?p`. Fixed by adding `ValueLimits` there too.
- **Meaning changed:** a `select` that ended with a nested limit meant to
  be one now limits its rows. No example did; one test in
  `src/graql/tests.rs` (`. ?p limit 2 . ?x limit 1`) did, and now writes
  `and ( ?p limit 2 . ?x limit 1 )`.
- Tests: `trailing_limit_is_the_statements`, `explain_shows_nested_limits`
  and `nested_limits_apply_before_aggregating` in `src/graql/tests.rs`, and
  `tests/cli/trailing-limit.graql`. Examples 11 (trailing `by`, `limit`,
  `offset`) and 22 (a nested limit in parentheses against the statement's)
  show both forms. The comparison (`docs/graql-comparison.md`) needed no
  change: its paging row names the clauses, not their place.

## Closed

Done.
