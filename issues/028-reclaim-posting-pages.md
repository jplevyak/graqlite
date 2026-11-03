# Reclaim the pages of compacted posting trees

**Status:** open · **Size:** medium · **Related:** [011](closed/011-adaptive-btree-index-entries.md) (compaction builds new trees), [018](closed/018-snapshot-readers.md) (snapshots say when readers are gone), [024](024-reader-reclamation-without-a-timer.md)

## Problem

Compacting a large entry (`LocalTable::compact`) builds a new tree and
`forget`s the old one; its pages are "not yet reclaimed", since a reader
may still hold the old tree. With `Options::history` set, `Graph::tick`
compacts an eighth of the log at a time, so a long-lived database's
posting files only grow: every compaction leaves behind the whole old
tree of every entry it touched. There is no vacuum, and `\info` doesn't
show how much is dead.

## Design

- **Free lists per segment.** `Postings` keeps the pages of forgotten
  trees in a free list, and `allocate` takes from it before extending a
  segment. Pages are the same size, so no fragmentation.
- **When a page is free.** A reader of a tree is a `Tree` or `Cursor`
  holding an `Arc<Shared>`, made from a graph reader that registered its
  snapshot (`Graph::snapshots`). A tree forgotten by a compaction to
  horizon `h` can only be held by a reader whose snapshot is before `h`,
  and compaction already never passes a live snapshot. So the old tree's
  pages are free once compaction returns, as far as graph readers go;
  a `TableReader` used directly (not through a `Graph`) has no snapshot,
  so either register one for it or document that compaction may pull a
  tree from under it.
- **Persistence.** The free list is rebuilt on open by walking the headers
  that no bucket names (`trees()` already finds them for compaction), so
  nothing new is written to the format.
- **Offline.** `\dump` and `--restore` already rewrite the postings; say
  in the docs that they also reclaim.

## Tasks

- [ ] Free list in `Postings`, filled by `forget`, used by `allocate`
- [ ] Rebuild it on open from unnamed headers
- [ ] Dead pages in `\info`
- [ ] A test: repeated compaction of a growing entry keeps the postings
      file size bounded
- [ ] Note the offline reclaim in the README's compaction paragraph
