# Sync only what a commit changed

**Status:** open · **Size:** medium · **Related:** [002](002-multi-process-access.md) (a server would group commits), [011](closed/011-adaptive-btree-index-entries.md) (postings syncs)

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

- [ ] Dirty flags on `LocalTable`, `Writer` and `Postings`; `sync` is a
      no-op when clean
- [ ] `Local::commit` syncs the directory once
- [ ] `Mapping::sync(range)`, with dirty ranges in the log, levels and
      segments
- [ ] `Options::synchronous` and a recovery path for `normal`
- [ ] Profile the `-1` load on disk to find what syncs within a
      transaction
- [ ] Measure with the 2 000-statement script before and after; record
      the syscall counts in this issue when closing it
- [ ] A test that a crash (dropping without sync) under `normal` reopens
      to the last log commit with the indexes rebuilt
