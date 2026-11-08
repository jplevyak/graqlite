//! GraQL: a textual query language for graphs compiled to [`Query`] values.
//!
//! A pattern is written as in Turtle: a subject, then predicates and their
//! objects, `;` between predicates of the subject, `,` between objects of a
//! predicate, and `.` between subjects.
//!
//! ```
//! use graqlite::graph::{Graph, Options};
//! use graqlite::graql::{self, Output};
//!
//! let mut g = Graph::open(None, Options::default()).unwrap();
//! graql::execute(&mut g, r#"
//!     insert /alice knows /bob, /carol ; age 30 .
//!            /bob knows /carol ; age 25 .
//!            /carol age 35
//! "#).unwrap();
//! let out = graql::execute(&mut g, r#"
//!     select ?who, ?age by ?age
//!     where /alice knows ?who . ?who age ?age . ?age < 30
//! "#).unwrap();
//! let Output::Rows { rows, .. } = &out[0] else { panic!() };
//! assert_eq!(rows.len(), 1);
//! assert_eq!(g.data(rows[0][1]).as_deref(), Some(&b"25"[..]));
//! ```
//!
//! # Statements
//!
//! - `select [distinct] ?x, ?y | * [having ...] [by ?x [desc], ...] [limit
//!   n] [offset n] [as of id] where pattern` returns rows of the variables (all of
//!   them, for `*`), ordered by the data of the values of the `by` variables,
//!   ascending or `desc`ending: numbers by value, then other data, then
//!   entries without data by id.  `limit` and `offset` (in either order)
//!   keep some of the rows, after ordering; `limit` limits only on predicates.
//!   As in SQL, `by`, `limit` and `offset` may instead end the statement,
//!   after the pattern: `select ?c where ?p city ?c by ?c limit 3`.  A
//!   limit ending the pattern is the statement's, and a nested limit there
//!   is written in parentheses (below).  Each goes in one place or the
//!   other.
//! - Aggregates: a column may be `count(?x)`, `count(distinct ?x)`,
//!   `count(*)`, `sum`, `avg`, `min`, `max` or `sample`, named with `as ?n`.
//!   Rows are grouped by the other columns (one group if there are none, even
//!   of no rows), and the results are an [`Output::Table`] of [`Cell`]s.
//!   `having c1 and c2 ...` keeps the groups whose comparisons of columns
//!   and constants hold, and `by` and `limit` then apply to the groups.
//!   Unbound values are skipped, except by `count(*)`; `sum` and `avg` read
//!   numbers from data.
//! - `verify [as of id] pattern` returns whether the pattern has a match.
//! - `lookup [as of id] pattern [where P]` returns the graph of the
//!   pattern's matches: the links they use (not those of negations, nor of
//!   `P`, which only constrains the matches), and the nodes they bind that
//!   aren't in those links.
//! - `as of id` sees the graph as it was when the entry with that id was
//!   appended (ids increase as entries are appended, so an id is a time),
//!   rules included.  `as of "2026-10-31T12:30Z"` sees it as of the last
//!   commit at or before an ISO 8601 time, UTC unless it gives an offset
//!   (see [`Graph::id_at_time`]).
//! - `insert template [where pattern]` links the template's triples, once,
//!   or for each match of the pattern, creating the nodes, values and keys
//!   the template names; a blank node in it is a new node each time.
//! - `delete template [where pattern]` deletes them.
//! - `rule name(?x ?y) pattern` defines the relation `name` as the values
//!   of its head variables in each match of the pattern, and saves the
//!   definition in the graph (see [Saved rules](#saved-rules)).  Several
//!   rules may define a relation, and rules may call themselves and each
//!   other, but not within a negation of themselves.
//! - `delete rule name` deletes every definition of the rule `name`.
//! - `index vector on P dims N [metric M] [m X] [ef Y]` declares a vector
//!   index of `P`'s objects, saved in the graph (see
//!   [`Graph::create_vector_index`]): a live link with `P` must then have
//!   a vector of `N` dimensions, and `nearest` searches by its metric
//!   (cosine, unless given) when given none.  Declaring it again with other
//!   parameters replaces it; `delete index vector on P` drops it.
//!
//! The changes are not committed.
//!
//! # Saved rules
//!
//! Rules are saved in the graph as the text of each definition, written
//! canonically (see [`RuleDef`]'s `Display`), so they commit and roll back
//! with the data, travel in snapshots, and are seen by every [`Session`] on
//! the graph.  Defining a rule saved already does nothing.  Each definition
//! also has reference links to the values and keys it names and the rules
//! it calls, to find the rules that use something ([`rules_using`]).  A query
//! includes only the rules it calls, directly or not, so a rule that cannot
//! be evaluated affects only the queries that call it; saved text that no
//! longer parses is left out, and reported by [`Session::skipped_rules`].
//! It uses the names [`RULES_KEY`], [`RULE_PREDICATE`], [`USES_PREDICATE`],
//! [`CALLS_PREDICATE`] and [`REFERS_TO_KEY_PREDICATE`].
//!
//! # Patterns
//!
//! - `?x` is a variable, `?` a new one; `_:x` and `[ predicates ]` are blank
//!   nodes, variables of their own, and `[ :?x predicates ]` names one.
//! - `/a/b` is the node with that key (see [`Graph::node_by_key`]), and
//!   `#123` the entry with that id.
//! - `$name` is a parameter, replaced by its value (a key, a value, or an
//!   id; see [`Param`]) before the statement runs: see
//!   [`Session::run_with`].  Or a statement can be planned once, its
//!   parameters given values each time it runs: see [`Session::prepare`].
//!   Rules can't have parameters.
//! - An identifier, a "string" or a number is the value with that data, and
//!   `a`, as a predicate, the value `type`.
//! - `p and q` both hold, `p or q` either does (both must bind the same
//!   variables), `not p` does not hold, and parentheses group.
//! - A negated subject (`not s p o`) or predicate (`s not p o`) is a
//!   negated pattern.
//! - A predicate needn't have an object (`?s type` is `?s type ?`), and a
//!   subject needn't have predicates (`lookup ?subjects`): it binds nothing
//!   if the rest of the statement binds it, and else every node in a link.
//!   Not in templates.
//! - `s << p o` reverses a predicate (`o p s`) and `s >> p o` keeps it
//!   forward; after objects, either continues from the last of them, so
//!   that `?d << directed_by ?f >> starring ?a` is `?f directed_by ?d .
//!   ?f starring ?a`.  `;` returns to the first subject.  `s !p o` reverses
//!   one predicate (MCL's convention).
//! - `+` after a predicate's objects makes it mandatory, the default; `-`
//!   optional.  `- pattern` makes its predicates optional unless marked
//!   `+`, and `+ pattern` mandatory unless marked `-` (asserted triples are
//!   neither).
//! - Nested limits: `limit n [offset m]` after a predicate's objects limits
//!   its object variable (or its subject, if the object is a constant), and
//!   `?v limit n` limits `?v`: the first `n` values, least first, within
//!   each value of the variables limited before (`lookup ?people limit 10
//!   and ?p type ?t limit 10`: at most 10 types for each of 10 people).  At
//!   the top of a statement's pattern (see [`Query::limit_values`]), and
//!   applied to the matches, before aggregates, `by` and the statement's
//!   `limit`.  In a `select`, a limit ending the pattern is the
//!   statement's, so a nested one is followed by more pattern or in
//!   parentheses: `select ?c where ( ?p city ?c limit 3 )` keeps the rows
//!   of the first three cities.  `\explain` shows each as `nested limit`.
//! - An object in parentheses is a test of it, its left side left out:
//!   `?p mass (< 40)`, `?p name (~"Bob.*")` (a regular expression, anywhere
//!   in the text: the function `matches`), `?p name (prefix("Bi"))` (a test
//!   function, the object its first argument), `?p mass (40)` (equal), and
//!   `( :?m > 100 )` names the object.  A blank node can be a comparison:
//!   `?m spouse [ :?other != /ann ]`.
//! - Comparisons are subjects: `?x = ?y` and `?x != ?y` compare ids, which
//!   for values is their data; `<`, `<=`, `>` and `>=` compare data,
//!   numerically if both are numbers.
//! - `s p++ o` holds where a chain of one or more `p` links joins `s` to
//!   `o`.
//! - A path expression, in parentheses in predicate position, matches the
//!   chains of links it describes: `^p` follows `p` backward, `p / q` is a
//!   sequence, `p | q` alternatives, and `p*`, `p+`, `p{m}`, `p{m,}` and
//!   `p{m,n}` repetitions, as in SPARQL (`{0,1}` for SPARQL's `p?`).  Its
//!   predicates are constants: words, strings and numbers (values),
//!   `/keys` and `#ids`.  A key is a key where a step starts: first, after
//!   `^`, `(` or `|`, or after a `/` with a space (`knows / /p/knows`).
//!   Right after a step, `/q` with no space is `/` then `q`, so
//!   `knows/likes` and `knows/p/knows` are sequences of values.  A chain of
//!   steps and inverses is matched by joining patterns; any other path by
//!   searching, from whichever end is known, matching each pair of ends
//!   once.  A path that may be empty needs one of its ends known.  A word
//!   followed by parentheses is a call of a rule, unless a node follows
//!   them: `friend(?x)` is a call, and `ann (knows+) ?x` a path.
//! - A call of a rule, `name(?x ?y)`, is a subject matching its relation.
//!   A rule's parameters can be bare names (`rule owns(who animal) { who
//!   pet animal }`), variables of the rule wherever its body names them.
//!   A *template rule*, whose body only asserts triples (`rule
//!   simple_property(?p ?g) { ?p gloss ?g! }`), defines no relation: a call
//!   of it in a template (`insert`, `assume`, a graph's `{ … }`) is its
//!   triples, its head the call's arguments, with new nodes for its blank
//!   nodes.  A rule that also matches can be called in `insert … where`,
//!   whose pattern gains its patterns.
//! - `function name(x, y) expr` defines a function of values for the
//!   session, callable in expressions, its parameters bare names in its
//!   body; a call is replaced by the body before the statement runs.
//!   Expressions can be conditions: comparisons, `&&`, `||` and `if c then a
//!   else b`; and an operator's type tag converts (`x +integer y` adds
//!   numbers, rounded to an integer; `+real`; `+string` concatenates).
//! - `test ["description"] R1 R2` fails unless two results (a `lookup`,
//!   `select` or `verify`, or `graph G`) are equal: tables as multisets of
//!   rows, graphs as sets of links and nodes.
//! - Expressions compute with values: `+ - * / %`, parentheses, and the
//!   functions of [`Func`] (`abs`, `round`, `str`, `number`, `length`,
//!   `upper`, `lower`, `contains`, `starts`, `ends`, `concat`, `bound`, `id`,
//!   `when`, `author`, `time`, `iso`, `vector`, `distance`).  Values are
//!   stored untyped, and
//!   computation is typed: text that is a number is a number, and an entry
//!   is its data (or key) as text.  A value that can't be computed is
//!   unbound, which no comparison holds for.  A comparison may compare
//!   expressions (`?p * ?q > 100`), a test function may be a subject
//!   (`contains(?n, "a")`), and `let ?v = expr` names an expression for
//!   later ones; `?v` can't be matched in a triple.  A select column may be
//!   an expression (`upper(?n) as ?name`), and an aggregate's argument too.
//!   An `insert … where` template may use a `let`'s variable: its value is
//!   written as a value.  Write `/` with spaces in an expression: `/2` is a
//!   key.
//! - Vectors (see [`vector`](crate::vector)) are values too: `vector(x)`
//!   makes one from text such as `"[0.1, 0.2]"` (or is the vector `x` is),
//!   `str` writes one as text, and `distance(a, b[, metric])` measures
//!   between two by `"cosine"` (the default), `"l2"` or `"dot"`, nearer
//!   smaller: `insert ?d embedding ?v where ?d text ?t . let ?v =
//!   vector(?t)` stores some, and `?d embedding ?v . distance(?v,
//!   vector($q)) < 0.2` finds those near another.
//! - `nearest(P q k ?v ?d [metric])` binds `?v` to each of the `k` values
//!   of `P`'s live links nearest the vector `q` (a constant, `$param`, or a
//!   variable the pattern binds), and `?d` to its distance, by cosine (or
//!   `l2`, `dot`); the rest of the pattern joins them: `select ?doc, ?d by
//!   ?d where nearest(embedding $q 10 ?v ?d) . ?doc embedding ?v ; writer
//!   /ann` (see [`query::Nearest`](crate::query::Nearest)).  It is the `k`
//!   nearest of all, then joined: fewer, if the rest of the pattern keeps
//!   fewer.  With a vector index on `P` (below), it is the index's search,
//!   approximate; else exact, a scan of the links.
//! - `<< s p o >>` is a link node: the live link `s p o`, as a node, to
//!   make statements about links (`<< /a knows /b >> since 2019`).  Unlike
//!   an RDF-star quoted triple, the link must be there.  `<< :?l s p o >>`
//!   also binds `?l` to the link (or `<< :_:x s p o >>` labels it), a link
//!   node alone is a subject matching its link, and link nodes nest.  In an
//!   `insert` template it links its triple if needed; in a `delete`
//!   template it deletes it.
//! - `optional ( p )` extends each row by each match of `p`, or keeps it
//!   with the variables only `p` binds unbound if it has none (SPARQL's
//!   `OPTIONAL`).  The optional patterns of a group are joined after the
//!   rest of it, wherever they are written.  A comparison with an unbound
//!   variable fails, and a negation of one holds.  The `-` modifier,
//!   after a predicate's objects (`?p email ?e -`), is short for
//!   an optional predicate.
//! - `( p ) as of id` sees `p` as of an id, and the rest of the pattern
//!   now.  It can't contain calls of rules, which are evaluated once per
//!   query: give the statement `as of` instead.
//! - Subgraphs (see [`graph`'s](crate::graph#subgraphs)): `insert T into g
//!   [where P]` adds the template's links to subgraph `g` (and the main
//!   graph), `delete T from g [where P]` removes them from `g`, leaving them
//!   live, and `select … from g`, `lookup … in g`, `verify … in g` and `( p )
//!   in g` match within `g`.  After a triple's object come its author and
//!   subgraph, `s p o author subgraph` (`?` for any): in a pattern what the
//!   link must have, binding a variable, and in a template what it is given.
//!   A subgraph is named like a node: a word or text is a value, or a `/key`,
//!   `#id` or `$param`.  Paths aren't searched within a subgraph.
//! - `s p o!` in a lookup's pattern asserts the triple rather than matching
//!   it: the lookup's graph has it, for each distinct binding of its
//!   variables, as a derived link (or its stored link, if it is live), and
//!   nothing is written.  A blank node object is a new node for each, with
//!   its own triples asserted too (`?p record [ animal ?pet ]!`).  `@?x`
//!   after it is its scope: instantiated for each value of `?x` too, and not
//!   where `?x` is unbound (`?p pet_lover true! @?pet`, only for those with
//!   a pet).  In a rule's body, `!` asserts for each tuple the rule derives,
//!   with the head's variables, when a lookup calls it.  Not within `or`,
//!   `not`, `optional`, `as of` or `in`, nor in other statements (see
//!   [`query::Assertion`](crate::query::Assertion)).
//! - Aggregates derive links: `COUNT([distinct] s p o)` a link `s p n` from
//!   each subject to the count of its rows binding `o` (or of their
//!   distinct values), and so `SUM`, `AVG`, `MIN`, `MAX` and `SAMPLE`; and
//!   the rankings `ORDER(s p key [desc])` (or `RANK`), `DENSE_RANK`,
//!   `ROW_NUMBER` and `REVERSE_ORDER`, a link from each subject to its rank,
//!   from 0, by its best key, as SQL's window functions rank (missing keys
//!   last).  `@?x` after one is its scope, SQL's `PARTITION BY`.  The rest
//!   of the pattern can match their links (`?m order (= 0)`): an aggregate
//!   folds the rows of the pattern without those patterns, those of later
//!   aggregates, and what uses variables only they bind.  Counts and ranks
//!   not in the graph are ephemeral values (see
//!   [`Graph::ephemeral_value`]), written as values when inserted, held by
//!   the statement's query and its output, and freed with them.  In a
//!   statement's pattern or a rule's body, not within `or`, `not`,
//!   `optional`, `as of` or `in` (see
//!   [`query::LinkAggregate`](crate::query::LinkAggregate)).
//! - `set name = value` names a constant (a key, id, number, string or
//!   boolean) or a graph for the session: an identifier it names is its
//!   value, where the session's statements are parsed (see
//!   [`parse_with_symbols`]; a string never is).
//! - Graphs (see [`Overlay`]): `database`, the
//!   stored graph; a name `set` gave one; `{ triples }`, a graph of its own
//!   links, of which a negated triple deletes a fact beneath; `base:G`,
//!   `G`'s links over `base`'s; and `( lookup … )`, a lookup's graph.  `set
//!   name = graph G` names one; `select … from graph G where …` and `lookup`
//!   / `verify … in graph G` read one; `insert graph G [into g]` writes one.
//!   A graph's values, keys and nodes not in the database are ephemeral
//!   (see [`Graph::ephemeral_value`]), held while it is, and made when it
//!   is inserted.
//! - `data G` sets the graph the session reads and writes: an overlay, such
//!   as `{}` for scratch data never written, or `database` again.
//!   `default G` sets the graph every graph read is over, such as a schema;
//!   `default {}` clears it.  `assume T [in g]` adds links over every graph
//!   read, never written, until the transaction ends
//!   ([`Session::end_transaction`]: the shell's `\commit` and
//!   `\rollback`), or the session, outside one.  `maintain` refuses them.
//! - `insert graph ( lookup … ) [into g]` links a lookup's links, those it
//!   derives by rules (predicate: the rule's name) and aggregates, and those
//!   it asserts,
//!   into `g`: a value or key made if not in the graph, and a new node for
//!   each blank.
//! - `maintain T into g where P` keeps subgraph `g`, for the rest of the
//!   session, as the template's links for each row of `P`: brought up to
//!   date by [`Session::update`], which the shell runs after each commit.
//!   It removes what else is in `g` to start.  `P` must be a pattern a
//!   [`StandingQuery`](crate::query::StandingQuery) can keep.
//! - `author a` and `subgraph g` set the session's author and subgraph,
//!   which later inserts give their links unless they name their own; and a
//!   `//key` is the key `/u/<id of the session's author>/key`.
//!
//! Potential features are reported as unsupported:
//! the other predicate modifiers (`*`, `/`), `@` after a call of a rule,
//! `let … in`, `implies`, `else` after a pattern, lists, the bit operators,
//! sequences and the other statements.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use crate::graph::{self, EphemeralHold, Graph, Holds};
use crate::query::ResultGraph;
use crate::query::aggregate::{Cell, Function};
use crate::query::expr::Expr as QExpr;
use crate::query::{
    Cmp, Filter, Group, Overlay, PathExpr, Query, Relation, SortKey, Term, Var, compare_data,
};
use crate::record::LinkTime;
use crate::times::parse_time;
pub use expr::{BinOp, Expr, Func, function_names};

/// Errors from parsing or running GraQL.
#[derive(Debug)]
pub enum Error {
    /// The text is not GraQL, at `line` and `column` (from 1).
    Syntax {
        line: usize,
        column: usize,
        message: String,
    },
    /// The text uses something not supported, at `line` and `column`.
    Unsupported {
        line: usize,
        column: usize,
        what: String,
    },
    /// The statement cannot run, such as deleting a rule that is not
    /// defined.
    Invalid(String),
    Graph(graph::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax {
                line,
                column,
                message,
            } => write!(f, "{line}:{column}: {message}"),
            Error::Unsupported { line, column, what } => {
                write!(f, "{line}:{column}: {what} is not supported")
            }
            Error::Invalid(message) => write!(f, "{message}"),
            Error::Graph(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Graph(e) => Some(e),
            _ => None,
        }
    }
}

impl From<graph::Error> for Error {
    fn from(e: graph::Error) -> Self {
        Error::Graph(e)
    }
}

/// A token of GraQL text.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A keyword, in lower case.
    Keyword(&'static str),
    /// A variable's name, without the `?`; empty for `?` alone.
    Var(String),
    /// A blank node's label, without the `_:`.
    Label(String),
    /// A key, with its leading `/`.
    Key(String),
    /// `#123`: the entry with that id.
    Id(u64),
    /// `$name`: a parameter, without the `$`.
    Param(String),
    Ident(String),
    Text(String),
    Number(String),
    Punct(&'static str),
    End,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Keyword(k) => write!(f, "'{k}'"),
            Token::Var(v) => write!(f, "?{v}"),
            Token::Label(l) => write!(f, "_:{l}"),
            Token::Key(k) | Token::Ident(k) | Token::Number(k) => write!(f, "{k}"),
            Token::Id(id) => write!(f, "#{id}"),
            Token::Param(p) => write!(f, "${p}"),
            Token::Text(t) => write!(f, "{t:?}"),
            Token::Punct(p) => write!(f, "'{p}'"),
            Token::End => write!(f, "the end"),
        }
    }
}

const KEYWORDS: &[&str] = &[
    "default", "data", "author", "subgraph", "set", "insert", "delete", "assume", "select",
    "lookup", "verify", "rule", "function", "test", "where", "from", "into", "in", "graph",
    "table", "not", "and", "or", "if", "then", "else", "let", "as", "list", "of", "implies",
    "limit", "offset", "by", "integer", "real", "string", "distinct", "boolean", "quit", "true",
    "false", "optional", "having", "maintain", "index",
];

/// Longest first, so that the longest punctuation matches.
const PUNCT: &[&str] = &[
    "||", "&&", "<=", "==", ">=", "!=", "<<", ">>", "++", "<", ">", "=", "+", "-", "*", "/", "%",
    "!", "&", "|", "^", "~", ":", "@", "{", "}", "(", ")", "[", "]", ".", ",", ";",
];

/// A token and where it starts.
#[derive(Debug, Clone)]
struct Spanned {
    token: Token,
    line: usize,
    column: usize,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn lex(text: &str) -> Result<Vec<Spanned>> {
    let chars: Vec<char> = text.chars().collect();
    let (mut i, mut line, mut column) = (0, 1, 1);
    let mut out = Vec::new();
    let advance = |i: &mut usize, line: &mut usize, column: &mut usize, n: usize| {
        for _ in 0..n {
            if chars[*i] == '\n' {
                *line += 1;
                *column = 1;
            } else {
                *column += 1;
            }
            *i += 1;
        }
    };
    let error = |line, column, message: String| Error::Syntax {
        line,
        column,
        message,
    };
    while i < chars.len() {
        let c = chars[i];
        let rest = &chars[i..];
        let starts = |s: &str| rest.iter().take(s.len()).copied().eq(s.chars());
        let (start_line, start_column) = (line, column);
        // Whitespace and comments.
        if c.is_whitespace() {
            advance(&mut i, &mut line, &mut column, 1);
            continue;
        }
        if starts("/*") {
            let end = (2..rest.len().saturating_sub(1))
                .find(|&k| rest[k] == '*' && rest[k + 1] == '/')
                .ok_or_else(|| error(line, column, "unterminated comment".into()))?;
            advance(&mut i, &mut line, &mut column, end + 2);
            continue;
        }
        if starts("--")
            && rest
                .get(2)
                .is_none_or(|&c| c == ' ' || c == '\t' || c == '\r' || c == '\n')
        {
            let end = rest.iter().position(|&c| c == '\n').unwrap_or(rest.len());
            advance(&mut i, &mut line, &mut column, end);
            continue;
        }
        let take_while = |from: usize, f: fn(char) -> bool| {
            from + rest[from..].iter().take_while(|&&c| f(c)).count()
        };
        let (token, len) = if c == '?' {
            let end = if rest.get(1).is_some_and(|&c| is_ident_start(c)) {
                take_while(1, is_ident)
            } else {
                1
            };
            (Token::Var(rest[1..end].iter().collect()), end)
        } else if c == '#' && rest.get(1).is_some_and(|c| c.is_ascii_digit()) {
            let end = take_while(1, |c| c.is_ascii_digit());
            let digits: String = rest[1..end].iter().collect();
            let id = digits
                .parse()
                .map_err(|_| error(line, column, format!("#{digits} is not an id")))?;
            (Token::Id(id), end)
        } else if c == '$' && rest.get(1).is_some_and(|&c| is_ident_start(c)) {
            let end = take_while(1, is_ident);
            (Token::Param(rest[1..end].iter().collect()), end)
        } else if starts("_:") {
            let end = take_while(2, is_ident);
            (Token::Label(rest[2..end].iter().collect()), end)
        } else if c == '/' && rest.get(1).is_some_and(|&c| is_ident(c) || c == '/') {
            let end = take_while(1, |c| is_ident(c) || c == '/');
            (Token::Key(rest[..end].iter().collect()), end)
        } else if c == '"' {
            let mut s = String::new();
            let mut k = 1;
            loop {
                match rest.get(k) {
                    None => return Err(error(line, column, "unterminated string".into())),
                    Some('"') => break,
                    Some('\\') => {
                        let escaped = rest
                            .get(k + 1)
                            .ok_or_else(|| error(line, column, "unterminated string".into()))?;
                        s.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            &c => c,
                        });
                        k += 2;
                    }
                    Some(&c) => {
                        s.push(c);
                        k += 1;
                    }
                }
            }
            (Token::Text(s), k + 1)
        } else if c.is_ascii_digit() {
            let mut end = take_while(0, |c| c.is_ascii_alphanumeric());
            if rest.get(end) == Some(&'.') && rest.get(end + 1).is_some_and(|c| c.is_ascii_digit())
            {
                end = take_while(end + 1, |c| c.is_ascii_digit());
            }
            (Token::Number(rest[..end].iter().collect()), end)
        } else if is_ident_start(c) {
            let end = take_while(0, is_ident);
            let word: String = rest[..end].iter().collect();
            let lower = word.to_ascii_lowercase();
            match KEYWORDS.iter().find(|&&k| k == lower) {
                Some(k) => (Token::Keyword(k), end),
                None => (Token::Ident(word), end),
            }
        } else if let Some(p) = PUNCT.iter().find(|p| starts(p)) {
            (Token::Punct(p), p.len())
        } else {
            return Err(error(line, column, format!("unexpected character {c:?}")));
        };
        advance(&mut i, &mut line, &mut column, len);
        out.push(Spanned {
            token,
            line: start_line,
            column: start_column,
        });
    }
    out.push(Spanned {
        token: Token::End,
        line,
        column,
    });
    Ok(out)
}

