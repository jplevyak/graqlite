# `\explain analyze`: actual rows and time beside the estimates

**Status:** closed · **Size:** small · **Related:** [012](012-reduce-only-when-it-pays.md) (estimates the planner makes), [013](013-prepared-queries.md), [026](026-index-lookup-allocations.md)

## Motivation

PostgreSQL's `EXPLAIN ANALYZE`, DuckDB's `EXPLAIN ANALYZE` and Kùzu's
`PROFILE` run the plan and print, for each step, the rows it produced
and the time it took next to what the planner expected. It is the tool
that makes a cost-based planner debuggable: a step whose estimate is off
by a thousand is where the statistics or the cost constants are wrong.
`\explain` prints the plan and its estimates (`rows ~23`), and `\timing`
the whole statement's time; nothing connects the two to a step.

The issues record several planner decisions tuned from benches
(`REDUCTION_FACTOR`, `LOOKUP_COST`, the exhaustive limit); each would
have been faster to tune with per-step numbers from real queries.

## Design

- **Counters on operators.** Each `Operator` in `exec.rs` counts the
  rows it emits and the batches it was asked for, and accumulates the
  time spent in its `next` (excluding its input's, so times are per
  step). A `Lookup` counts lookups and candidate ids read, which is the
  planner's unit of cost. `Plan::execute_profiled` builds the operators
  with counting on; the plain path pays nothing.
- **Output:** `\explain analyze STMT` runs the statement and prints the
  plan with `rows ~23 actual 23`, `entries 46 actual 51`, and the time per
  step and in total; for a reduction, the entries read and the candidates
  found against its estimate. Rules print per stratum and per round;
  standing queries per term.
- **Library:** `Plan::profile(&g) -> Profile` with the same numbers as
  data, so tests can assert on them (a planner test can check that an
  estimate is within a factor of the actual).
- **A step further:** `\explain analyze` on a prepared query shows
  whether a replan would choose differently, which is the question 013
  leaves open.

## Tasks

- [x] Counters and timing in the operators, behind a flag on the builder
- [x] `Plan::profile` and `\explain analyze`
- [x] Tests asserting estimate against actual for the examples' queries,
      loosely, so that a statistics regression is caught
- [x] Use it to revisit the cost constants in `plan.rs` on the benches'
      graphs; record what changed in this issue

## Outcome

`\explain analyze STMT` runs a `select`, `verify` or `lookup` and prints the
plan with what each step did:

```text
scan (?w age ?a) via predicate index, rows ~3 actual 3, entries ~3 actual 3, 1 lookups, 0.025 ms
filter test(?a) actual 2, 0.008 ms
bind join (/a knows ?w) via spo index, rows ~1 actual 1, entries ~1 actual 1, 2 lookups, 0.009 ms
total: 1 rows in 0.072 ms
```

`Plan::profile(&g)` returns the same as data, and `Plan::explain_profile`
prints it. A step in a group run per row adds up over its runs and says
`over N runs`, and one that never ran says so. Rules print per stratum and
round, and a reduction its entries, estimated cost and time.

Each step's operator and lookup are wrapped only when profiling, so a plain
run pays one test per step when building the plan and one addition per id
read, in `Matcher`. A step's time leaves out the steps it reads from, however
they nest. Steps are now shared behind an `Arc` by a plan's clones, so that a
group cloned at run time is still the same step in the profile.

### What was not done

- **Standing queries per term.** They have no GraQL syntax and no plan to
  print; a profile of one would need its own output.
- **Whether a prepared query would replan differently.** The issue had it as
  "a step further".

### The cost constants, measured and left alone

On `benches/graql.rs`'s graph (40 000 people, in memory, release build, the
best of seven runs), joining the people of one city with `type person`:

| | Cost | Time | Time per cost unit |
|---|---:|---:|---:|
| Bind join | 24 147 | 2.06 ms | 85 ns |
| Hash join | 92 008 | 10.1 ms | 110 ns |

The planner has them in the right order, at 3.8 to 1 against the 4.9 to 1
measured, and doesn't reduce this query, which is right too: the reduction
alone took 3.6 ms. Taking a unit as an entry scanned (159 ns), some single
constants are further out:

| Constant | Now | Measured |
|---|---:|---:|
| `LOOKUP_COST` | 4 | about 1 |
| `PROBE_COST` | 1 | about 0.3 |
| `REDUCED_LOOKUP_COST` + `REDUCED_ENTRY_COST` | 0.7 | about 1.5 |
| `REDUCTION_FACTOR` | 2 | about 2.8 |

None was changed. This is one shape of graph, warm and in memory, where a
lookup costs least, and no plan here came out wrong: a change wants more
shapes, and an on-disk graph, to show a plan that improves. These are the
numbers to start from.

The tests check every top-level step of the examples' queries to within 20
times (the worst today is 14, an author index on a graph of a few links),
and the benches' shape to within 1.5.
