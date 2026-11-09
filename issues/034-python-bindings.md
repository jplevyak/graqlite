# Python bindings

**Status:** open · **Size:** medium · **Related:** [018](closed/018-snapshot-readers.md) (readers release the GIL), [033](closed/033-bulk-loading.md), [002](002-multi-process-access.md)

## Motivation

Oxigraph (`pyoxigraph`), CozoDB (`pycozo`) and Kùzu (`kuzu`) all ship
Python packages, and for each it is how most users first try the
database: a notebook, a `pip install`, a few queries. graqlite's shell
is good for that, but a notebook with a dataframe is better, and the
library's API (`Graph`, `graql::Session`, `Output`) is already the
shape a binding needs.

## Design

- **A `pygraqlite` crate** in a `python/` directory, built with `pyo3`
  and `maturin`, feature gated so the main crate keeps its three
  dependencies. It exposes:
  - `Graph(path=None, **options)`: open or create; `in_memory` when no
    path; a context manager that commits on exit, or `rollback()`.
  - `graph.execute(text, **params) -> list[Output]`, each output a
    `Rows` (names, rows of `Entry` or Python values), a `Table` of
    cells (numbers, strings, entries), a `Graph` result (links and
    nodes, with `derived` and provenance), or a status. Parameters bind
    as `Session::run_with` does.
  - `graph.prepare(text) -> Prepared`, `prepared.run(**params)`.
  - `Entry`: `id`, `key`, `data` (bytes, or `str` when UTF-8), `links()`.
  - `graph.readers() -> Readers`, `readers.latest()`: a read-only graph
    for threads, releasing the GIL while a query runs (the Rust side is
    `Send + Sync` already).
  - `graph.snapshot(file)`, `Graph.restore(file)`, `graph.compact(...)`.
  - `to_pandas()` / `to_arrow()` on `Rows` and `Table`, optional
    extras.
- **Errors** map to Python exceptions with the line and column GraQL
  reports.
- **Wheels** for Linux, macOS and (once the crate builds there) Windows,
  built in CI (022) with `maturin-action`; the package version follows
  the crate's.
- **Docs:** a notebook in `python/examples/` that runs the GraQL tour.

## Tasks

- [ ] The crate, with `Graph`, `execute`, `Output` types and `Entry`
- [ ] Prepared statements and parameters
- [ ] Readers for threads, releasing the GIL
- [ ] Tests with `pytest` running the example scripts and comparing
      with their `.out` files
- [ ] Wheels in CI; `pip install graqlite`
- [ ] The notebook

## Open questions

- The name on PyPI: `graqlite` if free.
- A `Session` is per connection in the shell; in Python a `Graph` can own
  one session, with `graph.session()` for another when `author`,
  `subgraph`, symbols or a `data` graph should differ.
