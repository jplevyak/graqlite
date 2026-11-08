# Reclaim retired index levels when their readers leave, not after a delay

**Status:** open · **Size:** medium · **Related:** [018](closed/018-snapshot-readers.md) (readers), [002](002-multi-process-access.md) (cross-process readers need the same)

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

- [ ] `Arc<Level>` in `Shared`, pinned per lookup
- [ ] Remove `retire_delay`, `retiring` and `make_inaccessible`
- [ ] A test: a reader paused mid-lookup (a fault hook on the reader side)
      across a full expansion and retirement completes correctly
- [ ] Benchmark `benches/lookups.rs` before and after: the cost should be
      one atomic increment and decrement per lookup
