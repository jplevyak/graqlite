//! Graph pattern queries, planned by cost and evaluated in batches.
//!
//! A [`Query`] is a [`Group`] of triple patterns over variables, unions of
//! groups, negated groups and filters, with a selection of variables to
//! return, deduplication, ordering, a limit, and the id as of which to see
//! the graph.  Each pattern matches the live links of the graph; a link
//! pattern also binds the link's own id, so queries can match statements
//! about links.
//!
//! ```
//! use graqlite::graph::{Graph, Options};
//! use graqlite::query::{Filter, Group, Query, SortKey};
//!
//! let mut g = Graph::open(None, Options::default()).unwrap();
//! let knows = g.value(b"knows").unwrap();
//! let (a, b, c) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
//! g.link(a, knows, b).unwrap();
//! g.link(b, knows, c).unwrap();
//! g.link(c, knows, a).unwrap();
//!
//! // Who do the people a knows know?
//! let mut q = Query::new();
//! let (x, y) = (q.var("x"), q.var("y"));
//! q.pattern(a, knows, x).pattern(x, knows, y).select([y]);
//! assert_eq!(q.run(&g).unwrap(), [[c]]);
//!
//! // Who knows someone a does not know, other than a?
//! let mut q = Query::new();
//! let (x, y) = (q.var("x"), q.var("y"));
//! let mut known_by_a = Group::new();
//! known_by_a.pattern(a, knows, y);
//! q.pattern(x, knows, y)
//!     .not(known_by_a)
//!     .filter(Filter::Ne(x.into(), a.into()))
//!     .select([x])
//!     .distinct()
//!     .order_by(SortKey::id(x));
//! let plan = q.plan(&g).unwrap();
//! println!("{}", plan.explain());
//! assert_eq!(plan.execute(&g).collect::<Result<Vec<_>, _>>().unwrap(), [[b], [c]]);
//! ```
//!
//! # Groups
//!
//! A group's rows are those matching all of its patterns and unions, that
//! pass its filters, and for which none of its negated groups has a match.
//!
//! - A *union* matches the rows of any of its branch groups.  Every branch
//!   must bind the same variables, so that every row binds every variable.
//! - A *negated group* (`not`) removes the rows for which it has a match,
//!   given the values the row binds to its variables: SPARQL's `FILTER NOT
//!   EXISTS`.  Its other variables are its own.
//! - An *optional group* extends each row by each of its matches, or keeps
//!   the row with the variables only it binds unbound (0) if it has none: a
//!   left outer join, SPARQL's `OPTIONAL`.  A group's optional groups are
//!   joined after the rest of it, in order.  A filter of an unbound
//!   variable fails; a negation or optional group joined on one finds no
//!   match.  For stratifying rules, a call within an optional group counts
//!   as within a negation, since a left join is not monotonic.
//! - A *filter* may use the variables of its group and of the groups it is
//!   in.
//! - A pattern can be restricted to the links in a
//!   [subgraph](crate::graph#subgraphs) ([`Group::in_subgraph`], or for a
//!   whole group [`Group::in_graph`]) or by an author ([`Group::by_author`]):
//!   a constant, or a variable binding each subgraph the link is in, or its
//!   author.  A constant one can be read through the Subgraph or Author
//!   index, each link checked to be its fact's latest.  Such a query isn't
//!   reduced, and can't be a standing query.
//!
//! # Paths and rules
//!
//! A [path](Group::path) pattern matches the subjects and objects joined by
//! a chain of one or more links with its predicate: the transitive closure,
//! found by searching from whichever end is bound.  A
//! [path expression](Group::path_expr) generalizes it to a regular
//! expression over links ([`PathExpr`]), compiled to an automaton and
//! searched the same way.
//!
//! A [`Relation`] is defined by [`rule`](Query::rule)s, each a head of
//! variables and a body group, and matched by [`call`](Group::call)s.
//! Rules may be recursive, through calls to their own relation or others,
//! but a relation may not depend on its own negation.  The relations a
//! query calls are evaluated before it runs, semi-naively, and joined from
//! memory.
//!
//! # Planning
//!
//! [`Query::plan`] may first *reduce* the top group's patterns: by constraint
//! propagation, it narrows each variable to
//! a set of candidate ids and each pattern to its candidate links, starting
//! from the constants and the most selective patterns, until nothing changes
//! or a budget of index entries read
//! ([`reduction_budget`](Query::reduction_budget)) is spent.  The joins then
//! read the reduced links rather than the indexes, and their estimates are
//! exact.  A plan only uses its reduction while the graph is as it was when
//! planned.  It reduces only if its estimate of the cost of reducing, and
//! of joining what that finds, is below the cost of the plan without
//! reducing, which it plans first: a reduction that narrows nothing costs
//! more than the joins it is meant to save.
//!
//! It then orders the patterns and unions of each group to minimize
//! an estimated cost, using the counts of the indexes for constants and the
//! graph's [`Stats`](crate::stats::Stats) for variables, and chooses for
//! each how to join it:
//!
//! - a *bind join* evaluates it for each row so far, given the values bound:
//!   for a pattern, a lookup in the best index; good when the rows so far
//!   are few.
//! - a *hash join* evaluates it once, by its constants alone, and joins it
//!   to the rows so far on their shared variables; good when it alone
//!   matches few links.
//!
//! Negated groups are chosen between the same two ways.  Filters and
//! negations are applied as soon as the variables they share with their
//! group are bound.  Up to [`EXHAUSTIVE_PATTERNS`] patterns and unions of a
//! group are planned exhaustively over left-deep orders, more greedily.
//!
//! # Prepared queries
//!
//! A query run many times with different constants can make them
//! [parameters](Query::param), variables bound before the top group starts,
//! and be [prepared](Query::prepare) once: a [`Prepared`] query runs its plan
//! with the values given, without planning again until the graph's size
//! changes by more than [`REPLAN_FACTOR`].  Its plan estimates the lookups of
//! a parameter as those of any bound variable, from the statistics'
//! averages, so it serves every value; a point query then costs its lookups,
//! not its planning.
//!
//! # Computed values and aggregates
//!
//! Once the rows are matched, a query can [`compute`](Query::compute) values
//! from them with [`expr::Expr`]s (arithmetic, comparisons, string, number
//! and time functions), fold groups of them with
//! [`aggregate`](Query::aggregate)s ([`group_by`](Query::group_by),
//! [`having`](Query::having)), and order by what it computed.  Its rows are
//! then [`Cell`]s, values as well as ids: see [`cells`](Query::cells).  An
//! expression can also be a filter of the matched rows,
//! [`Filter::Holds`].  GraQL's computed columns, `let`s, aggregates and
//! `having` lower to these.
//!
//! # Standing queries
//!
//! A [`StandingQuery`] keeps a query's rows up to date as entries are
//! committed: each [`update`](StandingQuery::update) works out the
//! [`Changes`] from the links committed since, without evaluating the query
//! again.
//!
//! # Results
//!
//! [`Plan::execute`] returns the rows as they are produced, so a limit or a
//! caller that stops early stops the work, unless the rows are ordered.
//! Rows are deduplicated only if [`distinct`](Query::distinct) is set: since
//! a graph has one live link per subject, predicate and object, rows of
//! patterns binding every variable are distinct, but selecting fewer
//! variables, or unions, can repeat rows.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use crate::graph::{EphemeralHold, Error, Graph, Result};
use aggregate::{Cell, Function};