/// A node of a pattern, before it is resolved against a graph.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Var(String),
    /// A blank node: a variable of its own.
    Blank(String),
    Key(String),
    /// An identifier, string or number: a value's data.
    Value(String),
    /// `#123`: the entry with an id, bound rather than found by name.
    Id(u64),
    /// `$name`: a parameter, replaced by its value before the statement
    /// runs (see [`Session::run_with`]), or a query parameter given its
    /// value when it runs (see [`Session::prepare`]).
    Param(String),
    /// An identifier that `SET` named: replaced by its value before the
    /// statement runs.
    Symbol(String),
}

/// A graph, as GraQL names one: see [`query::Overlay`](crate::query::Overlay).
#[derive(Debug, Clone, PartialEq)]
pub enum GraphExpr {
    /// A graph `SET` named, or `database`: the stored graph.
    Name(String),
    /// `{ triples }`: a graph of its own links (a negated triple is a
    /// deleted one).
    Literal(Vec<Pattern>),
    /// `base:G`: `G`'s links over the graph named `base`.
    Over(String, Box<GraphExpr>),
    /// `( lookup … )`: a lookup's graph.
    Lookup(Box<Statement>),
}

/// What a `test` compares: a statement's result, or a graph.
#[derive(Debug, Clone, PartialEq)]
pub enum TestOperand {
    Statement(Box<Statement>),
    Graph(GraphExpr),
}

/// The value `SET` names.
#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    Node(Node),
    Graph(GraphExpr),
}

/// A path expression, before it is resolved against a graph: see
/// [`query::PathExpr`](crate::query::PathExpr).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// A link with the predicate that is the value with this data.
    Pred(String),
    /// A link with the predicate that is the node with this key.
    Key(String),
    /// A link with the predicate that is this entry.
    Id(u64),
    Inverse(Box<Path>),
    Seq(Vec<Path>),
    Alt(Vec<Path>),
    Repeat(Box<Path>, u32, Option<u32>),
}

impl Path {
    /// Whether the path is only steps and inverses, which can be matched by
    /// joining patterns.
    fn is_chain(&self) -> bool {
        match self {
            Path::Pred(_) | Path::Key(_) | Path::Id(_) => true,
            Path::Inverse(p) => p.is_chain(),
            Path::Seq(ps) => ps.iter().all(Path::is_chain),
            Path::Alt(_) | Path::Repeat(..) => false,
        }
    }

    /// The steps the path names: its `Pred`s, `Key`s and `Id`s.
    pub fn steps<'a>(&'a self, out: &mut Vec<&'a Path>) {
        match self {
            Path::Pred(_) | Path::Key(_) | Path::Id(_) => out.push(self),
            Path::Inverse(p) | Path::Repeat(p, ..) => p.steps(out),
            Path::Seq(ps) | Path::Alt(ps) => ps.iter().for_each(|p| p.steps(out)),
        }
    }

    /// The path for the engine, with each predicate the value, key or entry
    /// in `g` (or 0, matching nothing, if there is none).
    fn resolve(&self, g: &Graph) -> PathExpr {
        match self {
            Path::Pred(p) => PathExpr::Link(g.find_value(p.as_bytes()).unwrap_or(0)),
            Path::Key(k) => PathExpr::Link(g.find_by_key(k.as_bytes()).unwrap_or(0)),
            Path::Id(id) => PathExpr::Link(*id),
            Path::Inverse(p) => PathExpr::Inverse(Box::new(p.resolve(g))),
            Path::Seq(ps) => PathExpr::Seq(ps.iter().map(|p| p.resolve(g)).collect()),
            Path::Alt(ps) => PathExpr::Alt(ps.iter().map(|p| p.resolve(g)).collect()),
            Path::Repeat(p, min, max) => PathExpr::Repeat(Box::new(p.resolve(g)), *min, *max),
        }
    }
}

/// A comparison between two nodes.
pub use crate::query::expr::Comparison;

/// A parsed pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Triple(Node, Node, Node),
    /// A triple with its author and subgraph positions, `s p o author
    /// subgraph`, either left out: in a pattern, what the link must have
    /// (`?` for any), and in a template, what it is given.
    Tagged(Node, Node, Node, Option<Node>, Option<Node>),
    /// A pattern seen within a subgraph: `( p ) in g`, and the pattern of
    /// `select … from g`, `lookup … in g` and `verify … in g`.  Its triples
    /// that name no subgraph of their own are in `g`.
    InGraph(Box<Pattern>, Node),
    /// A subject alone, with no predicates: `lookup ?subjects`.  It holds
    /// for every node in a link, or binds nothing if the node is bound by
    /// the rest of the statement.
    Node(Node),
    /// A transitive predicate: `s p++ o`.
    Path(Node, Node, Node),
    /// A path expression in predicate position: `s (knows / likes+) o`.
    PathExpr(Node, Path, Node),
    /// A triple whose link is bound to its first node, a variable or blank
    /// node: written `<< :?l s p o >>`, which as a node stands for `?l`.
    Link(Node, Node, Node, Node),
    /// A pattern seeing the graph as it was at an id or a time:
    /// `( p ) as of 120`.
    AsOf(Box<Pattern>, AsOf),
    /// A call to a rule: `name(args)`.
    Call(String, Vec<Node>),
    /// A comparison of nodes or expressions: `?a < 30`, `?p * ?q > 100`.
    Compare(Expr, Comparison, Expr),
    /// A function that is a test, as a subject: `contains(?n, "a")`.
    Test(Expr),
    /// `let ?v = expr`: `?v` is the expression, wherever it is used in an
    /// expression.  It can't be matched in a triple, as it isn't an entry.
    Let(String, Expr),
    And(Vec<Pattern>),
    Or(Vec<Pattern>),
    Not(Box<Pattern>),
    /// `optional ( p )`, or the `-` modifier: each match of `p` extends a
    /// row, or the row is kept with `p`'s own variables unbound.
    Optional(Box<Pattern>),
    /// Triples asserted rather than matched: `s p o!`, followed by the
    /// triples of a blank node object (`s p [ q r ]!`), and a scope, `@?x`.  In a lookup's pattern, or a rule's body, not within
    /// `or`, `not`, `optional`, `as of` or `in` (see
    /// [`query::Assertion`](crate::query::Assertion)).
    Assert(Vec<Pattern>, Option<String>),
    /// A nested limit: `?v limit n [offset m]`, the first `n` values of
    /// `?v` (after `m`), in order, within each value of the variables of the
    /// limits before it (see [`Query::limit_values`]).
    LimitValues {
        var: String,
        limit: usize,
        offset: usize,
    },
    /// An aggregate deriving links: `COUNT([distinct] s p o) [@?x]`, and
    /// `ORDER(s p key [desc])` and the other rankings; its name upper case,
    /// as written (see [`query::LinkAggregate`](crate::query::LinkAggregate)).
    LinkAggregate {
        name: String,
        distinct: bool,
        triple: Box<(Node, Node, Node)>,
        descending: bool,
        scope: Option<String>,
    },
}

/// The names of the aggregates deriving links, as GraQL writes them.
const LINK_AGGREGATES: &[&str] = &[
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "SAMPLE",
    "ORDER",
    "RANK",
    "REVERSE_ORDER",
    "DENSE_RANK",
    "ROW_NUMBER",
];

/// A column of a `select`: a variable, an expression, or an aggregate.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Var(String),
    /// `expr [as ?name]`.
    Expr {
        expr: Expr,
        name: Option<String>,
    },
    /// `function([distinct] arg) [as ?name]`; `count(*)` has no `arg`.
    Aggregate {
        function: Function,
        distinct: bool,
        arg: Option<Expr>,
        name: Option<String>,
    },
}

impl SelectItem {
    /// The column's name: its variable, its `as` name, or else what is
    /// computed, as written.
    pub fn name(&self) -> String {
        match self {
            SelectItem::Var(v) => v.clone(),
            SelectItem::Expr { name: Some(n), .. }
            | SelectItem::Aggregate { name: Some(n), .. } => n.clone(),
            SelectItem::Expr { expr, name: None } => expr.to_string(),
            SelectItem::Aggregate {
                function,
                distinct,
                arg,
                name: None,
            } => {
                let arg = arg.as_ref().map_or("*".into(), |a| a.to_string());
                let distinct = if *distinct { "distinct " } else { "" };
                format!("{}({distinct}{arg})", function.name())
            }
        }
    }
}

/// A key of `by`: a variable, or a column's name, and its direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    pub var: String,
    pub descending: bool,
}

/// When `as of` sees the graph: as it was when the entry with an id was
/// appended, or at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsOf {
    Id(u64),
    /// Milliseconds since 1970: the graph as of the last commit then (see
    /// [`Graph::id_at_time`]).
    Time(LinkTime),
}

impl AsOf {
    /// The id as of which to see `g`.
    pub fn id(self, g: &Graph) -> u64 {
        match self {
            AsOf::Id(id) => id,
            AsOf::Time(t) => g.id_at_time(t),
        }
    }
}

/// A rule: its relation's name, head variables and body.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleDef {
    pub name: String,
    pub head: Vec<String>,
    pub body: Pattern,
}

impl Statement {
    /// Whether the statement only reads: a `select`, `verify`, `lookup` (of
    /// a graph, perhaps) or `test`.
    pub fn reads(&self) -> bool {
        match self {
            Statement::Select { .. }
            | Statement::Verify { .. }
            | Statement::Lookup { .. }
            | Statement::Test { .. } => true,
            Statement::From { statement, .. } => statement.reads(),
            _ => false,
        }
    }
}

impl RuleDef {
    /// Whether the rule's body only asserts triples: a *template rule*,
    /// which defines no relation, and whose calls in a template (`insert`,
    /// `assume`, a graph's `{ … }`) are its triples, its head the call's
    /// arguments.
    pub fn is_template(&self) -> bool {
        fn only_asserts(p: &Pattern) -> bool {
            match p {
                Pattern::Assert(..) => true,
                Pattern::And(ps) => !ps.is_empty() && ps.iter().all(only_asserts),
                _ => false,
            }
        }
        only_asserts(&self.body)
    }

    /// Whether the rule's body asserts triples, at its top.
    pub fn asserts(&self) -> bool {
        let mut parts = Vec::new();
        print::flatten_and(&self.body, &mut parts);
        parts.iter().any(|p| matches!(p, Pattern::Assert(..)))
    }
}

impl Pattern {
    /// Whether the pattern's last part, through `and`s, is a nested limit:
    /// one that, written last in a `select`, would be the statement's.
    pub(crate) fn ends_with_limit(&self) -> bool {
        match self {
            Pattern::LimitValues { .. } => true,
            Pattern::And(ps) => ps.last().is_some_and(Pattern::ends_with_limit),
            _ => false,
        }
    }
}

/// Removes the nested limit that ends `p`, through `and`s, returning its
/// limit and offset, if it ends with one; and the node it was of, which
/// `?v limit n` writes alone, if the pattern binds it elsewhere.
fn take_last_limit(p: &mut Pattern) -> Option<(usize, usize)> {
    /// Removes the last part of `p`, through `and`s, and any `and` it
    /// leaves empty.
    fn pop_last(p: &mut Pattern) -> Option<Pattern> {
        let Pattern::And(ps) = p else {
            return None;
        };
        let last = ps.last_mut()?;
        if matches!(last, Pattern::And(_)) {
            let popped = pop_last(last);
            if matches!(last, Pattern::And(qs) if qs.is_empty()) {
                ps.pop();
            }
            popped
        } else {
            ps.pop()
        }
    }
    fn last(p: &Pattern) -> &Pattern {
        match p {
            Pattern::And(ps) if !ps.is_empty() => last(&ps[ps.len() - 1]),
            p => p,
        }
    }
    let Pattern::LimitValues { limit, offset, var } = last(p).clone() else {
        return None;
    };
    pop_last(p);
    if let Pattern::Node(Node::Var(v) | Node::Blank(v)) = last(p)
        && *v == var
    {
        let mut uses = 0;
        print::each_node(p, &mut |n| {
            uses += usize::from(matches!(n, Node::Var(v) | Node::Blank(v) if *v == var));
        });
        if uses > 1 {
            pop_last(p);
        }
    }
    Some((limit, offset))
}

/// A call of a template rule, `name(args)`, expanded: its asserted
/// triples, and its other patterns (conditions, for a rule that also
/// matches), with the head's variables the arguments, and the body's other
/// variables and blank nodes its own (marked with `n`, a number for the
/// call).  `None` if no template rule has the name.
fn expand_call(
    rules: &[RuleDef],
    name: &str,
    args: &[Node],
    n: usize,
) -> Result<Option<(Vec<Pattern>, Vec<Pattern>)>> {
    let defs: Vec<&RuleDef> = rules.iter().filter(|r| r.name == name).collect();
    if defs.is_empty() || !defs.iter().all(|r| r.asserts()) {
        return Ok(None);
    }
    let (mut triples, mut conditions) = (Vec::new(), Vec::new());
    for (k, def) in defs.into_iter().enumerate() {
        if def.head.len() != args.len() {
            return Err(Error::Invalid(format!(
                "{name} takes {} arguments",
                def.head.len()
            )));
        }
        let mut rename = |node: &mut Node| match node {
            Node::Var(v) => match def.head.iter().position(|h| h == v) {
                Some(i) => *node = args[i].clone(),
                None => *v = format!("{v}~{n}.{k}"),
            },
            Node::Blank(b) => *b = format!("[{b}~{n}.{k}]"),
            _ => {}
        };
        let mut parts = Vec::new();
        print::flatten_and(&def.body, &mut parts);
        for p in parts {
            match p {
                Pattern::Assert(ts, _) => {
                    for t in ts {
                        let mut t = t.clone();
                        print::each_node_mut(&mut t, &mut rename);
                        triples.push(t);
                    }
                }
                p => {
                    let mut p = p.clone();
                    print::each_node_mut(&mut p, &mut rename);
                    conditions.push(p);
                }
            }
        }
    }
    Ok(Some((triples, conditions)))
}

/// Expands the calls of template rules in `statement`'s templates and
/// graphs (see [`RuleDef::is_template`]).  An `insert` or `delete` calling a
/// rule that also matches gains its patterns as conditions; elsewhere, its
/// rule must only assert.
fn expand_calls(statement: &mut Statement, rules: &[RuleDef]) -> Result<()> {
    let mut n = 0;
    let mut template =
        |t: &mut Vec<Pattern>, conditions: Option<&mut Vec<Pattern>>| -> Result<()> {
            if !t.iter().any(|p| matches!(p, Pattern::Call(..))) {
                return Ok(());
            }
            let mut out = Vec::new();
            let mut found = Vec::new();
            for p in std::mem::take(t) {
                let Pattern::Call(name, args) = &p else {
                    out.push(p);
                    continue;
                };
                n += 1;
                let Some((triples, conds)) = expand_call(rules, name, args, n)? else {
                    return Err(Error::Invalid(format!(
                        "{name} isn't a rule that asserts triples, to call in a template"
                    )));
                };
                out.extend(triples);
                found.extend(conds);
            }
            *t = out;
            match conditions {
                Some(c) => c.extend(found),
                None if found.is_empty() => {}
                None => {
                    return Err(Error::Invalid(
                        "a rule called in a graph or 'assume' can only assert triples".into(),
                    ));
                }
            }
            Ok(())
        };
    fn graphs<'s>(s: &'s mut Statement, out: &mut Vec<&'s mut Vec<Pattern>>) {
        fn graph<'e>(e: &'e mut GraphExpr, out: &mut Vec<&'e mut Vec<Pattern>>) {
            match e {
                GraphExpr::Literal(ps) => out.push(ps),
                GraphExpr::Over(_, e) => graph(e, out),
                GraphExpr::Lookup(s) => graphs(s, out),
                GraphExpr::Name(_) => {}
            }
        }
        match s {
            Statement::Set {
                value: SetValue::Graph(e),
                ..
            }
            | Statement::Data(e)
            | Statement::Default(e)
            | Statement::InsertGraph { source: e, .. } => graph(e, out),
            Statement::From { source, statement } => {
                graph(source, out);
                graphs(statement, out);
            }
            Statement::Test { operands, .. } => {
                for o in operands.iter_mut() {
                    match o {
                        TestOperand::Statement(s) => graphs(s, out),
                        TestOperand::Graph(e) => graph(e, out),
                    }
                }
            }
            _ => {}
        }
    }
    match statement {
        Statement::Insert {
            template: t,
            pattern,
            ..
        }
        | Statement::Delete {
            template: t,
            pattern,
            ..
        } => {
            let mut conditions = Vec::new();
            template(t, Some(&mut conditions))?;
            if !conditions.is_empty() {
                conditions.extend(pattern.take());
                *pattern = Some(Pattern::And(conditions));
            }
        }
        Statement::Assume { template: t, .. } => template(t, None)?,
        s => {
            let mut literals = Vec::new();
            graphs(s, &mut literals);
            for t in literals {
                template(t, None)?;
            }
        }
    }
    Ok(())
}

