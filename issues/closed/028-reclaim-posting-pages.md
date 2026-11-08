# Reclaim the pages of compacted posting trees

**Status:** closed · **Size:** medium · **Related:** [011](011-adaptive-btree-index-entries.md) (compaction builds new trees), [018](018-snapshot-readers.md) (snapshots say when readers are gone), [024](024-reader-reclamation-without-a-timer.md)

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

- [x] Free list in `Postings`, filled by `forget`, used by `allocate`
- [x] Rebuild it on open from unnamed headers
- [x] Dead pages in `\info`
- [x] A test: repeated compaction of a growing entry keeps the postings
      file size bounded
- [ ] Note the offline reclaim in the README's compaction paragraph
      (not so: see below)

## As implemented

The design above was missing one case. A graph reader whose snapshot is
*after* the horizon can still hold an old tree: one it looked up just
before compaction swapped the key's bucket. Snapshots bound what a reader
sees, not which pages it is reading, so they can't say when a page is
free. Instead, readers pin the trees:

- **Pins by epoch.** Every `PostingsReader` takes a pin when it is made,
  and every `Tree` and `Cursor` holds its reader's pin (the writer has
  its own, renewed at each release). `Postings::release` puts the pages
  of the released trees on a list for the current epoch and advances it.
  `allocate` moves them to the free list once no pin of that epoch or an
  earlier one is left. A reader made after a release looks keys up after
  the bucket swap, so it never finds the old tree, and doesn't hold its
  pages. This is the snapshot for a `TableReader`: one taken when it is
  made, and it holds back reuse until it is dropped, as a graph reader's
  snapshot holds back compaction. Taking a pin locks a mutex, once per
  reader; a tree or cursor only clones the reader's `Arc`, as it cloned
  the shared segments before.
- **Released only once durable.** `LocalTable::compact` releases the old
  trees after `Writer::sync`, so no bucket on disk can still name a page
  that gets reused. It also releases the trees of keys `set` or `delete`d
  since they were promoted, which were leaked before.
- **Rebuilt on open,** and on restore, by `Postings::reclaim_unnamed`.
  It walks the pages of each tree that its key's bucket names (and of
  any tree with no key recorded, to be safe), and frees every other page
  below the pages in use. That also frees trees a crash left unnamed: a
  promotion's tree made durable before its bucket was replaced, or pages
  reused after the last sync. If a tree kept isn't well formed, nothing
  is freed. Nothing new is written to the format.
- **`\info`** prints `postings: N pages of 4 KiB, M free` when there are
  trees; `Local::postings_pages` and `LocalTable::postings_pages` give
  the counts.
- **Offline reclaim isn't so:** `\dump` copies the postings' pages as
  they are, free ones included, so a restore doesn't shrink them. The
  restored table rebuilds its free list and reuses them, though. A dump
  that skips free pages would need trees renumbered; it isn't needed
  while the pages are reused.
- **Tests:** `repeated_compaction_reuses_pages` (40 rounds of growing
  and compacting one key: the page count is the same after round 10 as
  after round 40, and after reopening, and so is the file's length),
  `a_reader_holds_back_reuse_of_the_pages_it_may_read`,
  `opening_frees_the_pages_of_unnamed_trees`,
  `compaction_releases_the_trees_of_keys_set_since`, and in `postings`,
  `releasing_frees_every_page_of_a_tree_once_unpinned`. That last one
  checks a released tree of height 2 frees exactly its pages.
- **Cost:** the index bench, A/B against the previous commit at a load
  average under 4: within ±3%, except `limit 10 of type person` +4%
  (0.6 µs) and `type person and city paris` +3.5%. A second round, with
  the load rising to 10, showed the same pattern, and larger differences.
  Neither touches a compaction, so the cost is the pin's extra hop on
  each page a cursor reads, or noise.

## Closed

Done. Pages are reused, but the segment files never shrink; a vacuum
that moves trees down would.