pub mod aggregate;
mod compute;
mod exec;
pub mod expr;
pub mod paths;

pub use paths::PathExpr;
mod link_aggregate;
mod maintained;
mod overlay;
mod plan;
mod provenance;
mod reduce;
mod result;
mod rules;
mod standing;
#[cfg(test)]
mod tests;

pub use exec::Rows;
pub use link_aggregate::{LinkAggregate, LinkFunction};
pub use maintained::{MaintainedSubgraph, SubgraphChanges};
pub use overlay::{Overlay, OverlayLink};
pub use plan::{EXHAUSTIVE_PATTERNS, Plan, Prepared, REPLAN_FACTOR};
pub use reduce::DEFAULT_REDUCTION_BUDGET;
pub use result::{Derivation, ResultGraph, ResultLink, ResultNode};
pub use standing::{Changes, StandingQuery};

/// A query variable, made by [`Query::var`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Var(usize);

impl Var {
    /// The variable's number in its query, from 0.
    pub fn index(self) -> usize {
        self.0
    }
}

/// A variable or an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Term {
    Var(Var),
    Id(u64),
}

impl From<Var> for Term {
    fn from(v: Var) -> Self {
        Term::Var(v)
    }
}

impl From<u64> for Term {
    fn from(id: u64) -> Self {
        Term::Id(id)
    }
}

impl Term {
    fn var(self) -> Option<Var> {
        match self {
            Term::Var(v) => Some(v),
            Term::Id(_) => None,
        }
    }
}

/// A pattern matching the live links with its subject, predicate and object,
/// binding `link`, if given, to the link's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pattern {
    pub link: Option<Var>,
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
    /// The id as of which this pattern sees the graph, overriding the
    /// query's.
    pub as_of: Option<u64>,
    /// The subgraph the link must be in (see
    /// [Subgraphs](crate::graph#subgraphs)); a variable not bound binds each
    /// subgraph it is in.
    pub graph: Option<Term>,
    /// The author the link must have (the latest link's, who last changed
    /// the fact); a variable not bound binds it, if it has one.
    pub author: Option<Term>,
}

impl Pattern {
    fn terms(&self) -> [Term; 3] {
        [self.subject, self.predicate, self.object]
    }

    /// The pattern's variables, possibly repeated.
    fn vars(&self) -> impl Iterator<Item = Var> + '_ {
        self.terms()
            .into_iter()
            .filter_map(Term::var)
            .chain(self.link)
            .chain(self.graph.and_then(Term::var))
            .chain(self.author.and_then(Term::var))
    }

    /// Whether the pattern restricts the subgraph or author of its links.
    fn restricts(&self) -> bool {
        self.graph.is_some() || self.author.is_some()
    }
}

/// A test of one bound id.
pub type Test = Arc<dyn Fn(&Graph, u64) -> bool + Send + Sync>;

/// A test of a row, given all its ids (unbound ones 0).
pub type RowTest = Arc<dyn Fn(&Graph, &[u64]) -> bool + Send + Sync>;

/// A condition on the rows of a group.
#[derive(Clone)]
pub enum Filter {
    Eq(Term, Term),
    Ne(Term, Term),
    /// The first id is less than the second: useful for breaking symmetry.
    Lt(Term, Term),
    /// The data of the first value compares to the second's as given:
    /// numerically if both are numbers, else by bytes.  Fails for ids
    /// without data.
    Compare(Term, Cmp, Term),
    /// `test(graph, id)` holds for the id bound to the variable, for example
    /// a test of the data of a value.
    Test(Var, Test),
    /// `test(graph, row)` holds for the row, which binds `vars`, the
    /// variables it reads (some may be unbound, 0, from optional groups); for
    /// example an expression, described by `text`.
    Expr {
        vars: Vec<Var>,
        test: RowTest,
        text: Arc<str>,
    },
    /// The [expression](expr::Expr) is true for the row: a test of the
    /// variables it reads (some may be unbound, from optional groups), which
    /// must be matched by patterns, not computed.  Unlike the filters above,
    /// it reads a parameter given a value that is not an entry (see
    /// [`Plan::execute_values`]) as that value.
    Holds(expr::Expr),
}

/// A comparison of the data of two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    /// Whether `ordering` satisfies the comparison.
    pub fn holds(self, ordering: Ordering) -> bool {
        match self {
            Cmp::Lt => ordering.is_lt(),
            Cmp::Le => ordering.is_le(),
            Cmp::Gt => ordering.is_gt(),
            Cmp::Ge => ordering.is_ge(),
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Cmp::Lt => "<",
            Cmp::Le => "<=",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
        }
    }
}

/// Compares two data: numerically if both are numbers, else by bytes.
pub fn compare_data(a: &[u8], b: &[u8]) -> Ordering {
    let number = |d: &[u8]| std::str::from_utf8(d).ok()?.trim().parse::<f64>().ok();
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => a.cmp(b),
    }
}

impl fmt::Debug for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Filter::Eq(a, b) => write!(f, "Eq({a:?}, {b:?})"),
            Filter::Ne(a, b) => write!(f, "Ne({a:?}, {b:?})"),
            Filter::Lt(a, b) => write!(f, "Lt({a:?}, {b:?})"),
            Filter::Compare(a, c, b) => write!(f, "Compare({a:?}, {c:?}, {b:?})"),
            Filter::Test(v, _) => write!(f, "Test({v:?})"),
            Filter::Expr { text, .. } => write!(f, "Expr({text})"),
            Filter::Holds(e) => write!(f, "Holds({e:?})"),
        }
    }
}

impl Filter {
    fn vars(&self) -> Vec<Var> {
        match self {
            Filter::Eq(a, b) | Filter::Ne(a, b) | Filter::Lt(a, b) | Filter::Compare(a, _, b) => {
                [a, b].into_iter().filter_map(|t| t.var()).collect()
            }
            Filter::Test(v, _) => vec![*v],
            Filter::Expr { vars, .. } => vars.clone(),
            Filter::Holds(e) => e.vars().into_iter().collect(),
        }
    }

    /// Whether `row`, which binds the filter's variables, passes: never if
    /// one is unbound (0), from an optional group.  An expression reads the
    /// parameters given `values` that are not entries as those values.
    fn eval(&self, g: &Graph, row: &[u64], values: &[(usize, Cell)]) -> bool {
        let id = |t: &Term| match *t {
            Term::Var(v) => row[v.0],
            Term::Id(id) => id,
        };
        let unbound = |t: &Term| id(t) == 0;
        let any_unbound = match self {
            Filter::Eq(a, b) | Filter::Ne(a, b) | Filter::Lt(a, b) | Filter::Compare(a, _, b) => {
                unbound(a) || unbound(b)
            }
            Filter::Test(v, _) => row[v.0] == 0,
            // An expression decides for itself (`bound(?x)`).
            Filter::Expr { .. } | Filter::Holds(_) => false,
        };
        if any_unbound {
            return false;
        }
        match self {
            Filter::Eq(a, b) => id(a) == id(b),
            Filter::Ne(a, b) => id(a) != id(b),
            Filter::Lt(a, b) => id(a) < id(b),
            Filter::Compare(a, c, b) => match (g.data(id(a)), g.data(id(b))) {
                (Some(x), Some(y)) => c.holds(compare_data(&x, &y)),
                _ => false,
            },
            Filter::Test(v, test) => test(g, row[v.0]),
            Filter::Expr { test, .. } => test(g, row),
            Filter::Holds(e) if values.is_empty() => e.holds(g, row),
            Filter::Holds(e) => e.holds(g, &expr::WithValues { ids: row, values }),
        }
    }

