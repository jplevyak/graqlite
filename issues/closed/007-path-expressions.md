# Path expressions

**Status:** closed (decision: B, with A also accepted) · **Size:** large

## Problem

GraQL has one path form, `s p++ o`: one or more links with a single
predicate. SPARQL has property paths (`p*`, `p+`, `p?`, `^p`, `p/q`, `p|q`,
`!p`). GQL and Cypher have quantifiers (`{1,3}`), path modes and shortest
paths, and return paths as values. Gremlin has `repeat()` with `path()`.
Common questions GraQL can't ask today:

- friends of friends (a sequence);
- managed by, directly or not, including oneself (zero or more);
- parent or guardian (alternatives);
- who points at me (inverse);
- within three hops (bounded);
- the shortest chain between two people, and what it is.

## Design

Path expressions go in predicate position, in parentheses, with SPARQL's
operators:

```text
?x (knows*) ?y            -- zero or more
?x (knows+) ?y            -- one or more (p++ stays, meaning the same)
?x (^parent) ?y           -- inverse: ?y parent ?x
?x (parent | guardian) ?y -- alternatives
?x (knows{1,3}) ?y        -- bounded
?x (knows / likes) ?y     -- sequence (see below)
```

A predicate position doesn't take parentheses today, so this is unambiguous.
Inside them:

- `?` would be read as a variable, so zero-or-one is written `{0,1}`.
- `+` alone is free: it is a predicate modifier in the grammar ("must
  include"), but only outside parentheses.

### Decision needed: sequences

`/` starts a key (`/alice`), so `knows/likes` lexes as `knows` followed by
the key `/likes`.

| Option | Example | Notes |
| ------ | ------- | ----- |
| **A. `/` with spaces** | `(knows / likes)` | Familiar from SPARQL. `/likes` with no space would be a key, which is a subtle trap. |
| **B. Reinterpret keys** | `(knows/likes)` | Inside a path expression, the parser reads a key token `/a/b` as "then `a`, then `b`". Reads like SPARQL. The lexer stays context-free. |
| **C. Another operator** | `(knows then likes)`, `(knows ~ likes)` | No ambiguity, but unfamiliar. |

Recommendation: **B**, with A also accepted. Keys never appear inside path
expressions (predicates are values, or variables).

### The engine

- **Compiling:** path expressions compile to a small automaton over
  predicates (with inverse edges). Evaluate by breadth-first search over
  pairs of entity and automaton state, from whichever end is bound, as `p++`
  does now from its own ends. Without a quantifier, a path expression lowers
  to plain joins instead of a search.
- **Zero-length paths** (`*`, `{0,n}`) match `?x` to itself for every
  candidate. With both ends unbound that means every entity, which should be
  an error, as in many SPARQL engines, unless one end is bound or limited
  elsewhere.
- **Bounded lengths** track the depth in the search.
- **Estimates:** the planner uses the existing statistics per predicate,
  multiplied along the path.
- **Shortest paths and paths as values** (`shortest (knows+)` binding
  `?path`) need the computed values of [006](006-aggregates.md) and
  [009](009-expressions-and-literal-types.md): a path is a list of ids. That
  is a later phase.
- **Standing queries** for paths are as hard as for `p++` is today. Check
  what the existing code supports, and extend it.

## As implemented

- **Syntax:** path expressions in predicate position:
  - `^p`; `p / q`; `p | q`; `p*`, `p+`, `p{m}`, `p{m,}`, `p{m,n}`; and `++`
    after a parenthesized group.
  - The predicates are values (`a` is `type`, and words, numbers and quoted
    strings work).
  - **Sequences:** option **B**. In a path, a key token `/a/b` is read as
    `/ a / b`, so `knows/likes` is a sequence. A spaced `/` (option A) also
    works, and is how paths print.
- **The engine:** `PathPattern` gains `expr: Option<Arc<PathExpr>>`, so it is
  no longer `Copy`. `p++` is unchanged, including a variable predicate.
  - `query::paths` compiles a `PathExpr` to a Thompson automaton. Bounded
    repetitions are expanded into copies, up to 64.
  - It searches breadth-first over pairs of an entry and a state. It
    searches forward from a known subject, backward (the automaton of the
    inverse) from a known object, or from every subject and object of the
    expression's predicates when neither end is known. Neighbor lookups are
    cached.
  - Each pair of ends matches once.
- **Lowering:** a chain of steps and inverses (no `|` and no repetition)
  lowers to joins through internal variables named `[path n]`, so it repeats
  a pair once for each chain to it, like any join. Any other expression goes
  to the engine.
- **Empty paths:** an expression that may be empty (`*`, `{0,…}`) with both
  ends unknown is an error when it runs. The planner estimates that case as
  enormous, so it orders the path after something that binds an end.
- **Estimates:** fan-outs from the predicates' statistics, multiplied along
  sequences, summed over alternatives, and a few hops for repetitions.
- **Calls and paths:** a word followed by parentheses was always a call of a
  rule. Now the parser looks past the matching `)`: if a node follows, it
  is a path from a value (`ann (knows+) ?x`); otherwise it is a call
  (`friend(?x)`).
- **Saved rules:** their reference links now include the predicates of
  their path expressions.
- **Not done:** shortest paths, paths as values, and negated property sets
  (`!p`). These need computed values (009), or are rarely used.
- **Tests:**
  - the automaton (each operator, cycles, searching backward);
  - the random query test, which now generates path expressions and checks
    every plan against a brute force of relational composition, asserting
    that more than 20 of them matched;
  - GraQL behaviour and errors, and round trips;
  - `examples/10-paths.graql`.

## Tasks

- [x] Parse path expressions in predicate position; `Pattern::PathExpr`; printing
- [x] Compile to an automaton; search from either end; bounded depth; inverse
- [x] Lower expressions with no quantifier to joins
- [x] Planner estimates
- [x] Zero-length semantics and the both-ends-unbound error
- [ ] Later: shortest paths, and paths as values
- [x] Tests, an example, and docs