/// A parsed statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `rule name(?a ?b) pattern`: defines `name` for the rest of the
    /// session.
    Rule(RuleDef),
    Select {
        distinct: bool,
        /// The variables and aggregates, or `None` for all the variables.
        vars: Option<Vec<SelectItem>>,
        /// Comparisons of the results of aggregation.
        having: Vec<(Expr, Comparison, Expr)>,
        by: Vec<OrderKey>,
        pattern: Pattern,
        limit: Option<usize>,
        /// The rows to skip, after ordering.
        offset: Option<usize>,
        /// `as of`: when it sees the graph.
        as_of: Option<AsOf>,
    },
    Verify {
        pattern: Pattern,
        as_of: Option<AsOf>,
    },
    Lookup {
        pattern: Pattern,
        /// `where P`: rows that constrain the pattern's matches, whose own
        /// links aren't returned.
        where_: Option<Pattern>,
        as_of: Option<AsOf>,
    },
    /// The template is triples and links ([`Pattern::Triple`] and
    /// [`Pattern::Link`]).
    Insert {
        template: Vec<Pattern>,
        pattern: Option<Pattern>,
        /// `into g`: the subgraph the template's links are added to, unless
        /// they name their own.
        graph: Option<Node>,
    },
    Delete {
        template: Vec<Pattern>,
        pattern: Option<Pattern>,
        /// `from g`: the subgraph the template's links are removed from,
        /// leaving them live, unless they name their own.
        graph: Option<Node>,
    },
    /// `insert graph G [into g]`: a graph's links, such as a lookup's and
    /// those it derives, linked (into `g`).
    InsertGraph {
        source: GraphExpr,
        graph: Option<Node>,
    },
    /// `maintain T into g where P`: subgraph `g` kept, for the rest of the
    /// session, as the template's links for each row of `P` (see
    /// [`MaintainedSubgraph`](crate::query::MaintainedSubgraph)).
    Maintain {
        template: Vec<Pattern>,
        pattern: Pattern,
        graph: Node,
    },
    /// `author a`: the author of the links the session inserts, and whose
    /// `//keys` it names.
    Author(Node),
    /// `subgraph g`: the subgraph the session inserts links into, unless a
    /// statement or triple names one.
    Subgraph(Node),
    /// `delete rule name`: deletes every saved definition of the rule.
    DeleteRule(String),
    /// `index vector on P dims N [metric M] [m X] [ef Y]`: declares a vector
    /// index of `P`'s objects (see [`Graph::create_vector_index`]).
    IndexVector {
        predicate: Node,
        dims: usize,
        metric: crate::vector::Metric,
        m: usize,
        ef: usize,
    },
    /// `delete index vector on P`: drops `P`'s vector index.
    DeleteIndex(Node),
    /// `set name = value`: names a constant or a graph for the session.
    Set { name: String, value: SetValue },
    /// `data G`: the graph the session's statements read and write.
    Data(GraphExpr),
    /// `default G`: the graph every graph the session reads is over.
    Default(GraphExpr),
    /// `assume T [in g]`: links over every graph the session reads, until
    /// its transaction ends, never written.
    Assume {
        template: Vec<Pattern>,
        graph: Option<Node>,
    },
    /// `function name(x, y) expr`: a function of values for the session,
    /// callable in expressions, its parameters bare names in its body.
    Function {
        name: String,
        params: Vec<String>,
        body: Expr,
    },
    /// `test ["description"] R1 R2`: fails unless the two results are
    /// equal: tables as multisets of rows, graphs as sets of links and
    /// nodes.
    Test {
        description: Option<String>,
        operands: Box<[TestOperand; 2]>,
    },
    /// A `select`, `verify` or `lookup` reading a graph: `select … from
    /// graph G where …`, `lookup … in graph G`.
    From {
        source: GraphExpr,
        statement: Box<Statement>,
    },
}

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
    blanks: usize,
    /// Whether a template is being parsed, where a predicate needs an
    /// object.
    in_template: bool,
    /// Whether the last subject or predicate parsed was short (a subject
    /// alone, or a predicate with no object) and ended the input: a
    /// statement the shell should wait for more of.
    short_end: bool,
    /// The positions of the last nested limit parsed, and after it: if a
    /// `select`'s pattern ends there, the limit is the statement's.
    limit_at: (usize, usize),
    /// Whether the next call is a test of an object in parentheses, whose
    /// first argument, the object, is implied: `(starts("Bo"))`.
    implied_arg: bool,
    /// The names `SET` names: the session's, and those set before.
    symbols: HashSet<String>,
    /// The graph read by the statement being parsed, from `in graph G`.
    source: Option<GraphExpr>,
    /// Whether predicates are optional unless marked `+` (`- pattern`).
    optional_default: bool,
    /// The bare names of the rule being parsed's parameters: variables.
    rule_params: HashSet<String>,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    fn here(&self) -> (usize, usize) {
        let t = &self.tokens[self.pos];
        (t.line, t.column)
    }

    fn next(&mut self) -> Token {
        let t = self.tokens[self.pos].token.clone();
        if t != Token::End {
            self.pos += 1;
        }
        t
    }

    fn is(&self, t: &Token) -> bool {
        self.peek() == t
    }

    fn eat(&mut self, t: &Token) -> bool {
        let found = self.is(t);
        if found {
            self.next();
        }
        found
    }

    fn syntax<T>(&self, message: impl Into<String>) -> Result<T> {
        self.syntax_at(self.pos, message)
    }

    /// A syntax error at the token at `pos`.
    fn syntax_at<T>(&self, pos: usize, message: impl Into<String>) -> Result<T> {
        let t = &self.tokens[pos];
        let (line, column) = (t.line, t.column);
        Err(Error::Syntax {
            line,
            column,
            message: message.into(),
        })
    }

    fn unsupported<T>(&self, what: impl Into<String>) -> Result<T> {
        let (line, column) = self.here();
        Err(Error::Unsupported {
            line,
            column,
            what: what.into(),
        })
    }

    fn expect(&mut self, t: &Token) -> Result<()> {
        if self.eat(t) {
            Ok(())
        } else {
            self.syntax(format!("expected {t}, found {}", self.peek()))
        }
    }

    /// Whether `index vector on` starts at `pos`.
    fn index_ahead(&self, pos: usize) -> bool {
        let word = |i: usize, w: &str| matches!(self.tokens.get(pos + i), Some(t) if matches!(&t.token, Token::Ident(x) if x.eq_ignore_ascii_case(w)));
        matches!(self.tokens.get(pos), Some(t) if t.token == Token::Keyword("index"))
            && word(1, "vector")
            && word(2, "on")
    }

    /// A constant node: a value, key or id.
    fn node_constant(&mut self, what: &str) -> Result<Node> {
        match self.next() {
            Token::Ident(v) | Token::Text(v) => Ok(Node::Value(v)),
            Token::Key(k) => Ok(Node::Key(k)),
            Token::Id(id) => Ok(Node::Id(id)),
            t => self.syntax(format!("expected {what}, found {t}")),
        }
    }

    /// The rest of `index vector on P dims N [metric M] [m X] [ef Y]`.
    fn index_vector(&mut self) -> Result<Statement> {
        let predicate = self.node_constant("an index's predicate")?;
        let (mut dims, mut metric, mut m, mut ef) = (
            None,
            crate::vector::Metric::Cosine,
            crate::graph::VectorIndex::M,
            crate::graph::VectorIndex::EF,
        );
        // The options, until something else: the next statement.
        while let Token::Ident(word) = self.peek().clone() {
            let word = word.to_ascii_lowercase();
            if !["dims", "metric", "m", "ef"].contains(&word.as_str()) {
                break;
            }
            self.next();
            if word == "metric" {
                metric = match self.next() {
                    Token::Ident(name) | Token::Text(name) => crate::vector::Metric::of_name(&name),
                    _ => None,
                }
                .map_or_else(|| self.syntax("expected cosine, l2 or dot"), Ok)?;
                continue;
            }
            let n = match self.next() {
                Token::Number(n) => n.parse::<usize>().ok(),
                _ => None,
            };
            let Some(n) = n else {
                return self.syntax(format!("expected a number after '{word}'"));
            };
            match word.as_str() {
                "dims" => dims = Some(n),
                "m" => m = n,
                _ => ef = n,
            }
        }
        let Some(dims) = dims else {
            return self.syntax("an index needs 'dims N'");
        };
        Ok(Statement::IndexVector {
            predicate,
            dims,
            metric,
            m,
            ef,
        })
    }

    fn keyword(&mut self, k: &'static str) -> bool {
        self.eat(&Token::Keyword(k))
    }

    fn punct(&mut self, p: &'static str) -> bool {
        self.eat(&Token::Punct(p))
    }

    fn statement(&mut self) -> Result<Statement> {
        let (line, column) = self.here();
        match self.next() {
            Token::Keyword("select") => self.select(),
            Token::Keyword("verify") => {
                let as_of = self.as_of()?;
                let s = Statement::Verify {
                    pattern: self.pattern_in()?,
                    as_of,
                };
                Ok(self.reading(s))
            }
            Token::Keyword("lookup") => {
                let as_of = self.as_of()?;
                let pattern = self.pattern_in()?;
                let where_ = if self.keyword("where") {
                    Some(self.pattern()?)
                } else {
                    None
                };
                let s = Statement::Lookup {
                    pattern,
                    where_,
                    as_of,
                };
                Ok(self.reading(s))
            }
            Token::Keyword("set") => {
                let name = match self.next() {
                    Token::Ident(name) => name,
                    t => return self.syntax(format!("expected a name, found {t}")),
                };
                self.expect(&Token::Punct("="))?;
                let value = self.set_value()?;
                self.symbols.insert(name.clone());
                Ok(Statement::Set { name, value })
            }
            Token::Keyword("test") => {
                let description = match self.peek().clone() {
                    Token::Text(t) => {
                        self.next();
                        Some(t)
                    }
                    _ => None,
                };
                let a = self.test_operand()?;
                let b = self.test_operand()?;
                Ok(Statement::Test {
                    description,
                    operands: Box::new([a, b]),
                })
            }
            Token::Keyword("function") => {
                let name = match self.next() {
                    Token::Ident(name) if Func::of_name(&name).is_some() => {
                        return self.syntax(format!("{name} is a built-in function"));
                    }
                    Token::Ident(name) => name,
                    t => return self.syntax(format!("expected a function's name, found {t}")),
                };
                self.expect(&Token::Punct("("))?;
                let mut params = Vec::new();
                while !self.punct(")") {
                    match self.next() {
                        Token::Ident(p) => params.push(p),
                        t => return self.syntax(format!("expected a parameter, found {t}")),
                    }
                    self.punct(",");
                }
                // The parameters, in the body, are symbols the call replaces.
                let outer = self.symbols.clone();
                self.symbols.extend(params.iter().cloned());
                let body = self.full_expr();
                self.symbols = outer;
                Ok(Statement::Function {
                    name,
                    params,
                    body: body?,
                })
            }
            Token::Keyword("data") => Ok(Statement::Data(self.graph_expr()?)),
            Token::Keyword("default") => Ok(Statement::Default(self.graph_expr()?)),
            Token::Keyword("assume") => {
                let template = self.template()?;
                let graph = if self.keyword("in") {
                    Some(self.graph_name()?)
                } else {
                    None
                };
                Ok(Statement::Assume { template, graph })
            }
            Token::Keyword("delete") if self.index_ahead(self.pos) => {
                self.pos += 3;
                Ok(Statement::DeleteIndex(
                    self.node_constant("an index's predicate")?,
                ))
            }
            Token::Keyword("index") if self.index_ahead(self.pos - 1) => {
                self.pos += 2;
                self.index_vector()
            }
            Token::Keyword("index") => self.syntax("expected 'index vector on P dims N'"),
            Token::Keyword("delete") if self.keyword("rule") => match self.next() {
                Token::Ident(name) => Ok(Statement::DeleteRule(name)),
                t => self.syntax(format!("expected a rule's name, found {t}")),
            },
            // `insert graph G [into g]`.
            Token::Keyword("insert") if self.keyword("graph") => {
                let source = self.graph_expr()?;
                let graph = if self.keyword("into") {
                    Some(self.graph_name()?)
                } else {
                    None
                };
                Ok(Statement::InsertGraph { source, graph })
            }
            Token::Keyword(k @ ("insert" | "delete")) => {
                let template = self.template()?;
                let to = if k == "insert" { "into" } else { "from" };
                let graph = if self.keyword(to) {
                    Some(self.graph_name()?)
                } else {
                    None
                };
                let pattern = if self.keyword("where") {
                    Some(self.pattern()?)
                } else {
                    None
                };
                Ok(if k == "insert" {
                    Statement::Insert {
                        template,
                        pattern,
                        graph,
                    }
                } else {
                    Statement::Delete {
                        template,
                        pattern,
                        graph,
                    }
                })
            }
            Token::Keyword("maintain") => {
                let template = self.template()?;
                if !self.keyword("into") {
                    return self.syntax(format!("expected 'into', found {}", self.peek()));
                }
                let graph = self.graph_name()?;
                if !self.keyword("where") {
                    return self.syntax(format!("expected 'where', found {}", self.peek()));
                }
                let pattern = self.pattern()?;
                Ok(Statement::Maintain {
                    template,
                    pattern,
                    graph,
                })
            }
            Token::Keyword("author") => Ok(Statement::Author(self.graph_name()?)),
            Token::Keyword("subgraph") => Ok(Statement::Subgraph(self.graph_name()?)),
            Token::Keyword("rule") => {
                let name = match self.next() {
                    Token::Ident(name) if Func::of_name(&name).is_some() => {
                        return self.syntax(format!("{name} is a function, not a rule's name"));
                    }
                    Token::Ident(name) => name,
                    t => return self.syntax(format!("expected a rule's name, found {t}")),
                };
                self.expect(&Token::Punct("("))?;
                // Variables, or bare names: parameters that are variables
                // where the body names them.
                let mut head = Vec::new();
                let mut params = HashSet::new();
                while !self.punct(")") {
                    match self.peek().clone() {
                        Token::Ident(p) => {
                            self.next();
                            params.insert(p.clone());
                            head.push(p);
                        }
                        _ => head.push(self.var()?),
                    }
                    self.punct(",");
                }
                let outer = std::mem::replace(&mut self.rule_params, params);
                let body = self.pattern();
                self.rule_params = outer;
                Ok(Statement::Rule(RuleDef {
                    name,
                    head,
                    body: body?,
                }))
            }
            Token::Keyword(k) => Err(Error::Unsupported {
                line,
                column,
                what: format!("'{k}'"),
            }),
            t => Err(Error::Syntax {
                line,
                column,
                message: format!("expected a statement, found {t}"),
            }),
        }
    }

    /// A pattern, then `in g`: within the subgraph `g`; or `in graph G`:
    /// reading the graph `G` (see [`reading`](Self::reading)).
    fn pattern_in(&mut self) -> Result<Pattern> {
        let p = self.pattern()?;
        if !self.keyword("in") {
            return Ok(p);
        }
        if self.keyword("graph") {
            self.source = Some(self.graph_expr()?);
            return Ok(p);
        }
        Ok(Pattern::InGraph(Box::new(p), self.graph_name()?))
    }

    /// What `test` compares: a `select`, `verify` or `lookup`, or `graph G`.
    fn test_operand(&mut self) -> Result<TestOperand> {
        match self.peek() {
            Token::Keyword("select" | "lookup" | "verify") => {
                Ok(TestOperand::Statement(Box::new(self.statement()?)))
            }
            Token::Keyword("graph") => Ok(TestOperand::Graph(self.graph_expr()?)),
            t => self.syntax(format!(
                "expected a lookup, select, verify or graph, found {t}"
            )),
        }
    }

    /// `s`, reading the graph `in graph G` named, if it did.
    fn reading(&mut self, s: Statement) -> Statement {
        match self.source.take() {
            Some(source) => Statement::From {
                source,
                statement: Box::new(s),
            },
            None => s,
        }
    }

    /// A graph: a name (`database`, or one `SET` named), `{ triples }`,
    /// `name:G`, or `( lookup … )`, after `graph` or not.
    fn graph_expr(&mut self) -> Result<GraphExpr> {
        self.keyword("graph");
        if self.punct("(") {
            if !self.is(&Token::Keyword("lookup")) {
                return self.syntax(format!("expected 'lookup', found {}", self.peek()));
            }
            let lookup = self.statement()?;
            self.expect(&Token::Punct(")"))?;
            return Ok(GraphExpr::Lookup(Box::new(lookup)));
        }
        if self.is(&Token::Punct("{")) {
            return self.graph_literal();
        }
        let name = match self.next() {
            Token::Ident(name) => name,
            t => return self.syntax(format!("expected a graph, found {t}")),
        };
        if self.punct(":") {
            return Ok(GraphExpr::Over(name, Box::new(self.graph_expr()?)));
        }
        Ok(GraphExpr::Name(name))
    }

    /// `{ triples }`, each perhaps negated: a deleted link.
    fn graph_literal(&mut self) -> Result<GraphExpr> {
        self.expect(&Token::Punct("{"))?;
        let mut triples = Vec::new();
        if self.punct("}") {
            return Ok(GraphExpr::Literal(triples));
        }
        let (line, column) = self.here();
        self.in_template = true;
        let subjects = self.subjects();
        self.in_template = false;
        fn literal(p: Pattern, out: &mut Vec<Pattern>) -> std::result::Result<(), &'static str> {
            match p {
                Pattern::Not(inner) => {
                    let mut deleted = Vec::new();
                    literal(*inner, &mut deleted)?;
                    for d in deleted {
                        match d {
                            Pattern::Triple(..) => out.push(Pattern::Not(Box::new(d))),
                            _ => return Err("a deletion other than of triples"),
                        }
                    }
                    Ok(())
                }
                p => flatten(p, out),
            }
        }
        for p in subjects? {
            literal(p, &mut triples).map_err(|what| Error::Unsupported {
                line,
                column,
                what: what.into(),
            })?;
        }
        self.expect(&Token::Punct("}"))?;
        Ok(GraphExpr::Literal(triples))
    }

    /// What `SET` names: a graph, or a constant.
    fn set_value(&mut self) -> Result<SetValue> {
        let graph = match self.peek() {
            Token::Keyword("graph") | Token::Punct("{" | "(") => true,
            Token::Ident(n) => {
                n == "database"
                    || self.tokens.get(self.pos + 1).map(|t| &t.token) == Some(&Token::Punct(":"))
            }
            _ => false,
        };
        if graph {
            return Ok(SetValue::Graph(self.graph_expr()?));
        }
        let mut out = Vec::new();
        match self.node(&mut out)? {
            n
            @ (Node::Key(_) | Node::Value(_) | Node::Id(_) | Node::Param(_) | Node::Symbol(_))
                if out.is_empty() =>
            {
                Ok(SetValue::Node(n))
            }
            n => self.syntax(format!(
                "expected a constant or a graph, found {}",
                Expr::Node(n)
            )),
        }
    }

    /// A subgraph's (or an author's) name: a constant node, a word or text
    /// being the value with that data.
    fn graph_name(&mut self) -> Result<Node> {
        let (line, column) = self.here();
        if matches!(self.peek(), Token::Punct("(" | "{")) || self.is(&Token::Keyword("graph")) {
            return self.unsupported("a graph other than a subgraph's name");
        }
        let mut out = Vec::new();
        match self.node(&mut out)? {
            n @ (Node::Key(_) | Node::Value(_) | Node::Id(_) | Node::Param(_))
                if out.is_empty() =>
            {
                Ok(n)
            }
            n => Err(Error::Syntax {
                line,
                column,
                message: format!("expected a subgraph's name, found {}", Expr::Node(n)),
            }),
        }
    }

    fn select(&mut self) -> Result<Statement> {
        let distinct = self.keyword("distinct");
        let vars = if self.punct("*") {
            None
        } else {
            let mut vars = vec![self.select_item()?];
            while self.punct(",") {
                vars.push(self.select_item()?);
            }
            Some(vars)
        };
        let mut having = Vec::new();
        if self.keyword("having") {
            loop {
                having.push(self.having()?);
                if !self.keyword("and") {
                    break;
                }
            }
        }
        let mut by = self.order_keys()?;
        // `limit` and `offset`, in either order.
        let (mut limit, mut offset) = self.limits()?;
        let as_of = self.as_of()?;
        let mut source = None;
        let graph = if self.keyword("from") {
            if self.keyword("graph") {
                source = Some(self.graph_expr()?);
                None
            } else {
                Some(self.graph_name()?)
            }
        } else {
            None
        };
        if !self.keyword("where") {
            return self.syntax(format!("expected 'where', found {}", self.peek()));
        }
        let mut pattern = self.pattern()?;
        // `by`, `limit` and `offset` may instead end the statement, as in
        // SQL: a limit ending the pattern is the statement's, not a nested
        // one, which is followed by more pattern or in parentheses.
        let given_before = limit.is_some() || offset.is_some();
        let twice = "a select's 'limit' and 'offset' go either before 'where' or after the \
                     pattern (a nested limit ending it is in parentheses: '( ?p pet ?x limit 1 )')";
        let (start, end) = self.limit_at;
        if end == self.pos
            && let Some((l, o)) = take_last_limit(&mut pattern)
        {
            if given_before {
                return self.syntax_at(start, twice);
            }
            limit = (l != usize::MAX).then_some(l);
            offset = (o > 0).then_some(o);
        }
        if self.is(&Token::Keyword("by")) {
            if !by.is_empty() {
                return self
                    .syntax("a select's 'by' goes either before 'where' or after the pattern");
            }
            by = self.order_keys()?;
        }
        let start = self.pos;
        let (l, o) = self.limits()?;
        if l.is_some() || o.is_some() {
            if given_before || limit.is_some() || offset.is_some() {
                return self.syntax_at(start, twice);
            }
            (limit, offset) = (l, o);
        }
        if let Some(g) = graph {
            pattern = Pattern::InGraph(Box::new(pattern), g);
        }
        let select = Statement::Select {
            distinct,
            vars,
            having,
            by,
            pattern,
            limit,
            offset,
            as_of,
        };
        Ok(match source {
            Some(source) => Statement::From {
                source,
                statement: Box::new(select),
            },
            None => select,
        })
    }

    /// A column of a `select`: `?x`, an expression, or
    /// `function([distinct] expr | *)`, then perhaps `as ?n`.
    fn select_item(&mut self) -> Result<SelectItem> {
        let function = match (self.peek(), self.tokens.get(self.pos + 1).map(|t| &t.token)) {
            (Token::Ident(f), Some(Token::Punct("("))) => Function::of_name(f),
            _ => None,
        };
        let item = match function {
            Some(function) => {
                self.next();
                self.next();
                let distinct = self.keyword("distinct");
                let arg = if self.punct("*") {
                    if function != Function::Count || distinct {
                        return self.syntax("only count(*) takes '*'");
                    }
                    None
                } else {
                    Some(self.expr()?)
                };
                self.expect(&Token::Punct(")"))?;
                SelectItem::Aggregate {
                    function,
                    distinct,
                    arg,
                    name: None,
                }
            }
            None => match self.expr()? {
                Expr::Node(Node::Var(v)) => SelectItem::Var(v),
                Expr::Node(n @ (Node::Blank(_) | Node::Param(_))) => {
                    return self.syntax(format!("can't select {}", Expr::Node(n)));
                }
                expr => SelectItem::Expr { expr, name: None },
            },
        };
        if !(self.is(&Token::Keyword("as")) && !self.as_of_follows()) {
            return Ok(item);
        }
        self.next();
        let name = Some(self.var()?);
        Ok(match item {
            SelectItem::Var(v) => SelectItem::Expr {
                expr: Expr::Node(Node::Var(v)),
                name,
            },
            SelectItem::Expr { expr, .. } => SelectItem::Expr { expr, name },
            SelectItem::Aggregate {
                function,
                distinct,
                arg,
                ..
            } => SelectItem::Aggregate {
                function,
                distinct,
                arg,
                name,
            },
        })
    }

    /// A comparison of `having`: of expressions of the columns.
    fn having(&mut self) -> Result<(Expr, Comparison, Expr)> {
        let a = self.expr()?;
        let Some(cmp) = self.comparison() else {
            return self.syntax(format!("expected a comparison, found {}", self.peek()));
        };
        self.next();
        Ok((a, cmp, self.expr()?))
    }

    /// A condition or value: `||` of `&&` of comparisons of expressions.
    fn full_expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        while self.punct("||") {
            left = Expr::Or(Box::new(left), Box::new(self.and_expr()?));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.compare_expr()?;
        while self.punct("&&") {
            left = Expr::And(Box::new(left), Box::new(self.compare_expr()?));
        }
        Ok(left)
    }

    fn compare_expr(&mut self) -> Result<Expr> {
        let left = self.expr()?;
        match self.comparison() {
            Some(cmp) => {
                self.next();
                Ok(Expr::Compare(Box::new(left), cmp, Box::new(self.expr()?)))
            }
            None => Ok(left),
        }
    }

    /// An expression: terms joined by `+` and `-`.
    fn expr(&mut self) -> Result<Expr> {
        let first = self.expr_product()?;
        self.expr_rest(first, 1)
    }

    /// The rest of an expression after `left`, with operators of at least
    /// `precedence` (1: `+` `-`, 2: `*` `/` `%`).
    fn expr_rest(&mut self, mut left: Expr, precedence: u8) -> Result<Expr> {
        loop {
            let op = match self.peek() {
                Token::Punct("*") => BinOp::Mul,
                Token::Punct("/") => BinOp::Div,
                Token::Punct("%") => BinOp::Rem,
                Token::Punct("+") if precedence <= 1 => BinOp::Add,
                Token::Punct("-") if precedence <= 1 => BinOp::Sub,
                _ => return Ok(left),
            };
            self.next();
            // A type tag (`+integer`): a conversion of the operands, and of
            // the result.
            let tag = match self.peek() {
                Token::Keyword(t @ ("integer" | "real" | "string" | "boolean")) => {
                    let t = *t;
                    self.next();
                    Some(t)
                }
                _ => None,
            };
            let right = if matches!(op, BinOp::Add | BinOp::Sub) {
                self.expr_product()?
            } else {
                self.expr_unary()?
            };
            let number = |e: Expr| Expr::Call(Func::Number, vec![e]);
            left = match tag {
                Some("integer") => Expr::Call(
                    Func::Round,
                    vec![Expr::Binary(
                        op,
                        Box::new(number(left)),
                        Box::new(number(right)),
                    )],
                ),
                Some("real") => Expr::Binary(op, Box::new(number(left)), Box::new(number(right))),
                Some("string") if op == BinOp::Add => Expr::Call(
                    Func::Concat,
                    vec![
                        Expr::Call(Func::Str, vec![left]),
                        Expr::Call(Func::Str, vec![right]),
                    ],
                ),
                _ => Expr::Binary(op, Box::new(left), Box::new(right)),
            };
        }
    }

    /// Factors joined by `*`, `/` and `%`.
    fn expr_product(&mut self) -> Result<Expr> {
        let first = self.expr_unary()?;
        self.expr_rest(first, 2)
    }

    /// `-x`, a function call, `( expr )`, or a node.
    fn expr_unary(&mut self) -> Result<Expr> {
        if self.punct("-") {
            return Ok(Expr::Neg(Box::new(self.expr_unary()?)));
        }
        if self.punct("(") {
            let e = self.full_expr()?;
            self.expect(&Token::Punct(")"))?;
            return Ok(e);
        }
        if self.keyword("if") {
            let c = self.full_expr()?;
            if !self.keyword("then") {
                return self.syntax(format!("expected 'then', found {}", self.peek()));
            }
            let a = self.full_expr()?;
            if !self.keyword("else") {
                return self.syntax(format!("expected 'else', found {}", self.peek()));
            }
            let b = self.full_expr()?;
            return Ok(Expr::If(Box::new(c), Box::new(a), Box::new(b)));
        }
        if let (Token::Ident(name), Some(Token::Punct("("))) =
            (self.peek(), self.tokens.get(self.pos + 1).map(|t| &t.token))
        {
            let Some(f) = Func::of_name(name) else {
                // A function `FUNCTION` defined.
                let name = name.clone();
                self.next();
                self.next();
                let mut args = Vec::new();
                while !self.punct(")") {
                    args.push(self.full_expr()?);
                    self.punct(",");
                }
                return Ok(Expr::Apply(name, args));
            };
            self.next();
            self.next();
            let mut args = Vec::new();
            while !self.punct(")") {
                args.push(self.expr()?);
                self.punct(",");
            }
            let (least, most) = f.arity();
            let implied = std::mem::take(&mut self.implied_arg) && f.is_test();
            let given = args.len() + usize::from(implied);
            if given < least || given > most {
                return self.syntax(format!("{} takes {least} to {most} arguments", f.name()));
            }
            return Ok(Expr::Call(f, args));
        }
        let mut out = Vec::new();
        let node = self.node(&mut out)?;
        if !out.is_empty() {
            return self.syntax("an expression's nodes can't have predicates");
        }
        Ok(Expr::Node(node))
    }

    /// Whether `as of` comes next.
    fn as_of_follows(&self) -> bool {
        self.is(&Token::Keyword("as"))
            && self.tokens.get(self.pos + 1).map(|t| &t.token) == Some(&Token::Keyword("of"))
    }

    /// `as of id` or `as of "time"`, if it comes next.
    fn as_of(&mut self) -> Result<Option<AsOf>> {
        if !self.as_of_follows() {
            return Ok(None);
        }
        self.next();
        self.next();
        let (line, column) = self.here();
        let error = |message: String| {
            Err(Error::Syntax {
                line,
                column,
                message,
            })
        };
        match self.next() {
            Token::Number(n) => match n.parse() {
                Ok(id) => Ok(Some(AsOf::Id(id))),
                Err(_) => error(format!("expected an id, found {n}")),
            },
            Token::Text(t) => match parse_time(&t) {
                Some(time) => Ok(Some(AsOf::Time(time))),
                None => error(format!(
                    "{t:?} is not an ISO 8601 time, such as \"2026-10-31T12:30Z\""
                )),
            },
            t => error(format!("expected an id or a time after 'as of', found {t}")),
        }
    }

    fn var(&mut self) -> Result<String> {
        match self.next() {
            Token::Var(v) if !v.is_empty() => Ok(v),
            Token::Keyword("distinct") => self.unsupported("'distinct' on one variable"),
            t => self.syntax(format!("expected a variable, found {t}")),
        }
    }

    /// `pattern: [let ...] exclusion`, with `else` and `implies` unsupported.
    fn pattern(&mut self) -> Result<Pattern> {
        if matches!(self.peek(), Token::Punct("*" | "/")) {
            return self.unsupported("default modifiers");
        }
        if self.is(&Token::Keyword("graph")) {
            return self.unsupported("a named graph");
        }
        // `+ pattern` and `- pattern`: the default for its predicates,
        // mandatory or optional, unless marked otherwise.
        let outer = self.optional_default;
        if self.punct("+") {
            self.optional_default = false;
        } else if self.punct("-") {
            self.optional_default = true;
        }
        let p = self.disjunction();
        self.optional_default = outer;
        let p = p?;
        for k in ["else", "implies"] {
            if self.is(&Token::Keyword(k)) {
                return self.unsupported(format!("'{k}'"));
            }
        }
        Ok(p)
    }

    fn disjunction(&mut self) -> Result<Pattern> {
        let mut ps = vec![self.conjunction()?];
        while self.keyword("or") {
            ps.push(self.conjunction()?);
        }
        Ok(if ps.len() == 1 {
            ps.pop().unwrap()
        } else {
            Pattern::Or(ps)
        })
    }

    fn conjunction(&mut self) -> Result<Pattern> {
        let mut ps = vec![self.basic()?];
        while self.keyword("and") {
            ps.push(self.basic()?);
        }
        Ok(if ps.len() == 1 {
            ps.pop().unwrap()
        } else {
            Pattern::And(ps)
        })
    }

    /// `basic: subjects | ( pattern )`, and `not basic`.
    fn basic(&mut self) -> Result<Pattern> {
        if self.punct("(") {
            let p = self.pattern()?;
            self.expect(&Token::Punct(")"))?;
            let p = match self.as_of()? {
                Some(id) => Pattern::AsOf(Box::new(p), id),
                None => p,
            };
            // `( p ) in g`: the same whether this group's or the statement's.
            return Ok(if self.keyword("in") {
                Pattern::InGraph(Box::new(p), self.graph_name()?)
            } else {
                p
            });
        }
        if self.keyword("optional") {
            self.expect(&Token::Punct("("))?;
            let p = self.pattern()?;
            self.expect(&Token::Punct(")"))?;
            return Ok(Pattern::Optional(Box::new(p)));
        }
        if self.is(&Token::Keyword("not")) && !self.subject_follows_not() {
            self.next();
            return Ok(Pattern::Not(Box::new(self.basic()?)));
        }
        Ok(Pattern::And(self.subjects()?))
    }

    /// Whether `not` starts a negated subject (`not s p o`) rather than a
    /// negated pattern (`not ( ... )`).
    fn subject_follows_not(&self) -> bool {
        !matches!(
            self.tokens.get(self.pos + 1).map(|t| &t.token),
            Some(Token::Punct("(")) | Some(Token::Keyword("not"))
        )
    }

    /// Subjects separated by `.`, within optional braces.
    fn subjects(&mut self) -> Result<Vec<Pattern>> {
        if self.punct("{") {
            let ps = self.subjects()?;
            self.expect(&Token::Punct("}"))?;
            return Ok(ps);
        }
        let mut ps = Vec::new();
        loop {
            ps.push(self.subject()?);
            if !self.punct(".") || !self.starts_subject() {
                return Ok(ps);
            }
        }
    }

    fn starts_subject(&self) -> bool {
        matches!(
            self.peek(),
            Token::Var(_)
                | Token::Label(_)
                | Token::Key(_)
                | Token::Id(_)
                | Token::Param(_)
                | Token::Ident(_)
                | Token::Text(_)
                | Token::Number(_)
                | Token::Punct("[" | "<<" | "-")
                | Token::Keyword("not" | "true" | "false" | "list" | "optional" | "let")
        )
    }

    /// `subject: call | not node predicates | node predicate_phrase`.
    fn subject(&mut self) -> Result<Pattern> {
        if self.is(&Token::Keyword("optional")) {
            return self.basic();
        }
        if self.keyword("let") {
            let var = self.var()?;
            if !self.punct("=") {
                return self.syntax(format!("expected '=' after 'let ?{var}'"));
            }
            return Ok(Pattern::Let(var, self.expr()?));
        }
        let negated = self.keyword("not");
        let mut out = Vec::new();
        // A function, as a test or the start of a comparison.
        let function = match (self.peek(), self.tokens.get(self.pos + 1).map(|t| &t.token)) {
            (Token::Ident(f), Some(Token::Punct("("))) => Func::of_name(f),
            _ => None,
        };
        // A call followed by a comparison is of a function, not a rule.
        let compared = matches!(self.peek(), Token::Ident(_))
            && self.tokens.get(self.pos + 1).map(|t| &t.token) == Some(&Token::Punct("("))
            && self.compared_after(self.pos + 1);
        if function.is_some() || compared || self.is(&Token::Punct("-")) {
            let e = self.expr()?;
            let p = self.comparison_or_test(e)?;
            return Ok(if negated {
                Pattern::Not(Box::new(p))
            } else {
                p
            });
        }
        let is_call = matches!(self.peek(), Token::Ident(_))
            && self.tokens.get(self.pos + 1).map(|t| &t.token) == Some(&Token::Punct("("))
            && !self.path_follows(self.pos + 1);
        let is_aggregate = is_call
            && matches!(self.peek(), Token::Ident(n) if LINK_AGGREGATES.contains(&n.to_ascii_uppercase().as_str()));
        if is_aggregate {
            let Token::Ident(name) = self.next() else {
                unreachable!()
            };
            self.next();
            let distinct = self.keyword("distinct");
            let mut inner = Vec::new();
            let s = self.node(&mut inner)?;
            let p = self.predicate(&mut inner)?;
            let o = self.node(&mut inner)?;
            if !inner.is_empty() {
                return self.syntax("an aggregate's subject, predicate and object are nodes");
            }
            let descending =
                matches!(self.peek(), Token::Ident(d) if d.eq_ignore_ascii_case("desc"));
            if descending {
                self.next();
            }
            self.expect(&Token::Punct(")"))?;
            let scope = if self.punct("@") {
                match self.next() {
                    Token::Var(v) if !v.is_empty() => Some(v),
                    Token::Ident(p) if self.rule_params.contains(&p) => Some(p),
                    t => {
                        return self
                            .syntax(format!("the scope after '@' must be a variable, found {t}"));
                    }
                }
            } else {
                None
            };
            out.push(Pattern::LinkAggregate {
                name: name.to_ascii_uppercase(),
                distinct,
                triple: Box::new((s, p, o)),
                descending,
                scope,
            });
            let p = Pattern::And(out);
            return Ok(if negated {
                Pattern::Not(Box::new(p))
            } else {
                p
            });
        }
        if is_call {
            let Token::Ident(name) = self.next() else {
                unreachable!()
            };
            self.next();
            let mut args = Vec::new();
            while !self.punct(")") {
                args.push(self.node(&mut out)?);
                self.punct(",");
            }
            if self.is(&Token::Punct("@")) {
                return self.unsupported("'@'");
            }
            out.push(Pattern::Call(name, args));
            let p = Pattern::And(out);
            return Ok(if negated {
                Pattern::Not(Box::new(p))
            } else {
                p
            });
        }
        let is_link = self.is(&Token::Punct("<<"));
        let node = self.node(&mut out)?;
        // A link node alone matches its link.  `(` after a subject starts a
        // path expression.
        let predicate_follows = self.starts_node()
            || self.is(&Token::Keyword("not"))
            || self.is(&Token::Punct("!"))
            || self.is(&Token::Punct(">>"))
            || self.is(&Token::Punct("("));
        if is_link && !predicate_follows && !self.is_comparison() {
            let p = Pattern::And(out);
            return Ok(if negated {
                Pattern::Not(Box::new(p))
            } else {
                p
            });
        }
        // Arithmetic after a node: an expression to compare.
        if matches!(self.peek(), Token::Punct("+" | "-" | "*" | "/" | "%")) && out.is_empty() {
            let left = self.expr_rest(Expr::Node(node), 1)?;
            let p = self.comparison_or_test(left)?;
            return Ok(if negated {
                Pattern::Not(Box::new(p))
            } else {
                p
            });
        }
        // A subject alone, or a blank node of predicates alone: the node
        // (and the blank node's patterns).
        if !predicate_follows && !self.is_comparison() {
            if self.in_template && out.is_empty() {
                return self.syntax(format!("expected a predicate, found {}", self.peek()));
            }
            self.short_end = self.is(&Token::End);
            if out.is_empty() {
                out.push(Pattern::Node(node.clone()));
            }
            // `?var limit n`: a nested limit of the variable.
            if let Some((limit, offset)) = self.value_limit()? {
                let (Node::Var(v) | Node::Blank(v)) = node else {
                    return self.syntax("a limit is of a variable");
                };
                out.push(Pattern::LimitValues {
                    var: v,
                    limit,
                    offset,
                });
            }
            let p = Pattern::And(out);
            return Ok(if negated {
                Pattern::Not(Box::new(p))
            } else {
                p
            });
        }
        let cmp = self.comparison();
        if let Some(cmp) = cmp.filter(|_| !negated) {
            self.next();
            let other = self.expr()?;
            out.push(Pattern::Compare(Expr::Node(node), cmp, other));
        } else {
            self.predicates(&node, &mut out)?;
        }
        let p = Pattern::And(out);
        Ok(if negated {
            Pattern::Not(Box::new(p))
        } else {
            p
        })
    }

    fn is_comparison(&self) -> bool {
        self.comparison().is_some()
    }

    /// `left cmp expr`, or `left` alone if it is a test (a function giving
    /// a boolean).
    fn comparison_or_test(&mut self, left: Expr) -> Result<Pattern> {
        if let Some(cmp) = self.comparison() {
            self.next();
            return Ok(Pattern::Compare(left, cmp, self.expr()?));
        }
        match &left {
            Expr::Call(f, _) if f.is_test() => Ok(Pattern::Test(left)),
            _ => self.syntax(format!(
                "expected a comparison after {left}, found {}",
                self.peek()
            )),
        }
    }

    /// The comparison that comes next, if one does.
    fn comparison(&self) -> Option<Comparison> {
        match self.peek() {
            Token::Punct("=") | Token::Punct("==") => Some(Comparison::Eq),
            Token::Punct("!=") => Some(Comparison::Ne),
            Token::Punct("<") => Some(Comparison::Data(Cmp::Lt)),
            Token::Punct("<=") => Some(Comparison::Data(Cmp::Le)),
            Token::Punct(">") => Some(Comparison::Data(Cmp::Gt)),
            Token::Punct(">=") => Some(Comparison::Data(Cmp::Ge)),
            _ => None,
        }
    }

    /// Whether the parentheses opening at token `open` hold a path
    /// expression rather than a call's arguments: a node follows the
    /// matching `)`, as the path's object does (`ann (knows+) ?x`), where a
    /// call (`friend(?x)`) ends a subject.
    /// Whether a comparison follows the parentheses opening at `open`.
    fn compared_after(&self, open: usize) -> bool {
        let mut depth = 0usize;
        for (i, t) in self.tokens.iter().enumerate().skip(open) {
            match &t.token {
                Token::Punct("(") => depth += 1,
                Token::Punct(")") => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(i + 1).map(|t| &t.token),
                            Some(Token::Punct(
                                "=" | "==" | "!=" | "<" | "<=" | ">" | ">=" | "+" | "*" | "/" | "%"
                            ))
                        );
                    }
                }
                Token::End => return false,
                _ => {}
            }
        }
        false
    }

    fn path_follows(&self, open: usize) -> bool {
        let mut depth = 0usize;
        for (i, t) in self.tokens.iter().enumerate().skip(open) {
            match &t.token {
                Token::Punct("(") => depth += 1,
                Token::Punct(")") => {
                    depth -= 1;
                    if depth == 0 {
                        let next = self.tokens.get(i + 1).map(|t| &t.token);
                        return matches!(
                            next,
                            Some(
                                Token::Var(_)
                                    | Token::Label(_)
                                    | Token::Key(_)
                                    | Token::Id(_)
                                    | Token::Param(_)
                                    | Token::Ident(_)
                                    | Token::Text(_)
                                    | Token::Number(_)
                                    | Token::Punct("[" | "<<" | "++")
                                    | Token::Keyword("not" | "true" | "false")
                            )
                        );
                    }
                }
                Token::End => return false,
                _ => {}
            }
        }
        false
    }

    /// Predicates of `subject`, with their objects, separated by `;`.
    ///
    /// `<< p` reverses a predicate (`s << p o` is `o p s`) and `>> p` keeps
    /// it forward; after objects, either continues from the last of them, so
    /// that predicates chain (`?d << directed_by ?f >> starring ?p`).  `;`
    /// returns to `subject`.  `! p` reverses one predicate, of the same
    /// subject.
    fn predicates(&mut self, subject: &Node, out: &mut Vec<Pattern>) -> Result<()> {
        let mut current = subject.clone();
        let mut last_object: Option<Node> = None;
        loop {
            if let Token::Punct("+" | "-" | "*" | "/") | Token::Keyword("limit" | "offset") =
                self.peek()
            {
                return self.unsupported("predicate modifiers");
            }
            // The direction, and where a chain continues from.
            let mut reverse = false;
            if matches!(self.peek(), Token::Punct("<<" | ">>")) {
                reverse = self.punct("<<");
                if !reverse {
                    self.next();
                }
                if let Some(o) = last_object.take() {
                    current = o;
                }
            }
            let start = out.len();
            let negated = self.keyword("not");
            reverse |= self.punct("!");
            let orient = |s: &Node, o: Node| {
                if reverse {
                    (o, s.clone())
                } else {
                    (s.clone(), o)
                }
            };
            if self.punct("(") {
                let mut path = self.path_alt()?;
                self.expect(&Token::Punct(")"))?;
                if self.punct("++") {
                    path = Path::Repeat(Box::new(path), 1, None);
                }
                let mut first = true;
                loop {
                    let object_negated = self.keyword("not");
                    let object = self.object(out, first)?;
                    last_object = Some(object.clone());
                    let (s, o) = orient(&current, object);
                    let p = Pattern::PathExpr(s, path.clone(), o);
                    out.push(if negated || object_negated {
                        Pattern::Not(Box::new(p))
                    } else {
                        p
                    });
                    if !self.punct(",") {
                        break;
                    }
                    first = false;
                }
            } else {
                let predicate = self.predicate(out)?;
                let transitive = self.punct("++");
                let mut first = true;
                loop {
                    let object_negated = self.keyword("not");
                    // Where the object's own triples start, if asserted.
                    let object_start = out.len();
                    let object = self.object(out, first)?;
                    // `s p o author subgraph`.
                    let (mut author, mut graph) = (None, None);
                    if self.starts_node() && !self.is(&Token::Punct("<<")) {
                        if transitive {
                            return self.unsupported("author and subgraph positions of a path");
                        }
                        author = Some(self.node(out)?);
                        if self.starts_node() && !self.is(&Token::Punct("<<")) {
                            graph = Some(self.node(out)?);
                        }
                    }
                    last_object = Some(object.clone());
                    let (s, o) = orient(&current, object);
                    let triple = if transitive {
                        Pattern::Path(s, predicate.clone(), o)
                    } else if author.is_some() {
                        // A `?` author is any: none asked for.
                        let any = |n: &Option<Node>| matches!(n, Some(Node::Blank(b)) if b.starts_with('['));
                        let author = if any(&author) { None } else { author };
                        Pattern::Tagged(s, predicate.clone(), o, author, graph)
                    } else {
                        Pattern::Triple(s, predicate.clone(), o)
                    };
                    if self.is(&Token::Punct("!")) {
                        let asserted = self.asserted(triple, out.split_off(object_start))?;
                        if negated || object_negated {
                            return self.syntax("a negated triple can't be asserted with '!'");
                        }
                        out.push(asserted);
                    } else {
                        out.push(if negated || object_negated {
                            Pattern::Not(Box::new(triple))
                        } else {
                            triple
                        });
                    }
                    if !self.punct(",") {
                        break;
                    }
                    first = false;
                }
            }
            // `+` keeps the predicate mandatory; `-` makes it and its
            // objects optional; neither, the pattern's default (`- pattern`).
            // (A default isn't for asserted triples, which match nothing.)
            let asserted = out[start..]
                .iter()
                .all(|p| matches!(p, Pattern::Assert(..)));
            let optional = if self.punct("+") {
                false
            } else {
                self.punct("-") || (self.optional_default && !asserted)
            };
            if optional {
                let optional = out.split_off(start);
                out.push(Pattern::Optional(Box::new(Pattern::And(optional))));
            }
            // `limit n` (and `offset n`): a nested limit of the predicate's
            // object, or of the subject, if the object is a constant.
            if let Some((limit, offset)) = self.value_limit()? {
                let node = match (&last_object, &current) {
                    (Some(n @ (Node::Var(_) | Node::Blank(_))), _) => n.clone(),
                    (_, n @ (Node::Var(_) | Node::Blank(_))) => n.clone(),
                    _ => return self.syntax("a limit is of a variable"),
                };
                let (Node::Var(var) | Node::Blank(var)) = node.clone() else {
                    unreachable!("a variable")
                };
                // As `?var limit n` is: the node, and its limit.
                out.push(Pattern::Node(node));
                out.push(Pattern::LimitValues { var, limit, offset });
            }
            match self.peek() {
                Token::Punct("+" | "-" | "*" | "/" | "!") | Token::Keyword("limit" | "offset") => {
                    return self.unsupported("predicate modifiers");
                }
                // A chain continues.
                Token::Punct("<<" | ">>") => continue,
                _ => {}
            }
            if !self.punct(";") {
                return Ok(());
            }
            current = subject.clone();
            last_object = None;
            if !self.starts_node()
                && !self.is(&Token::Keyword("not"))
                && !self.is(&Token::Punct("("))
                && !self.is(&Token::Punct("!"))
            {
                // A trailing `;`.
                return Ok(());
            }
        }
    }

    /// `triple`, at `!`, asserted rather than matched, with `object`'s own
    /// triples (a blank node's), and a scope if `@?x` follows.
    fn asserted(&mut self, triple: Pattern, object: Vec<Pattern>) -> Result<Pattern> {
        self.expect(&Token::Punct("!"))?;
        let scope = if self.punct("@") {
            match self.next() {
                Token::Var(v) if !v.is_empty() => Some(v),
                Token::Ident(p) if self.rule_params.contains(&p) => Some(p),
                t => {
                    return self
                        .syntax(format!("the scope after '@' must be a variable, found {t}"));
                }
            }
        } else {
            None
        };
        // The triple, then its object's.
        let mut triples = Vec::new();
        match triple {
            Pattern::Triple(..) => triples.push(triple),
            Pattern::Path(..) => return self.syntax("a path can't be asserted with '!'"),
            _ => {
                return self.syntax("an asserted triple can't have author or subgraph positions");
            }
        }
        for p in object {
            match p {
                Pattern::Triple(..) => triples.push(p),
                // `s p [ q r! ]!`: the inner triple is asserted anyway.
                Pattern::Assert(inner, None) => triples.extend(inner),
                _ => return self.syntax("an asserted blank node can have only triples"),
            }
        }
        Ok(Pattern::Assert(triples, scope))
    }

    /// `limit n` and `offset n`, in either order, after a predicate's
    /// objects or a variable: a nested limit, if given.
    fn value_limit(&mut self) -> Result<Option<(usize, usize)>> {
        let start = self.pos;
        let (limit, offset) = self.limits()?;
        if limit.is_some() || offset.is_some() {
            self.limit_at = (start, self.pos);
        }
        Ok(match (limit, offset) {
            (None, None) => None,
            (l, o) => Some((l.unwrap_or(usize::MAX), o.unwrap_or(0))),
        })
    }

    /// `by ?x [desc | asc], ...`, if given.
    fn order_keys(&mut self) -> Result<Vec<OrderKey>> {
        let mut by = Vec::new();
        if !self.keyword("by") {
            return Ok(by);
        }
        loop {
            let var = self.var()?;
            let descending = match self.peek() {
                Token::Ident(w) if w.eq_ignore_ascii_case("desc") => Some(true),
                Token::Ident(w) if w.eq_ignore_ascii_case("asc") => Some(false),
                _ => None,
            };
            if descending.is_some() {
                self.next();
            }
            by.push(OrderKey {
                var,
                descending: descending.unwrap_or(false),
            });
            if !self.punct(",") {
                return Ok(by);
            }
        }
    }

    /// `limit n` and `offset n`, in either order, each perhaps.
    fn limits(&mut self) -> Result<(Option<usize>, Option<usize>)> {
        let (mut limit, mut offset) = (None, None);
        loop {
            let which = if self.keyword("limit") {
                &mut limit
            } else if self.keyword("offset") {
                &mut offset
            } else {
                break;
            };
            if which.is_some() {
                return self.syntax("'limit' and 'offset' may each be given once");
            }
            *which = match self.next() {
                Token::Number(n) => match n.parse() {
                    Ok(n) => Some(n),
                    Err(_) => return self.syntax("expected a count"),
                },
                _ => return self.syntax("expected a count"),
            };
        }
        Ok((limit, offset))
    }

    /// Alternatives of a path expression: `p | q`.
    fn path_alt(&mut self) -> Result<Path> {
        let mut ps = vec![self.path_seq()?];
        while self.punct("|") {
            ps.push(self.path_seq()?);
        }
        Ok(if ps.len() == 1 {
            ps.pop().unwrap()
        } else {
            Path::Alt(ps)
        })
    }

    /// A sequence of a path expression: `p / q`.  `/q` with no space lexes as
    /// a key, which in a path expression is read as `/` then `q`, so that
    /// `knows/likes` is a sequence; `/a/b` is `/ a / b`.
    fn path_seq(&mut self) -> Result<Path> {
        let mut ps = vec![self.path_unary()?];
        loop {
            if self.punct("/") {
                ps.push(self.path_unary()?);
                continue;
            }
            let Token::Key(key) = self.peek().clone() else {
                break;
            };
            self.next();
            let steps: Vec<&str> = key[1..].split('/').collect();
            if steps.iter().any(|s| s.is_empty()) {
                return self.syntax(format!("expected a predicate in the path {key}"));
            }
            let (last, rest) = steps.split_last().expect("a step");
            ps.extend(rest.iter().map(|s| Path::Pred(s.to_string())));
            let last = Path::Pred(last.to_string());
            ps.push(self.path_postfix(last)?);
        }
        Ok(if ps.len() == 1 {
            ps.pop().unwrap()
        } else {
            Path::Seq(ps)
        })
    }

    /// `^p`, or a step with its repetitions.
    fn path_unary(&mut self) -> Result<Path> {
        if self.punct("^") {
            return Ok(Path::Inverse(Box::new(self.path_unary()?)));
        }
        let step = match self.next() {
            Token::Punct("(") => {
                let p = self.path_alt()?;
                self.expect(&Token::Punct(")"))?;
                p
            }
            Token::Ident(a) if a == "a" => Path::Pred("type".into()),
            Token::Ident(p) | Token::Text(p) | Token::Number(p) => Path::Pred(p),
            Token::Keyword(k @ ("true" | "false")) => Path::Pred(k.into()),
            // A key or an id where a step starts.  (A key right after a step,
            // as in `knows/likes`, is more steps: see `path_seq`.)
            Token::Key(k) => Path::Key(k),
            Token::Id(id) => Path::Id(id),
            Token::Var(_) => {
                return self.syntax("a path expression's predicates must be constants");
            }
            t => return self.syntax(format!("expected a predicate in a path, found {t}")),
        };
        self.path_postfix(step)
    }

    /// `p` followed by any repetitions: `*`, `+`, `{m}`, `{m,}`, `{m,n}`.
    fn path_postfix(&mut self, mut p: Path) -> Result<Path> {
        loop {
            p = if self.punct("*") {
                Path::Repeat(Box::new(p), 0, None)
            } else if self.punct("+") || self.punct("++") {
                Path::Repeat(Box::new(p), 1, None)
            } else if self.punct("{") {
                let count = |parser: &mut Self| match parser.next() {
                    Token::Number(n) => n.parse::<u32>().ok(),
                    _ => None,
                };
                let Some(min) = count(self) else {
                    return self.syntax("expected a count in '{m,n}'");
                };
                let max = if self.punct(",") {
                    if self.is(&Token::Punct("}")) {
                        None
                    } else {
                        match count(self) {
                            Some(n) => Some(n),
                            None => return self.syntax("expected a count in '{m,n}'"),
                        }
                    }
                } else {
                    Some(min)
                };
                self.expect(&Token::Punct("}"))?;
                Path::Repeat(Box::new(p), min, max)
            } else {
                return Ok(p);
            };
        }
    }

    /// A predicate: a node, or `a` for the value `type`.
    fn predicate(&mut self, out: &mut Vec<Pattern>) -> Result<Node> {
        match self.peek() {
            Token::Ident(a) if a == "a" => {
                self.next();
                Ok(Node::Value("type".into()))
            }
            _ => self.node(out),
        }
    }

    /// An object: a node; a test of the object in parentheses, its left
    /// side elided (`(< 40)`, `(~"Bob.*")`, `(starts("Bo"))`, or `( :?x
    /// expr )`, the object equal to the expression); or, for a predicate's
    /// `first` object but in a template, nothing: a fresh variable.
    fn object(&mut self, out: &mut Vec<Pattern>, first: bool) -> Result<Node> {
        if self.punct("(") {
            let node = if self.punct(":") {
                Node::Var(self.var()?)
            } else {
                Node::Blank(self.fresh())
            };
            let this = Expr::Node(node.clone());
            let test = if let Some(cmp) = self.comparison() {
                self.next();
                Pattern::Compare(this, cmp, self.expr()?)
            } else if self.punct("~") {
                Pattern::Test(Expr::Call(Func::Matches, vec![this, self.expr()?]))
            } else {
                // A test whose first argument is the object, or a value it
                // equals.
                self.implied_arg = matches!(
                    (self.peek(), self.tokens.get(self.pos + 1).map(|t| &t.token)),
                    (Token::Ident(_), Some(Token::Punct("(")))
                );
                let e = self.expr();
                let implied = std::mem::take(&mut self.implied_arg);
                match e? {
                    Expr::Call(f, mut args) if f.is_test() && !implied => {
                        args.insert(0, this);
                        Pattern::Test(Expr::Call(f, args))
                    }
                    e => Pattern::Compare(this, Comparison::Eq, e),
                }
            };
            self.expect(&Token::Punct(")"))?;
            out.push(test);
            return Ok(node);
        }
        if !self.starts_node() {
            if self.in_template || !first {
                return self.syntax(format!("expected an object, found {}", self.peek()));
            }
            // A predicate with no object: any object.
            self.short_end = self.is(&Token::End);
            return Ok(Node::Blank(self.fresh()));
        }
        self.node(out)
    }

    fn starts_node(&self) -> bool {
        matches!(
            self.peek(),
            Token::Var(_)
                | Token::Label(_)
                | Token::Key(_)
                | Token::Id(_)
                | Token::Param(_)
                | Token::Ident(_)
                | Token::Text(_)
                | Token::Number(_)
                | Token::Punct("[" | "<<")
                | Token::Keyword("true" | "false" | "list")
        )
    }

    fn fresh(&mut self) -> String {
        self.blanks += 1;
        format!("[{}]", self.blanks)
    }

    /// A node; a blank node's predicates are added to `out`.
    fn node(&mut self, out: &mut Vec<Pattern>) -> Result<Node> {
        let (line, column) = self.here();
        match self.next() {
            Token::Var(v) if v.is_empty() => Ok(Node::Blank(self.fresh())),
            Token::Var(v) => Ok(Node::Var(v)),
            Token::Label(l) => Ok(Node::Blank(format!("_:{l}"))),
            Token::Key(k) => Ok(Node::Key(k)),
            Token::Id(id) => Ok(Node::Id(id)),
            Token::Param(p) => Ok(Node::Param(p)),
            Token::Ident(i) if self.rule_params.contains(&i) => Ok(Node::Var(i)),
            Token::Ident(i) if self.symbols.contains(&i) => Ok(Node::Symbol(i)),
            Token::Ident(i) | Token::Text(i) | Token::Number(i) => Ok(Node::Value(i)),
            Token::Keyword(k @ ("true" | "false")) => Ok(Node::Value(k.into())),
            Token::Keyword("list") => Err(Error::Unsupported {
                line,
                column,
                what: "lists".into(),
            }),
            Token::Punct("<<") => {
                let link = if self.punct(":") {
                    match self.next() {
                        Token::Var(v) if !v.is_empty() => Node::Var(v),
                        Token::Label(l) => Node::Blank(format!("_:{l}")),
                        t => {
                            return self
                                .syntax(format!("expected a variable or blank node, found {t}"));
                        }
                    }
                } else {
                    Node::Blank(self.fresh())
                };
                let subject = self.node(out)?;
                let predicate = self.predicate(out)?;
                let object = self.node(out)?;
                self.expect(&Token::Punct(">>"))?;
                out.push(Pattern::Link(link.clone(), subject, predicate, object));
                Ok(link)
            }
            Token::Punct("[") => {
                let node = if self.punct(":") {
                    match self.next() {
                        Token::Var(v) if !v.is_empty() => Node::Var(v),
                        t => return self.syntax(format!("expected a variable, found {t}")),
                    }
                } else {
                    Node::Blank(self.fresh())
                };
                if let Some(cmp) = self.comparison() {
                    // A comparison as a blank node: `[ :?x != y ]`.
                    self.next();
                    let other = self.expr()?;
                    out.push(Pattern::Compare(Expr::Node(node.clone()), cmp, other));
                } else if !self.is(&Token::Punct("]")) {
                    self.predicates(&node, out)?;
                }
                self.expect(&Token::Punct("]"))?;
                Ok(node)
            }
            t => Err(Error::Syntax {
                line,
                column,
                message: format!("expected a node, found {t}"),
            }),
        }
    }

    /// `template: subjects`, of triples only.
    fn template(&mut self) -> Result<Vec<Pattern>> {
        if self.is(&Token::Keyword("graph")) || self.is(&Token::Punct("(")) {
            return self.unsupported("a graph template");
        }
        let (line, column) = self.here();
        let mut triples = Vec::new();
        self.in_template = true;
        let subjects = self.subjects();
        self.in_template = false;
        for p in subjects? {
            flatten(p, &mut triples).map_err(|what| Error::Unsupported {
                line,
                column,
                what: what.into(),
            })?;
        }
        Ok(triples)
    }
}