    /// The estimated fraction of rows that pass.
    fn selectivity(&self) -> f64 {
        match self {
            Filter::Eq(..) => 0.1,
            Filter::Ne(..) => 0.9,
            Filter::Lt(..) | Filter::Compare(..) => 0.5,
            Filter::Test(..) | Filter::Expr { .. } | Filter::Holds(_) => 0.5,
        }
    }
}

/// A pattern matching the pairs of subject and object joined by a chain of
/// one or more live links with its predicate: its transitive closure.  With
/// `expr`, it matches the pairs joined by a chain of links following the
/// path expression instead, and `predicate` is unused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathPattern {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
    pub expr: Option<Arc<PathExpr>>,
    /// The id as of which this pattern sees the graph, overriding the
    /// query's.
    pub as_of: Option<u64>,
    /// A variable set (to 1) in each row the pattern matches, so that a query
    /// recording its links knows which paths a row followed (see
    /// [`Query::record_links`]).
    pub mark: Option<Var>,
}

impl PathPattern {
    fn terms(&self) -> [Term; 3] {
        [self.subject, self.predicate, self.object]
    }

    fn vars(&self) -> impl Iterator<Item = Var> + '_ {
        self.terms()
            .into_iter()
            .filter_map(Term::var)
            .chain(self.mark)
    }
}

/// A relation defined by rules, made by [`Query::relation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Relation(usize);

/// A pattern matching the tuples of a relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub relation: Relation,
    pub args: Vec<Term>,
    /// A variable set (to 1) in each row the call matches, as a path's
    /// `mark` is.
    pub mark: Option<Var>,
}

impl Call {
    fn vars(&self) -> impl Iterator<Item = Var> + '_ {
        self.args.iter().filter_map(|t| t.var()).chain(self.mark)
    }
}

/// A rule: the relation holds for the values of `head` in each row of
/// `body`.
#[derive(Debug, Clone)]
struct Rule {
    relation: Relation,
    head: Vec<Var>,
    body: Group,
    /// The triples asserted for each tuple the rule derives: see
    /// [`Query::rule_asserts`].
    assertions: Vec<Assertion>,
    /// The aggregates of its body, deriving links: see
    /// [`Query::rule_link_aggregate`].
    link_aggregates: Vec<LinkAggregate>,
    /// If the rule defines its relation by aggregating its body's rows,
    /// rather than projecting them: a link aggregate, planned as a rule.
    aggregate: Option<LinkAggregate>,
}

/// A field of an asserted triple (see [`Assertion`]): a term, or what
/// needn't be in the graph yet.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateNode {
    /// An entry, or a variable's value.
    Term(Term),
    /// The value with this data, which needn't be in the graph.
    Value(Cell),
    /// The node with this key, which needn't be in the graph.
    Key(String),
    /// A new node, for each instantiation of the assertion: the same number
    /// is the same node within one.
    Blank(u32),
}

impl From<Term> for TemplateNode {
    fn from(t: Term) -> Self {
        TemplateNode::Term(t)
    }
}

impl From<Var> for TemplateNode {
    fn from(v: Var) -> Self {
        TemplateNode::Term(Term::Var(v))
    }
}

impl From<u64> for TemplateNode {
    fn from(id: u64) -> Self {
        TemplateNode::Term(Term::Id(id))
    }
}

/// Triples a query's [graph](Query::graph) asserts rather than matches:
/// GraQL's `s p o!`, and `s p o! @?x` with a [scope](Self::scope).
///
/// The triples are instantiated once for each distinct binding of their
/// variables and the scope's, in the rows of the query (or for each tuple
/// of a rule, see [`Query::rule_asserts`]), and added to the result as
/// [`Derivation::Asserted`] links, supported by the links of the first row
/// instantiating them.  A row whose scope is unbound instantiates nothing;
/// a triple with a variable unbound is left out.  A triple that is stored
/// and live comes back as its stored link.  Instantiations giving the same
/// triple give one link: the links are a set.  Nothing is written.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Assertion {
    pub triples: Vec<[TemplateNode; 3]>,
    pub scope: Option<Var>,
}

impl Assertion {
    /// An assertion of no triples, to add them to.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a triple.
    pub fn triple(
        mut self,
        subject: impl Into<TemplateNode>,
        predicate: impl Into<TemplateNode>,
        object: impl Into<TemplateNode>,
    ) -> Self {
        self.triples
            .push([subject.into(), predicate.into(), object.into()]);
        self
    }

    /// Instantiates the triples for each value of `v` too: the scope, the
    /// variable the work is done separately for.  Rows where it is unbound
    /// instantiate nothing.
    pub fn scope(mut self, v: Var) -> Self {
        self.scope = Some(v);
        self
    }

    /// The variables an instantiation is distinct by: the triples' and the
    /// scope's, sorted.
    pub(crate) fn vars(&self) -> Vec<Var> {
        let mut vars: Vec<Var> = self
            .triples
            .iter()
            .flatten()
            .filter_map(|n| match n {
                TemplateNode::Term(Term::Var(v)) => Some(*v),
                _ => None,
            })
            .chain(self.scope)
            .collect();
        vars.sort_unstable();
        vars.dedup();
        vars
    }
}

/// Patterns, unions, negations and filters; see the [module](self) docs.
#[derive(Debug, Clone, Default)]
pub struct Group {
    patterns: Vec<Pattern>,
    paths: Vec<PathPattern>,
    calls: Vec<Call>,
    unions: Vec<Vec<Group>>,
    nots: Vec<Group>,
    /// Joined after the rest of the group, keeping rows they don't match.
    optionals: Vec<Group>,
    filters: Vec<Filter>,
    /// The subgraph of the patterns added, if set by `in_graph`.
    graph: Option<Term>,
}

impl Group {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a pattern matching live links with this subject, predicate and
    /// object.
    pub fn pattern(
        &mut self,
        subject: impl Into<Term>,
        predicate: impl Into<Term>,
        object: impl Into<Term>,
    ) -> &mut Self {
        self.patterns.push(Pattern {
            link: None,
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
            as_of: None,
            graph: self.graph,
            author: None,
        });
        self
    }

    /// Restricts the last pattern added to links in `subgraph`: a constant,
    /// or a variable, which binds each subgraph the link is in.
    pub fn in_subgraph(&mut self, subgraph: impl Into<Term>) -> &mut Self {
        self.patterns.last_mut().expect("a pattern").graph = Some(subgraph.into());
        self
    }

    /// Restricts the last pattern added to links by `author`: a constant, or
    /// a variable, which binds the link's author.
    pub fn by_author(&mut self, author: impl Into<Term>) -> &mut Self {
        self.patterns.last_mut().expect("a pattern").author = Some(author.into());
        self
    }

    /// Restricts every pattern of the group, and of the groups in it, that
    /// names no subgraph of its own, to `subgraph`, including those added
    /// to this group later.
    pub fn in_graph(&mut self, subgraph: impl Into<Term>) -> &mut Self {
        let t = subgraph.into();
        fn set(g: &mut Group, t: Term) {
            g.graph = Some(t);
            for p in g.patterns.iter_mut().filter(|p| p.graph.is_none()) {
                p.graph = Some(t);
            }
            for inner in g
                .unions
                .iter_mut()
                .flatten()
                .chain(&mut g.nots)
                .chain(&mut g.optionals)
            {
                set(inner, t);
            }
        }
        set(self, t);
        self
    }

