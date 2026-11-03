# A trailing `limit` silently becomes a nested limit

**Status:** open · **Size:** small · **Related:** [016](closed/016-graql-gaps.md) (nested limits), [008](closed/008-parameters-order-offset.md) (`limit` and `offset`)

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

- [ ] Decide between 1 and 2; do 3 regardless
- [ ] Update the grammar in `graql.rs` and its module docs
- [ ] Examples 11 and 22, and the comparison (010), show both forms
- [ ] Tests for the trailing form with and without `by`