/// The triples and links of a template pattern.
fn flatten(p: Pattern, out: &mut Vec<Pattern>) -> std::result::Result<(), &'static str> {
    match p {
        p @ (Pattern::Triple(..) | Pattern::Tagged(..) | Pattern::Link(..)) => out.push(p),
        // A template rule's triples, expanded when the statement runs.
        p @ Pattern::Call(..) => out.push(p),
        Pattern::And(ps) => {
            for p in ps {
                flatten(p, out)?;
            }
        }
        Pattern::Assert(..) => return Err("'!' in a template"),
        Pattern::LinkAggregate { .. } => return Err("an aggregate in a template"),
        _ => return Err("a template other than triples"),
    }
    Ok(())
}

/// GraQL's keywords, in lower case.  They are not case sensitive, and a
/// value whose data is one must be written as a string.
pub fn keywords() -> &'static [&'static str] {
    KEYWORDS
}

/// Whether `text` is the start of GraQL statements that more text could
/// complete: it fails to parse only because it ends too soon, in a
/// statement, string or comment.  Text that parses, or that has an error
/// before its end, is not.
pub fn is_incomplete(text: &str) -> bool {
    let tokens = match lex(text) {
        Ok(tokens) => tokens,
        Err(Error::Syntax { message, .. }) => return message.starts_with("unterminated"),
        Err(_) => return false,
    };
    let end = tokens.last().map(|t| (t.line, t.column));
    let mut parser = Parser {
        tokens,
        pos: 0,
        blanks: 0,
        in_template: false,
        short_end: false,
        limit_at: (0, 0),
        implied_arg: false,
        symbols: HashSet::new(),
        source: None,
        optional_default: false,
        rule_params: HashSet::new(),
    };
    while !parser.is(&Token::End) {
        if parser.keyword("quit") {
            return false;
        }
        match parser.statement() {
            Ok(_) => {}
            Err(Error::Syntax { line, column, .. } | Error::Unsupported { line, column, .. }) => {
                return Some((line, column)) == end;
            }
            Err(_) => return false,
        }
    }
    false
}

