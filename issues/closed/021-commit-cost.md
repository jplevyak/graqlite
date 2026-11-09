# Sync only what a commit changed

**Status:** closed · **Size:** medium · **Related:** [002](../002-multi-process-access.md) (a server would group commits), [011](011-adaptive-btree-index-entries.md) (postings syncs)

## Problem

`Local::commit` starts the log's commit, syncs all nine index tables, then
completes it. Each table's `sync` flushes every posting segment, copies
the changed trees' live words to their durable ones, flushes every
segment again, syncs the directory, then `msync`s each live hash level
and the table's metadata and syncs the directory again. None of it asks
whether the table changed since the last commit.

`strace` of 50 single-statement inserts on disk counted 2 009 `msync` and
903 `fsync` calls: about 40 and 18 per commit. The shell commits after
every statement, as `psql` does, so loading 2 000 people one statement at
a time:

| | Time |
|---|---:|
| 2 000 statements, one commit each, on disk | 11.3 s |
| The same in memory | 0.11 s |
| The same in one transaction (`-1`), on disk | 2.1 s |

Even in one transaction the on-disk load is twenty times the in-memory
one, so something besides the commit syncs per statement: the
candidates are `LocalTable::promote` (a postings sync each time a key
crosses `PROMOTE_AT`) and the log's `reserved_id` metadata writes. That
wants a profile before the design below is taken as the whole story.

Each `msync` is also over the file's whole allocated length
(`LocalRecordLog::sync_files`, `Writer::sync`, `Postings::flush`). The
kernel writes only dirty pages, but it walks the page tables of the whole
range, so a small commit on a multi-gigabyte log costs time linear in
the log's size.

## Design

Four changes, independent of each other:

1. **Dirty flags.** A table records whether its writer or postings were
   written since its last `sync`, and `sync` returns at once if not. The
   postings' second flush is skipped when `dirty` is empty. The log's
   metadata is written once per commit phase, as now.
2. **One directory sync per commit.** Files are created rarely; each
   component reports whether it created one (the log has `dir_dirty`
   already), and `Local::commit` syncs the directory once if any did.
3. **Dirty ranges.** Each mapping tracks the lowest and highest offsets
   written since its last `msync` (the log knows them exactly: the
   committed end and the new end) and syncs only that range, page
   aligned. `Mapping::sync` takes a range.
4. **A durability option.** `Options::synchronous`, as SQLite's pragma:
   `full` (today), `normal` (flush the log, let the indexes reach disk
   when they do: a crash then rebuilds from the log, which
   `index_log` can already do from the last index horizon if that is
   recorded), and `off` (no flushes; for bulk loads and tests). The shell
   gets `--synchronous`, and `-f` could default to one transaction, which
   `-1` does today.

Group commit (commit every `k` statements or `t` milliseconds) belongs to
a server (002) rather than the library.

## Tasks

- [x] Dirty flags on `LocalTable`, `Writer` and `Postings`; `sync` is a
      no-op when clean
- [x] `Local::commit` syncs the directory once
- [x] `Mapping::sync(range)`, with dirty ranges in the log, levels and
      segments
- [x] `Options::synchronous` and a recovery path for `normal`
- [x] Profile the `-1` load on disk to find what syncs within a
      transaction
- [x] Measure with the 2 000-statement script before and after; record
      the syscall counts in this issue when closing it
- [x] A test that a crash (dropping without sync) under `normal` reopens
      to the last log commit with the indexes rebuilt

## Outcome

A commit under `full` now flushes only what changed, and
`Options::synchronous` (the shell's `--synchronous`) adds `normal` and
`off`. Loading 2 000 two-triple statements with `-f`, one commit each, on
an NVMe drive under XFS (the mean of two runs):

| | `msync` | `fsync` | Time |
|---|---:|---:|---:|
| Before | 84 741 | 36 006 | 156 s |
| `full` | 26 099 | 25 | 84.6 s |
| `normal` | 4 033 | 21 | 12.9 s |
| `off` | 31 | 20 | 0.37 s |

That is 42 `msync` and 18 `fsync` per commit before, and 13 and none after
under `full`. The times here are far above the 11.3 s measured when this
issue was opened, presumably on faster storage.

### How it differs from the plan

- **Dirty flags are the dirty ranges.** `Mapping` records the lowest and
  highest offsets written, and an empty range makes no system call, so no
  separate flags are needed. `Mapping::sync` takes a range; the log passes
  the exact range since its last commit.
- **A range doesn't shrink a hash level's flush.** A commit's keys scatter
  over the level, so the range spans most of it. One call per level is
  still right: each `msync` that writes waits for the device, so one per
  page would be slower. `full` still does 13 `msync` a commit, consistent
  with one for each of the nine tables and four for the log, and only about
  halves the time.
- **The first commit of a session flushes everything each table has.** A
  killed process can leave pages unflushed that a later commit depends on,
  so opening marks whole files dirty. It costs little, as a clean file has
  nothing to write.
- **`normal` rebuilds from the whole log, not from a horizon.** Replaying
  from a recorded point can't repair a table whose pages reached the disk
  in no order: a bucket may link to one that was lost, or a level's overflow
  count be older than its buckets, which then hands a bucket out twice. So
  the first commit under `normal` or `off` marks the indexes stale in the
  log's metadata, with no extra flush. A clean close flushes them and clears
  the mark. Opening with the mark set deletes the index files and
  `Graph::open` indexes the whole log again, so recovery takes time in
  proportion to the log. A compaction horizon is lost with the files, which
  only means more history is kept.
- **`normal` also skips what recovery makes unnecessary:** the log commits in
  one metadata write, the `reserved_id` flush goes, and a promotion doesn't
  flush its tree. That is 2 `msync` a commit.
- **`-f` still commits per statement.** Defaulting it to one transaction
  would change what a failed script leaves behind, and `-1` already does it.

### The profile

Within one transaction nothing syncs per statement: the `-1` load made 88
`msync` and 42 `fsync` in all before this change (59 and 4 after), so neither
`promote` nor the `reserved_id` writes explain a gap to the in-memory load.
On this machine `-1` takes about 0.5 s before and after.
