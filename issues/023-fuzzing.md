# Fuzz the parsers and the restore paths

**Status:** open · **Size:** small · **Related:** [022](022-release-readiness.md) (CI)

## Problem

Three hand-written binary formats are decoded from bytes that may be
truncated or corrupt: record log entries (`record_log.rs`), bucket records
(`dynamic_table/record.rs`) and posting pages (`postings.rs`). The docs
promise that decoding is bounds checked and never reads out of range, and
the tests cover truncation systematically, which is better than most. But
the GraQL lexer and parser (5 700 lines in `graql.rs`) and the three
snapshot `restore` functions (`local_record_log`, `dynamic_table`,
`postings`, framed by `local::restore`) take input of any shape and have
only example-based tests. There are 26 `panic!`, `unreachable!` and
`expect` sites in non-test code that a crafted input might reach.

## Design

`cargo-fuzz` targets, in `fuzz/`, each a few lines:

- `entry_parse`: `Entry::parse` on arbitrary bytes, then `complete_len`
  and `estimate_len` on every prefix, asserting the invariants the tests
  state (a prefix of a complete entry is never complete; the estimate is
  never short).
- `bucket_parse`: `parse_bucket` on 128 arbitrary bytes.
- `leaf_decode`: `leaf_ids` on an arbitrary page with an arbitrary count.
- `graql_parse`: `graql::parse` on arbitrary text, then `Display` of what
  parses and a second parse, asserting the round trip (the printer is
  meant to be canonical).
- `graql_run`: a fixed small graph, arbitrary text, `Session::execute`,
  asserting only that it returns rather than panics.
- `restore`: `Local::restore` from arbitrary bytes, in memory.

Each runs for a few minutes in CI, and the corpus is committed.

## Tasks

- [ ] The six targets above
- [ ] Fix what they find; the parser target will likely find panics in
      its first hour
- [ ] A CI job that runs each for 2 minutes
- [ ] Replace reachable `expect`s in the parsers with errors
