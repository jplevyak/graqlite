# Write a comparison of GraQL with GQL, Cypher, SPARQL and Gremlin

**Status:** closed · **Size:** small

## Problem

People who know another graph query language want to know how GraQL relates
to it: what maps one to one, what is missing, and what GraQL does that the
others don't. A first comparison was written in conversation (2026-10-31).
It found GraQL closest to SPARQL in model and syntax. It also found GraQL
ahead on saved recursive rules, history (`as_of`), statements about links,
and standing queries, and behind on optional patterns, aggregates, paths,
parameters, ordering and offset, and expressions. Issues 004–009 close those
gaps, so the page waits for them, rather than going stale as each one lands.

## Plan

Once 004–009 are closed:

- A page (in `docs/`, and published for sharing) with a feature table across
  the five languages, and a section per language: what maps, what differs,
  and the idioms to translate.
- **Side-by-side translations** of the same queries in all five, taken from
  `examples/`, so each translation is backed by a GraQL script that the tests
  run.
- **What GraQL does that the others don't:** saved Datalog rules, `as of`,
  link nodes, and standing queries.
- An honest list of what is still missing then.

## As implemented

- **`docs/graql-comparison.md`:** the comparison, linked from the README.
  - A feature table across the five languages.
  - Nine queries side by side: a filtered pattern, optional, aggregates,
    paths, recursive rules with negation, statements about links,
    expressions, parameters, and history.
  - A section per language, what only GraQL has, and what it still lacks.
- **`examples/13-comparison.graql`:** every GraQL query of the page, on one
  graph, so the tests check that each one runs and gives the answers the
  page describes. The other languages' versions are marked as idiomatic
  sketches to check against an engine's dialect, since they aren't run.
  The history query is shown with a time on the page, and with the
  equivalent id in the script, because the CLI tests fix every commit's
  time.
- **A published page for sharing:**
  https://claude.ai/artifact/EScM9hZ5CqzZdohghYcCdZ
  - It carries the same content, with a language filter for the
    translations.
  - It is private until it is shared from its Share menu.
  - It is generated from the same content; update it when the Markdown
    changes.

## Tasks

- [x] Close 004–009
- [x] Translations of the examples into GQL, Cypher, SPARQL and Gremlin
- [x] The page, linked from the README
