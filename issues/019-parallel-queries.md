# Parallelism within a query

**Status:** open · **Size:** large · **Needs:** queries slow enough to be worth it · **Related:** [018](closed/018-snapshot-readers.md) (snapshot readers)

## Problem

A `Graph` can be shared between threads, and queries read it through
`&Graph`, but each query runs on one thread.  Large analytic queries (rules
over the whole graph, big joins, path searches from many starts,
aggregates over many rows) could use several.

## Where parallelism fits

The executor streams batches of rows through operators (`exec.rs`), one
operator calling the next.  The places that do independent work:

- **Rules:** within a semi-naive round, each rule's body (and each of its
  calls' delta variants) is a separate query over the same relations: run
  them in parallel, merging the new tuples after.  Strata whose relations
  don't depend on each other can run in parallel too.
- **Union branches** (`UnionBind`, and the hash join's build side over
  branches): one per thread.
- **Hash joins:** the build side is a whole sub-plan, run once; build it
  while the probe side starts, or in partitions.
- **Path searches** from many start nodes (`PathLookup` with neither end
  bound, `Automaton::reach` per start): partition the starts.
- **Scans of large index entries:** a bind join's input batch can be
  partitioned by row, each part looked up on its own thread, then the
  output concatenated in order.
- **Aggregates, link aggregates and nested limits:** group in parallel and
  merge.
- **`graph()`** (result graphs): witnesses, support and assertions per row
  are independent.

## Design

- A thread pool (`rayon`, or `std::thread::scope` per query to avoid a
  dependency), used only when the planner estimates enough work
  (`Plan::estimated_cost` above a threshold), so that small queries pay
  nothing.
- Operators stay single-threaded; parallelism is added at the boundaries
  above (rules, unions, hash builds, partitioned bind joins), each taking
  `&Graph` (or 018's reader) and producing rows merged in a deterministic
  order, so results don't change.
- `Query::parallelism(n)` and an `Options` default; `1` is today's
  behaviour.

## Phases

1. **Rules:** parallel rule bodies within a round.  Benchmark: a recursive
   rule over 100 000 links.
2. **Unions and hash join builds.**
3. **Partitioned bind joins** for large inputs, and path searches.
4. **Aggregates.**

Each phase: the random query tests run with parallelism forced on, and
results must equal the single-threaded ones exactly (rows in the same
order, where ordered).

## Open questions

- Is there a workload to measure against?  Without one, this is
  speculative: decide with benchmarks of real queries first.
- `rayon` (a dependency, a global pool) or scoped threads (no dependency,
  a spawn cost per query)?
