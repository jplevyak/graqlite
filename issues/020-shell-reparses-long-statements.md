# The shell re-parses a growing statement on every line

**Status:** open · **Size:** small · **Related:** [016](closed/016-graql-gaps.md) (the grammar `is_incomplete` follows)

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

- [ ] Skip the completeness check for lines that end in a continuation
      token (`shell.rs` and the `Validator` in `main.rs`)
- [ ] Lex incrementally: keep the chunk's tokens and lex only the new line
- [ ] A test in `tests/cli.rs`: a script of one statement over 10 000 lines
      runs in well under a second
- [ ] Note in the README that a blank line, not the end of a line, runs a
      statement in a script (it does, but the cost of the alternative was
      invisible until now)
