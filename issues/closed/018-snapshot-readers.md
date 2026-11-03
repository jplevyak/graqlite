# Snapshot readers beside a writer

**Status:** closed · **Size:** large · **Related:** [002](../002-multi-process-access.md) (option 1 is this, across processes), [019](../019-parallel-queries.md) (parallel queries)

## Problem

A `Graph` can be shared between threads (it is `Send` and `Sync`), but only
as an `RwLock<Graph>`: queries run in parallel with each other, and a write
waits until every query has finished, and blocks new ones.  A long query
stalls every writer; a busy writer stalls every reader.  Readers should
instead run beside the writer, each on a snapshot of the graph as of its
last commit, taking no lock.

## What already helps

The storage was built for readers that don't lock:

- **The log** is append-only: an entry never moves, and a reader that stops
  at the committed `next_id` sees only complete, committed entries.  Ids
  are never reused.
- **The indexes** (`dynamic_table`, `postings`, `local_table`) already have
  lock-free `Reader`s: the writer only appends whole words, readers skip
  incomplete records, and index levels are swapped through `AtomicPtr`s
  (`readers_during_growth` tests this).  Retired levels are kept until no
  reader can be using them.
- **Queries** read the graph only through `&Graph`, and the graph copes
  with index entries naming ids that aren't (or aren't yet) entries:
  `Graph::check_entry`, and `as of` cut-offs by id.

## Design

- **`Graph::reader() -> GraphReader`**: a handle, `Send + Sync + Clone`,
  that sees the graph as of its last commit.  It holds its own index
  readers, and an `Arc` of what the writer publishes at each commit:
  - the committed `next_id`, which bounds every lookup (ids at or past it
    are dropped, as `Graph::contains` does);
  - the statistics, as of the commit (`Stats` is cloned, or kept in
    persistent structures so that publishing is cheap), for planning;
  - the commit times (`Times`), for `as of` a time;
  - the horizon, so that compaction doesn't remove what a reader may read.
  `GraphReader::refresh()` moves it to the latest commit.
- **Queries on either:** the query executor reads through a trait (`Read`:
  `get`, `data`, `key`, the index lookups, `stats`, `next_id`, `horizon`)
  that `Graph` and `GraphReader` both implement.  `Graph`'s read methods
  move onto it; `Plan::execute` and friends take `&impl Read` (or `&dyn`).
  `graql::Session` gains a read-only mode over a reader, for `select`,
  `verify` and `lookup`.
- **Ephemeral values** (`Graph::ephemeral_value`) are shared by the writer
  and its readers (one `Arc<Mutex<…>>`), so ids agree across them.
- **Retiring:** the writer must not free an index level, or truncate the
  log after a rollback, while a reader's snapshot can see it.  Each reader
  registers its snapshot's id (an epoch); the writer retires only what is
  older than every registered snapshot, as levels are retired now by time.
- **Rollback:** a rollback discards uncommitted entries, which no snapshot
  sees; but the log may then reuse their space.  Readers bounded by the
  committed `next_id` never read it, so this is safe as long as the bound is
  read before the lookup.

## As implemented

The design changed in one big way: rather than a `Read` trait that the
executor reads through, a reader **is a `Graph`**, read-only, so every query,
rule, overlay and GraQL statement runs on it unchanged.

- **`Graph::reader()`** returns a `Graph` as of the last commit.  Its
  `Local` shares the log's mappings (each data file's `Mapping` is now an
  `Arc`), bounded at the committed `next_id`, and reads the tables through
  their `TableReader`s: `Local::table(i)` returns a `TableRef`, a table or a
  reader, with the same lookups.  Ids the indexes give past the reader's end
  are unreadable in its log, so lookups skip them, and liveness is the
  latest link it can read.  Its statistics are the commit's (`Arc<Stats>`:
  the writer already cloned them at each commit), its commit times the
  writer's list bounded at the commit's length (`Times` shares them behind
  an `RwLock`), and its ephemeral values the writer's.  Its writes fail
  (the log, `Local` and `Graph` check), and dropping it saves nothing.
- **`Graph::readers()`** returns `Readers`, `Send + Sync + Clone`, whose
  `latest()` is an `Arc<Graph>` of the latest commit.  From the first call
  on, each commit (and rollback, whose abandoned ids are committed as
  nodes) publishes a new one.
- **Epochs, simply:** index levels needed none (a lookup re-reads the
  current level, and a retired one lasts five minutes), postings pages are
  never reclaimed, and the log is never rewritten below the committed end.
  Compaction was the exception: compacting past a reader's snapshot drops
  links it still sees as live (replaced after its snapshot), which the
  stress test caught.  So every reader registers its snapshot while it
  lives, and `Graph::compact` never compacts past the oldest; and a reader
  older than the horizon (none, then) fails its queries with `Compacted`,
  as an `as of` before the horizon does.
- **GraQL:** `Session::read(&Graph, statement)` runs a `select`, `verify`,
  `lookup` (from a graph, perhaps) or `test` on a reader; `Session::run`
  now prepares a statement once, and runs those through the same path.
- **Open questions, answered:** cloning the statistics per commit was
  already the cost; overlays on a reader use the writer's ephemeral ids,
  shared.

Tests: `readers` (what a reader sees, its writes failing, its statistics),
`compaction_waits_for_readers`, `readers_beside_a_writer` (three reader
threads, each check comparing every link through the indexes with a scan of
the reader's log, and a random query with brute force on the reader, while
the writer appends, deletes, commits, rolls back, grows its tables and
compacts; run 300 times in release mode without a failure, and failing
without the compaction bound), and `sessions_on_readers` (GraQL sessions on
reader threads beside a writer session).  The GraQL benchmarks are as
before.

## Closed

Done.  Option 1 of [002](../002-multi-process-access.md), a read-only open in
another process, is the same across processes: its reader has to refresh
the log's files and the tables' levels from their files, which in one
process are shared memory.
