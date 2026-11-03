# Release readiness: license, metadata, toolchain, CI, docs

**Status:** open · **Size:** small · **Related:** [010](closed/010-language-comparison.md) (the missing document), [023](023-fuzzing.md)

## Problem

The first commit is titled "Initial release", but several things a
release needs are missing:

- **No license.** There is no `LICENSE` file and `Cargo.toml` has no
  `license` field. Nobody can legally use or contribute to the code, and
  crates.io refuses a package without one.
- **No package metadata.** `repository`, `readme`, `keywords` and
  `categories` are unset.
- **No minimum Rust version.** The code uses edition 2024 let chains,
  `slice::as_chunks`, `is_multiple_of`, `split_at_checked`,
  `Option::is_none_or` and `std::io::pipe` (44 uses). Together they need
  Rust 1.88 or later, and nothing says so: an older toolchain fails with a
  wall of errors and no hint.
- **No CI.** Tests, clippy and the golden scripts pass locally, but
  nothing runs them on push, on another OS, or on the minimum toolchain.
  The non-Linux paths in `mmap.rs` (temporary files in place of
  `memfd_create`, `set_len` in place of `posix_fallocate`) have never run.
- **`cargo fmt --check` fails** in `benches/record_log.rs` and
  `src/graql/tests.rs`: the style edition moved and the code didn't.
- **A dead link.** The README (lines 61 and 576) and
  `examples/README.md` point to `docs/graql-comparison.md`, the comparison
  with GQL, Cypher, SPARQL and Gremlin that issue 010 closed as done.
  There is no `docs/` directory. Example 13 is "the GraQL side" of it.
- **No benchmark baseline.** Four criterion benches exist and the issues
  quote their numbers, but nothing in the repository records them, so a
  regression has nothing to be compared with.
- **The name.** TypeDB's query language was called Graql from 2016 to
  2021. Searches for "GraQL" still find it, and its users expect a typed
  schema. Worth a sentence in the README, or a rename, before anything is
  published.

## Tasks

- [ ] Choose a license (MIT OR Apache-2.0 is the Rust convention), add
      `LICENSE-MIT` and `LICENSE-APACHE`, and set `license` in
      `Cargo.toml`
- [ ] Set `repository`, `readme`, `keywords`, `categories` and
      `rust-version = "1.88"`
- [ ] `cargo fmt` the two files
- [ ] A workflow: `cargo fmt --check`, `cargo clippy --all-targets -- -D
      warnings`, `cargo test`, on Linux and macOS, and `cargo check` on the
      declared minimum toolchain
- [ ] Commit `docs/graql-comparison.md`, or remove the three links and
      fold a short comparison into the README; a link check in CI
- [ ] `BENCHMARKS.md` with each bench's numbers for one reference machine,
      updated when a change affects them
- [ ] A README note on the TypeDB name, or a decision to rename