    /// Adds a pattern like [`pattern`](Self::pattern) that also binds `link`
    /// to the id of the link matched.
    pub fn link_pattern(
        &mut self,
        link: Var,
        subject: impl Into<Term>,
        predicate: impl Into<Term>,
        object: impl Into<Term>,
    ) -> &mut Self {
        self.pattern(subject, predicate, object);
        self.patterns.last_mut().unwrap().link = Some(link);
        self
    }

    /// Makes the last pattern added see the graph as it was when `id` was
    /// appended.
    pub fn at(&mut self, id: u64) -> &mut Self {
        if let Some(p) = self.patterns.last_mut() {
            p.as_of = Some(id);
        }
        self
    }

    /// Makes the last path added see the graph as it was when `id` was
    /// appended.
    pub fn path_at(&mut self, id: u64) -> &mut Self {
        if let Some(p) = self.paths.last_mut() {
            p.as_of = Some(id);
        }
        self
    }

    /// Adds a union of `branches`, which must bind the same variables.
    pub fn union(&mut self, branches: impl IntoIterator<Item = Group>) -> &mut Self {
        self.unions.push(branches.into_iter().collect());
        self
    }

    /// Removes the rows for which `group` has a match.
    pub fn not(&mut self, group: Group) -> &mut Self {
        self.nots.push(group);
        self
    }

    /// Extends each row with each match of `group`, or keeps it with the
    /// variables only `group` binds unbound (0) if it has none: a left
    /// outer join, SPARQL's `OPTIONAL`.  Optional groups are joined after
    /// the rest of the group, in the order added.
    pub fn optional(&mut self, group: Group) -> &mut Self {
        self.optionals.push(group);
        self
    }

    pub fn filter(&mut self, filter: Filter) -> &mut Self {
        self.filters.push(filter);
        self
    }

    /// Adds a pattern matching the subjects and objects joined by a chain of
    /// links with `predicate`.
    pub fn path(
        &mut self,
        subject: impl Into<Term>,
        predicate: impl Into<Term>,
        object: impl Into<Term>,
    ) -> &mut Self {
        self.paths.push(PathPattern {
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
            expr: None,
            as_of: None,
            mark: None,
        });
        self
    }

    /// Adds a pattern matching the pairs of subject and object joined by a
    /// chain of links following `expr` (see [`paths`]), each pair once.
    pub fn path_expr(
        &mut self,
        subject: impl Into<Term>,
        expr: PathExpr,
        object: impl Into<Term>,
    ) -> &mut Self {
        self.paths.push(PathPattern {
            subject: subject.into(),
            predicate: Term::Id(0),
            object: object.into(),
            expr: Some(Arc::new(expr)),
            as_of: None,
            mark: None,
        });
        self
    }

    /// Adds a pattern matching the tuples of `relation`.
    pub fn call<T: Into<Term>>(
        &mut self,
        relation: Relation,
        args: impl IntoIterator<Item = T>,
    ) -> &mut Self {
        self.calls.push(Call {
            relation,
            args: args.into_iter().map(Into::into).collect(),
            mark: None,
        });
        self
    }

    /// Marks the rows the last path pattern added matches, with `v`.
    pub(crate) fn mark_path(&mut self, v: Var) {
        self.paths.last_mut().expect("a path").mark = Some(v);
    }

    /// Marks the rows the last call added matches, with `v`.
    pub(crate) fn mark_call(&mut self, v: Var) {
        self.calls.last_mut().expect("a call").mark = Some(v);
    }

    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// The groups directly in this one: union branches, negations and
    /// optional groups.
    fn groups(&self) -> impl Iterator<Item = &Group> {
        self.unions
            .iter()
            .flatten()
            .chain(&self.nots)
            .chain(&self.optionals)
    }

    /// The earliest `as_of` of the group's patterns and paths, and those of
    /// the groups in it.
    fn earliest_as_of(&self) -> Option<u64> {
        let own = self.patterns.iter().filter_map(|p| p.as_of);
        let paths = self.paths.iter().filter_map(|p| p.as_of);
        let inner = self.groups().filter_map(Group::earliest_as_of);
        own.chain(paths).chain(inner).min()
    }

    /// Whether the group, or one in it, has an optional group.
    pub(crate) fn has_optionals(&self) -> bool {
        !self.optionals.is_empty() || self.groups().any(Group::has_optionals)
    }

    /// Whether the group, or one in it, has a path or a call.
    fn has_paths_or_calls(&self) -> bool {
        !self.paths.is_empty()
            || !self.calls.is_empty()
            || self.groups().any(Group::has_paths_or_calls)
    }

    /// The variables every row of the group binds.
    fn bound_vars(&self) -> BTreeSet<Var> {
        let mut vars: BTreeSet<Var> = self.patterns.iter().flat_map(Pattern::vars).collect();
        vars.extend(self.paths.iter().flat_map(PathPattern::vars));
        vars.extend(self.calls.iter().flat_map(Call::vars));
        // Every branch binds the same variables, but for the hidden link
        // variables of a query recording its links, which differ.
        for b in self.unions.iter().flatten() {
            vars.extend(b.bound_vars());
        }
        vars
    }

    /// The variables a row of the group may bind: those every row binds,
    /// and those its optional groups may.
    pub(crate) fn maybe_vars(&self) -> BTreeSet<Var> {
        let mut vars = self.bound_vars();
        for g in &self.optionals {
            vars.extend(g.maybe_vars());
        }
        vars
    }

    /// The filters of the group and those in it.
    fn all_filters(&self) -> Vec<&Filter> {
        let mut filters: Vec<&Filter> = self.filters.iter().collect();
        for g in self.groups() {
            filters.extend(g.all_filters());
        }
        filters
    }

    /// Every variable the group uses.
    fn all_vars(&self) -> BTreeSet<Var> {
        let mut vars = self.bound_vars();
        vars.extend(self.filters.iter().flat_map(Filter::vars));
        for g in self.groups() {
            vars.extend(g.all_vars());
        }
        vars
    }

    /// Checks that the group and those in it have a pattern or union, and
    /// that the branches of each union bind the same variables, apart from
    /// `params`, which are bound already.
    fn check(&self, relations: &[(String, usize)], params: &[Var]) -> Result<()> {
        if self.patterns.is_empty()
            && self.paths.is_empty()
            && self.calls.is_empty()
            && self.unions.is_empty()
        {
            return Err(Error::InvalidArgument("a group needs a pattern or a union"));
        }
        for p in &self.paths {
            if let Some(e) = &p.expr {
                e.check().map_err(Error::InvalidArgument)?;
            }
        }
        for c in &self.calls {
            if relations
                .get(c.relation.0)
                .is_none_or(|r| r.1 != c.args.len())
            {
                return Err(Error::InvalidArgument(
                    "a call has the wrong number of arguments",
                ));
            }
        }
        for branches in &self.unions {
            let Some(first) = branches.first() else {
                return Err(Error::InvalidArgument("a union needs a branch"));
            };
            let binds = |b: &Group| {
                let mut vars = b.bound_vars();
                vars.retain(|v| !params.contains(v));
                vars
            };
            if branches.iter().any(|b| binds(b) != binds(first)) {
                return Err(Error::InvalidArgument(
                    "the branches of a union must bind the same variables",
                ));
            }
        }
        for g in self.groups() {
            g.check(relations, params)?;
        }
        Ok(())
    }
}

