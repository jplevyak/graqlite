# A full-text index over values

**Status:** open · **Size:** medium · **Related:** [030](closed/030-vector-index.md), [011](closed/011-adaptive-btree-index-entries.md) (postings: the inverted index's data structure, already built), [026](026-index-lookup-allocations.md)

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

- [ ] The tokenizer, with tests on Unicode text
- [ ] The term table, fed by `index_records`; `\reindex` from the log
- [ ] `(match "...")` and `text_match` as a relation with streamed
      intersection; `\explain` shows the terms and their counts
- [ ] A benchmark: 200 000 short texts, a rare and a common term, with
      and without the index
- [ ] Example: searching names and biographies, joined with the graph
- [ ] Decide on indexing every value or declared predicates; record it
      here

## Open questions

- Do the index's entries count in the statistics and `\d`? They aren't
  links, so no; `\info` should give its size.
- `contains(?n, "a")` as a function stays a scan. Should the planner
  rewrite a `contains` of a constant word into a `match` when the index
  exists? Only when the constant is a whole word; say so in the docs.