/// Whether `text` parses, but ends with a subject alone or a predicate with
/// no object (`lookup ?subjects`, `?s type`): complete, but more likely to be
/// continued on the next line when typed.  The shell waits for a blank line
/// to run such a statement.
pub fn ends_short(text: &str) -> bool {
    let Ok(tokens) = lex(text) else {
        return false;
    };
    let mut parser = Parser {
        tokens,
        pos: 0,
        blanks: 0,
        in_template: false,
        short_end: false,
        limit_at: (0, 0),
        implied_arg: false,
        symbols: HashSet::new(),
        source: None,
        optional_default: false,
        rule_params: HashSet::new(),
    };
    while !parser.is(&Token::End) {
        parser.short_end = false;
        if parser.statement().is_err() {
            return false;
        }
    }
    parser.short_end
}

/// Parses GraQL statements.
pub fn parse(text: &str) -> Result<Vec<Statement>> {
    Ok(parse_with_lines(text)?
        .into_iter()
        .map(|(_, s)| s)
        .collect())
}

/// Parses GraQL statements, with the line each starts on (from 1), to
/// report errors in running them.
pub fn parse_with_lines(text: &str) -> Result<Vec<(usize, Statement)>> {
    parse_with_symbols(text, std::iter::empty::<String>())
}

/// Parses GraQL statements, with the line each starts on, reading the
/// identifiers in `symbols` (and those `SET` names) as symbols: see
/// [`Session::symbols`].
pub fn parse_with_symbols(
    text: &str,
    symbols: impl IntoIterator<Item = impl Into<String>>,
) -> Result<Vec<(usize, Statement)>> {
    let mut parser = Parser {
        tokens: lex(text)?,
        pos: 0,
        blanks: 0,
        in_template: false,
        short_end: false,
        limit_at: (0, 0),
        implied_arg: false,
        symbols: symbols.into_iter().map(Into::into).collect(),
        source: None,
        optional_default: false,
        rule_params: HashSet::new(),
    };
    let mut statements = Vec::new();
    while !parser.is(&Token::End) {
        if parser.keyword("quit") {
            break;
        }
        let (line, _) = parser.here();
        statements.push((line, parser.statement()?));
    }
    Ok(statements)
}

/// Builds queries from parsed patterns, against a graph.
struct Lowering<'g> {
    g: &'g Graph,
    query: Query,
    vars: HashMap<String, Var>,
    relations: HashMap<String, Relation>,
    /// The id as of which the patterns being lowered see the graph, from an
    /// `as of` group.
    as_of: Option<u64>,
    /// The expressions of the `let`s of the pattern being lowered.
    lets: HashMap<String, Expr>,
    /// The parameters, by name: `$name`s, given values when the query runs.
    params: HashMap<String, Var>,
    /// The link variables of the patterns lowered while recording a
    /// lookup's links: of its pattern, but not of negations or `where`.
    recording: Option<Vec<Var>>,
    /// The names the statement's patterns bind by matching (not by a subject
    /// alone), and the relation of every node in a link, once defined.
    matched: BTreeSet<String>,
    node_relation: Option<Relation>,
    /// The subgraph of the patterns being lowered, from `( p ) in g`.
    graph: Option<Term>,
    /// Whether the pattern being lowered may assert triples (a lookup's, or
    /// a rule's body), and those it asserts.  Only at its top: `depth`
    /// counts the `or`s, `not`s, `optional`s, `as of`s and `in`s around.
    asserting: bool,
    asserts: Vec<Pattern>,
    depth: usize,
    /// The aggregates deriving links of the group being lowered.
    link_aggregates: Vec<Pattern>,
    /// Whether a statement's pattern is being lowered, which may have
    /// nested limits (a rule's body may not).
    limiting: bool,
    /// The names of the template rules, which define no relation.
    templates: BTreeSet<String>,
}

impl Lowering<'_> {
    /// Adds `nearest(P q k ?v ?d [metric])` to `group`: the `k` values of
    /// `P`'s links nearest the vector `q` (a constant, parameter or
    /// variable), as `?v`, and their distances, as `?d`.
    fn nearest(&mut self, group: &mut Group, args: &[Node]) -> Result<()> {
        let invalid = |what: &str| Error::Invalid(format!("nearest's {what}"));
        let (p, q, k, v, d, metric) = match args {
            [p, q, k, v, d] => (p, q, k, v, d, None),
            [p, q, k, v, d, m] => (p, q, k, v, d, Some(m)),
            _ => {
                return Err(Error::Invalid(
                    "nearest takes a predicate, a vector, k, a value's variable, a \
                     distance's and, if not cosine, a metric"
                        .into(),
                ));
            }
        };
        let Term::Id(predicate) = self.term(p) else {
            return Err(invalid("predicate must be a constant"));
        };
        let query = match q {
            Node::Blank(_) => return Err(invalid("vector can't be a blank node")),
            q => self.term(q),
        };
        let k = match k {
            Node::Value(k) => k.parse::<usize>().ok().filter(|&k| k > 0),
            _ => None,
        }
        .ok_or_else(|| invalid("k must be a number above 0"))?;
        let mut var = |n: &Node, what: &str| match n {
            Node::Var(name) | Node::Blank(name) => Ok(self.var(name)),
            _ => Err(invalid(&format!("{what} must be a variable"))),
        };
        let (value, distance) = (var(v, "value")?, var(d, "distance")?);
        // The index's, if the predicate has one.
        let metric = match metric {
            None => self
                .g
                .vector_index(predicate)
                .map_or(crate::vector::Metric::Cosine, |i| i.metric),
            Some(Node::Value(m) | Node::Symbol(m)) => crate::vector::Metric::of_name(m)
                .ok_or_else(|| invalid("metric must be cosine, l2 or dot"))?,
            Some(_) => return Err(invalid("metric must be cosine, l2 or dot")),
        };
        let mut search =
            crate::query::Nearest::new(predicate, query, k, value, distance).metric(metric);
        search.as_of = self.as_of;
        let subject = self
            .query
            .var(&format!("nearest subject {}", self.vars.len()));
        group.nearest(search, subject);
        Ok(())
    }

    /// The hold of the query's ephemeral ids.
    fn hold(&self) -> &EphemeralHold {
        self.query.ephemeral_hold().expect("a hold")
    }

    /// The aggregates deriving links collected, for the query.
    fn link_aggregates(&mut self) -> Result<Vec<crate::query::LinkAggregate>> {
        use crate::query::{LinkAggregate, LinkFunction};
        let mut out = Vec::new();
        for p in std::mem::take(&mut self.link_aggregates) {
            let Pattern::LinkAggregate {
                name,
                distinct,
                triple,
                descending,
                scope,
            } = p
            else {
                unreachable!("an aggregate")
            };
            let (s, pr, o) = *triple;
            let mut var = |n: &Node, what: &str| match n {
                Node::Var(v) | Node::Blank(v) => Ok(self.var(v)),
                _ => Err(Error::Invalid(format!(
                    "an aggregate's {what} must be a variable"
                ))),
            };
            let (s, o) = (var(&s, "subject")?, var(&o, "object")?);
            let predicate = match &pr {
                Node::Value(v) => self.hold().value(self.g, v.as_bytes()),
                Node::Key(k) => self.hold().key(self.g, k.as_bytes()),
                Node::Id(id) => *id,
                _ => {
                    return Err(Error::Invalid(
                        "an aggregate's predicate must be a constant".into(),
                    ));
                }
            };
            let (function, reversed) = match name.as_str() {
                "ORDER" | "RANK" => (LinkFunction::Rank, false),
                "REVERSE_ORDER" => (LinkFunction::Rank, true),
                "DENSE_RANK" => (LinkFunction::DenseRank, false),
                "ROW_NUMBER" => (LinkFunction::RowNumber, false),
                f => (
                    LinkFunction::Fold(Function::of_name(f).expect("an aggregate's name")),
                    false,
                ),
            };
            if descending && matches!(function, LinkFunction::Fold(_)) {
                return Err(Error::Invalid(format!(
                    "{name} has no order: 'desc' is for rankings"
                )));
            }
            let mut a = LinkAggregate::new(function, s, predicate, o);
            if distinct {
                a = a.distinct();
            }
            if descending != reversed {
                a = a.desc();
            }
            if let Some(v) = scope {
                if !self.matched.contains(&v) {
                    return Err(Error::Invalid(format!(
                        "the scope ?{v} isn't bound by the pattern"
                    )));
                }
                a = a.scope(self.var(&v));
            }
            out.push(a);
        }
        Ok(out)
    }

    /// The asserted triples collected, for the query.  Their variables must
    /// be bound by the pattern (or, for a rule, be its head's); their other
    /// blank nodes are new nodes.
    fn assertions(&mut self) -> Result<Vec<crate::query::Assertion>> {
        use crate::query::{Assertion, TemplateNode};
        let mut out = Vec::new();
        for p in std::mem::take(&mut self.asserts) {
            let Pattern::Assert(triples, scope) = p else {
                unreachable!("an assertion")
            };
            let mut blanks: HashMap<String, u32> = HashMap::new();
            let mut nodes = Vec::new();
            for t in &triples {
                let Pattern::Triple(s, pr, o) = t else {
                    unreachable!("asserted triples")
                };
                let mut fields = Vec::new();
                for n in [s, pr, o] {
                    fields.push(match n {
                        Node::Var(v) | Node::Blank(v) if self.matched.contains(v) => {
                            TemplateNode::from(self.var(v))
                        }
                        Node::Var(v) => {
                            return Err(Error::Invalid(format!(
                                "?{v} in an asserted triple isn't bound by the pattern"
                            )));
                        }
                        Node::Blank(b) => {
                            let next = blanks.len() as u32;
                            TemplateNode::Blank(*blanks.entry(b.clone()).or_insert(next))
                        }
                        Node::Key(k) => TemplateNode::Key(k.clone()),
                        Node::Value(v) | Node::Symbol(v) => {
                            TemplateNode::Value(Cell::Text(v.clone()))
                        }
                        Node::Id(id) => TemplateNode::from(*id),
                        Node::Param(name) => TemplateNode::from(self.param(name)),
                    });
                }
                nodes.push([fields[0].clone(), fields[1].clone(), fields[2].clone()]);
            }
            let mut a = Assertion {
                triples: nodes,
                scope: None,
            };
            if let Some(v) = scope {
                if !self.matched.contains(&v) {
                    return Err(Error::Invalid(format!(
                        "the scope ?{v} isn't bound by the pattern"
                    )));
                }
                a = a.scope(self.var(&v));
            }
            out.push(a);
        }
        Ok(out)
    }
    /// Adds a pattern to `group`, binding a link variable if recording.
    fn triple(&mut self, group: &mut Group, s: Term, p: Term, o: Term) {
        match &mut self.recording {
            Some(recorded) => {
                let link = self.query.hidden_var(&format!("_link{}", recorded.len()));
                recorded.push(link);
                group.link_pattern(link, s, p, o);
            }
            None => {
                group.pattern(s, p, o);
            }
        }
        if let Some(id) = self.as_of {
            group.at(id);
        }
        if let Some(g) = self.graph {
            group.in_subgraph(g);
        }
    }

    /// Marks the rows the last path added to `group` matches, if recording,
    /// so that its derived links are returned.
    fn mark_path(&mut self, group: &mut Group) {
        if self.recording.is_some() {
            let v = self.query.hidden_var(&format!("_path{}", self.vars.len()));
            group.mark_path(v);
        }
    }

    /// The relation of every node in a link: `_node(?x)` holds if `?x` is a
    /// link's subject or object.
    fn node_relation(&mut self) -> Relation {
        if let Some(r) = self.node_relation {
            return r;
        }
        let r = self.query.relation("_node", 1);
        for subject in [true, false] {
            let (x, p, o) = (
                self.query.var("[node]"),
                self.query.var("[node p]"),
                self.query.var("[node o]"),
            );
            let mut body = Group::new();
            if subject {
                body.pattern(x, p, o);
            } else {
                body.pattern(o, p, x);
            }
            self.query.rule(r, [x], body);
        }
        self.node_relation = Some(r);
        r
    }

    fn param(&mut self, name: &str) -> Var {
        if let Some(&v) = self.params.get(name) {
            return v;
        }
        let v = self.query.param(name);
        self.params.insert(name.to_string(), v);
        v
    }

    /// Whether `e` is a node that is not a `let`: compared as an entry.
    fn plain(&self, e: &Expr) -> bool {
        matches!(e, Expr::Node(n) if !matches!(n, Node::Var(v) if self.lets.contains_key(v)))
    }

    /// `e` compiled to read a row of the query being built, with `let`s
    /// expanded.
    fn compile(&mut self, e: &Expr) -> Result<QExpr> {
        self.compile_within(e, 0)
    }

    fn compile_within(&mut self, e: &Expr, depth: usize) -> Result<QExpr> {
        if depth > 32 {
            return Err(Error::Invalid(
                "the lets refer to each other in a cycle".into(),
            ));
        }
        Ok(match e {
            Expr::Node(Node::Var(v)) if self.lets.contains_key(v) => {
                let e = self.lets[v].clone();
                self.compile_within(&e, depth + 1)?
            }
            Expr::Node(n @ (Node::Var(_) | Node::Blank(_))) => match self.term(n) {
                Term::Var(v) => QExpr::Var(v),
                Term::Id(_) => unreachable!("a variable"),
            },
            Expr::Node(Node::Param(name)) => QExpr::Var(self.param(name)),
            Expr::Node(n) => QExpr::Const(const_cell(self.g, n)),
            Expr::Call(f, args) => QExpr::Call(
                *f,
                args.iter()
                    .map(|a| self.compile_within(a, depth))
                    .collect::<Result<_>>()?,
            ),
            Expr::Binary(op, a, b) => QExpr::Binary(
                *op,
                Box::new(self.compile_within(a, depth)?),
                Box::new(self.compile_within(b, depth)?),
            ),
            Expr::Neg(a) => QExpr::Neg(Box::new(self.compile_within(a, depth)?)),
            Expr::Compare(a, cmp, b) => {
                let a = self.compile_within(a, depth)?;
                a.compare(*cmp, self.compile_within(b, depth)?)
            }
            Expr::And(a, b) => QExpr::And(vec![
                self.compile_within(a, depth)?,
                self.compile_within(b, depth)?,
            ]),
            Expr::Or(a, b) => QExpr::Or(vec![
                self.compile_within(a, depth)?,
                self.compile_within(b, depth)?,
            ]),
            Expr::If(c, a, b) => QExpr::If(
                Box::new(self.compile_within(c, depth)?),
                Box::new(self.compile_within(a, depth)?),
                Box::new(self.compile_within(b, depth)?),
            ),
            Expr::Apply(name, _) => {
                return Err(Error::Invalid(format!("{name} is not a function")));
            }
        })
    }

    fn var(&mut self, name: &str) -> Var {
        if let Some(&v) = self.vars.get(name) {
            return v;
        }
        // A blank node's variable (`[1]`, `_:x`, a path's step) is
        // existential: the branches of a union needn't share it.
        let v = if name.starts_with('[') || name.starts_with("_:") {
            self.query.hidden_var(name)
        } else {
            self.query.var(name)
        };
        self.vars.insert(name.to_string(), v);
        v
    }

    /// The term for `node`; a constant missing from the graph is 0, which
    /// matches nothing.
    fn term(&mut self, node: &Node) -> Term {
        match node {
            Node::Var(v) | Node::Blank(v) => Term::Var(self.var(v)),
            // A key the graph doesn't have may be an aggregate's predicate:
            // an ephemeral one.
            Node::Key(k) => Term::Id(self.hold().key(self.g, k.as_bytes())),
            // A value the graph doesn't have may be computed (a count, a
            // rank), and compared by id: an ephemeral value.
            Node::Value(v) | Node::Symbol(v) => Term::Id(self.hold().value(self.g, v.as_bytes())),
            Node::Id(id) => Term::Id(*id),
            // Given when the query runs, unless bound before lowering (see
            // `Statement::bind`).
            Node::Param(name) => Term::Var(self.param(name)),
        }
    }

    /// Joins the patterns of a path of steps and inverses from `s` to `o`,
    /// through new variables.
    fn chain(&mut self, group: &mut Group, s: Term, path: &Path, o: Term) {
        match path {
            Path::Pred(p) => {
                let p = self.term(&Node::Value(p.clone()));
                self.triple(group, s, p, o);
            }
            Path::Key(k) => {
                let p = self.term(&Node::Key(k.clone()));
                self.triple(group, s, p, o);
            }
            Path::Id(id) => self.triple(group, s, Term::Id(*id), o),
            Path::Inverse(p) => self.chain(group, o, p, s),
            Path::Seq(ps) => {
                let mut from = s;
                for (i, p) in ps.iter().enumerate() {
                    let to = if i + 1 == ps.len() {
                        o
                    } else {
                        // A name no variable of the text can have.
                        let name = format!("[path {}]", self.vars.len());
                        Term::Var(self.var(&name))
                    };
                    self.chain(group, from, p, to);
                    from = to;
                }
            }
            Path::Alt(_) | Path::Repeat(..) => unreachable!("not a chain"),
        }
    }

    fn group(&mut self, p: &Pattern) -> Result<Group> {
        let mut group = Group::new();
        self.add(&mut group, p)?;
        Ok(group)
    }

    fn add(&mut self, group: &mut Group, p: &Pattern) -> Result<()> {
        let nested = matches!(
            p,
            Pattern::Or(_)
                | Pattern::Not(_)
                | Pattern::Optional(_)
                | Pattern::AsOf(..)
                | Pattern::InGraph(..)
        );
        if nested {
            self.depth += 1;
        }
        let added = self.add_within(group, p);
        if nested {
            self.depth -= 1;
        }
        added
    }

    fn add_within(&mut self, group: &mut Group, p: &Pattern) -> Result<()> {
        match p {
            Pattern::Assert(..) if !self.asserting => {
                return Err(Error::Invalid(
                    "'!' asserts triples only in a lookup's pattern or a rule's body".into(),
                ));
            }
            Pattern::Assert(..) if self.depth > 0 => {
                return Err(Error::Invalid(
                    "'!' can't be within 'or', 'not', 'optional', 'as of' or 'in'".into(),
                ));
            }
            Pattern::Assert(..) => self.asserts.push(p.clone()),
            Pattern::LinkAggregate { .. } if self.depth > 0 => {
                return Err(Error::Invalid(
                    "an aggregate can't be within 'or', 'not', 'optional', 'as of' or 'in'".into(),
                ));
            }
            Pattern::LinkAggregate { .. } => self.link_aggregates.push(p.clone()),
            Pattern::LimitValues { .. } if self.depth > 0 || !self.limiting => {
                return Err(Error::Invalid(
                    "a nested limit is at the top of a statement's pattern".into(),
                ));
            }
            Pattern::LimitValues { var, limit, offset } => {
                let v = self.var(var);
                self.query.limit_values(v, *limit, *offset);
            }
            Pattern::Triple(s, pr, o) => {
                let (s, pr, o) = (self.term(s), self.term(pr), self.term(o));
                self.triple(group, s, pr, o);
            }
            Pattern::Tagged(s, pr, o, author, graph) => {
                let (s, pr, o) = (self.term(s), self.term(pr), self.term(o));
                self.triple(group, s, pr, o);
                if let Some(a) = author {
                    let a = self.term(a);
                    group.by_author(a);
                }
                if let Some(g) = graph {
                    let g = self.term(g);
                    group.in_subgraph(g);
                }
            }
            Pattern::InGraph(p, g) => {
                let g = self.term(g);
                let outer = self.graph.replace(g);
                let added = self.add(group, p);
                self.graph = outer;
                added?;
            }
            // A subject alone: nothing, if the statement binds it, else
            // every node in a link.
            Pattern::Node(n) => {
                let bound = matches!(n, Node::Var(v) | Node::Blank(v) if self.matched.contains(v));
                if !bound {
                    let r = self.node_relation();
                    let t = self.term(n);
                    group.call(r, [t]);
                }
            }
            Pattern::Link(link, s, pr, o) => {
                let Term::Var(link) = self.term(link) else {
                    unreachable!("a link is bound to a variable or blank node")
                };
                if let Some(recorded) = &mut self.recording {
                    recorded.push(link);
                }

                let (s, pr, o) = (self.term(s), self.term(pr), self.term(o));
                group.link_pattern(link, s, pr, o);
                if let Some(id) = self.as_of {
                    group.at(id);
                }
                if let Some(g) = self.graph {
                    group.in_subgraph(g);
                }
            }
            Pattern::Path(..) | Pattern::PathExpr(..) if self.graph.is_some() && !is_chain(p) => {
                return Err(Error::Unsupported {
                    line: 0,
                    column: 0,
                    what: "a path within a subgraph".into(),
                });
            }
            Pattern::Path(s, pr, o) => {
                let (s, pr, o) = (self.term(s), self.term(pr), self.term(o));
                group.path(s, pr, o);
                if let Some(id) = self.as_of {
                    group.path_at(id);
                }
                self.mark_path(group);
            }
            Pattern::PathExpr(s, path, o) => {
                let (s, o) = (self.term(s), self.term(o));
                if path.is_chain() {
                    self.chain(group, s, path, o);
                } else {
                    group.path_expr(s, path.resolve(self.g), o);
                    if let Some(id) = self.as_of {
                        group.path_at(id);
                    }
                    self.mark_path(group);
                }
            }
            Pattern::AsOf(p, as_of) => {
                let outer = self.as_of.replace(as_of.id(self.g));
                let result = self.add(group, p);
                self.as_of = outer;
                result?;
            }
            // The built-in search, unless a rule has its name.
            Pattern::Call(name, args)
                if name.eq_ignore_ascii_case("nearest") && !self.relations.contains_key(name) =>
            {
                self.nearest(group, args)?;
            }
            Pattern::Call(_, _) if self.as_of.is_some() => {
                return Err(Error::Invalid(
                    "a rule can't be called in an 'as of' group: give the statement 'as of'".into(),
                ));
            }
            Pattern::Call(name, _) if self.templates.contains(name) => {
                return Err(Error::Invalid(format!(
                    "{name} only asserts triples: call it in a template, as 'insert {name}(…)'"
                )));
            }
            Pattern::Call(name, args) => {
                let relation = *self.relations.get(name).ok_or_else(|| Error::Syntax {
                    line: 0,
                    column: 0,
                    message: format!("unknown rule '{name}'"),
                })?;
                let args: Vec<Term> = args.iter().map(|a| self.term(a)).collect();
                group.call(relation, args);
                if self.recording.is_some() {
                    let v = self.query.hidden_var(&format!("_call{}", self.vars.len()));
                    group.mark_call(v);
                }
            }
            // An expression, or a parameter's value compared by its data,
            // which need not be in the graph.
            Pattern::Compare(a, cmp, b)
                if !self.plain(a)
                    || !self.plain(b)
                    || (matches!(cmp, Comparison::Data(_)) && (is_param(a) || is_param(b))) =>
            {
                let (a, b) = (self.compile(a)?, self.compile(b)?);
                group.filter(Filter::Holds(a.compare(*cmp, b)));
            }
            Pattern::Test(e) => {
                let e = self.compile(e)?;
                group.filter(Filter::Holds(e));
            }
            Pattern::Let(..) => {}
            // A negation of only comparisons and tests is a filter.
            Pattern::Not(inner) if only_tests(inner) => {
                let mut tests = Vec::new();
                tests_of(inner, &mut tests);
                let compiled: Vec<QExpr> = tests
                    .iter()
                    .map(|t| -> Result<_> {
                        Ok(match t {
                            Pattern::Compare(a, cmp, b) => {
                                self.compile(a)?.compare(*cmp, self.compile(b)?)
                            }
                            Pattern::Test(e) => self.compile(e)?,
                            _ => unreachable!("a test"),
                        })
                    })
                    .collect::<Result<_>>()?;
                group.filter(Filter::Holds(QExpr::And(compiled).not()));
            }
            Pattern::Compare(a, cmp, b) => {
                let (Expr::Node(a), Expr::Node(b)) = (a, b) else {
                    unreachable!("plain")
                };
                let (ta, tb) = (self.term(a), self.term(b));
                let filter = match (*cmp, ta, b, tb, a) {
                    (Comparison::Eq, ..) => Filter::Eq(ta, tb),
                    (Comparison::Ne, ..) => Filter::Ne(ta, tb),
                    // A variable and a value's data, which need not be in
                    // the graph.
                    (Comparison::Data(c), Term::Var(v), Node::Value(data), ..)
                    | (Comparison::Data(c), _, _, Term::Var(v), Node::Value(data)) => {
                        let c = if matches!(ta, Term::Var(_)) {
                            c
                        } else {
                            flip(c)
                        };
                        let data = data.clone().into_bytes();
                        Filter::Test(
                            v,
                            Arc::new(move |g: &Graph, id| {
                                g.data(id).is_some_and(|d| c.holds(compare_data(&d, &data)))
                            }),
                        )
                    }
                    (Comparison::Data(c), ..) => Filter::Compare(ta, c, tb),
                };
                group.filter(filter);
            }
            Pattern::And(ps) => {
                for p in ps {
                    self.add(group, p)?;
                }
            }
            Pattern::Or(ps) => {
                let branches: Vec<Group> =
                    ps.iter().map(|p| self.group(p)).collect::<Result<_>>()?;
                group.union(branches);
            }
            Pattern::Not(p) => {
                // A negation's links are no row's.
                let recording = self.recording.take();
                let not = self.group(p);
                self.recording = recording;
                group.not(not?);
            }
            Pattern::Optional(p) => {
                let optional = self.group(p)?;
                group.optional(optional);
            }
        }
        Ok(())
    }
}