/// A key to order rows by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub var: Var,
    /// Orders by data rather than by id, as
    /// [`compare_cells`](aggregate::compare_cells) does: numbers by value,
    /// then other data, then entries without data by id.
    pub by_data: bool,
    pub descending: bool,
}

impl SortKey {
    /// Orders by the id bound to `var`.
    pub fn id(var: Var) -> Self {
        SortKey {
            var,
            by_data: false,
            descending: false,
        }
    }

    /// Orders by the data of the value bound to `var`, then by id.
    pub fn data(var: Var) -> Self {
        SortKey {
            by_data: true,
            ..Self::id(var)
        }
    }

    pub fn desc(self) -> Self {
        SortKey {
            descending: true,
            ..self
        }
    }
}

/// A graph pattern query; see the [module](self) docs.
#[derive(Debug, Clone, Default)]
pub struct Query {
    names: Vec<String>,
    root: Group,
    select: Option<Vec<Var>>,
    distinct: bool,
    order: Vec<SortKey>,
    limit: Option<usize>,
    /// The rows to skip, after ordering.
    offset: usize,
    as_of: Option<u64>,
    reduction_budget: Option<u64>,
    /// The parameters, in the order their values are given: variables bound
    /// before the query starts (see [`param`](Self::param)).
    params: Vec<Var>,
    /// Whether to record the links each row matches (see
    /// [`record_links`](Self::record_links)); and once planned so, the link
    /// variables recording them, of which those added are `hidden`.
    record: bool,
    recorded: Vec<Var>,
    hidden: Vec<Var>,
    /// Whether `recorded` was given (see `record_links_of`), so that the
    /// patterns aren't given link variables when planned.
    record_given: bool,
    /// The variables computed from each row, or folded over each group of
    /// rows, in the order declared (see [`compute`](Self::compute) and
    /// [`aggregate`](Self::aggregate)).
    computed: Vec<(Var, Computed)>,
    /// The variables to group rows by for the aggregates, if given.
    group_by: Option<Vec<Var>>,
    /// Tests of the groups.
    having: Vec<expr::Expr>,
    /// The name and arity of each relation.
    relations: Vec<(String, usize)>,
    rules: Vec<Rule>,
    /// The triples the query's graph asserts.
    assertions: Vec<Assertion>,
    /// The aggregates of the top group, deriving links (see
    /// [`link_aggregate`](Self::link_aggregate)); and once planned, their
    /// relations.
    link_aggregates: Vec<LinkAggregate>,
    aggregated: Vec<(Relation, LinkAggregate)>,
    /// The nested limits: each variable's values kept, in order (see
    /// [`limit_values`](Self::limit_values)).
    value_limits: Vec<(Var, usize, usize)>,
    /// The hold of the ephemeral ids it gives (see
    /// [`hold_ephemeral`](Self::hold_ephemeral)).
    held: Option<EphemeralHold>,
}

/// How a variable's value is computed, after the rows are matched.
#[derive(Debug, Clone)]
pub(crate) enum Computed {
    /// From the row.
    Value(expr::Expr),
    /// Folded over the rows of a group: of the argument's values, or the
    /// rows, for `count(*)`.
    Aggregate {
        function: Function,
        arg: Option<expr::Expr>,
        distinct: bool,
    },
}

/// Names an id in a description of a query.
type Namer<'a> = &'a dyn Fn(u64) -> String;

impl Query {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new relation of `arity` columns, defined by [`rule`](Self::rule)s;
    /// `name` is for [`Plan::explain`].
    pub fn relation(&mut self, name: &str, arity: usize) -> Relation {
        self.relations.push((name.to_string(), arity));
        Relation(self.relations.len() - 1)
    }

    /// Adds a rule: `relation` holds for the values of `head` in each row of
    /// `body`.  A relation's rules may call it and each other, but not
    /// within a negation of themselves.
    pub fn rule(
        &mut self,
        relation: Relation,
        head: impl IntoIterator<Item = Var>,
        body: Group,
    ) -> &mut Self {
        self.rules.push(Rule {
            relation,
            head: head.into_iter().collect(),
            body,
            assertions: Vec::new(),
            link_aggregates: Vec::new(),
            aggregate: None,
        });
        self
    }

    /// Keeps, of the rows, those whose value of `v` is among the first
    /// `limit` values of `v` after `offset`, in order (as
    /// [`SortKey::data`] orders them: numbers by value, other values by
    /// their data, nodes by id), within each group of rows sharing the
    /// values of the variables of the limits added before: GraQL's nested
    /// limits ("at most 10 people, and at most 10 types for each").  Applied
    /// after matching, before `distinct`, `order_by` and `limit`.  Rows with
    /// `v` unbound are kept.
    pub fn limit_values(&mut self, v: Var, limit: usize, offset: usize) -> &mut Self {
        self.value_limits.push((v, limit, offset));
        self
    }

    /// Adds an aggregate deriving links to the top group (see
    /// [`LinkAggregate`]): the rest of the group can match its links.  Its
    /// rows are those of the rest of the group: so a query with an
    /// aggregate whose links it doesn't match has the rows it has without
    /// it, and its [graph](Self::graph) has the links.
    /// Holds the ephemeral ids the query gives (the values it computes, and
    /// a parameter's value not in the graph) with `hold`, so that they are
    /// freed with it, rather than kept while the graph is open (see
    /// [`Graph::ephemeral_hold`]).  Its constants may be held by it too.
    pub fn hold_ephemeral(&mut self, hold: EphemeralHold) -> &mut Self {
        self.held = Some(hold);
        self
    }

    /// The hold of the ephemeral ids it gives, if given one.
    pub fn ephemeral_hold(&self) -> Option<&EphemeralHold> {
        self.held.as_ref()
    }

    /// The id of the value with `data`, ephemeral if the graph hasn't it:
    /// held by the query's hold, if any.
    pub(crate) fn ephemeral_value(&self, g: &Graph, data: &[u8]) -> u64 {
        match &self.held {
            Some(h) => h.value(g, data),
            None => g.ephemeral_value(data),
        }
    }

    pub fn link_aggregate(&mut self, a: LinkAggregate) -> &mut Self {
        self.link_aggregates.push(a);
        self
    }

    /// Adds an aggregate deriving links to the body of the last rule added.
    pub fn rule_link_aggregate(&mut self, a: LinkAggregate) -> &mut Self {
        self.rules
            .last_mut()
            .expect("a rule")
            .link_aggregates
            .push(a);
        self
    }

    /// Asserts triples in the query's [graph](Self::graph) (see
    /// [`Assertion`]).  Their variables must be bound by the query's
    /// patterns (optionally, at least).  The query's rows are unchanged.
    pub fn assert(&mut self, a: Assertion) -> &mut Self {
        self.assertions.push(a);
        self
    }

    /// Makes the last rule added assert triples for each tuple it derives,
    /// as [`Derivation::Rule`] links, when a query recording its links calls
    /// it: GraQL's `!` in a rule's body.  Their variables must be the
    /// rule's head's.  A tuple derived by several rules gets the assertions
    /// of each.
    pub fn rule_asserts(&mut self, a: Assertion) -> &mut Self {
        self.rules.last_mut().expect("a rule").assertions.push(a);
        self
    }

