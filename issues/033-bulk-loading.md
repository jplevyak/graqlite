# Bulk loading from files

**Status:** open · **Size:** medium · **Related:** [021](closed/021-commit-cost.md) (one commit, and `synchronous`), [020](closed/020-shell-reparses-long-statements.md) (the other reason loading is slow), [032](032-rdf-and-sparql.md) (RDF formats use this)

## Motivation

Kùzu's `COPY FROM`, Oxigraph's `load`, SQLite's `.import`: every
embedded database has a fast path for getting a file in, because it is
the first thing a new user does and the slowest thing they do by hand.
graqlite's only way in is `insert` statements, which means generating
GraQL text from the file, hitting 020 and 021, and then holding the
whole statement in memory to parse.

The library can load well over 100 000 links per second in memory
(`benches/index.rs` loads 800 000 in a few seconds); the shell should
get there from a CSV.

## Design

- **A file as a source of rows.** `insert TEMPLATE from csv "FILE"`
  (also `tsv`, `jsonl`): each record is a row binding the template's
  variables by column name (the header) or position (`$1`, `$2`), and
  the template is instantiated per row as `insert ... where` does today.
  So `insert ?p a person ; name ?name ; city ?city from csv
  "people.csv"` with columns `p,name,city`, where `p` holds keys. A
  column's text is a key if it starts with `/`, an `#id` if it starts
  with `#`, else a value; a `(key: p, number: age)` clause can override.
  The same statement shape serves `delete ... from csv`.
- **Streaming.** The reader yields rows in batches; nothing holds the
  file. Blank nodes in the template are new per row, as now.
- **One transaction, few syncs.** The load runs in one `batch`, with
  `synchronous` (021) lowered for its duration if the caller asks
  (`\copy` does by default, and syncs once at the end). The statistics
  and the planner see the result at the end.
- **Presizing.** The reader counts lines (or takes a `rows N` hint) and
  the graph grows its files once, as `Options::for_tuples` does on open.
- **The shell:** `\copy FILE TEMPLATE` is the same statement for people
  who think in psql; `\copy` also reports rows and links per second.
- **Output:** `\o FILE` with `--csv` exists; `select ... into csv "FILE"`
  makes the pair symmetric.

## Tasks

- [ ] CSV and TSV readers (RFC 4180 quoting), JSON lines reader; no
      dependency needed for either
- [ ] `from csv|tsv|jsonl "FILE"` in `insert` and `delete`; column
      typing clause
- [ ] Batched, single-transaction run with presizing and reduced syncs
- [ ] `\copy`, with a rate report; `into csv`
- [ ] A benchmark: 1 000 000 rows of three columns, in memory and on
      disk, against the same data as `insert` statements
- [ ] Example: loading a CSV and querying it

## Open questions

- Parquet, which Kùzu and DuckDB read, needs a dependency with a long
  tail; not in the first version.
- Should a load that fails on row n keep rows before it? One
  transaction says no; a `continue on error` flag with a report of the
  rows skipped is the alternative, and matches `Graph::import`'s
  `OnError::Skip`.