/// The comparison with its sides swapped.
fn flip(c: Cmp) -> Cmp {
    match c {
        Cmp::Lt => Cmp::Gt,
        Cmp::Le => Cmp::Ge,
        Cmp::Gt => Cmp::Lt,
        Cmp::Ge => Cmp::Le,
    }
}

/// The query of a pattern, with `rules`, and its variables by name.
fn lower(g: &Graph, rules: &[RuleDef], p: &Pattern) -> Result<(Query, HashMap<String, Var>)> {
    let (q, vars, _) = lower_full(g, rules, p, None, false)?;
    Ok((q, vars))
}

/// `p`, and `where_` if given, lowered to one query; if `record`, with the
/// link variables of `p`'s patterns, which record its links.
fn lower_full(
    g: &Graph,
    rules: &[RuleDef],
    p: &Pattern,
    where_: Option<&Pattern>,
    record: bool,
) -> Result<(Query, HashMap<String, Var>, Vec<Var>)> {
    let whole = match where_ {
        Some(w) => Pattern::And(vec![p.clone(), w.clone()]),
        None => p.clone(),
    };
    // Only the rules the pattern calls, directly or through other rules: a
    // rule that cannot be evaluated affects only the queries that use it.
    let reached = reachable(rules, &whole);
    let rules: Vec<&RuleDef> = rules.iter().filter(|r| reached.contains(&r.name)).collect();
    let mut query = Query::new();
    // The constants it doesn't have, and the values it computes, are held by
    // the query, and freed with it.
    query.hold_ephemeral(g.ephemeral_hold());
    let mut l = Lowering {
        g,
        query,
        vars: HashMap::new(),
        relations: HashMap::new(),
        as_of: None,
        lets: HashMap::new(),
        params: HashMap::new(),
        recording: None,
        matched: BTreeSet::new(),
        node_relation: None,
        graph: None,
        asserting: false,
        asserts: Vec::new(),
        depth: 0,
        link_aggregates: Vec::new(),
        limiting: false,
        templates: BTreeSet::new(),
    };
    // A template rule defines no relation: its calls in templates are its
    // triples (see `expand_calls`).
    l.templates = rules
        .iter()
        .filter(|r| r.is_template())
        .map(|r| r.name.clone())
        .collect();
    let rules: Vec<&RuleDef> = rules.into_iter().filter(|r| !r.is_template()).collect();
    for &rule in &rules {
        if !l.relations.contains_key(&rule.name) {
            let r = l.query.relation(&rule.name, rule.head.len());
            l.relations.insert(rule.name.clone(), r);
        }
    }
    for &rule in &rules {
        l.lets = lets(&rule.body)?;
        l.matched = names_bound(&rule.body, true, false).into_iter().collect();
        l.asserting = true;
        let body = l.group(&rule.body)?;
        l.asserting = false;
        let head: Vec<Var> = rule.head.iter().map(|v| l.var(v)).collect();
        l.query.rule(l.relations[&rule.name], head, body);
        for a in l.assertions()? {
            l.query.rule_asserts(a);
        }
        for a in l.link_aggregates()? {
            l.query.rule_link_aggregate(a);
        }
    }
    // The pattern's variables are its own, not the rules'.
    l.vars.clear();
    l.lets = lets(&whole)?;
    l.matched = names_bound(&whole, true, false).into_iter().collect();
    let mut root = Group::new();
    l.recording = record.then(Vec::new);
    // A lookup's pattern may assert triples; its `where` doesn't.
    l.asserting = record;
    l.limiting = true;
    l.add(&mut root, p)?;
    l.asserting = false;
    for a in l.assertions()? {
        l.query.assert(a);
    }
    let recorded = l.recording.take().unwrap_or_default();
    if let Some(w) = where_ {
        l.add(&mut root, w)?;
    }
    for a in l.link_aggregates()? {
        l.query.link_aggregate(a);
    }
    *l.query.group() = root;
    Ok((l.query, l.vars, recorded))
}

/// Whether `p` is a path expression that is a chain of steps, matched by
/// joining patterns.
fn is_chain(p: &Pattern) -> bool {
    matches!(p, Pattern::PathExpr(_, path, _) if path.is_chain())
}

/// Whether `e` is a parameter.
fn is_param(e: &Expr) -> bool {
    matches!(e, Expr::Node(Node::Param(_)))
}

/// The value of a constant node in an expression.
fn const_cell(g: &Graph, n: &Node) -> Cell {
    match n {
        Node::Key(k) => g.find_by_key(k.as_bytes()).map_or(Cell::Unbound, Cell::Id),
        Node::Value(v) | Node::Symbol(v) => Cell::Text(v.clone()),
        Node::Id(id) => Cell::of_id(*id),
        Node::Var(_) | Node::Blank(_) | Node::Param(_) => Cell::Unbound,
    }
}

/// Whether `p` is only comparisons, tests and `let`s.
fn only_tests(p: &Pattern) -> bool {
    match p {
        Pattern::Compare(..) | Pattern::Test(_) => true,
        Pattern::And(ps) => {
            !ps.is_empty()
                && ps
                    .iter()
                    .all(|p| only_tests(p) || matches!(p, Pattern::Let(..)))
        }
        _ => false,
    }
}

/// The comparisons and tests of `p`, which [`only_tests`].
fn tests_of<'p>(p: &'p Pattern, out: &mut Vec<&'p Pattern>) {
    match p {
        Pattern::And(ps) => ps.iter().for_each(|p| tests_of(p, out)),
        Pattern::Compare(..) | Pattern::Test(_) => out.push(p),
        _ => {}
    }
}

/// The `let`s of `p`, by name, checking that each is defined once and is
/// not matched as an entry (in a triple, path, call or link).
fn lets(p: &Pattern) -> Result<HashMap<String, Expr>> {
    fn collect(p: &Pattern, out: &mut HashMap<String, Expr>) -> Result<()> {
        match p {
            Pattern::Let(v, e) => {
                if out.insert(v.clone(), e.clone()).is_some() {
                    return Err(Error::Invalid(format!("?{v} is let more than once")));
                }
            }
            Pattern::And(ps) | Pattern::Or(ps) => {
                for p in ps {
                    collect(p, out)?;
                }
            }
            Pattern::Not(p) | Pattern::AsOf(p, _) | Pattern::Optional(p) => collect(p, out)?,
            _ => {}
        }
        Ok(())
    }
    fn matched(p: &Pattern, lets: &HashMap<String, Expr>) -> Result<()> {
        let check = |n: &Node| match n {
            Node::Var(v) if lets.contains_key(v) => Err(Error::Invalid(format!(
                "?{v} is computed by 'let', so it can't be matched in a pattern"
            ))),
            _ => Ok(()),
        };
        match p {
            Pattern::Triple(a, b, c) | Pattern::Path(a, b, c) => {
                [a, b, c].into_iter().try_for_each(check)
            }
            Pattern::PathExpr(a, _, c) => [a, c].into_iter().try_for_each(check),
            Pattern::Link(l, a, b, c) => [l, a, b, c].into_iter().try_for_each(check),
            Pattern::Call(_, args) => args.iter().try_for_each(check),
            Pattern::Node(n) => check(n),
            Pattern::Tagged(a, b, c, author, graph) => [a, b, c]
                .into_iter()
                .chain(author)
                .chain(graph)
                .try_for_each(check),
            Pattern::And(ps) | Pattern::Or(ps) => ps.iter().try_for_each(|p| matched(p, lets)),
            Pattern::Not(p)
            | Pattern::AsOf(p, _)
            | Pattern::Optional(p)
            | Pattern::InGraph(p, _) => matched(p, lets),
            Pattern::Compare(..)
            | Pattern::Test(_)
            | Pattern::Let(..)
            | Pattern::Assert(..)
            | Pattern::LinkAggregate { .. }
            | Pattern::LimitValues { .. } => Ok(()),
        }
    }
    let mut out = HashMap::new();
    collect(p, &mut out)?;
    matched(p, &out)?;
    Ok(out)
}

/// The names of the rules `p` calls, and those they call, and so on.
fn reachable(rules: &[RuleDef], p: &Pattern) -> BTreeSet<String> {
    let mut reached = BTreeSet::new();
    let mut todo = Vec::new();
    calls(p, &mut todo);
    while let Some(name) = todo.pop() {
        if reached.insert(name.clone()) {
            for rule in rules.iter().filter(|r| r.name == name) {
                calls(&rule.body, &mut todo);
            }
        }
    }
    reached
}

/// The names of the rules `p` calls.
fn calls(p: &Pattern, out: &mut Vec<String>) {
    match p {
        Pattern::Call(name, _) => out.push(name.clone()),
        Pattern::And(ps) | Pattern::Or(ps) => ps.iter().for_each(|p| calls(p, out)),
        Pattern::Not(p) | Pattern::AsOf(p, _) | Pattern::Optional(p) => calls(p, out),
        Pattern::InGraph(p, _) => calls(p, out),
        Pattern::Triple(..)
        | Pattern::Tagged(..)
        | Pattern::Node(_)
        | Pattern::Path(..)
        | Pattern::PathExpr(..)
        | Pattern::Link(..)
        | Pattern::Compare(..)
        | Pattern::Test(_)
        | Pattern::Let(..)
        | Pattern::Assert(..)
        | Pattern::LinkAggregate { .. }
        | Pattern::LimitValues { .. } => {}
    }
}

/// The names of the variables (and blank nodes, if `blanks`) every row of
/// a pattern binds, in order of appearance.
fn bound_names(p: &Pattern, blanks: bool) -> Vec<String> {
    names_bound(p, blanks, true)
}

/// The same, with the nodes of subjects alone (`Pattern::Node`) if `nodes`.
fn names_bound(p: &Pattern, blanks: bool, nodes: bool) -> Vec<String> {
    fn collect(p: &Pattern, blanks: bool, nodes: bool, out: &mut Vec<String>) {
        let collect = |p: &Pattern, out: &mut Vec<String>| collect(p, blanks, nodes, out);
        match p {
            Pattern::Node(n) if nodes => match n {
                Node::Var(name) => out.push(name.clone()),
                Node::Blank(name) if blanks => out.push(name.clone()),
                _ => {}
            },
            Pattern::Node(_) => {}
            Pattern::InGraph(p, _) => collect(p, out),
            Pattern::Tagged(s, pr, o, author, graph) => {
                for n in [s, pr, o].into_iter().chain(author).chain(graph) {
                    match n {
                        Node::Var(name) => out.push(name.clone()),
                        Node::Blank(name) if blanks => out.push(name.clone()),
                        _ => {}
                    }
                }
            }
            Pattern::Triple(s, pr, o) | Pattern::Path(s, pr, o) => {
                for n in [s, pr, o] {
                    match n {
                        Node::Var(name) => out.push(name.clone()),
                        Node::Blank(name) if blanks => out.push(name.clone()),
                        _ => {}
                    }
                }
            }
            Pattern::Call(_, args) => {
                for n in args {
                    match n {
                        Node::Var(name) => out.push(name.clone()),
                        Node::Blank(name) if blanks => out.push(name.clone()),
                        _ => {}
                    }
                }
            }
            Pattern::Link(l, s, pr, o) => {
                for n in [l, s, pr, o] {
                    match n {
                        Node::Var(name) => out.push(name.clone()),
                        Node::Blank(name) if blanks => out.push(name.clone()),
                        _ => {}
                    }
                }
            }
            Pattern::AsOf(p, _) | Pattern::Optional(p) => collect(p, out),
            Pattern::PathExpr(s, _, o) => {
                for n in [s, o] {
                    match n {
                        Node::Var(name) => out.push(name.clone()),
                        Node::Blank(name) if blanks => out.push(name.clone()),
                        _ => {}
                    }
                }
            }
            Pattern::And(ps) => ps.iter().for_each(|p| collect(p, out)),
            // Every branch binds the same variables.
            Pattern::Or(ps) => {
                if let Some(p) = ps.first() {
                    collect(p, out);
                }
            }
            // An asserted triple matches nothing.
            Pattern::Compare(..)
            | Pattern::Test(_)
            | Pattern::Let(..)
            | Pattern::Not(_)
            | Pattern::Assert(..)
            | Pattern::LinkAggregate { .. }
            | Pattern::LimitValues { .. } => {}
        }
    }
    let mut names = Vec::new();
    collect(p, blanks, nodes, &mut names);
    let mut seen = BTreeSet::new();
    names.retain(|n| seen.insert(n.clone()));
    names
}

/// The positive triples of a pattern, as instantiated by its rows.
#[cfg(test)]
fn triples(p: &Pattern, out: &mut Vec<(Node, Node, Node)>) {
    match p {
        Pattern::Triple(s, pr, o) | Pattern::Link(_, s, pr, o) | Pattern::Tagged(s, pr, o, ..) => {
            out.push((s.clone(), pr.clone(), o.clone()))
        }
        Pattern::And(ps) | Pattern::Or(ps) => ps.iter().for_each(|p| triples(p, out)),
        Pattern::AsOf(p, _) | Pattern::Optional(p) | Pattern::InGraph(p, _) => triples(p, out),
        Pattern::Compare(..)
        | Pattern::Node(_)
        | Pattern::Test(_)
        | Pattern::Let(..)
        | Pattern::Not(_)
        | Pattern::Assert(..)
        | Pattern::LinkAggregate { .. }
        | Pattern::LimitValues { .. }
        | Pattern::Path(..)
        | Pattern::PathExpr(..)
        | Pattern::Call(..) => {}
    }
}

/// `e` resolved for evaluating: its variables (with `let`s expanded) are
/// those `var` names, and its parameters those of `params`.
fn compile_with(
    g: &Graph,
    e: &Expr,
    lets: &HashMap<String, Expr>,
    params: &HashMap<String, Var>,
    var: &mut dyn FnMut(&str) -> Result<Var>,
    depth: usize,
) -> Result<QExpr> {
    if depth > 32 {
        return Err(Error::Invalid(
            "the lets refer to each other in a cycle".into(),
        ));
    }
    Ok(match e {
        Expr::Node(Node::Var(v)) if lets.contains_key(v) => {
            compile_with(g, &lets[v], lets, params, var, depth + 1)?
        }
        Expr::Node(Node::Var(v) | Node::Blank(v)) => QExpr::Var(var(v)?),
        Expr::Node(Node::Param(p)) if params.contains_key(p) => QExpr::Var(params[p]),
        Expr::Node(n) => QExpr::Const(const_cell(g, n)),
        Expr::Call(f, args) => QExpr::Call(
            *f,
            args.iter()
                .map(|a| compile_with(g, a, lets, params, var, depth))
                .collect::<Result<_>>()?,
        ),
        Expr::Binary(op, a, b) => QExpr::Binary(
            *op,
            Box::new(compile_with(g, a, lets, params, var, depth)?),
            Box::new(compile_with(g, b, lets, params, var, depth)?),
        ),
        Expr::Neg(a) => QExpr::Neg(Box::new(compile_with(g, a, lets, params, var, depth)?)),
        Expr::Compare(a, cmp, b) => compile_with(g, a, lets, params, var, depth)?
            .compare(*cmp, compile_with(g, b, lets, params, var, depth)?),
        Expr::And(a, b) => QExpr::And(vec![
            compile_with(g, a, lets, params, var, depth)?,
            compile_with(g, b, lets, params, var, depth)?,
        ]),
        Expr::Or(a, b) => QExpr::Or(vec![
            compile_with(g, a, lets, params, var, depth)?,
            compile_with(g, b, lets, params, var, depth)?,
        ]),
        Expr::If(c, a, b) => QExpr::If(
            Box::new(compile_with(g, c, lets, params, var, depth)?),
            Box::new(compile_with(g, a, lets, params, var, depth)?),
            Box::new(compile_with(g, b, lets, params, var, depth)?),
        ),
        Expr::Apply(name, _) => {
            return Err(Error::Invalid(format!("{name} is not a function")));
        }
    })
}

impl Statement {
    /// The query of a `select`, `verify` or `lookup` statement against `g`,
    /// and the names of the variables its rows return.
    pub fn query(&self, g: &Graph) -> Result<(Query, Vec<String>)> {
        self.query_with(g, &[])
    }

    /// The same, with `rules` defined.
    pub fn query_with(&self, g: &Graph, rules: &[RuleDef]) -> Result<(Query, Vec<String>)> {
        let unsupported = |what: &str| Error::Unsupported {
            line: 0,
            column: 0,
            what: what.into(),
        };
        match self {
            Statement::Select {
                distinct,
                vars,
                having,
                by,
                pattern,
                limit,
                offset,
                as_of,
            } => {
                let (mut q, map) = lower(g, rules, pattern)?;
                if let Some(as_of) = as_of {
                    q.as_of(as_of.id(g));
                }
                let lookup = |name: &str| {
                    map.get(name).copied().ok_or_else(|| Error::Syntax {
                        line: 0,
                        column: 0,
                        message: format!("?{name} is not in the pattern"),
                    })
                };
                let lets = lets(pattern)?;
                let items = vars.as_deref().unwrap_or_default();
                // Whether a column is computed: an expression, a `let`, or
                // an aggregate.
                let computes = items.iter().any(|i| match i {
                    SelectItem::Var(v) => lets.contains_key(v),
                    _ => true,
                });
                let aggregated = items
                    .iter()
                    .any(|i| matches!(i, SelectItem::Aggregate { .. }));
                if !having.is_empty() && !aggregated {
                    return Err(Error::Invalid("'having' needs an aggregate".into()));
                }
                let names: Vec<String> = match vars {
                    Some(items) => items.iter().map(SelectItem::name).collect(),
                    None => bound_names(pattern, false),
                };
                // The parameters of the columns and `having`: the pattern's,
                // or new ones.
                let mut params: HashMap<String, Var> = q
                    .params()
                    .iter()
                    .map(|&v| (q.name(v).to_string(), v))
                    .collect();
                let mut names_of_params = Vec::new();
                let mut each = |n: &Node| {
                    if let Node::Param(p) = n {
                        names_of_params.push(p.clone());
                    }
                };
                for item in items {
                    match item {
                        SelectItem::Var(_) => {}
                        SelectItem::Expr { expr, .. } => expr.each_node(&mut each),
                        SelectItem::Aggregate { arg, .. } => {
                            arg.iter().for_each(|a| a.each_node(&mut each))
                        }
                    }
                }
                for (a, _, b) in having {
                    a.each_node(&mut each);
                    b.each_node(&mut each);
                }
                for p in names_of_params {
                    if let std::collections::hash_map::Entry::Vacant(e) = params.entry(p.clone()) {
                        e.insert(q.param(&p));
                    }
                }
                let selected: Vec<Var> = if computes {
                    let mut var = |v: &str| lookup(v);
                    let mut compile = |e: &Expr| compile_with(g, e, &lets, &params, &mut var, 0);
                    let mut columns = Vec::new();
                    for item in items {
                        columns.push(match item {
                            SelectItem::Var(v) if !lets.contains_key(v) => lookup(v)?,
                            SelectItem::Var(v) => {
                                let e = compile(&Expr::Node(Node::Var(v.clone())))?;
                                q.compute(v, e)
                            }
                            SelectItem::Expr { expr, .. } => {
                                let e = compile(expr)?;
                                q.compute(&item.name(), e)
                            }
                            SelectItem::Aggregate {
                                function,
                                distinct,
                                arg,
                                ..
                            } => {
                                let arg = arg.as_ref().map(&mut compile).transpose()?;
                                q.aggregate(&item.name(), *function, arg, *distinct)
                            }
                        });
                    }
                    columns
                } else {
                    names.iter().map(|n| lookup(n)).collect::<Result<_>>()?
                };
                q.select(selected.clone());
                let column = |v: &str| names.iter().position(|n| n == v).map(|i| selected[i]);
                if *distinct {
                    q.distinct();
                }
                for key in by {
                    let v = match column(&key.var) {
                        Some(v) if computes => v,
                        None if aggregated => {
                            return Err(Error::Invalid(format!(
                                "?{} is not a column of the select: with aggregates, 'by' uses its columns",
                                key.var
                            )));
                        }
                        _ => lookup(&key.var)?,
                    };
                    let k = SortKey::data(v);
                    q.order_by(if key.descending { k.desc() } else { k });
                }
                let mut column_var = |v: &str| {
                    column(v).ok_or_else(|| {
                        Error::Invalid(format!(
                            "?{v} is not a column of the select: 'having' uses its columns"
                        ))
                    })
                };
                let no_lets = HashMap::new();
                for (a, cmp, b) in having {
                    let a = compile_with(g, a, &no_lets, &params, &mut column_var, 0)?;
                    let b = compile_with(g, b, &no_lets, &params, &mut column_var, 0)?;
                    q.having(a.compare(*cmp, b));
                }
                if let Some(n) = limit {
                    q.limit(*n);
                }
                if let Some(n) = offset {
                    q.offset(*n);
                }
                Ok((q, names))
            }
            Statement::Verify { pattern, as_of } => {
                let (mut q, _) = lower(g, rules, pattern)?;
                q.select([]).limit(1);
                if let Some(as_of) = as_of {
                    q.as_of(as_of.id(g));
                }
                Ok((q, Vec::new()))
            }
            Statement::Lookup {
                pattern,
                where_,
                as_of,
            } => {
                let (mut q, map, recorded) = lower_full(g, rules, pattern, where_.as_ref(), true)?;
                let names = bound_names(pattern, true);
                q.select(names.iter().map(|n| map[n]));
                q.record_links_of(recorded);
                if let Some(as_of) = as_of {
                    q.as_of(as_of.id(g));
                }
                Ok((q, names))
            }
            _ => Err(unsupported("a query of an insert or delete")),
        }
    }
}

/// The result of a statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// The rows of a `select`, with the names of their variables.
    Rows {
        vars: Vec<String>,
        rows: Vec<Vec<u64>>,
        /// The holds of the ephemeral ids the rows name, which they keep.
        holds: Holds,
    },
    /// The rows of a `select` with aggregates: computed values, and entries.
    Table {
        vars: Vec<String>,
        rows: Vec<Vec<Cell>>,
        /// The holds of the ephemeral ids the cells name, which they keep.
        holds: Holds,
    },
    /// Whether a `verify`'s pattern has a match.
    Verified(bool),
    /// A `lookup`'s graph: the links its matches use (not those of its
    /// negations), and the entries they bind that aren't in those links.
    Graph(ResultGraph),
    /// The links an `insert` added.
    Inserted(usize),
    /// The links a `delete` deleted.
    Deleted(usize),
    /// The rule a `rule` defined (or found already saved).
    Defined(String),
    /// The rule a `delete rule` deleted, and how many definitions it had.
    DeletedRule { name: String, definitions: usize },
    /// The session's author or subgraph, set by `author` or `subgraph`.
    Default { what: &'static str, id: u64 },
    /// A subgraph `maintain` keeps, and the links it added to and removed
    /// from it to start.
    Maintained {
        subgraph: u64,
        added: usize,
        removed: usize,
    },
    /// A statement setting the session's state: `SET`, `DATA`, `DEFAULT`,
    /// `ASSUME`, with what it did (`SET me`, `ASSUME 2`).
    Done(String),
}

/// The values of a pattern's variables in each of its rows (or a single row
/// binding nothing, with no pattern).
fn bindings(
    g: &Graph,
    rules: &[RuleDef],
    pattern: Option<&Pattern>,
    view: Option<&Arc<Overlay>>,
) -> Result<(Vec<HashMap<String, u64>>, Holds)> {
    let Some(pattern) = pattern else {
        return Ok((vec![HashMap::new()], Holds::default()));
    };
    let (mut q, map) = lower(g, rules, pattern)?;
    let names = binding_names(pattern)?;
    let lets = lets(pattern)?;
    let params: HashMap<String, Var> = q
        .params()
        .iter()
        .map(|&v| (q.name(v).to_string(), v))
        .collect();
    let mut selected = Vec::new();
    for n in &names {
        selected.push(match map.get(n) {
            Some(&v) if !lets.contains_key(n) => v,
            _ => {
                let mut var = |v: &str| {
                    map.get(v).copied().ok_or_else(|| Error::Syntax {
                        line: 0,
                        column: 0,
                        message: format!("?{v} is not in the pattern"),
                    })
                };
                let e = compile_with(
                    g,
                    &Expr::Node(Node::Var(n.clone())),
                    &lets,
                    &params,
                    &mut var,
                    0,
                )?;
                q.compute(n, e)
            }
        });
    }
    q.select(selected);
    let holds = Holds::of(&q, view.map(|v| &**v));
    // A value computed by a `let` is an ephemeral one, held by the query,
    // which an insert writes as a value.
    let rows = if q.computes() {
        let rows = match view {
            Some(v) => q.cells_over(g, v)?,
            None => q.cells(g)?,
        };
        rows.into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|c| match c {
                        Cell::Id(id) => id,
                        Cell::Unbound => 0,
                        c => c.data(g).map_or(0, |d| q.ephemeral_value(g, &d)),
                    })
                    .collect()
            })
            .collect()
    } else {
        match view {
            Some(v) => q.run_over(g, v)?,
            None => q.run(g)?,
        }
    };
    Ok((
        rows.into_iter()
            .map(|row: Vec<u64>| names.iter().cloned().zip(row).collect())
            .collect(),
        holds,
    ))
}

/// The names a template can use of `pattern`'s rows: those it binds by
/// matching, and those its `let`s compute.
fn binding_names(pattern: &Pattern) -> Result<Vec<String>> {
    let mut names = bound_names(pattern, true);
    let mut computed: Vec<String> = lets(pattern)?.into_keys().collect();
    computed.sort();
    for n in computed {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    Ok(names)
}

/// Runs a parsed statement on `g`, with no rules defined.
pub fn run(g: &mut Graph, statement: &Statement) -> Result<Output> {
    Session::new().run(g, statement)
}

/// The value of a parameter (`$name`): a key, a value's data, or an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    Key(String),
    Value(String),
    Id(u64),
}

