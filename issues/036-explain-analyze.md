# `\explain analyze`: actual rows and time beside the estimates

**Status:** open · **Size:** small · **Related:** [012](closed/012-reduce-only-when-it-pays.md) (estimates the planner makes), [013](closed/013-prepared-queries.md), [026](026-index-lookup-allocations.md)

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

- [ ] Counters and timing in the operators, behind a flag on the builder
- [ ] `Plan::profile` and `\explain analyze`
- [ ] Tests asserting estimate against actual for the examples' queries,
      loosely, so that a statistics regression is caught
- [ ] Use it to revisit the cost constants in `plan.rs` on the benches'
      graphs; record what changed in this issue
