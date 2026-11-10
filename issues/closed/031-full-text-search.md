# A full-text index over values

**Status:** closed · **Size:** medium · **Related:** [030](030-vector-index.md), [011](011-adaptive-btree-index-entries.md) (postings: the inverted index's data structure, already built), [026](026-index-lookup-allocations.md)

## Motivation

Text search is the most used index after the primary key: SQLite's FTS5
is the reason many apps never need another database, and CozoDB and
every graph database with users have one. GraQL has `contains`,
`starts` and `~` (a regular expression), each a scan of every value a
pattern reaches. A query like "people whose biography mentions Paris"
reads every biography.

graqlite is unusually well placed to add one. Values are interned and
never change, so an inverted index is append only: a value is tokenized
once, when it is appended. The index data structure is already here:
`postings.rs` keeps large id sets as B+ trees of ids, streamed newest
first, and `LocalTable` maps a 128 bit key to such a set. A term is a
key; its posting list is the ids of the values containing it.

## Design

- **One more index table** (`Local::new_table`; `Options::tables` already
  allows more than the built-ins), keyed by the uuid of a term, holding
  the ids of the values that contain it. `index_records` tokenizes each
  appended value and inserts its id under each term. Nothing is deleted:
  values aren't, and a search joins a pattern that decides liveness.
- **Tokenizing:** Unicode words (`char::is_alphanumeric` runs), lower
  cased, with no stemming in the first version, so no dependency. Terms
  longer than a limit are hashed; values longer than a limit (say 64
  KiB) are indexed up to it. A stemmer and stop words can come as
  options, since changing them means rebuilding the index, which a
  `\reindex` does from the log.
- **What is indexed:** every value, by default, since the value index
  already hashes every one; or only the objects of declared predicates
  (`index text on bio`) to keep the table small. Start with every value
  and measure the table's size on the benches' graphs.
- **Querying:** a test of an object, as `(~"re")` is:
  `?p bio (match "paris louvre")` matches values containing every
  term, `(match "paris | louvre")` any. It lowers to a built-in relation
  `text_match("paris louvre" ?v)` whose tuples are the intersection of
  the terms' posting lists (streamed, so a term with a million postings
  is read in step with the rarest term), joined like any call. The
  planner estimates it from the rarest term's count, which the table
  gives without reading the list.
- **Ranking, later:** BM25 needs term frequencies and value lengths. The
  posting list holds ids only; a second list under a `(term, "tf")` key
  with the frequency per id, or a per-value length table, can be added
  when ranking is wanted. Phrase queries need positions, which can go the
  same way. Neither changes the first version's format if the tables are
  separate.
- **Snapshot readers** see the index as of their end by dropping ids
  past it, as every index lookup does.

## Tasks

- [x] The tokenizer, with tests on Unicode text
- [x] The term table, fed by `index_records`; `\reindex` from the log
- [x] `(match "...")` and `text_match` as a relation with streamed
      intersection; `\explain` shows the terms and their counts
- [x] A benchmark: 200 000 short texts, a rare and a common term, with
      and without the index
- [x] Example: searching names and biographies, joined with the graph
- [x] Decide on indexing every value or declared predicates; record it
      here

## Open questions

- Do the index's entries count in the statistics and `\d`? They aren't
  links, so no; `\info` should give its size.
- `contains(?n, "a")` as a function stays a scan. Should the planner
  rewrite a `contains` of a constant word into a `match` when the index
  exists? Only when the constant is a whole word; say so in the docs.

## Outcome

`?p bio (match "paris louvre")` finds objects with every word, `|` separates
alternatives, and `match(?v, q)` is the same test anywhere a test goes. The
term table is built-in index 9, `src/text.rs` has the tokenizer and the
streamed intersection, `\reindex` rebuilds it, and `\info` gives its size.
There is an example, `examples/25-full-text-search.graql`, and a bench,
`benches/text.rs`.

200 000 texts of about twelve words, on disk (release, one core, an NVMe drive
under XFS):

| | Time |
|---|---:|
| `match`, a rare word (100 texts) | 71 µs |
| `match`, a rare and a common word | 0.56 ms |
| `match`, a common word (100 000 texts), by a scan and the test | 155 ms |
| `contains`, a rare word | 77 ms |
| `contains`, a common word | 84 ms |

### Every value or declared predicates

Every value, as the first version; a declaration can come later. On the
graph above, the load took 3.9 s against 1.7 s without the index, and the
index is 105.6 MiB of a 167.1 MiB database: about 46 bytes per posting, with
every text unique. On short values (a name and two numbers per node, 200 000
nodes) it is 16.6 MiB of 224 MiB. No declaration means any predicate can be
searched, and `?s ?p (match "x")` searches them all.

### How it differs from the plan

- **`match` is a filter the planner can replace,** not a relation. A test
  goes wherever filters go (`not`, `optional`, a `since` window) and is right
  on its own; where a pattern of its group binds the
  value, the planner may read the index instead, dropping the filter. There
  is no `text_match` relation to write, and `?s ?p (match "x")` is the form
  for any predicate, whose pattern also decides what is live.
- **Promotions wait, for every table.** Moving a term's list to a tree flushed
  the postings each time, so a 5 000 word vocabulary took minutes: over two
  minutes for 40 000 texts. `LocalTable` now promotes up to 1 024 keys behind
  one flush, by the next sync, or when a key reaches 128 ids. It helps
  anything with many keys passing 64 ids: 200 000 nodes of three short values
  loaded in 4.2 s against 145 s before, using 224 MiB against 175 MiB.
- **Long values are cut at 64 KiB less any word split by the cut,** so the
  index and the test agree on what is in a value.
- **Values only an overlay has** are found by testing the overlay's links, as
  the index can't have them; values rolled back or lost to a crash are dropped
  by checking the id still names a value.
- **The first open of an older database** indexes its text: a key in the table
  records that it covers the log, and a crash during that rebuilds the indexes.
- **`as of` needs no filter of its own:** the step reads up to the log's end,
  and the join with the pattern applies the `as of`.

The open questions are settled as the issue guessed: the index isn't in the
statistics or `\d`, and `\info` gives its size. `contains` is not rewritten
to `match`: the two differ on partial words, and the planner can't know a
constant is a whole word in every value.
