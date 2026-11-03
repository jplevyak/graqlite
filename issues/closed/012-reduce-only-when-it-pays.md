# Reduce only when it pays

**Status:** closed · **Size:** medium · **Related:** [011](011-adaptive-btree-index-entries.md) (large entries; its phase 3 blamed this cost on hash lookups)

## Problem

Planning a query whose top group has two or more patterns always runs a
*reduction* (`src/query/reduce.rs`) first: constraint propagation that reads
index entries to narrow each variable's candidates and each pattern's links,
up to a budget of 65 536 entries. Nothing compares what reducing costs with
what it saves.

`benches/lookups.rs` (40 000 people, each with a `type`, `city` and `age`)
shows what that costs for `?p type person . ?p city paris`, 4 000 rows:

| | Time |
|---|---:|
| As planned today, with reduction | 7.0 ms |
| … of which planning alone | 5.3 ms |
| Without reduction (a bind join) | 3.0 ms |
| … of which planning alone | 15 µs |

The reduction reads 8 000 index entries and narrows nothing: every one of the
4 000 candidates is in the result. The query is 2.3 times slower for it.

Reduction pays when the joins' intermediate rows far outnumber the result:
dead ends that a bind join would follow, row by row, and the reduction prunes
first. A join that is one to one, as here, has none.

## Design

Plan the query without reduction first, which costs microseconds. Then
estimate, optimistically, what reducing would cost, in the planner's units,
and reduce only if that is below the unreduced plan's cost:

- **The reduction itself:** the pattern with the fewest entries is read from
  its constants, then each other pattern by the cheaper of its own constants
  or a lookup per candidate of a variable it shares with a pattern already
  read. A variable's candidates are taken to be the fewest links of a
  pattern read with it, and no more than the rows estimated: optimistic, as
  if the reduction narrowed everything to what the rows use.
- **The reduced plan:** each pattern's reduced links, at most the rows
  estimated, and a reduced lookup per row for each pattern after the first.

When the planner's estimates say the joins are one to one, both terms are at
least the unreduced plan's cost, and it is used as is.

An explicit `reduction_budget` still forces a reduction (and 0 still turns it
off), so that tests can exercise it.

## Open questions

- **Estimates can be wrong,** and skipping a reduction that would have pruned
  is the cost of a wrong one. The random query tests check answers, not
  plans, so they say nothing here; the benchmarks do. The estimator assumes
  every value has the commonest fan-out, so it rarely sees the dead ends a
  reduction would prune, and a reduction that would prune them is skipped.

## As implemented

`reduction_may_pay` in `src/query/plan.rs`, called from `plan_full`, as
designed, with one addition: the reduction's cost is counted at
`REDUCTION_FACTOR` (2) times a join's for the same lookups and entries. The
benchmark measured that: the reduction above spent 5.3 ms on about 24 000
units of the planner's cost model, where the join without it spent 3.0 ms on
about as many, as the reduction builds sets of candidates and links as it
reads. Without the factor, a chain of four patterns from five people in
Paris was a near tie (76 against 79) and reduced, for no gain.

The gate applies only when no budget is given and no links are (standing
queries give them); either still reduces as before. Tests of the reduction
itself now give a budget, and assert that the default skips it.

In the random query tests, the gate reduces for about 60% of the plans it
decides (1 418 against 924 skipped), so both ways are exercised against the
brute-force answers.

### Results

On 2026-10-31, 40 000 people:

| | Before | After |
|---|---:|---:|
| `type person` and `city paris` (`benches/lookups.rs`) | 7.0 ms | 2.98 ms |
| … planning it | 5.3 ms | 20 µs |
| the same in `benches/index.rs` | 6.8 ms | 2.9 ms |

The 4 µs more planning is the plan without reduction made first. `plan type
person and city paris` in `benches/index.rs` gives a budget, so it still
reduces and still measures the reduction (5.2 ms).

`examples/06-shell.out` changed: its `\explain` no longer reduces.
