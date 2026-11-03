# Large index entries as B+ trees, adaptively

**Status:** closed (phases 1 to 4 done) · **Size:** large · **Related:** [004](004-history-and-link-variables.md) (`as of`), [003](../003-queries-as-graph-data.md).

## Problem

Each built-in index (`src/tuple_index.rs`) is a `dynamic_table`: a hash table
whose key holds its ids as varint records in a bucket, chained to overflow
buckets. The table itself grows well (levels, incremental expansion, lock-free
readers). A large *entry* is the weak spot: one key with very many ids, such
as the predicate index entry for `type` or `graqlite:key`, or the
predicate+object entry for `type person`.

- **A lookup reads the whole entry.** `LocalTable::find` walks the chain into
  a `Vec` and then a `BTreeSet`, so nothing streams. A `limit 10` query over
  ten million matches builds ten million ids first.
- **A count reads, sorts and deduplicates every id.** The planner counts
  entries for patterns with constants, so a constant with a large entry makes
  planning itself slow.
- **`as of` reads the whole entry** and then drops what is too new.
- **Intersections build both sets.**
- **Duplicates are removed on every read,** not on insert.
- **Entries only grow.** Deleting a link appends a dead link, which is indexed
  too. See compaction below.
- **Inserting into a large entry is slow, and loading is quadratic.**
  - The cause: `Writer::append` (`src/dynamic_table/mod.rs`) walks and parses
    the key's whole bucket chain on every insert, to skip an operation that
    repeats the key's last one. So each insert costs time in proportion to
    the entry, and loading a graph with large entries is quadratic (see
    the baseline below).
  - It hurts neighbours too: a chain is shared by every key that hashes to
    the bucket, so one large entry slows inserts and lookups of its
    neighbours until the next expansion.

## The key fact: each key's ids only increase

Index entries are written in append order: `index_records` runs on each record
as it is appended, and `index_log` rebuilds in log order. Ids are never
reused, even after a rollback or a crash (the log fills lost ids with nodes).
So a key's ids are non-decreasing. The one exception is a rebuild after a
crash adding ids that are already there, and those are at most the last id
stored. So the B+ tree of a large entry grows only at its right edge: no
splits in the middle, no rebalancing, and no rewriting of old pages.

## Design

### Small entries stay as they are

Most keys hold one or a few ids: every SPO is unique, and most
subject+predicate pairs are small. They stay inline in the hash buckets,
where one bucket read answers a lookup.

### Large entries are promoted

When an entry reaches **64 ids** (about two buckets of records), it is
**promoted**:

1. Its ids, sorted and deduplicated, are written into a B+ tree in the
   index's **posting file**, `{prefix}index{n}.postings`. There is **one
   posting file per index**, so the indexes stay independent, as their files
   and locks already are. The file is 4 KiB pages:
   - **Leaf pages:** ids as varint deltas from the page's first id, about 500
     to 1000 ids each.
   - **Internal pages:** each child's first id and page number.
   - **A header page per entry:** the root's page and the tree's height, the
     count of ids, and the last id. These are updated in place.
2. The key's bucket entry is **replaced** by one record whose extra bit is
   set (the index tables always write it clear today), and whose single value
   is the header page's number. It changes only on promotion, never as the
   tree grows.
3. The bucket chain is **rewritten** without the promoted key's old records,
   as an expansion step's merge does. This shortens the chain for the
   bucket's other keys right away, rather than at the next expansion.
4. **Finding the promoted entry** to append to costs a walk of the chain,
   which is short once rewritten. A small in-memory map from key to header
   page, filled as entries are found, skips even that for hot keys.

### Appending to a promoted entry

Appending costs O(1), not a walk of the chain, which fixes the quadratic
load.

1. An id at most the header's last id is a duplicate, and is dropped:
   deduplication on insert for free.
2. Append to the last leaf. When it fills, start a new leaf and add it to its
   parent, adding parents up to a new root as needed.
