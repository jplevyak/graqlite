# Pending examples

Scripts for features that open issues will add. Each sets up its data with
what runs today, then lists what the issue should make work, with the
expected result in an `-- expect:` comment.

The golden tests (`cargo test --test cli`) run only `examples/*.graql`, not
this directory. When an issue is done:

1. Move its script up to `examples/` (numbered after the others).
2. Turn its `-- expect:` comments into prose.
3. Run `GRAQLITE_BLESS=1 cargo test --test cli` and check the new `.out`
   against them.

There are none now: every open issue's examples have moved up.  Issue
014's are [`17-subgraphs`](../17-subgraphs.graql); 015's
[`16-derived-links`](../16-derived-links.graql) to
[`20-symbols-and-graphs`](../20-symbols-and-graphs.graql); 016's
[`15-lookups`](../15-lookups.graql), [`20-symbols-and-graphs`](../20-symbols-and-graphs.graql)
and [`22-the-rest-of-the-tour`](../22-the-rest-of-the-tour.graql); and
017's [`21-graph-layering`](../21-graph-layering.graql).

Its runnable parts are [`14-reference-tour`](../14-reference-tour.graql).
