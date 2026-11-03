# Access from more than one process

**Status:** open · **Size:** large · **Needs:** a decision on which use case comes first (below) · **Related:** [018](closed/018-snapshot-readers.md) (snapshot readers in one process), [019](019-parallel-queries.md) (parallel queries)

## Problem

Only one process at a time can open a database. `LocalRecordLog::open` takes
`flock(LOCK_EX | LOCK_NB)` on `{prefix}record_log.metadata`, and each dynamic
table takes one on its meta file (`mmap::lock_exclusive`). So the `graqlite`
shell can't look at a database that an application has open, and two writers
can't share one. `psql` works because PostgreSQL is a server, and graqlite has
no server.

There are two separate ways to fix this. Each solves a different problem, and
neither needs the other.

Within one process, threads already share a graph: as an `RwLock<Graph>`,
or through [018](closed/018-snapshot-readers.md)'s readers
(`Graph::readers`), read-only graphs of the last commit that run beside
the writer, taking no locks.  Option 1 here is the same across processes,
and starts from them: a reader `Local` (the log's committed entries, the
tables through their readers, writes failing), statistics and commit
times as of a commit, and compaction held back by readers' snapshots.
What differs is that another process has no shared memory: its reader
maps the files itself, and refreshes the log's files and the tables'
levels from their metadata (items 2 and 3 below), and the writer learns of
its snapshots through a file rather than a shared map.

## Option 1: a read-only open beside one writer

Other processes open the database read-only, with no lock, while one writer
keeps its exclusive lock. This is for "inspect or query the database my
application has open", and for running reporting queries next to a writer.

### What already helps

- The log is append-only. Each data file is mapped with address space reserved
  for it to grow, and an entry never moves. A reader that stops at
  `Metadata::next_id` (the committed end) only sees complete, committed
  entries.
- Ids are never reused (`reserved_id`, plus the filler nodes written by
  `abandon_commit`), so an id that a reader saw never comes to name a
  different entry.
- Dynamic table `Reader`s already take no locks: the writer only appends 8
  byte words, and readers ignore incomplete records. This works across
  processes as well as threads, because a shared `MAP_SHARED` mapping gives
  the same ordering.
- On Linux, a mapping of a file that has since been unlinked stays valid, so
  a reader that maps an old index level is not hurt when the writer retires
  and deletes it.
- The graph already copes with indexes that name ids which turned out not to
  be entries (after a crash or a rollback), and `Graph::check_entry` builds
  on that.

### What has to be built

1. **`Options::read_only`** (or `Graph::open_read_only`): skip the locks,
   open the files `O_RDONLY` and map them `PROT_READ`. Every write path then
   returns an error, and `Drop` must not save the statistics.
2. **Refreshing the log:** the reader reads `next_id` from the metadata file
   at the start of each query, or on an explicit `refresh()`, and treats it as
   its end. When the writer has started a new data file (`files.len()` grew),
   the reader maps it.
3. **Refreshing the indexes:** an in-process reader finds the current level
   through a shared `AtomicPtr`, which a reader in another process doesn't
   have. The reader has to read the current level and the expansion progress
   from the table's meta file, and map new levels as they appear. During an
   expansion it has to look in both levels, as an in-process reader does.
   This is most of the work and most of the risk.
4. **Uncommitted index entries:** the indexes may name entries that are past
   the committed `next_id`. The reader drops ids `>= next_id` before
   resolving them; `Graph::contains` already uses that bound.
5. **Statistics:** the reader loads `{prefix}stats` and counts the entries
   after `saved_id` up to `next_id`, as `with_stats` already does when a
   graph is opened. Plans may be a little stale, which is acceptable.
6. **Shell:** `graqlite --read-only DIR`, used automatically (with a notice)
   when the lock is held. Statements that write report "read only".

### Risks

- Cross-process memory ordering on the table's 128 byte buckets, and a reader
  that opens in the middle of an expansion step.
- A writer that grows a file past a reader's reserved address space. Today
  the reservation is per process, so the reader has to remap that file, and
  the mapping's address must stay stable while a lookup is using it.
- macOS and other Unixes: check what happens to a mapping when its file is
  unlinked, and how `flock` behaves.

## Option 2: a server

`graqlite serve DIR [--socket PATH | --listen ADDR]` owns the graph, and
clients send it GraQL. The shell gets `--connect` and keeps its current
output. This is for several writers, the `psql` model, and pushing changes to
clients.

### Shape

- **Process:** one writer thread applies statements in order. Reads take a
  read lock on the graph (`RwLock<Graph>`, which works today), and later
  run on [018](closed/018-snapshot-readers.md)'s snapshot readers, beside the
  writer; commits are grouped.
- **Protocol:** framed messages over a Unix socket first (TCP later, which
  needs authentication):
  - The request is `{session, text}`.
  - The response is a list of `Output`s, each row with ids plus names (key or
    data), so a client never makes another round trip to show a row.
  - Errors keep their line and column.
- **Sessions:** each connection has its own `graql::Session` and its own
  transaction (`\begin` ... `\commit`). Two open transactions that both
  append conflict with `Graph`'s single uncommitted tail, so the first
  version should allow one open write transaction at a time and make others
  wait.
- **Standing queries:** `subscribe <select>` registers a `StandingQuery`. The
  server calls `update(&g)` after each commit, and pushes the `Changes` to the
  subscriber. This is the feature a library alone can't offer, and the
  strongest reason to build a server. Subscriptions could be stored as query graphs
  ([003](003-queries-as-graph-data.md)), so they survive restarts and can be
  queried.
- **Shell:** move the rendering (`render.rs`) so that it formats rows that
  already carry their names, and give `Shell` a backend trait with local and
  remote implementations.

### Risks

- Transactions on the server: `Graph::rollback` discards everything since the
  last commit, so per-connection transactions need either the "one writer at
  a time" rule above or a change to `Graph` to keep one uncommitted tail per
  transaction.
- Results are built up in memory today (`Session::run` uses `Query::run`).
  Big results over the wire need a streaming `Session` built on
  `Plan::execute`.

## Decision needed

| If the main use is… | Do first |
| ------------------- | -------- |
| Querying or debugging a database that an application has open | **Option 1** (after 018) |
| Several clients writing, a shared service, live subscriptions | **Option 2** |

They don't block each other, and both can exist eventually: a server could
use read-only opens in its reader threads, but doesn't need them.

## Tasks (Option 1)

- [ ] Read-only `Local` and `LocalRecordLog`: no lock, `PROT_READ`, writes return an error
- [ ] Refresh the log's `next_id` and its files from the metadata
- [ ] A cross-process dynamic table reader: read the level state from the meta file, map new levels, look up during expansion
- [ ] Filter index ids `>= next_id`
- [ ] `Graph::open_read_only` and `refresh`, with no stats save on drop
- [ ] Tests: a writer process and a reader process (fork) with continuous appends, commits, rollbacks and table expansion
- [ ] Shell: `--read-only`, and fall back to it when the lock is held

## Tasks (Option 2)

- [ ] Wire protocol, and a client library module
- [ ] `graqlite serve`: Unix socket, per-connection sessions, one write transaction at a time
- [ ] A streaming `Session` built on `Plan::execute`
- [ ] Shell: a backend trait, and `--connect`
- [ ] `subscribe`, which pushes `StandingQuery` changes