3. Publish by storing the new count and last id in the header (a release
   store), after the pages are written.

Readers take no locks, as the dynamic table's readers don't. A reader loads
the count (an acquire load) and reads only that many ids, the same way readers
of the record log stop at its end. Pages past the count are never read until
they are published. Pages are never freed while the posting file is open. The
file is mapped with address space reserved to grow, like the table's levels,
so mapped pages never move.

There is no demotion, since entries only grow. Compaction rebuilds an entry in
whichever form fits.

## What it gives

- **`count(key)` in O(1)** for promoted entries, from the header. This fixes
  the planner's cost of estimating patterns with large constants.
- **`iter(key)` and `iter_rev(key)`:** ids in increasing or decreasing
  order, reading leaves as needed, so a query that stops early stops reading.
  Going backward descends the tree and keeps the path to the root, so leaves
  need no backward links.
- **`seek(key, id)` and `seek_rev(key, id)`:** the first id at least `id`,
  or the last at most `id`, by descending the tree.
  - **`range(key, ..=as_of)`, either way:** `as of` as a cutoff, without
    reading what is too new. Read newest first, it walks back from the
    cutoff, which is the order "the latest link of an SPO wins" wants.
  - **Merge and galloping intersection** of two entries, without building
    either as a set.

## Changes around it

1. **`local_table`:**
   - add `iter`, `iter_rev`, `seek`, `seek_rev`, `range` (in either
     direction) and an O(1) `count`, that work for both forms;
   - keep `find(key) -> IdSet`, implemented on them, so that callers move
     over one at a time;
   - promote on `insert`.
2. **`tuple_index`:** the same streaming lookups for its finds: by linkage,
   by SPO, and by intersection.
3. **The engine:**
   - the pattern matcher's candidate lookups stream;
   - a hash join's scan reads promoted entries by range;
   - the reducer reads ranges within its budget;
   - `as of` passes a cutoff to the lookups;
   - **operators can seek:** `Operator` (`src/query/exec.rs`) gains a
     `seek(id)`, so that a consumer can tell its producer to skip ahead, as
     the dataflow's `SEEK_REQUEST` did. This makes galloping
     intersections work across operators, not only within one lookup;
   - **two large entries** intersect by a merge of their streams, which
     gallop with seeks
   - **a large entry with many small sets:** a bind join whose rows each
     bring a small set checked against one large entry (`?x knows ?y . ?y
     type person`) probes the large entry with a seek per element. That costs
     the small set's size times the log of the large one, not a read of the
     large entry
4. **Durability:**
   - Posting files are flushed with the tables, on commit.
   - After a crash, an entry may name ids past the committed end, as it can
     today. Readers cap at the committed end, and the graph checks what ids
     name.
   - A header records its tree's last published state, and a tree's pages
     past it are reused on reopen.
5. **Snapshots:** keep the snapshot format. Write promoted entries inline,
   and promote them again on restore.
6. **Compaction (later):** rebuild an entry without the links that later
   links supersede (and dead links older than any `as of` still needed), as
   a new tree, then point the bucket at it. It runs as maintenance, from
   `tick`, with a policy for how much history to keep.

## Phases

1. **A benchmark:** a graph with a few million links on a few predicates,
   timing counts, full lookups, an early-stopping query, an intersection of
   two large entries, planning with a large constant, and `as of`. This is
   the baseline to beat (`benches/index.rs`).
2. **Storage:** posting files, promotion, appending, the header, and the new
   `local_table` lookups behind the existing `find`. Tests compare the
   answers with the inline form, across promotion, a reopen, a crash in the
   middle of appending, and a snapshot and restore.
3. **The engine streams:** candidates, scans, `seek` on operators,
   merging and galloping intersections, the large-with-small probe, and the
   `as of` cutoff.
4. **Compaction.**

## The set engine

- **The runtime:** an event loop (`BaseFlow`) runs operators (`Stream`s)
  joined by `Channel`s of 128 KiB blocks of ids, driven by `READABLE` and
  `WRITABLE` events.
