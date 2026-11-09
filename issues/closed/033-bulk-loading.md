# Bulk loading from files

**Status:** closed · **Size:** medium · **Related:** [021](021-commit-cost.md) (one commit, and `synchronous`), [020](020-shell-reparses-long-statements.md) (the other reason loading is slow), [032](../032-rdf-and-sparql.md) (RDF formats use this)

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

- [x] CSV and TSV readers (RFC 4180 quoting), JSON lines reader; no
      dependency needed for either
- [x] `from csv|tsv|jsonl "FILE"` in `insert` and `delete`; column
      typing clause
- [x] Batched, single-transaction run with presizing and reduced syncs
- [x] `\copy`, with a rate report; `into csv`
- [x] A benchmark: 1 000 000 rows of three columns, in memory and on
      disk, against the same data as `insert` statements
- [x] Example: loading a CSV and querying it

## Open questions

- Parquet, which Kùzu and DuckDB read, needs a dependency with a long
  tail; not in the first version.
- Should a load that fails on row n keep rows before it? One
  transaction says no; a `continue on error` flag with a report of the
  rows skipped is the alternative, and matches `Graph::import`'s
  `OnError::Skip`.

## Outcome

`insert TEMPLATE from csv|tsv|jsonl "FILE"`, also `delete`, runs the template
for each record, streaming the file. `\copy FILE TEMPLATE` is the same in one
transaction with a rate report, and `select ... into csv|tsv|jsonl "FILE"`
writes rows. There is an example, `examples/24-loading-files.graql`, and a
bench, `benches/load.rs`.

Loading 1 000 000 rows of three columns (3 000 000 links), on an NVMe drive
under XFS, one shell process each:

| | Time |
|---|---:|
| `from csv`, in memory | 18.8 s |
| `from csv`, on disk | 54 s |
| `\copy`, on disk | 20 to 25 s |
| The same as `insert` statements with `-1`, in memory | 36.2 s |
| The same as `insert` statements with `-1`, on disk | 74.5 s |

`\copy` loads 120 000 to 150 000 links a second on disk, past the 100 000 the
issue wanted the shell to reach. It is faster than the statement because it marks the indexes stale first, so that
neither a promotion nor a log reservation need flush during the load: a crash
then has the indexes rebuilt, the recovery 021 built. It flushes them at the
end. These numbers are in this issue and not in `BENCHMARKS.md`, whose tables
are from its one reference machine.

### How it differs from the plan

- **A session must be allowed files.** The statements run with the process's
  access to the file system, so `Session::allow_files(true)` turns them on and
  they fail without it. The shell does; a library taking GraQL from elsewhere
  isn't exposed by default.
- **An empty field is unbound,** quoted or not: the template's triples using
  it are left out, as for an unbound variable from an optional pattern.
- **`$1` is a column, not a parameter,** when the statement has a file: it
  needs no value given, and other `$name`s are still parameters. A header's
  columns go by position too. JSON lines have no positions.
- **The clause** is `( key: p, id: q, value: r, number: s, header: false )`,
  with `rows N` after it.
- **Presizing is for an empty graph only.** A million rows in memory took 25.6
  s without it and 15.7 s with it, and on disk 59.0 s and 57.6 s. A graph with
  entries already would need its indexes moved, so it is left to grow. The
  lines are only counted when it will be used.
- **`\copy` in a transaction** just runs in it, with no change to the syncs, as
  it can't end a transaction it didn't start.
- **`from` a file can't be combined with `where`,** or load into a data graph.
- **A failed load** leaves its rows appended and uncommitted, as a failed
  statement does: the shell rolls them back, and a library caller should too.

The open questions are still open: Parquet, and `continue on error`.
