# Facts of more than three fields

**Status:** open · **Size:** medium · **Needs:** a decision between the two designs below · **Related:** [004](closed/004-history-and-link-variables.md) (link variables), [014](closed/014-subgraphs.md) (subgraphs, the fields after the fifth), [033](closed/033-bulk-loading.md) (bulk loading)

## Problem

A link is a subject, a predicate and an object (`LinkFields`), with an
author, a time and subgraphs. A fact with more columns has to be
reified: a node for the fact and a link per column, or links about a link
(`<< s p o >> q v`). Either way one fact costs several entries in the log,
several index updates, and a join per column to read back.

The record log has no such limit: a tuple holds any number of ids
(`push_tuple`), and the README calls them "edges / hyperedges". Only the
graph layer fixes the first three fields' meaning.

The use case is loading relations produced by a program analysis (Scyth's
fact layer), where many relations are wider than a triple:

| Relation | Columns |
| --- | --- |
| `param_arg_edge` | caller, call site, parameter index, callee, parameter node, argument node |
| `ret_call_edge` | callee, return node, caller, call site |
| `local_data_dep` | definition, from node, to node |
| `direct_call` | caller, call site, callee |

Of the thirteen relations that program's graph is built from, six have
three or more columns, where a triple holds two. Loaded as triples they
need a node per `(definition, node)` pair and a link per column, which is
two to six times the links, and loading is already the slowest step
([040](040-traversal-and-load-speed-against-an-adjacency-array.md):
about 200,000 links a second in memory).

## Two designs

1. **Qualifier fields on a link.** A link may carry named extra fields
   after its subgraphs (or in place of them, tagged), stored in the same
   tuple. They are not indexed by default; a pattern reads them from the
   link it matched: `?c arg ?callee [site ?s ; index ?i]`, or through the
   link variable of 004. This keeps the triple as the indexed unit and
   makes a wide fact one entry. An index on a qualifier is declared, as a
   vector index is.
2. **Relations beside links.** `relation name(a b c d)` declares a stored
   n-ary relation: tuples in the log, an index per declared key. Rules can
   already produce n-ary relations in memory; this stores them. Patterns
   call them as rules are called: `param_arg(?c ?s ?i ?callee ?p ?a)`.

Design 1 is smaller and keeps one data model. Design 2 fits data that was
never a graph, and gives a home to the tuples of
[037](037-standing-queries-with-paths-and-rules.md)'s maintained
relations.

## Tasks

- [ ] Decide between the designs, with the log format each needs
- [ ] The chosen form in `Graph`, with `as of`, deletion and compaction
- [ ] GraQL syntax, `\copy` and `from csv` loading a wide record as one
      fact
- [ ] A benchmark: the same six-column facts loaded reified and loaded
      wide, with the read of one fact's columns