impl Param {
    /// The parameter written as GraQL: `/key`, `#123`, or a value (a word, a
    /// number, or a quoted string).
    pub fn parse(text: &str) -> Result<Param> {
        let tokens = lex(text)?;
        match &tokens[..] {
            [t, end] if end.token == Token::End => match &t.token {
                Token::Key(k) => Ok(Param::Key(k.clone())),
                Token::Id(id) => Ok(Param::Id(*id)),
                Token::Ident(v) | Token::Text(v) | Token::Number(v) => Ok(Param::Value(v.clone())),
                Token::Keyword(k @ ("true" | "false")) => Ok(Param::Value(k.to_string())),
                t => Err(Error::Invalid(format!(
                    "{t} is not a key, an id or a value"
                ))),
            },
            _ => Err(Error::Invalid(format!(
                "{text:?} is not one key, id or value"
            ))),
        }
    }

    fn node(&self) -> Node {
        match self {
            Param::Key(k) => Node::Key(k.clone()),
            Param::Value(v) => Node::Value(v.clone()),
            Param::Id(id) => Node::Id(*id),
        }
    }
}

/// Parameters by name, without the `$`.
pub type Params = HashMap<String, Param>;

/// Replaces `n`, if a parameter, by its value in `params`.
fn bind_node(n: &mut Node, params: &Params) -> Result<()> {
    if let Node::Param(name) = n {
        match params.get(name.as_str()) {
            Some(value) => *n = value.node(),
            None => return Err(Error::Invalid(format!("${name} is not given"))),
        }
    }
    Ok(())
}

/// Replaces each parameter of `p` by its value in `params`.
fn bind_pattern(p: &mut Pattern, params: &Params) -> Result<()> {
    let mut error = None;
    print::each_node_mut(p, &mut |n| {
        if let Node::Param(name) = n {
            match params.get(name.as_str()) {
                Some(value) => *n = value.node(),
                None => {
                    error.get_or_insert_with(|| Error::Invalid(format!("${name} is not given")));
                }
            }
        }
    });
    error.map_or(Ok(()), Err)
}

impl Statement {
    /// The statement with each parameter replaced by its value in `params`,
    /// or an error naming one not given.
    pub fn bind(&self, params: &Params) -> Result<Statement> {
        let mut s = self.clone();
        match &mut s {
            Statement::Rule(r) => {
                let mut has = false;
                print::each_node(&r.body, &mut |n| has |= matches!(n, Node::Param(_)));
                if has {
                    return Err(Error::Invalid("a rule can't have parameters".into()));
                }
            }
            Statement::Select {
                pattern,
                having,
                vars,
                ..
            } => {
                bind_pattern(pattern, params)?;
                let mut missing = None;
                let columns = vars.iter_mut().flatten().flat_map(|item| match item {
                    SelectItem::Var(_) => None,
                    SelectItem::Expr { expr, .. } => Some(expr),
                    SelectItem::Aggregate { arg, .. } => arg.as_mut(),
                });
                let having = having.iter_mut().flat_map(|(a, _, b)| [a, b]);
                for e in columns.chain(having) {
                    {
                        e.each_node_mut(&mut |n| {
                            if let Node::Param(name) = n {
                                match params.get(name.as_str()) {
                                    Some(v) => *n = v.node(),
                                    None => {
                                        missing.get_or_insert_with(|| name.clone());
                                    }
                                }
                            }
                        });
                    }
                }
                if let Some(name) = missing {
                    return Err(Error::Invalid(format!("${name} is not given")));
                }
            }
            Statement::Verify { pattern, .. } => bind_pattern(pattern, params)?,
            Statement::Lookup {
                pattern, where_, ..
            } => {
                bind_pattern(pattern, params)?;
                if let Some(w) = where_ {
                    bind_pattern(w, params)?;
                }
            }
            Statement::Insert {
                template,
                pattern,
                graph,
            }
            | Statement::Delete {
                template,
                pattern,
                graph,
            } => {
                for t in template.iter_mut() {
                    bind_pattern(t, params)?;
                }
                if let Some(p) = pattern {
                    bind_pattern(p, params)?;
                }
                if let Some(g) = graph {
                    bind_node(g, params)?;
                }
            }
            Statement::Maintain {
                template,
                pattern,
                graph,
            } => {
                for t in template.iter_mut() {
                    bind_pattern(t, params)?;
                }
                bind_pattern(pattern, params)?;
                bind_node(graph, params)?;
            }
            Statement::InsertGraph { source, graph } => {
                bind_graph(source, params)?;
                if let Some(g) = graph {
                    bind_node(g, params)?;
                }
            }
            Statement::Author(n) | Statement::Subgraph(n) => bind_node(n, params)?,
            Statement::DeleteRule(_)
            | Statement::IndexVector { .. }
            | Statement::DeleteIndex(_) => {}
            Statement::Set { value, .. } => match value {
                SetValue::Node(n) => bind_node(n, params)?,
                SetValue::Graph(e) => bind_graph(e, params)?,
            },
            Statement::Data(e) | Statement::Default(e) => bind_graph(e, params)?,
            Statement::Assume { template, graph } => {
                for t in template.iter_mut() {
                    bind_pattern(t, params)?;
                }
                if let Some(g) = graph {
                    bind_node(g, params)?;
                }
            }
            Statement::From { source, statement } => {
                bind_graph(source, params)?;
                **statement = statement.bind(params)?;
            }
            Statement::Test { operands, .. } => {
                for o in operands.iter_mut() {
                    match o {
                        TestOperand::Statement(s) => **s = s.bind(params)?,
                        TestOperand::Graph(e) => bind_graph(e, params)?,
                    }
                }
            }
            Statement::Function { body, .. } => {
                let mut error = None;
                body.each_node_mut(&mut |n| {
                    if let Err(e) = bind_node(n, params) {
                        error.get_or_insert(e);
                    }
                });
                if let Some(e) = error {
                    return Err(e);
                }
            }
        }
        Ok(s)
    }
}

/// Binds the parameters of a graph's triples and lookups.
fn bind_graph(e: &mut GraphExpr, params: &Params) -> Result<()> {
    match e {
        GraphExpr::Name(_) => Ok(()),
        GraphExpr::Literal(ps) => ps.iter_mut().try_for_each(|p| bind_pattern(p, params)),
        GraphExpr::Over(_, e) => bind_graph(e, params),
        GraphExpr::Lookup(s) => {
            **s = s.bind(params)?;
            Ok(())
        }
    }
}

/// Runs statements, keeping the [rules saved](RULES_KEY) in the graph they run
/// on, read again when they change (by another session, a rollback, ...).
#[derive(Debug, Clone, Default)]
pub struct Session {
    saved: Option<Saved>,
    /// The author of the links the session inserts, whose `//keys` it
    /// names; and the subgraph it inserts them into (see `author` and
    /// `subgraph`).
    author: Option<u64>,
    subgraph: Option<u64>,
    /// The subgraphs `maintain` keeps.
    maintained: Vec<crate::query::MaintainedSubgraph>,
    /// The names `SET` named.
    symbols: HashMap<String, Symbol>,
    /// The data graph, if not the database; the default graph; and the
    /// links `ASSUME`d (see `data`, `default` and `assume`).
    data: Option<Arc<Overlay>>,
    default: Option<Arc<Overlay>>,
    assumed: Option<Arc<Overlay>>,
    /// The graph the statement being run reads, from `from graph G`.
    reading: Option<Arc<Overlay>>,
    /// The functions `function` defined: their parameters and bodies.
    functions: HashMap<String, (Vec<String>, Expr)>,
}

/// A result a `test` compares.
#[derive(Debug, PartialEq)]
enum TestValue {
    Table(Vec<Vec<String>>),
    Graph(BTreeSet<[String; 3]>, BTreeSet<String>),
}

/// What `SET` named: a constant, or a graph.
#[derive(Debug, Clone)]
pub enum Symbol {
    Node(Node),
    Graph(Arc<Overlay>),
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// The rules saved in `g`, read again if they have changed.
    fn saved(&mut self, g: &Graph) -> Result<&Saved> {
        let saved = Saved::load(g, self.saved.take())?;
        Ok(self.saved.insert(saved))
    }

    /// The rules saved in `g`, in the order they were saved.
    pub fn rules(&mut self, g: &Graph) -> Result<&[RuleDef]> {
        Ok(&self.saved(g)?.defs)
    }

    /// The rules saved in `g`, and the links saving them.
    pub fn saved_rules(&mut self, g: &Graph) -> Result<&[(u64, RuleDef)]> {
        Ok(&self.saved(g)?.rules)
    }

    /// The text saved as rules in `g` that does not define one, such as
    /// text from a later version of GraQL, which is left out.
    pub fn skipped_rules(&mut self, g: &Graph) -> Result<&[SkippedRule]> {
        Ok(&self.saved(g)?.skipped)
    }

    /// Parses and runs GraQL statements on `g`, returning each one's
    /// output.
    pub fn execute(&mut self, g: &mut Graph, text: &str) -> Result<Vec<Output>> {
        self.execute_with(g, text, &Params::new())
    }

    /// The same, with the parameters `params`.
    pub fn execute_with(
        &mut self,
        g: &mut Graph,
        text: &str,
        params: &Params,
    ) -> Result<Vec<Output>> {
        parse(text)?
            .iter()
            .map(|s| self.run_with(g, s, params))
            .collect()
    }

    /// Runs a parsed statement on `g`, with its parameters (`$name`) given
    /// by `params`.  A statement parsed once can run many times so.
    pub fn run_with(
        &mut self,
        g: &mut Graph,
        statement: &Statement,
        params: &Params,
    ) -> Result<Output> {
        self.run(g, &statement.bind(params)?)
    }

    /// The session's author, set by `author`.
    pub fn author(&self) -> Option<u64> {
        self.author
    }

    /// The session's subgraph, set by `subgraph`.
    pub fn subgraph(&self) -> Option<u64> {
        self.subgraph
    }

    /// The subgraphs the session maintains (see `maintain`).
    pub fn maintained(&self) -> &[crate::query::MaintainedSubgraph] {
        &self.maintained
    }

    /// The names `SET` named, which [`parse_with_symbols`] should read as
    /// symbols in the statements the session runs next.
    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.symbols.keys().map(String::as_str)
    }

    /// What `SET` named `name`.
    pub fn symbol(&self, name: &str) -> Option<&Symbol> {
        self.symbols.get(name)
    }

    /// The data graph, if it isn't the database (see `data`).
    pub fn data(&self) -> Option<&Arc<Overlay>> {
        self.data.as_ref()
    }

    /// Ends the session's transaction: what it `ASSUME`d is dropped.  The
    /// shell calls this on `\commit` and `\rollback`.
    pub fn end_transaction(&mut self) {
        self.assumed = None;
    }

    /// The graph a statement reads: the graph it names, or the data graph,
    /// over the default graph, beneath the links assumed; `None` for the
    /// database alone.
    fn view(&self) -> Option<Arc<Overlay>> {
        let top = self.reading.as_ref().or(self.data.as_ref());
        if top.is_none() && self.default.is_none() && self.assumed.is_none() {
            return None;
        }
        let mut v = if top.is_none_or(|t| t.over_database()) {
            Overlay::new()
        } else {
            Overlay::alone()
        };
        for layer in [&self.default, &top.cloned(), &self.assumed]
            .into_iter()
            .flatten()
        {
            v = layer.over(&v);
        }
        Some(Arc::new(v))
    }

    /// The graph `e` names.
    fn graph_of(&mut self, g: &Graph, e: &GraphExpr) -> Result<Arc<Overlay>> {
        Ok(match e {
            GraphExpr::Name(n) if n == "database" => Arc::new(Overlay::new()),
            GraphExpr::Name(n) => match self.symbols.get(n) {
                Some(Symbol::Graph(o)) => o.clone(),
                _ => return Err(Error::Invalid(format!("no graph is named {n}"))),
            },
            GraphExpr::Literal(triples) => {
                let mut o = Overlay::alone();
                self.set_triples(g, &mut o, triples, None)?;
                Arc::new(o)
            }
            GraphExpr::Over(base, e) => {
                let base = self.graph_of(g, &GraphExpr::Name(base.clone()))?;
                Arc::new(self.graph_of(g, e)?.over(&base))
            }
            GraphExpr::Lookup(s) => {
                let Output::Graph(found) = self.run_reading(g, s)? else {
                    unreachable!("a lookup's graph")
                };
                Arc::new(Overlay::of_graph(g, &found, false))
            }
        })
    }

    /// What a `test` compares, as a value that is equal for equal results:
    /// a table's rows, sorted; a graph's links' facts and its nodes, each a
    /// name the same for an entry and an ephemeral one of the same data.
    fn test_value(&mut self, g: &Graph, o: &TestOperand) -> Result<TestValue> {
        use crate::query::{ResultLink, ResultNode};
        let name = |g: &Graph, id: u64| -> String {
            match (g.key(id), g.data(id)) {
                (Some(k), _) => format!("key {}", String::from_utf8_lossy(&k)),
                (_, Some(d)) => format!("value {}", String::from_utf8_lossy(&d)),
                _ => format!("#{id}"),
            }
        };
        let node = |g: &Graph, n: &ResultNode| -> String {
            match n {
                ResultNode::Entry(id) => name(g, *id),
                ResultNode::Value(c) => format!("value {}", c.text(g).unwrap_or_default()),
                ResultNode::Key(k) => format!("key {k}"),
                other => format!("{other:?}"),
            }
        };
        Ok(match o {
            TestOperand::Graph(e) => {
                let o = self.graph_of(g, e)?;
                let links = o
                    .links()
                    .filter(|l| l.live)
                    .map(|l| l.spo.map(|e| name(g, e)))
                    .collect();
                TestValue::Graph(links, BTreeSet::new())
            }
            TestOperand::Statement(s) => match self.run_reading(g, s)? {
                Output::Graph(found) => {
                    let links = found
                        .links
                        .iter()
                        .filter_map(|l| match l {
                            ResultLink::Stored(id) => g
                                .get(*id)
                                .ok()
                                .and_then(|r| r.as_link().map(|l| l.fields()))
                                .map(|f| [f.subject, f.predicate, f.object].map(|e| name(g, e))),
                            ResultLink::Derived { fields, .. } => {
                                Some(fields.clone().map(|f| node(g, &f)))
                            }
                        })
                        .collect();
                    let nodes = found.nodes.iter().map(|&id| name(g, id)).collect();
                    TestValue::Graph(links, nodes)
                }
                Output::Rows { rows, .. } => {
                    let mut rows: Vec<Vec<String>> = rows
                        .iter()
                        .map(|r| r.iter().map(|&id| name(g, id)).collect())
                        .collect();
                    rows.sort();
                    TestValue::Table(rows)
                }
                Output::Table { rows, .. } => {
                    let mut rows: Vec<Vec<String>> = rows
                        .iter()
                        .map(|r| r.iter().map(|c| c.text(g).unwrap_or_default()).collect())
                        .collect();
                    rows.sort();
                    TestValue::Table(rows)
                }
                Output::Verified(v) => TestValue::Table(vec![vec![v.to_string()]]),
                _ => unreachable!("a select, verify or lookup"),
            },
        })
    }

    /// Sets `triples` in `o`, live (or dead, if negated), by the session's
    /// author, in `subgraph` if given: their constants and blank nodes
    /// ephemeral if not in the graph.
    fn set_triples(
        &self,
        g: &Graph,
        o: &mut Overlay,
        triples: &[Pattern],
        subgraph: Option<&Node>,
    ) -> Result<usize> {
        let h = o.ephemeral_hold(g).clone();
        let mut blanks: HashMap<String, u64> = HashMap::new();
        let mut id = |n: &Node| -> Result<u64> {
            Ok(match n {
                Node::Blank(b) => *blanks.entry(b.clone()).or_insert_with(|| h.node(g)),
                n => ephemeral(g, &h, n).ok_or_else(|| {
                    Error::Invalid(format!(
                        "{} in a graph isn't a constant",
                        Expr::Node(n.clone())
                    ))
                })?,
            })
        };
        let sg = match subgraph {
            Some(n) => vec![id(n)?],
            None => Vec::new(),
        };
        let mut count = 0;
        for t in triples {
            let (live, t) = match t {
                Pattern::Not(t) => (false, &**t),
                t => (true, t),
            };
            let (s, p, ob, author, graph) = match t {
                Pattern::Triple(s, p, o) => (s, p, o, None, None),
                Pattern::Tagged(s, p, o, a, sg) => (s, p, o, a.as_ref(), sg.as_ref()),
                _ => {
                    return Err(Error::Unsupported {
                        line: 0,
                        column: 0,
                        what: "a link node in a graph".into(),
                    });
                }
            };
            let spo = [id(s)?, id(p)?, id(ob)?];
            let author = match author {
                Some(a) => id(a)?,
                None => self.author.unwrap_or(0),
            };
            let mut sgs = sg.clone();
            if let Some(n) = graph {
                sgs.push(id(n)?);
            }
            o.set(g, spo, live, author, &sgs);
            count += 1;
        }
        Ok(count)
    }

    /// Replaces the calls of functions `function` defined in `statement`'s
    /// expressions by their bodies, with the arguments for the parameters.
    fn expand_functions(&self, statement: &mut Statement) -> Result<()> {
        if let Statement::Function { .. } = statement {
            return Ok(());
        }
        // A column computed by a function is named as written.
        if let Statement::Select {
            vars: Some(items), ..
        } = statement
        {
            for item in items {
                if let SelectItem::Expr { expr, name: None } = item {
                    let mut calls = false;
                    expr.each_expr_mut(&mut |e| calls |= matches!(e, Expr::Apply(..)));
                    if calls {
                        let text = expr.to_string();
                        *item = SelectItem::Expr {
                            expr: expr.clone(),
                            name: Some(text),
                        };
                    }
                }
            }
        }
        let mut error = None;
        each_statement_expr(statement, &mut |e| {
            if let Err(err) = self.expand(e, 0) {
                error.get_or_insert(err);
            }
        });
        error.map_or(Ok(()), Err)
    }

    fn expand(&self, e: &mut Expr, depth: usize) -> Result<()> {
        if depth > 32 {
            return Err(Error::Invalid(
                "functions call each other in a cycle".into(),
            ));
        }
        let mut error = None;
        e.each_expr_mut(&mut |e| {
            let Expr::Apply(name, args) = e else {
                return;
            };
            let Some((params, body)) = self.functions.get(name) else {
                error.get_or_insert(Error::Invalid(format!("{name} is not a function")));
                return;
            };
            if params.len() != args.len() {
                error.get_or_insert(Error::Invalid(format!(
                    "{name} takes {} arguments",
                    params.len()
                )));
                return;
            }
            let mut out = body.clone();
            // The parameters, by the arguments; then calls in the body.
            out.each_expr_mut(&mut |x| {
                if let Expr::Node(Node::Symbol(p)) = x
                    && let Some(i) = params.iter().position(|q| q == p)
                {
                    *x = args[i].clone();
                }
            });
            if let Err(err) = self.expand(&mut out, depth + 1) {
                error.get_or_insert(err);
            }
            *e = out;
        });
        error.map_or(Ok(()), Err)
    }

    /// Replaces the symbols of `statement` by their values.
    fn resolve_symbols(&self, statement: &mut Statement) -> Result<()> {
        // `set a = b`, where `b` names a graph: `a` names it too.
        if let Statement::Set { value, .. } = statement
            && let SetValue::Node(Node::Symbol(n)) = value
            && let Some(Symbol::Graph(_)) = self.symbols.get(n)
        {
            *value = SetValue::Graph(GraphExpr::Name(n.clone()));
            return Ok(());
        }
        let mut error = None;
        each_statement_node(statement, &mut |n| {
            if let Node::Symbol(name) = n {
                match self.symbols.get(name) {
                    Some(Symbol::Node(v)) => *n = v.clone(),
                    Some(Symbol::Graph(_)) => {
                        error = Some(format!("{name} names a graph, not a node"));
                    }
                    None => error = Some(format!("{name} isn't set")),
                }
            }
        });
        match error {
            Some(e) => Err(Error::Invalid(e)),
            None => Ok(()),
        }
    }

    /// Brings the subgraphs the session maintains up to date with the
    /// entries committed to `g`, writing (not committing) their changes.
    pub fn update(&mut self, g: &mut Graph) -> Result<Vec<(u64, crate::query::SubgraphChanges)>> {
        let mut out = Vec::new();
        for m in &mut self.maintained {
            let changes = m.update(g)?;
            if !changes.is_empty() {
                out.push((m.subgraph(), changes));
            }
        }
        Ok(out)
    }

    /// `statement` ready to run: its parameters bound (there must be
    /// none), `//keys` scoped, functions' calls expanded, symbols replaced,
    /// and template rules' calls expanded.
    fn prepared(&mut self, g: &Graph, statement: &Statement) -> Result<Statement> {
        let mut statement = statement.bind(&Params::new())?;
        scope_keys(&mut statement, self.author)?;
        self.expand_functions(&mut statement)?;
        self.resolve_symbols(&mut statement)?;
        let defs = self.saved(g)?.defs.clone();
        expand_calls(&mut statement, &defs)?;
        Ok(statement)
    }

    /// Runs a statement that only reads (`select`, `verify`, `lookup`,
    /// `test`) on `g`, which may be a [reader](Graph::reader), as other
    /// threads may hold while a writer goes on: see [`Graph::readers`].
    /// Other statements fail.
    pub fn read(&mut self, g: &Graph, statement: &Statement) -> Result<Output> {
        let statement = &self.prepared(g, statement)?;
        if !statement.reads() {
            return Err(Error::Invalid(
                "only a select, verify, lookup or test can run on a graph read only".into(),
            ));
        }
        self.run_reading(g, statement)
    }

    /// Runs a prepared statement that only reads.
    fn run_reading(&mut self, g: &Graph, statement: &Statement) -> Result<Output> {
        let defs = self.saved(g)?.defs.clone();
        let rules = &defs[..];
        let view = self.view();
        match statement {
            Statement::Test {
                description,
                operands,
            } => {
                let [a, b] = &**operands;
                let (a, b) = (self.test_value(g, a)?, self.test_value(g, b)?);
                let name = description
                    .as_ref()
                    .map_or(String::new(), |d| format!(" {}", print::literal_text(d)));
                if a == b {
                    Ok(Output::Done(format!("TEST{name} passed")))
                } else {
                    Err(Error::Invalid(format!(
                        "TEST{name} failed: the results differ"
                    )))
                }
            }
            Statement::From { source, statement } => {
                let source = self.graph_of(g, source)?;
                let outer = self.reading.replace(source);
                let out = self.run_reading(g, statement);
                self.reading = outer;
                out
            }
            Statement::Select { .. } => {
                let (q, vars) = statement.query_with(g, rules)?;
                let holds = Holds::of(&q, view.as_deref());
                Ok(match (q.computes(), &view) {
                    (true, None) => Output::Table {
                        vars,
                        rows: q.cells(g)?,
                        holds,
                    },
                    (true, Some(v)) => Output::Table {
                        vars,
                        rows: q.cells_over(g, v)?,
                        holds,
                    },
                    (false, None) => Output::Rows {
                        vars,
                        rows: q.run(g)?,
                        holds,
                    },
                    (false, Some(v)) => Output::Rows {
                        vars,
                        rows: q.run_over(g, v)?,
                        holds,
                    },
                })
            }
            Statement::Verify { .. } => {
                let (q, _) = statement.query_with(g, rules)?;
                let rows = match &view {
                    Some(v) => q.run_over(g, v)?,
                    None => q.run(g)?,
                };
                Ok(Output::Verified(!rows.is_empty()))
            }
            Statement::Lookup { .. } => {
                let (q, _) = statement.query_with(g, rules)?;
                let plan = q.plan(g)?;
                Ok(Output::Graph(match &view {
                    Some(v) => plan.execute_graph_over(g, v, &[])?,
                    None => plan.execute_graph(g, &[])?,
                }))
            }
            _ => unreachable!("a statement that reads"),
        }
    }

    /// Runs a parsed statement on `g`, which must have no parameters.
    pub fn run(&mut self, g: &mut Graph, statement: &Statement) -> Result<Output> {
        let statement = &self.prepared(g, statement)?;
        if statement.reads() {
            return self.run_reading(g, statement);
        }
        let defs = self.saved(g)?.defs.clone();
        let rules = &defs[..];
        let view = self.view();
        match statement {
            Statement::Set { name, value } => {
                let symbol = match value {
                    SetValue::Node(n) => Symbol::Node(n.clone()),
                    SetValue::Graph(e) => Symbol::Graph(self.graph_of(g, e)?),
                };
                self.symbols.insert(name.clone(), symbol);
                Ok(Output::Done(format!("SET {name}")))
            }
            Statement::Data(e) => {
                self.data = match e {
                    GraphExpr::Name(n) if n == "database" => None,
                    e => Some(self.graph_of(g, e)?),
                };
                Ok(Output::Done("DATA".into()))
            }
            Statement::Default(e) => {
                let d = self.graph_of(g, e)?;
                self.default = (!d.is_empty()).then_some(d);
                Ok(Output::Done("DEFAULT".into()))
            }
            Statement::Assume { template, graph } => {
                let mut assumed = self.assumed.take().unwrap_or_default();
                let n = self.set_triples(g, Arc::make_mut(&mut assumed), template, graph.as_ref());
                self.assumed = Some(assumed);
                Ok(Output::Done(format!("ASSUME {}", n?)))
            }
            Statement::Function { name, params, body } => {
                self.functions
                    .insert(name.clone(), (params.clone(), body.clone()));
                Ok(Output::Done(format!("FUNCTION {name}")))
            }
            Statement::Rule(rule) => {
                saved::save(g, rules, rule)?;
                Ok(Output::Defined(rule.name.clone()))
            }
            Statement::IndexVector {
                predicate,
                dims,
                metric,
                m,
                ef,
            } => {
                let p = resolve(g, predicate, true)?.expect("created");
                let index = graph::VectorIndex {
                    predicate: p,
                    dims: *dims,
                    metric: *metric,
                    m: *m,
                    ef: *ef,
                };
                let changed = g.create_vector_index(&index)?;
                Ok(Output::Done(format!(
                    "INDEX vector on {}{}",
                    Expr::Node(predicate.clone()),
                    if changed { "" } else { " (unchanged)" }
                )))
            }
            Statement::DeleteIndex(predicate) => {
                let dropped = match resolve(g, predicate, false)? {
                    Some(p) => g.drop_vector_index(p)?,
                    None => false,
                };
                if !dropped {
                    return Err(Error::Invalid(format!(
                        "{} has no vector index",
                        Expr::Node(predicate.clone())
                    )));
                }
                Ok(Output::Done(format!(
                    "DELETE INDEX vector on {}",
                    Expr::Node(predicate.clone())
                )))
            }
            Statement::DeleteRule(name) => {
                let saved = self.saved(g)?.clone();
                let definitions = saved::delete(g, &saved, name)?;
                Ok(Output::DeletedRule {
                    name: name.clone(),
                    definitions,
                })
            }
            Statement::Maintain { .. } if view.is_some() => Err(Error::Invalid(
                "'maintain' keeps a subgraph of the database: not with 'data', 'default' or 'assume'"
                    .into(),
            )),
            Statement::Maintain {
                template,
                pattern,
                graph,
            } => {
                let (mut q, map) = lower(g, rules, pattern)?;
                let names = bound_names(pattern, true);
                let selected: Vec<Var> = names.iter().map(|n| map[n]).collect();
                q.select(selected);
                let mut triples = Vec::new();
                for t in template {
                    let Pattern::Triple(s, p, o) = t else {
                        return Err(Error::Unsupported {
                            line: 0,
                            column: 0,
                            what: "a maintained template other than triples".into(),
                        });
                    };
                    let mut term = |n: &Node| -> Result<Term> {
                        Ok(match n {
                            Node::Var(v) if map.contains_key(v) && names.contains(v) => {
                                Term::Var(map[v])
                            }
                            Node::Var(_) => {
                                return Err(Error::Syntax {
                                    line: 0,
                                    column: 0,
                                    message: "a template variable is not bound by the pattern"
                                        .into(),
                                });
                            }
                            Node::Blank(_) => {
                                return Err(Error::Unsupported {
                                    line: 0,
                                    column: 0,
                                    what: "a blank node in a maintained template".into(),
                                });
                            }
                            n => Term::Id(resolve(g, n, true)?.expect("created")),
                        })
                    };
                    triples.push([term(s)?, term(p)?, term(o)?]);
                }
                let subgraph = resolve(g, graph, true)?.expect("created");
                let (m, changes) = crate::query::MaintainedSubgraph::new(&q, triples, subgraph, g)?;
                // A subgraph maintained again replaces its maintainer.
                self.maintained.retain(|m| m.subgraph() != subgraph);
                self.maintained.push(m);
                Ok(Output::Maintained {
                    subgraph,
                    added: changes.added.len(),
                    removed: changes.removed.len(),
                })
            }
            Statement::Author(n) | Statement::Subgraph(n) => {
                let id = resolve(g, n, true)?.expect("created");
                let what = if matches!(statement, Statement::Author(_)) {
                    self.author = Some(id);
                    "AUTHOR"
                } else {
                    self.subgraph = Some(id);
                    "SUBGRAPH"
                };
                Ok(Output::Default { what, id })
            }
            Statement::InsertGraph { source, graph } => {
                let source = self.graph_of(g, source)?;
                if let Some(data) = &mut self.data {
                    // Into the data graph.
                    let n = source.len();
                    *data = Arc::new(source.over(data));
                    return Ok(Output::Inserted(n));
                }
                let subgraph = match graph {
                    Some(n) => resolve(g, n, true)?,
                    None => self.subgraph,
                };
                let n = source.insert_into(g, self.author.unwrap_or(0), subgraph)?;
                Ok(Output::Inserted(n))
            }
            Statement::Select { .. }
            | Statement::Verify { .. }
            | Statement::Lookup { .. }
            | Statement::From { .. }
            | Statement::Test { .. } => unreachable!("a statement that reads"),
            Statement::Insert {
                template,
                pattern,
                graph,
            }
            | Statement::Delete {
                template,
                pattern,
                graph,
            } => {
                let insert = matches!(statement, Statement::Insert { .. });
                if self.data.is_some() {
                    let (rows, _holds) = bindings(g, rules, pattern.as_ref(), view.as_ref())?;
                    let n = self.write_data(g, insert, template, graph.as_ref(), rows)?;
                    return Ok(if insert {
                        Output::Inserted(n)
                    } else {
                        Output::Deleted(n)
                    });
                }
                let bound = match pattern {
                    Some(p) => binding_names(p)?,
                    None => Vec::new(),
                };
                // Variables naming a template's links are bound by it.
                let named: Vec<&String> = template
                    .iter()
                    .filter_map(|t| match t {
                        Pattern::Link(Node::Var(v), ..) => Some(v),
                        _ => None,
                    })
                    .collect();
                let unbound = template.iter().any(|t| {
                    let mut unbound = false;
                    print::each_node(t, &mut |n| {
                        if let Node::Var(v) = n
                            && !bound.contains(v)
                            && !named.contains(&v)
                        {
                            unbound = true;
                        }
                    });
                    unbound
                });
                if unbound {
                    return Err(Error::Syntax {
                        line: 0,
                        column: 0,
                        message: "a template variable is not bound by the pattern".into(),
                    });
                }
                // The statement's subgraph: `into g` or `from g`; inserts
                // go into the session's, if it has one, else.
                let statement_graph = match graph {
                    Some(n) => resolve(g, n, insert)?.or(Some(0)),
                    None => None,
                };
                let mut count = 0;
                let (rows, _holds) = bindings(g, rules, pattern.as_ref(), view.as_ref())?;
                for row in rows {
                    // A blank node of the template is a new node for each row,
                    // and a link node's name is bound to its link.
                    let mut blanks: HashMap<String, u64> = HashMap::new();
                    for t in template {
                        let (link, s, p, o, author, sg) = match t {
                            Pattern::Triple(s, p, o) => (None, s, p, o, None, None),
                            Pattern::Tagged(s, p, o, a, sg) => {
                                (None, s, p, o, a.as_ref(), sg.as_ref())
                            }
                            Pattern::Link(l, s, p, o) => (Some(l), s, p, o, None, None),
                            _ => unreachable!("a template is triples and links"),
                        };
                        let mut id = |g: &mut Graph, n: &Node| -> graph::Result<Option<u64>> {
                            Ok(match n {
                                Node::Var(v) | Node::Blank(v) if blanks.contains_key(v) => {
                                    Some(blanks[v])
                                }
                                // Unbound, from an optional pattern: the
                                // triple is left out.  A value computed (a
                                // count, a rank) is written as a value, if
                                // inserted.
                                Node::Var(v) | Node::Blank(v) if row.contains_key(v) => {
                                    match row[v] {
                                        0 => None,
                                        id if id & graph::EPHEMERAL != 0 => {
                                            let data = g.data(id).map(Cow::into_owned);
                                            let key = g.key(id).map(Cow::into_owned);
                                            match (data, key) {
                                                (Some(d), _) if insert => Some(g.value(&d)?),
                                                (_, Some(k)) if insert => {
                                                    Some(g.node_by_key(&k)?)
                                                }
                                                // A node of an overlay isn't
                                                // the database's.
                                                _ => None,
                                            }
                                        }
                                        id => Some(id),
                                    }
                                }
                                Node::Var(_) => None,
                                Node::Blank(b) if insert => Some(match blanks.get(b) {
                                    Some(&id) => id,
                                    None => {
                                        let id = g.node()?;
                                        blanks.insert(b.clone(), id);
                                        id
                                    }
                                }),
                                Node::Blank(_) => None,
                                n => resolve(g, n, insert)?,
                            })
                        };
                        // Deleting a link to a constant not in the graph does nothing.
                        let (Some(s), Some(p), Some(o)) = (id(g, s)?, id(g, p)?, id(g, o)?) else {
                            continue;
                        };
                        let author = match author {
                            Some(a) => id(g, a)?,
                            None => None,
                        };
                        let sg = match sg {
                            Some(n) => Some(id(g, n)?.unwrap_or(0)),
                            None => statement_graph,
                        };
                        let id = if insert {
                            let sg = sg.or(self.subgraph);
                            let changed = self.add_fact(g, (s, p, o), author, sg)?;
                            count += usize::from(changed);
                            g.find_link(s, p, o)
                        } else {
                            let live = g.find_link(s, p, o);
                            let removed = match sg {
                                // Out of a subgraph not in the graph: nothing.
                                Some(0) => false,
                                Some(sg) => g.remove_from_subgraph(s, p, o, sg)?.is_some(),
                                None => g.delete_link(s, p, o)?.is_some(),
                            };
                            count += usize::from(removed);
                            live
                        };
                        if let (Some(Node::Var(name) | Node::Blank(name)), Some(id)) = (link, id) {
                            blanks.insert(name.clone(), id);
                        }
                    }
                }
                Ok(if insert {
                    Output::Inserted(count)
                } else {
                    Output::Deleted(count)
                })
            }
        }
    }
}