- **Requests upstream:** consumers can send `SEEK_REQUEST` and
  `RANGE_REQUEST` to the operators feeding them.
- **Sets are immediate or stored.** An immediate set is small and in
  memory. A stored set is read from storage asynchronously, as
  `Read(start, end, callback)`, a few buffers ahead, pausing and resuming
  as the consumer keeps up.
- **The operators:** streams of sets and ranges, index lookups, flattening,
  sorted-merge intersection (`Intersect2Stream`), one large set with many
  small ones (`UXStream`), Cartesian products feeding pair-index lookups,
  filters, and materialization.
- **Unfinished parts:** N-way intersection, `Probe`, and the sort and
  unique streams. A TODO on `Set<ID>` asks for a "combined representation"
  of the two kinds of set.

What this issue takes from it:

- **Immediate and stored sets are inline and promoted entries.** This
  issue's adaptive scheme is the combined representation that TODO asks
  for.
- **Ranges in both directions.** We read stored sets newest first
  (`Read(kuint64max, 0)`), and its default order were descending. So the
  trees support reverse iteration and reverse ranges, which `as of` and
  "the latest link wins" want.
- **Seeking between operators** (`SEEK_REQUEST`), as `Operator::seek`.
- **Its operators, in phase 3:** merge intersection, and the large-with-small
  probe.

What it does not take:

- **The event-loop runtime and its asynchrony:** posting across threads,
  mutexes, closures that restart reads, and handing buffers between
  threads. That machinery exists so potentially storage could be remote
  and read asynchronously. graqlite reads memory-mapped files synchronously,
  and its engine's operators already pull batches of rows, which gives the
  channels' bounded buffering without an event loop.
- **Its resource manager and reference-counted buffers:** memory mapping
  covers them.
- **Its unfinished operators,** which are intentions, not a design.

## Baseline

From `cargo bench --bench index` (`benches/index.rs`), on 2026-10-31. The
graph is people with `type person`, a `city` (of ten) and an `age` (of 90),
so the entries for `type`, `city`, `age` and `type person` hold one id per
person. The graph is in memory.

**Loading** (three links a person, committed every 100 000 people):

| People | Links | Load time |
|-------:|------:|----------:|
| 5 000 | 15 000 | 0.8 s |
| 10 000 | 30 000 | 3.0 s |
| 20 000 | 60 000 | 12.1 s |
| 40 000 | 120 000 | 47.4 s |

That is four times the time for twice the links, about 400 µs a link at
40 000 people. A million people would take hours, so the benchmark defaults
to 40 000.

**Lookups and queries** (the median time, at 20 000 and 40 000 people):

| Benchmark | 20 000 | 40 000 | Should become |
|-----------|-------:|-------:|---------------|
| count predicate `type` | 160 µs | 320 µs | O(1), a header read |
| count `type person` | 179 µs | 373 µs | O(1) |
| find predicate `type` (the whole entry) | 2.1 ms | 4.4 ms | about the same, read in order |
| `limit 10` of `type person` | 9.6 ms | 20.2 ms | microseconds, stopping after 10 |
| `type person` and `city paris` | 2.9 ms | 5.8 ms | a merge of two streams |
| plan `type person` and `age` (no reduction) | 204 µs | 396 µs | microseconds, from O(1) counts |
| `as of` a tenth, `type person` | 2.2 ms | 4.7 ms | a tenth of a full read, from the cutoff |

Every lookup grows in proportion to the entry. Two results stand out:

- **`limit 10` is the slowest of all,** five times a full lookup. Nothing
  stops early: the planner counts the entry, the reducer reads up to its
  budget, and execution reads the whole entry before its first row.
- **`as of` an early id costs as much as a full read,** since nothing cuts
  the read off at the id.

## Phase 2, as implemented

The storage is in `src/postings.rs`, and `LocalTable` uses it.