    /// See [`Group::path`].
    pub fn path(
        &mut self,
        subject: impl Into<Term>,
        predicate: impl Into<Term>,
        object: impl Into<Term>,
    ) -> &mut Self {
        self.root.path(subject, predicate, object);
        self
    }

    /// See [`Group::call`].
    pub fn call<T: Into<Term>>(
        &mut self,
        relation: Relation,
        args: impl IntoIterator<Item = T>,
    ) -> &mut Self {
        self.root.call(relation, args);
        self
    }

    /// A new variable; `name` is for [`Plan::explain`].
    pub fn var(&mut self, name: &str) -> Var {
        self.names.push(name.to_string());
        Var(self.names.len() - 1)
    }

    /// A new parameter: a variable whose value is given when the query
    /// runs, by [`Plan::execute_with`] or [`Prepared::execute`], in the order
    /// the parameters are declared.  It is used like any variable, but is
    /// bound before the top group starts, so a plan made once serves every
    /// value; see [`prepare`](Self::prepare).  A parameter is not selected
    /// unless named in [`select`](Self::select), and a rule's body can't use
    /// one.  A query with parameters is not reduced.
    pub fn param(&mut self, name: &str) -> Var {
        let v = self.var(name);
        self.params.push(v);
        v
    }

    /// The parameters, in the order their values are given.
    pub fn params(&self) -> &[Var] {
        &self.params
    }

    /// The name `v` was made with.
    pub fn name(&self, v: Var) -> &str {
        &self.names[v.0]
    }

    /// Whether each variable is a parameter.
    pub(crate) fn param_mask(&self) -> Vec<bool> {
        let mut mask = vec![false; self.names.len()];
        self.params.iter().for_each(|v| mask[v.0] = true);
        mask
    }

    /// A new variable whose value is `e`, computed for each row once the
    /// rows are matched: `e` reads the variables the patterns bind, the
    /// parameters and the variables computed before.  A computed variable
    /// can be selected, ordered by, grouped by and aggregated, but a pattern
    /// or filter can't use it.  A query computing values returns its rows
    /// as [`Cell`]s: see [`cells`](Self::cells).
    pub fn compute(&mut self, name: &str, e: expr::Expr) -> Var {
        let v = self.var(name);
        self.computed.push((v, Computed::Value(e)));
        v
    }

    /// A new variable whose value is `function` folded over the values of
    /// `arg` (of the distinct values, if `distinct`) in each group of rows,
    /// or over the rows, for `count(*)`, if `arg` is `None`.  The rows are
    /// grouped by [`group_by`](Self::group_by), or by default by the
    /// variables selected that are not aggregates; with none, every row is
    /// in one group, which there is even with no rows.  Unbound values
    /// (from optional groups, or that can't be computed) are skipped.
    pub fn aggregate(
        &mut self,
        name: &str,
        function: Function,
        arg: Option<expr::Expr>,
        distinct: bool,
    ) -> Var {
        let v = self.var(name);
        self.computed.push((
            v,
            Computed::Aggregate {
                function,
                arg,
                distinct,
            },
        ));
        v
    }

    /// Groups the rows by the values of `vars` for the aggregates, rather
    /// than by the variables selected that are not aggregates.  Every
    /// variable selected must then be one of `vars` or an aggregate.
    pub fn group_by(&mut self, vars: impl IntoIterator<Item = Var>) -> &mut Self {
        self.group_by = Some(vars.into_iter().collect());
        self
    }

    /// Keeps the groups for which `e`, of the variables grouped by, the
    /// aggregates and the parameters, is true.
    pub fn having(&mut self, e: expr::Expr) -> &mut Self {
        self.having.push(e);
        self
    }

    /// Makes the query record the links each of its rows matches, so that
    /// it can return them as a graph: see [`graph`](Self::graph).  Each
    /// pattern of the top group, of its unions' branches and of its optional
    /// groups (not its negations, which match nothing in a row) binds its
    /// link, to a hidden variable if it names none.  The rows are the same.
    pub fn record_links(&mut self) -> &mut Self {
        self.record = true;
        self
    }

    /// Plans and runs the query, returning the links its rows match, and
    /// the entries its selected variables bind that aren't in them, as a
    /// [`ResultGraph`].
    pub fn graph(&self, g: &Graph) -> Result<ResultGraph> {
        let mut q = self.clone();
        q.record_links();
        q.plan(g)?.execute_graph(g, &[])
    }

    /// Makes the query record the links bound to `vars`, link variables of
    /// its patterns, rather than of every pattern: as GraQL's `lookup … where`
    /// records its pattern's links and not its `where`'s.
    pub(crate) fn record_links_of(&mut self, vars: impl IntoIterator<Item = Var>) -> &mut Self {
        self.record = true;
        self.record_given = true;
        self.recorded = vars.into_iter().collect();
        self
    }

    /// A new variable that is not selected by default, and that a union's
    /// branches needn't all bind: a link variable recording a link.
    pub(crate) fn hidden_var(&mut self, name: &str) -> Var {
        let v = self.var(name);
        self.hidden.push(v);
        v
    }

    /// The query with a link variable for each pattern that records links,
    /// if it records them and doesn't have them yet.
    pub(crate) fn with_link_vars(&self) -> Option<Query> {
        if !self.record || self.record_given || !self.recorded.is_empty() {
            return None;
        }
        fn group(q: &mut Query, g: &mut Group) {
            for p in &mut g.paths {
                p.mark = Some(q.hidden_var(&format!("_path{}", q.hidden.len())));
            }
            for c in &mut g.calls {
                c.mark = Some(q.hidden_var(&format!("_call{}", q.hidden.len())));
            }
            for p in &mut g.patterns {
                let v = match p.link {
                    Some(v) => v,
                    None => {
                        let v = q.var(&format!("_link{}", q.hidden.len()));
                        q.hidden.push(v);
                        p.link = Some(v);
                        v
                    }
                };
                q.recorded.push(v);
            }
            for b in g.unions.iter_mut().flatten() {
                group(q, b);
            }
            for o in &mut g.optionals {
                group(q, o);
            }
        }
        let mut q = self.clone();
        // The selected variables, as they were, since link variables are
        // not.
        let selected = q.selected();
        let mut root = std::mem::take(&mut q.root);
        group(&mut q, &mut root);
        q.root = root;
        q.select = Some(selected);
        q.record_given = true;
        Some(q)
    }

    /// Whether the query computes values or aggregates, so that its rows
    /// are [`cells`](Self::cells) rather than ids.
    pub fn computes(&self) -> bool {
        !self.computed.is_empty()
    }

    /// Whether the query has aggregates.
    fn aggregates(&self) -> bool {
        self.computed
            .iter()
            .any(|(_, c)| matches!(c, Computed::Aggregate { .. }))
    }

    /// The query's top group.
    pub fn group(&mut self) -> &mut Group {
        &mut self.root
    }

    /// See [`Group::pattern`].
    pub fn pattern(
        &mut self,
        subject: impl Into<Term>,
        predicate: impl Into<Term>,
        object: impl Into<Term>,
    ) -> &mut Self {
        self.root.pattern(subject, predicate, object);
        self
    }

    /// See [`Group::link_pattern`].
    pub fn link_pattern(
        &mut self,
        link: Var,
        subject: impl Into<Term>,
        predicate: impl Into<Term>,
        object: impl Into<Term>,
    ) -> &mut Self {
        self.root.link_pattern(link, subject, predicate, object);
        self
    }

