# Limits on a query's time and work, and cancelling one

**Status:** open · **Size:** small · **Related:** [012](closed/012-reduce-only-when-it-pays.md) (the reduction budget), [036](closed/036-explain-analyze.md) (counting each step's work), [002](002-multi-process-access.md)

## Problem

A query runs until it finishes. Nothing stops one after a time, after a
number of index entries, or when its caller no longer wants it. `limit`
bounds the rows returned, not the work: a closure over a large graph, or a
join the planner estimated badly, runs to the end before `limit` sees a
row it can stop at. The only budget in the engine is the reduction's
(`reduction_budget`), which bounds an optimization, not the query.

This matters when the queries are not written by the application. A
service that lets an agent or a user send GraQL (Scyth's `query_facts`
tool gives agents read-only queries capped at 200 rows, 2 seconds and
16 KB of result) needs every query to be stoppable, or one bad query holds
a thread and, with a reader held, holds back compaction.

## Design

- **A limit on a query:** `Query::deadline(Instant)` and
  `Query::work_limit(entries)`, checked by the executor between batches
  and inside the loops that can run long without producing a row (path
  search, semi-naive rounds, hash join builds). On reaching one the query
  returns `Error::Limit { rows_so_far, .. }`. `Plan::profile` already
  counts index entries per step, so the counter exists.
- **Cancelling:** `Query::cancel_token()` gives an `Arc<AtomicBool>`
  checked at the same points, so another thread can stop it.
- **A result size limit** in bytes for GraQL output is the caller's to
  apply while streaming rows; no change needed beyond the streaming
  executor's existing batches.
- **GraQL and the shell:** `set timeout 2s`, `set work_limit N`, and
  Ctrl-C in the shell cancels the running statement and not the shell.
- **A read-only session:** `Session::read` already refuses writes; state
  in its docs that with a deadline it is the form to give untrusted text.

## Tasks

- [ ] `deadline`, `work_limit` and a cancel token on `Query` and
      `Prepared`, checked in `exec.rs`, `paths.rs` and `rules.rs`
- [ ] `Error::Limit`, and a test that a closure over a long chain stops
      within a few milliseconds of its deadline
- [ ] `set timeout`, `set work_limit`; Ctrl-C in the shell
- [ ] A benchmark showing the checks cost nothing measurable on the
      `graql` and `join` benches