- **Trees:** `Postings` keeps one table's trees in segment files,
  `{prefix}postings.{n}`. Each segment is twice the size of the one before,
  mapped with its whole size reserved, and grown as pages are used.
  - Pages are 4 KiB: a file header; a header page per tree; leaves of
    varint deltas, with their rank; and internal pages of up to 170
    children, each with its first id and rank.
  - Every word is read and written atomically. Readers load a tree's count
    first, and read only that many ids.
  - Ranks let a reader stop at the count, even in the rightmost leaf, and
    in reverse.
- **Lookups:** `iter`, `iter_rev`, `seek`, `seek_rev`, `range`, `range_rev`
  and `count` on `LocalTable` and `TableReader`, the same for inline and
  promoted keys. `find` is built on `iter`, so callers are unchanged. A
  promoted key's count is its header's.
- **Promotion and appending:**
  - `Writer::insert_unless_flagged` inserts into a bucket, or stops at a key
    whose extra bit is set (a promoted key), and reports the key's size,
    in the one walk of the chain the insert makes anyway.
  - `LocalTable` promotes a key once it holds 64 ids, and remembers
    promoted keys in a map, so appending to a promoted key doesn't walk its
    chain.
  - Appending costs O(1), skipping ids not past the tree's last id.
- **The bucket chain is not rewritten** on promotion. Since a key is
  promoted at 64 ids, its old records add at most two buckets to the chain,
  and readers can't be given a chain changed in place. The next expansion
  merges them away.
- **Durability:**
  - Each tree's header has live words (count, last id, root, height) and a
    durable copy. `Postings::sync` flushes the pages, copies the changed
    trees' live words to durable, and flushes again.
  - Opening resets every tree to its durable words, so a crash leaves each
    tree as of its last sync. Pages past the durable count are ignored, then
    reused.
  - **Promoting syncs the postings before replacing the bucket's records**
    with one naming the tree. A bucket can reach the disk at any time, so
    this keeps it from naming a tree a crash lost. Without it, a crash could
    lose a promoted key's earlier ids, including committed ones. This costs
    one flush per promotion, which happens once per key, at 64 ids.
  - `LocalTable::sync` (on commit) syncs the postings before the dynamic
    table.
- **Snapshots** include the postings, after each table's chunks. The
  snapshot format is version 2, and version 1 snapshots, which have none,
  still restore. This is simpler than writing promoted entries inline, since
  a table's snapshot is its raw files.
- **The cost of a promoted key:** a header page (4 KiB) and at least one leaf,
  for at least 64 ids.
- **Tests:**
  - The trees, against a sorted `Vec`: every operation, three heights,
    appends in uneven batches, duplicates, a reopen, a crash, a snapshot,
    and a reader on another thread while appending.
  - `LocalTable`: every lookup across promotion, 300 keys sharing buckets,
    and set and delete on promoted keys.
  - Persistence: promotion then a crash, appends lost in a crash, and a
    snapshot.
  - A graph of 3 000 people with promoted entries, with deletions, a
    reopen, and a snapshot and restore.

### After phase 2

The same benchmark, on 2026-10-31.

| | Baseline, 40 000 | Phase 2, 40 000 | Phase 2, 400 000 |
|---|---:|---:|---:|
| load | 47.4 s | 0.33 s | 3.7 s |
| count predicate `type` | 320 µs | 1.6 µs | 1.6 µs |
| count `type person` | 373 µs | 1.6 µs | 1.6 µs |
| plan `type person` and `age` | 396 µs | 18 µs | 18 µs |
| find predicate `type` | 4.4 ms | 4.3 ms | |
| `limit 10` of `type person` | 20.2 ms | 19.8 ms | |
| `type person` and `city paris` | 5.8 ms | 6.0 ms | |
| `as of` a tenth | 4.7 ms | 4.5 ms | |

- **Loading is linear** (0.34 s at 40 000 people, 0.74 s at 80 000, 1.5 s at
  160 000), and 140 times faster at 40 000.