    /// See [`Group::in_subgraph`].
    pub fn in_subgraph(&mut self, subgraph: impl Into<Term>) -> &mut Self {
        self.root.in_subgraph(subgraph);
        self
    }

    /// See [`Group::by_author`].
    pub fn by_author(&mut self, author: impl Into<Term>) -> &mut Self {
        self.root.by_author(author);
        self
    }

    /// See [`Group::in_graph`]: the query's patterns, in `subgraph`.
    pub fn in_graph(&mut self, subgraph: impl Into<Term>) -> &mut Self {
        self.root.in_graph(subgraph);
        self
    }

    /// Whether a pattern of the query, or of its rules, restricts the
    /// subgraph or author of its links.
    pub(crate) fn restricts_links(&self) -> bool {
        fn group(g: &Group) -> bool {
            g.patterns.iter().any(Pattern::restricts)
                || g.unions.iter().flatten().any(group)
                || g.nots.iter().chain(&g.optionals).any(group)
        }
        group(&self.root) || self.rules.iter().any(|r| group(&r.body))
    }

    /// See [`Group::at`].
    pub fn at(&mut self, id: u64) -> &mut Self {
        self.root.at(id);
        self
    }

    /// See [`Group::union`].
    pub fn union(&mut self, branches: impl IntoIterator<Item = Group>) -> &mut Self {
        self.root.union(branches);
        self
    }

    /// See [`Group::not`].
    pub fn not(&mut self, group: Group) -> &mut Self {
        self.root.not(group);
        self
    }

    /// See [`Group::optional`].
    pub fn optional(&mut self, group: Group) -> &mut Self {
        self.root.optional(group);
        self
    }

    /// See [`Group::filter`].
    pub fn filter(&mut self, filter: Filter) -> &mut Self {
        self.root.filter(filter);
        self
    }

    /// The variables each row returns, in order; by default, all those the
    /// query binds.
    pub fn select(&mut self, vars: impl IntoIterator<Item = Var>) -> &mut Self {
        self.select = Some(vars.into_iter().collect());
        self
    }

    /// Returns each selected row once.
    pub fn distinct(&mut self) -> &mut Self {
        self.distinct = true;
        self
    }

    /// Orders the rows by `key`, after any keys given before.
    pub fn order_by(&mut self, key: SortKey) -> &mut Self {
        self.order.push(key);
        self
    }

    /// Returns at most `n` rows.
    /// Skips the first `n` rows, after ordering: for paging, with
    /// [`limit`](Self::limit).
    pub fn offset(&mut self, n: usize) -> &mut Self {
        self.offset = n;
        self
    }

    pub fn limit(&mut self, n: usize) -> &mut Self {
        self.limit = Some(n);
        self
    }

    /// Sees the graph as it was when `id` was appended, except in patterns
    /// given their own.
    pub fn as_of(&mut self, id: u64) -> &mut Self {
        self.as_of = Some(id);
        self
    }

    /// Limits the reduction of the top group to reading about `entries`
    /// index entries ([`DEFAULT_REDUCTION_BUDGET`] by default); 0 turns it
    /// off.  Giving a budget also makes the plan reduce whether or not it
    /// estimates that reducing pays.
    pub fn reduction_budget(&mut self, entries: u64) -> &mut Self {
        self.reduction_budget = Some(entries);
        self
    }

    /// The patterns of the top group.
    pub fn patterns(&self) -> &[Pattern] {
        &self.root.patterns
    }

    /// Plans the query against `g`'s indexes and statistics.  Fails if a
    /// group has no pattern or union, the branches of a union bind different
    /// variables, or a variable used is not bound where it is used.
    pub fn plan(&self, g: &Graph) -> Result<Plan> {
        plan::plan(self, g)
    }

    /// Checks what [`plan`](Self::plan) checks before it reads the graph:
    /// that unions bind the same variables, rule heads are bound, selected
    /// variables are bound, and rules are stratified.  It evaluates
    /// nothing.
    pub fn validate(&self) -> Result<()> {
        self.check()?;
        rules::check(self)
    }

    /// Fails if the query reads the graph as of an id before the horizon its
    /// large index entries are compacted to (see
    /// [`Graph::compact`](crate::graph::Graph::compact)), whose answers may
    /// have changed.
    pub(crate) fn check_horizon(&self, g: &Graph) -> Result<()> {
        let horizon = g.horizon();
        // A reader sees the graph as of its end, which a compaction since may
        // have passed.
        let reader = g.is_reader().then(|| g.next_id().saturating_sub(1));
        let earliest = [self.as_of, self.root.earliest_as_of(), reader]
            .into_iter()
            .chain(self.rules.iter().map(|r| r.body.earliest_as_of()))
            .flatten()
            .min();
        match earliest {
            Some(as_of) if as_of < horizon => Err(Error::Compacted { as_of, horizon }),
            _ => Ok(()),
        }
    }

    /// Plans and runs the query, collecting its rows.
    pub fn run(&self, g: &Graph) -> Result<Vec<Vec<u64>>> {
        self.plan(g)?.execute(g).collect()
    }

    /// Plans and runs the query beneath `overlay` (see [`Overlay`]),
    /// collecting its rows.
    pub fn run_over(&self, g: &Graph, overlay: &Arc<Overlay>) -> Result<Vec<Vec<u64>>> {
        self.plan(g)?.execute_over(g, overlay, &[]).collect()
    }

    /// The same as cells: see [`cells`](Self::cells).
    pub fn cells_over(&self, g: &Graph, overlay: &Arc<Overlay>) -> Result<Vec<Vec<Cell>>> {
        self.plan(g)?.execute_cells_over(g, overlay, &[])
    }

    /// The same as a graph: see [`graph`](Self::graph).
    pub fn graph_over(&self, g: &Graph, overlay: &Arc<Overlay>) -> Result<ResultGraph> {
        let mut q = self.clone();
        q.record_links();
        q.plan(g)?.execute_graph_over(g, overlay, &[])
    }

    /// Plans and runs the query, collecting its rows as cells: the values of
    /// the variables selected, computed ones included.  Any query can be run
    /// so; one that [`computes`](Self::computes) values must be.
    pub fn cells(&self, g: &Graph) -> Result<Vec<Vec<Cell>>> {
        self.plan(g)?.execute_cells(g, &[])
    }

    /// Plans the query once, to run many times with different values of its
    /// [parameters](Self::param); see [`Prepared`].
    pub fn prepare(&self, g: &Graph) -> Result<Prepared> {
        Prepared::new(self, g)
    }

    /// The selected variables: by default, those the top group may bind,
    /// apart from the parameters, then those computed.
    fn selected(&self) -> Vec<Var> {
        self.select.clone().unwrap_or_else(|| {
            self.root
                .maybe_vars()
                .into_iter()
                .filter(|v| !self.params.contains(v) && !self.hidden.contains(v))
                .chain(self.computed.iter().map(|(v, _)| *v))
                .collect()
        })
    }

    /// The variables the rows are grouped by for the aggregates: as given,
    /// or the variables selected that are not aggregates.
    fn grouping(&self) -> Vec<Var> {
        self.group_by.clone().unwrap_or_else(|| {
            let aggregate = |v: &Var| {
                self.computed
                    .iter()
                    .any(|(w, c)| w == v && matches!(c, Computed::Aggregate { .. }))
            };
            self.selected()
                .into_iter()
                .filter(|v| !aggregate(v))
                .collect()
        })
    }

