# RDF formats and a SPARQL front end

**Status:** open · **Size:** large · **Related:** [010](closed/010-language-comparison.md) (the comparison with SPARQL), [014](closed/014-subgraphs.md) (named graphs map to subgraphs), [017](closed/017-graph-layering.md), [033](033-bulk-loading.md), [002](002-multi-process-access.md) (a SPARQL endpoint needs a server)

## Motivation

GraQL patterns are Turtle with variables, and the data model is
triples with statements about links: RDF-star, nearly. Oxigraph shows
what standards buy a small Rust store: every RDF dataset on the web
loads into it, the W3C test suites test it, and tools, editors and
libraries that speak SPARQL work with it unchanged. graqlite can have
that without giving up GraQL: Turtle and N-Triples in and out first,
then SPARQL as a second front end lowering to the same `query::Query`.

The first half is the valuable, cheap part: most of the world's graph
data is in these formats, and today it can only reach graqlite by being
rewritten as `insert` statements.

## Design

### Mapping

| RDF | graqlite |
|---|---|
| IRI | a node whose key is the IRI (`<http://ex.org/alice>`); GraQL gains `<...>` as a node, and `prefix ex: <http://ex.org/>` for `ex:alice` |
| Blank node | a node, keyless; labels kept per file |
| Plain literal | a value with its text |
| Typed or tagged literal | a value with the text, and the datatype or tag as a link on the value (`"42" graqlite:datatype xsd:integer`), so nothing is lost and plain comparisons still work on the text; expressions already type by content (009) |
| Named graph | a subgraph (014); the default graph is the main graph |
| RDF-star quoted triple | a link node `<< s p o >>`; an RDF-star triple about a triple that isn't asserted has no counterpart (GraQL's link must be there): assert it, and say so |

### Formats

`\import FILE [into g]` and `\export FILE [from g]` in the shell, and
`graql::rdf::{read, write}` in the library, for N-Triples, N-Quads and
Turtle (TriG when subgraphs are involved). Writing is a scan of the
links (or a subgraph's), which `lookup` already produces as a graph.
Reading goes through `Graph::batch` with the loader of 033. The parsers
are a few hundred lines each without a dependency; Oxigraph's `oxttl`
crate is the alternative if the dependency is acceptable.

### SPARQL

A `sparql` module lowering SPARQL 1.1 Query to `query::Query`, covering
what the engine has: basic graph patterns, `OPTIONAL`, `UNION`,
`MINUS` and `FILTER NOT EXISTS` (both as `not`), `FILTER` with the
expression functions, property paths (`paths.rs` has them), `GROUP BY`,
`HAVING`, aggregates, `ORDER BY`, `LIMIT`, `OFFSET`, `DISTINCT`,
`VALUES` (given tuples, as a call), `GRAPH ?g { }` (a group `in_graph`),
`SELECT`, `ASK` (`verify`), `CONSTRUCT` (a lookup's graph) and
`DESCRIBE` (`\show`). Unsupported forms (`SERVICE`, `BIND` of
subqueries, some functions) report "unsupported" with a position, as
GraQL does. The shell runs it with `\sparql`, or a `--sparql` mode for
scripts. The W3C SPARQL 1.1 query test suite, filtered to the supported
forms, becomes a golden test like `tests/cli.rs`.

SPARQL Update and the SPARQL protocol (an HTTP endpoint) wait for 002's
server.

## Tasks

- [ ] `<iri>` nodes and `prefix` in GraQL
- [ ] N-Triples and N-Quads read and write; Turtle and TriG read and write
- [ ] `\import` and `\export`; datatype links on literals
- [ ] The SPARQL parser and lowering, in the order above; `\sparql`
- [ ] The W3C test suite subset as a golden test; record what fails and
      why in this issue
- [ ] The comparison document (022) gains a "run it as SPARQL" column

## Open questions

- Keys today start with `/`. An IRI as a key means keys are any text;
  check that nothing depends on the prefix (author-scoped `//keys`
  might).
- Literal identity: in RDF `"42"^^xsd:integer` and `"42"` are different
  terms; here they are one value with a datatype link that one has.
  Round trips won't preserve that distinction for a literal that appears
  both ways. Acceptable, if documented.