- **Counts and planning take constant time.**
- **The rest is unchanged,** since the engine still reads whole entries:
  that is phase 3. The benchmark now defaults to 200 000 people.

## Phase 3, as implemented

The engine streams large entries, newest first.

- **Streaming lookups:** `tuple_index::links_newest_first` streams any index
  entry (`LinkKey`) down from an `as of` cutoff. `links_up_to` reads one up
  to the cutoff, and `count_links_at` counts one. Finding the latest link of
  an SPO, or of a pair, reads its newest id, not its whole entry.
- **Liveness from the stream:** a link is live if it is the latest link of
  its subject, predicate and object, as of the query, and not dead. Read
  newest first, the first link seen of each SPO is its latest, so the
  matcher decides liveness as it reads, with a set of the SPOs seen. That
  needs an entry with every link of each of its SPOs, which every entry but
  those by author or subgraph is (`LinkKey::has_whole_spos`).
- **Resumable lookups:** `Lookup` gains `begin` and `resume`. A pattern's
  lookup of a promoted entry (the matcher, when not reduced) streams it, and
  `BindJoin` resumes it as its output drains.
  - **Small, inline entries** keep the old path and order, so results on
    small graphs (and the CLI's outputs) are unchanged.
  - **Large entries' rows come newest first.** Rows are unordered without
    `by`.
- **Reading no more than wanted:** a `Batch` has a row capacity. `Limit`
  reads into one sized to the rows it still wants, and `BindJoin` sizes its
  pending rows to its output's room, so `limit 10` reads about ten links.
- **Planning:**
  - The reducer checks an entry's count against its budget before reading
    it, and reads only up to its pattern's `as of`.
  - Reduction now runs only for two or more patterns (one alone has nothing
    to narrow it by), and not for a query with a limit and no order, which
    can stop at its first rows.
  - So `verify` (a limit of 1) plans a lookup, not a reduction.
- **Tests:**
  - The random query test now also runs on graphs of 1 500 links, whose
    predicate, subject and object entries (and some pairs) are promoted,
    with deletions and re-links. It checks every plan against brute force,
    and asserts that entries were promoted.
  - Breaking liveness in the stream, on purpose, makes it fail.

### What the plan above got wrong

- **Merge and galloping intersection don't fit.** An entry's ids are link
  ids, sorted by when each link was made, not by subject or object. So two
  entries joined on a variable (`?p type person . ?p city paris`, joined on
  `?p`) can't be merged. A bind join probing the SPO or pair index for each
  row costs one hash lookup per row, which beats a seek, and the engine
  already plans that.
- **So `Operator::seek` and the large-with-small probe were not built.**
  They would only help where the joined variable is the link itself, which
  is rare.

### After phase 3

The same benchmark, on 2026-10-31.

| | Baseline, 40 000 | Phase 2, 40 000 | Phase 3, 40 000 | Phase 3, 400 000 |
|---|---:|---:|---:|---:|
| `limit 10` of `type person` | 20.2 ms | 19.8 ms | 17.7 µs | 16 µs |
| `as of` a tenth, `type person` | 4.7 ms | 4.5 ms | 0.83 ms | 8.2 ms |
| planning that `as of` | | 1.6 ms | 7.4 µs | 7.4 µs |
| `type person` and `city paris` | 5.8 ms | 6.0 ms | 6.8 ms | 58 ms |
| count `type person` | 373 µs | 1.6 µs | 1.6 µs | 1.6 µs |

- **`limit 10` takes constant time,** about 1 100 times faster at 40 000 people.
- **`as of` reads only what is before the cutoff,** so its cost grows with the
  links it matches (a tenth), not with the entry.
- **The intersection is not faster.** Its cost is about 1.2 µs a row: a
  point lookup of each person's other link, in the hash table, whose buckets
  are read and parsed per lookup. Faster point lookups in the dynamic table
  (caching parsed buckets, for one) are a separate change.

## Phase 4, as implemented

Compaction drops history from the large entries, up to a *horizon* id:

- **The rule:** of the links at or before the horizon, an entry keeps only
  the latest of each subject, predicate and object as of the horizon, and
  only if it is live. Every link after the horizon stays. The rule depends
  only on the link, not the entry (`tuple_index::compact`, deciding each id
  once, from the SPO index, as of the horizon), so every entry drops the
  same links.
- **Why answers as of the horizon or later do not change:** liveness is read
  from within an entry, as the latest link of each SPO it holds. For an SPO,
  an entry still has every link after the horizon, and the latest at or
  before it if that one is live. If it was dead, the entry has none of the
  SPO's links at or before the horizon, which reads the same: not live.
- **Earlier `as of` fails rather than answering wrongly.** Planning a query
  any of whose `as of` ids is before the horizon gives
  `Error::Compacted { as_of, horizon }`, as does refreshing a standing query
  last refreshed before it. The horizon is word 3 of each posting file's
  header page, so it persists and travels in snapshots. It is written, and
  flushed, before anything is compacted, so a crash part way through leaves
  it recorded; `Graph::horizon` is the latest of the tables'.
- **Rebuilding an entry:** `LocalTable::compact` reads each promoted key's
  tree and builds a new one of the ids kept. The new trees are synced
  before the buckets name them, as in promotion, and a reader holding an
  old tree keeps reading it, since its pages are not reused. A key left
  with ids that fit one bucket record (8) goes back to its bucket in one
  replace, and a key left with none is deleted. Each tree's header page now
  records its key (words 9 and 10), so compaction can list the trees
  without a way to iterate the hash table; trees whose bucket no longer
  names them (replaced by compaction, or by `set`) are skipped.
- **The policy:** `Options::history`, in milliseconds, is how much history to
  keep, `None` (the default) keeping all of it. `Graph::tick` then compacts
  to the last id committed that long ago, once the ids since the last
  horizon are at least an eighth of the log, so that compacting costs a
  constant amount per entry appended. `Graph::compact(horizon)` compacts by
  hand, as does `\compact ID|TIME` in the shell; `\info` shows the horizon.
- **Only committed horizons:** a link a horizon keeps or drops by could
  otherwise be rolled back.

### What the plan above got wrong

- **"Point the bucket at it"** was right, but the plan had no way to find the
  promoted keys: the dynamic table cannot be iterated. Recording the key in
  the tree's header page fixed that.
- **"Dead links older than any `as of` still needed"** became one horizon,
  checked when planning, rather than tracking which `as of` ids are in use.
- **A bug from phase 2:** a key deleted after promotion kept the extra bit of
  its last record, so inserting into it again found it "promoted" with no
  tree. `insert_unless_flagged` now treats a key with no values as not
  flagged. Compaction deletes keys, which found the bug.

### After phase 4

A graph of people, each with a `type` and a `city`, where everyone then
moves four times (each move deletes the old `city` link and links the new
one), compacted to its last commit, on 2026-10-31:

| | 40 000 people | 400 000 people |
|---|---:|---:|
| ids dropped (21 entries) | 960 000 | 9 600 000 |
| the `city` entry | 360 000 → 40 000 ids | 3 600 000 → 400 000 ids |
| compacting | 0.23 s | 4.2 s |
| everyone in one city | 4.9 → 0.84 ms | 66 → 8.7 ms |
| `limit 10` of them | 17 → 15 µs | 14 → 15 µs |

Queries that read a whole entry read only its live links again; those that
stop early were already unaffected by history.

## What is left

Follow-ups, not part of this issue:

- **Reclaiming the pages of old trees.** Compaction leaves them in place for
  their readers, and nothing yet frees them: the posting files only grow. A
  free list, filled once no reader can hold an old tree (by an epoch, as the
  dynamic table retires levels), would let new trees reuse them.
- **Compacting small entries.** Entries in their buckets keep all their
  history. They are bounded at 64 ids, so it costs little.
- **Faster point lookups in the dynamic table,** which bound joins of large
  entries, such as the intersection above.