    /// Checks the computed variables, the grouping and `having`.
    fn check_computed(&self, matched: &BTreeSet<Var>) -> Result<()> {
        let invalid = |what| Err(Error::InvalidArgument(what));
        // What a computed value can read: the matched variables, the
        // parameters, and the values computed before it.
        let mut values = matched.clone();
        let mut aggregates = BTreeSet::new();
        for (v, c) in &self.computed {
            match c {
                Computed::Value(e) => {
                    if !e.vars().is_subset(&values) {
                        return invalid(
                            "a computed value reads a variable not matched or computed before",
                        );
                    }
                    values.insert(*v);
                }
                Computed::Aggregate { arg, .. } => {
                    if arg.as_ref().is_some_and(|e| !e.vars().is_subset(&values)) {
                        return invalid(
                            "an aggregate reads a variable not matched or computed before",
                        );
                    }
                    aggregates.insert(*v);
                }
            }
        }
        let filters = self.root.all_filters();
        let computed: BTreeSet<Var> = self.computed.iter().map(|(v, _)| *v).collect();
        if filters
            .iter()
            .any(|f| f.vars().iter().any(|v| computed.contains(v)))
        {
            return invalid("a filter can't read a computed variable: write its expression");
        }
        if aggregates.is_empty() {
            if self.group_by.is_some() || !self.having.is_empty() {
                return invalid("'group by' and 'having' need an aggregate");
            }
            return Ok(());
        }
        let grouping = self.grouping();
        if !grouping.iter().all(|v| values.contains(v)) {
            return invalid("a variable grouped by is not matched or computed");
        }
        // After grouping: the groups' values, the aggregates, the parameters.
        let mut grouped: BTreeSet<Var> = grouping.into_iter().collect();
        grouped.extend(&aggregates);
        grouped.extend(&self.params);
        if !self.selected().iter().all(|v| grouped.contains(v)) {
            return invalid(
                "with aggregates, a variable selected must be grouped by or aggregated",
            );
        }
        if !self.order.iter().all(|k| grouped.contains(&k.var)) {
            return invalid(
                "with aggregates, a variable ordered by must be grouped by or aggregated",
            );
        }
        if !self.having.iter().all(|e| e.vars().is_subset(&grouped)) {
            return invalid("'having' reads a variable not grouped by or aggregated");
        }
        Ok(())
    }

    /// Checks the query's groups, and that the variables selected and
    /// ordered by are bound.
    fn check(&self) -> Result<()> {
        let bound_first: Vec<Var> = self.params.iter().chain(&self.hidden).copied().collect();
        self.root.check(&self.relations, &bound_first)?;
        for rule in &self.rules {
            rule.body.check(&self.relations, &[])?;
            if rule.body.all_vars().iter().any(|v| self.params.contains(v)) {
                return Err(Error::InvalidArgument(
                    "a rule's body can't use a parameter",
                ));
            }
            let bound = rule.body.maybe_vars();
            if self.relations[rule.relation.0].1 != rule.head.len()
                || !rule.head.iter().all(|v| bound.contains(v))
            {
                return Err(Error::InvalidArgument(
                    "a rule's head must have its relation's arity and be bound by its body",
                ));
            }
            if rule
                .assertions
                .iter()
                .flat_map(Assertion::vars)
                .any(|v| !rule.head.contains(&v))
            {
                return Err(Error::InvalidArgument(
                    "a rule's asserted triples can use only its head's variables",
                ));
            }
        }
        let mut bound = self.root.maybe_vars();
        if self
            .assertions
            .iter()
            .flat_map(Assertion::vars)
            .any(|v| !bound.contains(&v) && !self.params.contains(&v))
        {
            return Err(Error::InvalidArgument(
                "an asserted triple's variables must be bound by the query's patterns",
            ));
        }
        bound.extend(self.params.iter().copied());
        self.check_computed(&bound)?;
        bound.extend(self.computed.iter().map(|(v, _)| *v));
        let used = self
            .selected()
            .into_iter()
            .chain(self.order.iter().map(|k| k.var));
        for v in used {
            if !bound.contains(&v) {
                return Err(Error::InvalidArgument(
                    "a variable selected or ordered by is not bound",
                ));
            }
        }
        Ok(())
    }

    fn var_name(&self, v: Var) -> String {
        let sigil = if self.params.contains(&v) { '$' } else { '?' };
        format!("{sigil}{}", self.names[v.0])
    }

    /// The name of `t`: a variable's, or `name` of an id.
    fn term_name(&self, t: Term, name: Namer<'_>) -> String {
        match t {
            Term::Var(v) => self.var_name(v),
            Term::Id(id) => name(id),
        }
    }

    fn pattern_name(&self, p: &Pattern, name: Namer<'_>) -> String {
        let spo = format!(
            "{} {} {}",
            self.term_name(p.subject, name),
            self.term_name(p.predicate, name),
            self.term_name(p.object, name)
        );
        let mut spo = match p.link {
            Some(l) => format!("{} = ({spo})", self.var_name(l)),
            None => format!("({spo})"),
        };
        if let Some(t) = p.graph {
            spo = format!("{spo} in {}", self.term_name(t, name));
        }
        if let Some(t) = p.author {
            spo = format!("{spo} by {}", self.term_name(t, name));
        }
        match p.as_of {
            Some(id) => format!("{spo} as of {id}"),
            None => spo,
        }
    }

    fn path_name(&self, p: &PathPattern, name: Namer<'_>) -> String {
        let predicate = match &p.expr {
            Some(e) => format!("({})", e.display(name)),
            None => format!("{}++", self.term_name(p.predicate, name)),
        };
        let spo = format!(
            "({} {predicate} {})",
            self.term_name(p.subject, name),
            self.term_name(p.object, name)
        );
        match p.as_of {
            Some(id) => format!("{spo} as of {id}"),
            None => spo,
        }
    }

    fn call_name(&self, c: &Call, name: Namer<'_>) -> String {
        let args: Vec<String> = c.args.iter().map(|&t| self.term_name(t, name)).collect();
        format!("{}({})", self.relations[c.relation.0].0, args.join(" "))
    }

    fn filter_name(&self, f: &Filter, name: Namer<'_>) -> String {
        match f {
            Filter::Eq(a, b) => format!(
                "{} = {}",
                self.term_name(*a, name),
                self.term_name(*b, name)
            ),
            Filter::Ne(a, b) => format!(
                "{} != {}",
                self.term_name(*a, name),
                self.term_name(*b, name)
            ),
            Filter::Lt(a, b) => format!(
                "{} < {}",
                self.term_name(*a, name),
                self.term_name(*b, name)
            ),
            Filter::Compare(a, c, b) => format!(
                "data({}) {} data({})",
                self.term_name(*a, name),
                c.symbol(),
                self.term_name(*b, name)
            ),
            Filter::Test(v, _) => format!("test({})", self.var_name(*v)),
            Filter::Expr { text, .. } => text.to_string(),
            Filter::Holds(e) => e.show(&|v| self.var_name(v), name),
        }
    }

    fn sort_key_name(&self, k: &SortKey) -> String {
        let v = self.var_name(k.var);
        let key = if k.by_data { format!("data({v})") } else { v };
        if k.descending {
            format!("{key} desc")
        } else {
            key
        }
    }
}