impl Session {
    /// Inserts (or deletes) the template's triples for each of `rows` into
    /// the data graph, an overlay: its constants and blank nodes ephemeral
    /// if not in the graph.  Returns the number of links changed.
    fn write_data(
        &mut self,
        g: &Graph,
        insert: bool,
        template: &[Pattern],
        graph: Option<&Node>,
        rows: Vec<HashMap<String, u64>>,
    ) -> Result<usize> {
        let mut data = self.data.take().expect("an overlay");
        let o = Arc::make_mut(&mut data);
        let h = o.ephemeral_hold(g).clone();
        let statement_graph = graph.and_then(|n| ephemeral(g, &h, n));
        let mut count = 0;
        for row in rows {
            let mut blanks: HashMap<String, u64> = HashMap::new();
            for t in template {
                let (link, s, p, ob, author, sg) = match t {
                    Pattern::Triple(s, p, o) => (None, s, p, o, None, None),
                    Pattern::Tagged(s, p, o, a, sg) => (None, s, p, o, a.as_ref(), sg.as_ref()),
                    Pattern::Link(l, s, p, o) => (Some(l), s, p, o, None, None),
                    _ => unreachable!("a template is triples and links"),
                };
                let mut id = |n: &Node| -> Option<u64> {
                    match n {
                        Node::Var(v) | Node::Blank(v) if blanks.contains_key(v) => Some(blanks[v]),
                        Node::Var(v) | Node::Blank(v) if row.contains_key(v) => {
                            Some(row[v]).filter(|&id| id != 0)
                        }
                        Node::Var(_) => None,
                        Node::Blank(b) if insert => {
                            let id = h.node(g);
                            blanks.insert(b.clone(), id);
                            Some(id)
                        }
                        Node::Blank(_) => None,
                        n => ephemeral(g, &h, n),
                    }
                };
                let (Some(s), Some(p), Some(ob)) = (id(s), id(p), id(ob)) else {
                    continue;
                };
                let spo = [s, p, ob];
                let author = author.and_then(&mut id).or(self.author).unwrap_or(0);
                let sg = match sg {
                    Some(n) => id(n),
                    None => statement_graph,
                };
                let link_id = if insert {
                    let sg = sg.or(self.subgraph);
                    let before = o.fact(spo).map(|l| (l.live, l.subgraphs.clone()));
                    let unchanged = match &before {
                        Some((live, sgs)) => *live && sg.is_none_or(|x| sgs.contains(&x)),
                        None => sg.is_none() && o.live(g, spo).is_some(),
                    };
                    let id = o.set(g, spo, true, author, sg.as_slice());
                    count += usize::from(!unchanged);
                    Some(id)
                } else {
                    let live = o.live(g, spo);
                    let removed = match sg {
                        Some(sg) => o.remove_from_subgraph(g, spo, sg),
                        None if live.is_some() => {
                            o.delete(g, spo);
                            true
                        }
                        None => false,
                    };
                    count += usize::from(removed);
                    live
                };
                // A link node's name is bound to its link.
                if let (Some(Node::Var(name) | Node::Blank(name)), Some(id)) = (link, link_id) {
                    blanks.insert(name.clone(), id);
                }
            }
        }
        self.data = Some(data);
        Ok(count)
    }

    /// Links the fact `(s, p, o)`, by `author` (else the session's) and into
    /// `subgraph` if given, returning whether that appended a link: the fact
    /// wasn't live, or wasn't in `subgraph`.
    fn add_fact(
        &self,
        g: &mut Graph,
        (subject, predicate, object): (u64, u64, u64),
        author: Option<u64>,
        subgraph: Option<u64>,
    ) -> graph::Result<bool> {
        let before = g.find_link(subject, predicate, object);
        let id = g.add_link(&crate::record::LinkFields {
            subject,
            predicate,
            object,
            author: author.or(self.author).unwrap_or(0),
            when: crate::record::now(),
            subgraphs: subgraph.into_iter().filter(|&sg| sg != 0).collect(),
            ..Default::default()
        })?;
        Ok(before != Some(id))
    }
}

/// The entry a constant node names, or an ephemeral one (see
/// [`Graph::ephemeral_value`]) held by `h` if the graph hasn't it; `None`
/// for a variable, blank node or parameter.
fn ephemeral(g: &Graph, h: &EphemeralHold, n: &Node) -> Option<u64> {
    match n {
        Node::Key(k) => Some(h.key(g, k.as_bytes())),
        Node::Value(v) | Node::Symbol(v) => Some(h.value(g, v.as_bytes())),
        Node::Id(id) => Some(*id),
        Node::Var(_) | Node::Blank(_) | Node::Param(_) => None,
    }
}

/// Calls `f` with each node of `statement`: of its patterns, templates,
/// expressions, graphs and subgraphs' names.
fn each_statement_node(statement: &mut Statement, f: &mut impl FnMut(&mut Node)) {
    fn graph(e: &mut GraphExpr, f: &mut impl FnMut(&mut Node)) {
        match e {
            GraphExpr::Name(_) => {}
            GraphExpr::Literal(ps) => ps.iter_mut().for_each(|p| print::each_node_mut(p, f)),
            GraphExpr::Over(_, e) => graph(e, f),
            GraphExpr::Lookup(s) => each_statement_node(s, f),
        }
    }
    match statement {
        Statement::Rule(r) => print::each_node_mut(&mut r.body, f),
        Statement::Select {
            pattern,
            having,
            vars,
            ..
        } => {
            print::each_node_mut(pattern, f);
            let exprs = vars
                .iter_mut()
                .flatten()
                .flat_map(|item| match item {
                    SelectItem::Var(_) => None,
                    SelectItem::Expr { expr, .. } => Some(expr),
                    SelectItem::Aggregate { arg, .. } => arg.as_mut(),
                })
                .chain(having.iter_mut().flat_map(|(a, _, b)| [a, b]));
            for e in exprs {
                e.each_node_mut(f);
            }
        }
        Statement::Verify { pattern, .. } => print::each_node_mut(pattern, f),
        Statement::Lookup {
            pattern, where_, ..
        } => {
            print::each_node_mut(pattern, f);
            if let Some(w) = where_ {
                print::each_node_mut(w, f);
            }
        }
        Statement::Insert {
            template,
            pattern,
            graph: g,
        }
        | Statement::Delete {
            template,
            pattern,
            graph: g,
        } => {
            template.iter_mut().for_each(|t| print::each_node_mut(t, f));
            if let Some(p) = pattern {
                print::each_node_mut(p, f);
            }
            if let Some(n) = g {
                f(n);
            }
        }
        Statement::InsertGraph { source, graph: g } => {
            graph(source, f);
            if let Some(n) = g {
                f(n);
            }
        }
        Statement::Maintain {
            template,
            pattern,
            graph: g,
        } => {
            template.iter_mut().for_each(|t| print::each_node_mut(t, f));
            print::each_node_mut(pattern, f);
            f(g);
        }
        Statement::Author(n) | Statement::Subgraph(n) => f(n),
        Statement::IndexVector { predicate: n, .. } | Statement::DeleteIndex(n) => f(n),
        Statement::DeleteRule(_) => {}
        Statement::Set { value, .. } => match value {
            SetValue::Node(n) => f(n),
            SetValue::Graph(e) => graph(e, f),
        },
        Statement::Data(e) | Statement::Default(e) => graph(e, f),
        Statement::Assume { template, graph: g } => {
            template.iter_mut().for_each(|t| print::each_node_mut(t, f));
            if let Some(n) = g {
                f(n);
            }
        }
        Statement::From { source, statement } => {
            graph(source, f);
            each_statement_node(statement, f);
        }
        Statement::Test { operands, .. } => {
            for o in operands.iter_mut() {
                match o {
                    TestOperand::Statement(s) => each_statement_node(s, f),
                    TestOperand::Graph(e) => graph(e, f),
                }
            }
        }
        // Its parameters are symbols until it is called.
        Statement::Function { .. } => {}
    }
}

/// Calls `f` with each expression of `statement`: of its columns,
/// comparisons, tests and `let`s, and of the statements within it.
fn each_statement_expr(statement: &mut Statement, f: &mut impl FnMut(&mut Expr)) {
    fn pattern(p: &mut Pattern, f: &mut impl FnMut(&mut Expr)) {
        match p {
            Pattern::Compare(a, _, b) => {
                f(a);
                f(b);
            }
            Pattern::Test(e) | Pattern::Let(_, e) => f(e),
            Pattern::And(ps) | Pattern::Or(ps) | Pattern::Assert(ps, _) => {
                ps.iter_mut().for_each(|p| pattern(p, f))
            }
            Pattern::Not(p)
            | Pattern::AsOf(p, _)
            | Pattern::Optional(p)
            | Pattern::InGraph(p, _) => pattern(p, f),
            _ => {}
        }
    }
    fn graph(e: &mut GraphExpr, f: &mut impl FnMut(&mut Expr)) {
        match e {
            GraphExpr::Over(_, e) => graph(e, f),
            GraphExpr::Lookup(s) => each_statement_expr(s, f),
            GraphExpr::Name(_) | GraphExpr::Literal(_) => {}
        }
    }
    match statement {
        Statement::Rule(r) => pattern(&mut r.body, f),
        Statement::Select {
            pattern: p,
            having,
            vars,
            ..
        } => {
            pattern(p, f);
            for item in vars.iter_mut().flatten() {
                match item {
                    SelectItem::Var(_) => {}
                    SelectItem::Expr { expr, .. } => f(expr),
                    SelectItem::Aggregate { arg, .. } => arg.iter_mut().for_each(&mut *f),
                }
            }
            for (a, _, b) in having {
                f(a);
                f(b);
            }
        }
        Statement::Verify { pattern: p, .. } => pattern(p, f),
        Statement::Lookup {
            pattern: p, where_, ..
        } => {
            pattern(p, f);
            if let Some(w) = where_ {
                pattern(w, f);
            }
        }
        Statement::Insert { pattern: p, .. } | Statement::Delete { pattern: p, .. } => {
            if let Some(p) = p {
                pattern(p, f);
            }
        }
        Statement::Maintain { pattern: p, .. } => pattern(p, f),
        Statement::InsertGraph { source, .. } => graph(source, f),
        Statement::Set {
            value: SetValue::Graph(e),
            ..
        }
        | Statement::Data(e)
        | Statement::Default(e) => graph(e, f),
        Statement::From { source, statement } => {
            graph(source, f);
            each_statement_expr(statement, f);
        }
        Statement::Test { operands, .. } => {
            for o in operands.iter_mut() {
                match o {
                    TestOperand::Statement(s) => each_statement_expr(s, f),
                    TestOperand::Graph(e) => graph(e, f),
                }
            }
        }
        Statement::Set { .. }
        | Statement::Assume { .. }
        | Statement::Author(_)
        | Statement::Subgraph(_)
        | Statement::DeleteRule(_)
        | Statement::IndexVector { .. }
        | Statement::DeleteIndex(_)
        | Statement::Function { .. } => {}
    }
}

/// The entry a constant node names: found, or if `create`, made (a key's
/// node, or a value).
fn resolve(g: &mut Graph, n: &Node, create: bool) -> graph::Result<Option<u64>> {
    Ok(match n {
        Node::Key(k) if create => Some(g.node_by_key(k.as_bytes())?),
        Node::Key(k) => g.find_by_key(k.as_bytes()),
        Node::Value(v) | Node::Symbol(v) if create => Some(g.value(v.as_bytes())?),
        Node::Value(v) | Node::Symbol(v) => g.find_value(v.as_bytes()),
        Node::Id(id) => {
            if g.check_entry(*id) == graph::EntryCheck::NotEntry {
                return Err(graph::Error::InvalidArgument(
                    "an #id in a template names no entry",
                ));
            }
            Some(*id)
        }
        Node::Var(_) | Node::Blank(_) | Node::Param(_) => None,
    })
}

/// Rewrites the author-scoped keys of `statement`, `//name`, to the keys of
/// the session's author: `/u/<author's id>/name`.
fn scope_keys(statement: &mut Statement, author: Option<u64>) -> Result<()> {
    let mut missing = false;
    let mut scope = |k: &mut String| {
        if let Some(rest) = k.strip_prefix("//") {
            match author {
                Some(a) => *k = format!("/u/{a}/{rest}"),
                None => missing = true,
            }
        }
    };
    each_statement_key(statement, &mut scope);
    if missing {
        return Err(Error::Invalid(
            "a //key names a key of the session's author: set one with 'author'".into(),
        ));
    }
    Ok(())
}

/// Calls `f` with each key of `statement`: of its nodes, and of its path
/// expressions' steps.
fn each_statement_key(statement: &mut Statement, f: &mut impl FnMut(&mut String)) {
    fn pattern(p: &mut Pattern, f: &mut impl FnMut(&mut String)) {
        print::each_node_mut(p, &mut |n| {
            if let Node::Key(k) = n {
                f(k);
            }
        });
        paths(p, f);
    }
    fn paths(p: &mut Pattern, f: &mut impl FnMut(&mut String)) {
        fn steps(path: &mut Path, f: &mut impl FnMut(&mut String)) {
            match path {
                Path::Key(k) => f(k),
                Path::Pred(_) | Path::Id(_) => {}
                Path::Inverse(p) | Path::Repeat(p, ..) => steps(p, f),
                Path::Seq(ps) | Path::Alt(ps) => ps.iter_mut().for_each(|p| steps(p, f)),
            }
        }
        match p {
            Pattern::PathExpr(_, path, _) => steps(path, f),
            Pattern::And(ps) | Pattern::Or(ps) => ps.iter_mut().for_each(|p| paths(p, f)),
            Pattern::Not(p)
            | Pattern::AsOf(p, _)
            | Pattern::Optional(p)
            | Pattern::InGraph(p, _) => paths(p, f),
            _ => {}
        }
    }
    let node = |n: &mut Node, f: &mut dyn FnMut(&mut String)| {
        if let Node::Key(k) = n {
            f(k);
        }
    };
    match statement {
        Statement::Rule(r) => pattern(&mut r.body, f),
        Statement::Select {
            pattern: p,
            having,
            vars,
            ..
        } => {
            pattern(p, f);
            let exprs = vars
                .iter_mut()
                .flatten()
                .flat_map(|item| match item {
                    SelectItem::Var(_) => None,
                    SelectItem::Expr { expr, .. } => Some(expr),
                    SelectItem::Aggregate { arg, .. } => arg.as_mut(),
                })
                .chain(having.iter_mut().flat_map(|(a, _, b)| [a, b]));
            for e in exprs {
                e.each_node_mut(&mut |n| node(n, f));
            }
        }
        Statement::Verify { pattern: p, .. } => pattern(p, f),
        Statement::Lookup {
            pattern: p, where_, ..
        } => {
            pattern(p, f);
            if let Some(w) = where_ {
                pattern(w, f);
            }
        }
        Statement::Insert {
            template,
            pattern: p,
            graph,
        }
        | Statement::Delete {
            template,
            pattern: p,
            graph,
        } => {
            template.iter_mut().for_each(|t| pattern(t, f));
            if let Some(p) = p {
                pattern(p, f);
            }
            if let Some(n) = graph {
                node(n, f);
            }
        }
        Statement::InsertGraph { source, graph } => {
            each_graph_key(source, f);
            if let Some(n) = graph {
                node(n, f);
            }
        }
        Statement::Set { value, .. } => match value {
            SetValue::Node(n) => node(n, f),
            SetValue::Graph(e) => each_graph_key(e, f),
        },
        Statement::Data(e) | Statement::Default(e) => each_graph_key(e, f),
        Statement::Assume { template, graph } => {
            template.iter_mut().for_each(|t| pattern(t, f));
            if let Some(n) = graph {
                node(n, f);
            }
        }
        Statement::From { source, statement } => {
            each_graph_key(source, f);
            each_statement_key(statement, f);
        }
        Statement::Test { operands, .. } => {
            for o in operands.iter_mut() {
                match o {
                    TestOperand::Statement(s) => each_statement_key(s, f),
                    TestOperand::Graph(e) => each_graph_key(e, f),
                }
            }
        }
        Statement::Function { body, .. } => body.each_node_mut(&mut |n| node(n, f)),
        Statement::Maintain {
            template,
            pattern: p,
            graph,
        } => {
            template.iter_mut().for_each(|t| pattern(t, f));
            pattern(p, f);
            node(graph, f);
        }
        Statement::Author(n) | Statement::Subgraph(n) => node(n, f),
        Statement::IndexVector { predicate: n, .. } | Statement::DeleteIndex(n) => node(n, f),
        Statement::DeleteRule(_) => {}
    }
}

/// Calls `f` with each key a graph names, in its triples and lookups.
fn each_graph_key(e: &mut GraphExpr, f: &mut impl FnMut(&mut String)) {
    match e {
        GraphExpr::Name(_) => {}
        GraphExpr::Literal(ps) => {
            for p in ps {
                print::each_node_mut(p, &mut |n| {
                    if let Node::Key(k) = n {
                        f(k);
                    }
                });
            }
        }
        GraphExpr::Over(_, e) => each_graph_key(e, f),
        GraphExpr::Lookup(s) => each_statement_key(s, f),
    }
}

/// Parses and runs GraQL statements on `g`, returning each one's output.
pub fn execute(g: &mut Graph, text: &str) -> Result<Vec<Output>> {
    Session::new().execute(g, text)
}

mod expr;
mod prepared;
mod print;
mod saved;
#[cfg(test)]
mod tests;

pub use prepared::PreparedStatement;
pub use print::literal;
use saved::Saved;
pub use saved::{
    CALLS_PREDICATE, REFERS_TO_KEY_PREDICATE, RULE_PREDICATE, RULES_KEY, SkippedRule,
    USES_PREDICATE, key_reference, key_reference_or_new, rebuild_references, rules_using,
};
