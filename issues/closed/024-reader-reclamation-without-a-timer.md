# Reclaim retired index levels when their readers leave, not after a delay

**Status:** closed · **Size:** medium · **Related:** [018](018-snapshot-readers.md) (readers), [002](../002-multi-process-access.md) (cross-process readers need the same)

## Problem

When a dynamic table level finishes expanding, the writer keeps the old
level mapped for `Options::retire_delay` (5 minutes) and then remaps it
`PROT_NONE` (`Writer::retire`, `Mapping::make_inaccessible`). A reader
still inside a lookup on that level after the delay faults. The module
docs call this the intended failure: "faults rather than reading
garbage", which is the right choice for a timer. But it means a reader
thread that is descheduled, stopped in a debugger, swapped out or simply
slow for five minutes brings down the whole process, readers and writer
alike. And every expansion of a busy table pins its old level's file and
reserved address space (at least `min_level_reserve`, 256 MiB) for five
minutes.

The same timer would not work across processes (002), where a reader's
progress is invisible to the writer.

## Design

Pin a level for as long as a lookup uses it, with a count rather than a
clock:

- `Shared::levels` holds `Arc<Level>`s behind an atomic pointer
  (`arc-swap`'s `load` is one atomic increment per lookup, or a hand
  rolled hazard slot per reader, which avoids the dependency). A lookup
  clones the `Arc` of the level it starts on and of any level it follows
  an `EXTENDED` header into, and drops them when it returns.
- `retire` stores a null pointer and drops the writer's `Arc`; the
  `Level`'s `Drop` unmaps and unlinks the file when the last reader's
  clone is gone. No delay, no `PROT_NONE`.
- `retiring` and `retire_delay` go away; `tick` no longer needs to run
  for retirement, only for expansion steps.

Across processes (002), the equivalent is LMDB's reader table: each
reader records the level it is on in a shared file, and the writer
retires a level only when no slot names it.

## Tasks

- [x] `Arc<Level>` in `Shared`, pinned per lookup
- [x] Remove `retire_delay`, `retiring` and `make_inaccessible`
- [x] A test: a reader paused mid-lookup (a fault hook on the reader side)
      across a full expansion and retirement completes correctly
- [x] Benchmark `benches/lookups.rs` before and after: the cost should be
      one atomic increment and decrement per lookup

## As implemented

As designed, with `arc-swap`:

- **`Shared::levels` is `[ArcSwapOption<Level>; MAX_LEVELS]`.** A lookup
  `load`s a guard of the level it starts on, and of each level it follows
  an `EXTENDED` header into, and drops it when it moves on or returns.
  Retiring stores `None`; `arc-swap` then turns any outstanding guard into
  a full reference, so the `Level` (and its `Mapping`) is dropped, and
  unmapped, when the last lookup on it returns. The `owned` vector, which
  kept every level ever mapped, and with it its reserved address space,
  until the table was dropped, is gone.
- **Retirement is immediate,** at the end of the expansion step that
  finishes the level; `tick` only runs expansion steps. `retire_delay`,
  `retiring`, `Writer::retire` and `Mapping::make_inaccessible` are
  removed.
- **The file is deleted at the next `sync`, not at retirement.** The old
  code deleted it after the delay whether or not the metadata recording
  the finished expansion had been synced, so a power loss in between could
  reopen the table with the old level current and its file gone. Now
  `Writer::sync` (which takes `&mut self` for this) deletes the retired
  levels' files after syncing the metadata and the directory; a crash
  before that leaves files that `open` already deletes, as levels below the
  current one. Unlinking a file doesn't unmap it, so readers still on the
  level are unaffected either way.
- **Tests:** `a_paused_lookup_keeps_its_level_through_retirement` pauses a
  lookup (a test-only hook in `Shared::lookup`, called while it holds a
  level) across a full expansion and retirement, checks the level is still
  mapped, then that the lookup returns the right values and the level is
  freed. `retired_level_is_unmapped_and_deleted` checks a level with no
  lookups is freed at once and its file deleted at the sync, or by the next
  open if the table is dropped before one.
- **Cost:** the lookups bench on a quiet machine, before and after:
  `local table find` +4% (about 12 ns a lookup), `seek_rev` +5%, the rest
  within ±2%, including both query-per-person benches. An earlier run
  showed +80% on the query benches; it was the machine's load (a load
  average of 18), and repeated A/B runs at a load of 3 showed no
  difference, between the guard and a full `Arc` clone either. The guard
  is kept because, unlike a clone, it doesn't make every reader thread
  write the same reference count.

## Closed

Done. [028](../028-reclaim-posting-pages.md) needs the same for posting
pages; across processes ([002](../002-multi-process-access.md)) it still
needs a reader table in shared memory.
