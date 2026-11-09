# The shell re-parses a growing statement on every line

**Status:** closed · **Size:** small · **Related:** [016](016-graql-gaps.md) (the grammar `is_incomplete` follows)

## Problem

When the shell reads a script (`-f`, `\i`, or standard input) or a
statement typed over several lines, it appends each line to the current
chunk and asks `graql::is_incomplete` whether the chunk is a complete
statement (`src/bin/graqlite/shell.rs:221`, and `main.rs:380` for the
line editor). `is_incomplete` lexes and fully parses the chunk every
time, so a statement of `n` lines is parsed `n` times: quadratic in its
length.

A single `insert` of 4 000 triples, one subject per line, which is how the
examples are written:

| | Time |
|---|---:|
| Through the shell, 4 000 lines | 21.3 s |
| Through the shell, the same text on one line | 0.15 s |
| `graql::parse` then `graql::run`, from a program | 0.18 s |

The engine is not at fault; the shell is. Loading data is the most natural
use of a long statement, so this is the first thing a new user hits.

## Design

Two independent fixes, either enough on its own:

1. **Check only when the line could end a statement.** A line that ends in
   `.`, `;`, `,`, `and`, `or` or an open parenthesis continues the
   statement whatever follows (the README says as much), so the check can
   be skipped for it. Most lines of a long `insert` end in `.` or `;`, so
   the check runs once per statement rather than once per line. A blank
   line still runs the chunk, as it does today.
2. **Keep the lexer's tokens between lines.** Lex only the new line, append
   its tokens to the chunk's, and parse the tokens. Parsing is then linear
   per check, and the checks together are quadratic only in the number of
   tokens, not of characters; combined with 1 they are linear.

Either way, `is_incomplete` should take the tokens, or a token-producing
reader, rather than text, so that the shell and the line editor's
validator share one implementation.

## Tasks

- [x] Skip the completeness check for lines that end in a continuation
      token (`shell.rs` and the `Validator` in `main.rs`): done otherwise,
      see below
- [x] ~~Lex incrementally: keep the chunk's tokens and lex only the new
      line~~: not needed
- [x] A test in `tests/cli.rs`: a script of one statement over 5 000 lines
      (see below)
- [x] Note in the README that a blank line, not the end of a line, runs a
      statement in a script

## As implemented

The cause was simpler than the design supposed: `Shell::script` parsed
the chunk at every line, but used the answer only when the line was blank
or a backslash command, the only lines that can end the chunk; every
other line was appended whatever it said. Now it parses only then, so a
script costs a parse per blank line or command: linear in its length.

- The line editor's validator (`continues` in `main.rs`) already answered
  a line ending in `.` or `;` without parsing, so typing was mostly fine;
  it is unchanged.
- Lexing incrementally isn't needed: parsing happens once per blank line
  or command in a script, so only a statement with blank lines inside it
  is parsed more than once.
- Measured, the issue's 4 000-line `insert` (release): 13.2 s before,
  0.10 s after, the same as on one line, from a file or piped.
- `a_long_statement_is_parsed_once` (`tests/cli.rs`) runs one of 5 000
  lines through the shell in a debug build: 1.8 s, against more than two
  minutes before (it timed out); it allows 15 s, for slow machines.

## Closed

Closed by the commit parsing a script's chunk only where it can end.

