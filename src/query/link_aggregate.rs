//! Aggregates as links: `COUNT(s p o)`, the rankings, and the other folds,
//! deriving a link `s p v` for each subject (see [`LinkAggregate`]).
//!
//! A query's link aggregates are planned as rules: each defines a relation
//! of `(s, v)` (and its scope, if it has one) by folding the rows of its
//! *base*, the rest of its group without the patterns that match its own
//! links, or those of the aggregates after it, and what depends on them.
//! Those patterns become calls of the relations.  A relation is evaluated,
//! as a negation is, after what its base reads, so the rest of the group,
//! and the aggregates after it, can match its links.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use super::aggregate::{self, Aggregate, Cell, Function, compare_cells};
use super::provenance::RowProvenance;
use super::{Call, Group, Query, Relation, Rule, Term, Var};
use crate::graph::{Error, Graph, Result};

/// What a [`LinkAggregate`] derives for each subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkFunction {
    /// A fold of its objects: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `SAMPLE`.
    Fold(Function),
    /// Its rank among the subjects by key, from 0, equal keys ranked equal
    /// and the ranks after them skipped (`0, 0, 2`): SQL's `RANK`, and
    /// GraQL's `ORDER`.
    Rank,
    /// The same, not skipping (`0, 0, 1`): `DENSE_RANK`.
    DenseRank,
    /// A rank of its own (`0, 1, 2`), equal keys ordered by the subjects'
    /// ids: `ROW_NUMBER`.
    RowNumber,
}

impl LinkFunction {
    /// The name, as GraQL writes it: `COUNT`, `RANK`, ...
    pub fn name(self) -> String {
        match self {
            LinkFunction::Fold(f) => f.name().to_ascii_uppercase(),
            LinkFunction::Rank => "RANK".into(),
            LinkFunction::DenseRank => "DENSE_RANK".into(),
            LinkFunction::RowNumber => "ROW_NUMBER".into(),
        }
    }

    pub(crate) fn ranks(self) -> bool {
        !matches!(self, LinkFunction::Fold(_))
    }
}

/// An aggregate deriving links: for each subject `s` (in each group of the
/// scope, if any), the link `s predicate v`, where `v` folds the subject's
/// objects in the rows of the rest of the pattern, or ranks the subject by
/// them.  GraQL's `COUNT(?g grandkids ?c)` and `ORDER(?m order ?from desc)
/// @?p`.
///
/// - **Folds** group the rows by `subject` and `scope` and fold `object`
///   as a select's aggregate does: `COUNT` counts the rows binding it (or
///   its distinct values, if `distinct`), `SUM` and `AVG` its numbers, and
///   so on.  A group whose fold is unbound (the average of no numbers)
///   derives nothing.
/// - **Rankings** rank, within each group of the scope (or all together,
///   without one), the subjects by their best `object`, the key: the least,
///   or greatest if `descending`.  Ranks start at 0; a subject with no key
///   is ranked last, in either direction.
/// - **The rows** folded are those of the rest of the query's (or rule
///   body's) top group but the patterns matching the aggregate's own links
///   (by predicate), or a later aggregate's, and the parts using variables
///   only they bind: its negations and filters are folded.  So an aggregate
///   can fold or rank the links of those before it.  Rows with the subject
///   or the scope unbound fold nothing.
/// - **The links** can be matched by the group's patterns with the
///   predicate, as calls of the aggregate's relation: `?m order (= 0)`.
///   Such a pattern can't bind its link, nor name a subgraph or author, and
///   gives each link once, whatever the scopes giving it.
///   Their values are entries, or [ephemeral values](Graph::ephemeral_value)
///   for numbers not in the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkAggregate {
    pub function: LinkFunction,
    pub subject: Var,
    pub predicate: u64,
    pub object: Var,
    pub distinct: bool,
    pub descending: bool,
    pub scope: Option<Var>,
}

impl LinkAggregate {
    pub fn new(function: LinkFunction, subject: Var, predicate: u64, object: Var) -> Self {
        LinkAggregate {
            function,
            subject,
            predicate,
            object,
            distinct: false,
            descending: false,
            scope: None,
        }
    }

    /// Folds the distinct objects.
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Ranks by the keys descending.
    pub fn desc(mut self) -> Self {
        self.descending = true;
        self
    }

    /// Aggregates separately for each value of `v` (SQL's `PARTITION BY`);
    /// rows where it is unbound fold nothing.
    pub fn scope(mut self, v: Var) -> Self {
        self.scope = Some(v);
        self
    }

    /// The columns of its rule's head, and of its relation's tuples:
    /// subject, object (the value, in a tuple), and scope.
    pub(crate) fn head(&self) -> Vec<Var> {
        let mut head = vec![self.subject, self.object];
        head.extend(self.scope);
        head
    }

    /// The tuples of its relation, `(s, v[, scope])`, from `rows` of its
    /// head, and the rows each folds.
    pub(crate) fn tuples(
        &self,
        q: &Query,
        g: &Graph,
        rows: &[Vec<u64>],
    ) -> Vec<(Vec<u64>, Vec<usize>)> {
        let scoped = self.scope.is_some();
        // The rows of each subject and scope, in order.
        let mut order = Vec::new();
        let mut groups: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            let scope = if scoped { row[2] } else { 0 };
            if row[0] == 0 || (scoped && scope == 0) {
                continue;
            }
            groups
                .entry((row[0], scope))
                .or_insert_with(|| {
                    order.push((row[0], scope));
                    Vec::new()
                })
                .push(i);
        }
        let tuple = |s: u64, v: u64, scope: u64| {
            let mut t = vec![s, v];
            if scoped {
                t.push(scope);
            }
            t
        };
        let mut out = Vec::new();
        if let LinkFunction::Fold(function) = self.function {
            let a = Aggregate {
                function,
                column: Some(0),
                distinct: self.distinct,
            };
            for key in order {
                let members = &groups[&key];
                let cells: Vec<Vec<Cell>> = members
                    .iter()
                    .map(|&i| vec![Cell::of_id(rows[i][1])])
                    .collect();
                if let Some(v) = value_id(q, g, aggregate::fold(g, &a, &cells)) {
                    out.push((tuple(key.0, v, key.1), members.clone()));
                }
            }
            return out;
        }
        // Rankings: each scope's subjects, by their best keys.
        let mut scopes: Vec<u64> = Vec::new();
        let mut subjects: HashMap<u64, Vec<(u64, Option<Cell>)>> = HashMap::new();
        for key in &order {
            let members = &groups[key];
            let best = members
                .iter()
                .map(|&i| rows[i][1])
                .filter(|&k| k != 0)
                .map(Cell::Id)
                .reduce(|a, b| {
                    let better = match compare_cells(g, &b, &a) {
                        Ordering::Less => !self.descending,
                        Ordering::Greater => self.descending,
                        Ordering::Equal => false,
                    };
                    if better { b } else { a }
                });
            subjects
                .entry(key.1)
                .or_insert_with(|| {
                    scopes.push(key.1);
                    Vec::new()
                })
                .push((key.0, best));
        }
        for scope in scopes {
            let mut ranked = subjects.remove(&scope).unwrap();
            // Keys in the order's direction, those with none last; then the
            // subjects' ids, for ROW_NUMBER.
            let by_key = |a: &Option<Cell>, b: &Option<Cell>| match (a, b) {
                (Some(a), Some(b)) => {
                    let o = compare_cells(g, a, b);
                    if self.descending { o.reverse() } else { o }
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            ranked.sort_by(|a, b| by_key(&a.1, &b.1).then(a.0.cmp(&b.0)));
            let (mut rank, mut dense) = (0, 0);
            for (i, (s, key)) in ranked.iter().enumerate() {
                if i > 0 && by_key(&ranked[i - 1].1, key) != Ordering::Equal {
                    rank = i;
                    dense += 1;
                }
                let r = match self.function {
                    LinkFunction::Rank => rank,
                    LinkFunction::DenseRank => dense,
                    _ => i,
                };
                let v = q.ephemeral_value(g, r.to_string().as_bytes());
                out.push((tuple(*s, v, scope), groups[&(*s, scope)].clone()));
            }
        }
        out
    }
}

/// The id of an aggregate's value: an entry, or a number's value, ephemeral
/// if the graph doesn't have it.
fn value_id(q: &Query, g: &Graph, c: Cell) -> Option<u64> {
    match c {
        Cell::Id(id) => Some(id),
        Cell::Unbound => None,
        c => Some(q.ephemeral_value(g, &c.data(g)?)),
    }
}

/// The provenance of rows merged: what a tuple folding them was derived
/// from.
pub(crate) fn merge(rows: impl IntoIterator<Item = RowProvenance>) -> RowProvenance {
    let mut out = RowProvenance::default();
    for r in rows {
        out.links.extend(r.links);
        out.paths.extend(r.paths);
        out.calls.extend(r.calls);
    }
    out.links.sort_unstable();
    out.links.dedup();
    out
}

/// A part of a group: what the rows of an aggregate's base can leave out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Pattern(usize),
    Path(usize),
    Call(usize),
    Nearest(usize),
    Union(usize),
    Not(usize),
    Optional(usize),
    Filter(usize),
}

/// Whether `group`, or one in it, calls `r`.
fn calls_relation(group: &Group, r: Relation) -> bool {
    group.calls.iter().any(|c| c.relation == r) || group.groups().any(|g| calls_relation(g, r))
}

impl Group {
    fn parts(&self) -> Vec<Part> {
        let mut parts = Vec::new();
        parts.extend((0..self.patterns.len()).map(Part::Pattern));
        parts.extend((0..self.paths.len()).map(Part::Path));
        parts.extend((0..self.calls.len()).map(Part::Call));
        parts.extend((0..self.nearest.len()).map(Part::Nearest));
        parts.extend((0..self.unions.len()).map(Part::Union));
        parts.extend((0..self.nots.len()).map(Part::Not));
        parts.extend((0..self.optionals.len()).map(Part::Optional));
        parts.extend((0..self.filters.len()).map(Part::Filter));
        parts
    }

    /// The variables a part uses, and those it binds.
    fn part_vars(&self, p: Part) -> (BTreeSet<Var>, BTreeSet<Var>) {
        match p {
            Part::Pattern(i) => {
                let v: BTreeSet<Var> = self.patterns[i].vars().collect();
                (v.clone(), v)
            }
            Part::Path(i) => {
                let v: BTreeSet<Var> = self.paths[i].vars().collect();
                (v.clone(), v)
            }
            Part::Call(i) => {
                let v: BTreeSet<Var> = self.calls[i].vars().collect();
                (v.clone(), v)
            }
            Part::Nearest(i) => {
                let n = &self.nearest[i];
                let bound: BTreeSet<Var> = [n.value, n.distance].into();
                let mut used = bound.clone();
                used.extend(n.query.var());
                (used, bound)
            }
            Part::Union(i) => {
                let mut used = BTreeSet::new();
                let mut bound = BTreeSet::new();
                for b in &self.unions[i] {
                    used.extend(b.all_vars());
                    bound.extend(b.bound_vars());
                }
                (used, bound)
            }
            Part::Not(i) => (self.nots[i].all_vars(), BTreeSet::new()),
            Part::Optional(i) => (self.optionals[i].all_vars(), self.optionals[i].maybe_vars()),
            Part::Filter(i) => (
                self.filters[i].vars().into_iter().collect(),
                BTreeSet::new(),
            ),
        }
    }

    /// Whether a part calls `r`.
    fn part_calls(&self, p: Part, r: Relation) -> bool {
        match p {
            Part::Call(i) => self.calls[i].relation == r,
            Part::Union(i) => self.unions[i].iter().any(|b| calls_relation(b, r)),
            Part::Not(i) => calls_relation(&self.nots[i], r),
            Part::Optional(i) => calls_relation(&self.optionals[i], r),
            _ => false,
        }
    }

    /// The group without `left_out`.
    fn without(&self, left_out: &BTreeSet<Part>) -> Group {
        let keep = |p: Part| !left_out.contains(&p);
        let mut g = Group {
            graph: self.graph,
            ..Group::default()
        };
        for p in self.parts() {
            if !keep(p) {
                continue;
            }
            match p {
                Part::Pattern(i) => g.patterns.push(self.patterns[i]),
                Part::Path(i) => g.paths.push(self.paths[i].clone()),
                Part::Call(i) => g.calls.push(self.calls[i].clone()),
                Part::Nearest(i) => g.nearest.push(self.nearest[i].clone()),
                Part::Union(i) => g.unions.push(self.unions[i].clone()),
                Part::Not(i) => g.nots.push(self.nots[i].clone()),
                Part::Optional(i) => g.optionals.push(self.optionals[i].clone()),
                Part::Filter(i) => g.filters.push(self.filters[i].clone()),
            }
        }
        g
    }

    /// The base of an aggregate: the group without its calls of
    /// `relations` (those matching the links of the aggregate, and of the
    /// aggregates reading them), and the parts that depend on them, using
    /// variables only they bind.
    fn base(&self, relations: &BTreeSet<Relation>) -> Group {
        let parts = self.parts();
        let mut left_out: BTreeSet<Part> = parts
            .iter()
            .copied()
            .filter(|&p| relations.iter().any(|&r| self.part_calls(p, r)))
            .collect();
        loop {
            let mut kept_bound = BTreeSet::new();
            let mut left_used = BTreeSet::new();
            for &p in &parts {
                let (used, bound) = self.part_vars(p);
                if left_out.contains(&p) {
                    left_used.extend(used);
                } else {
                    kept_bound.extend(bound);
                }
            }
            let derived: BTreeSet<Var> = left_used.difference(&kept_bound).copied().collect();
            let more: Vec<Part> = parts
                .iter()
                .copied()
                .filter(|p| !left_out.contains(p))
                .filter(|&p| !self.part_vars(p).0.is_disjoint(&derived))
                .collect();
            if more.is_empty() {
                return self.without(&left_out);
            }
            left_out.extend(more);
        }
    }

    /// Replaces the patterns, here and in the groups within, whose
    /// predicate is one of `predicates` by calls of its relation of links.
    fn call_aggregates(&mut self, predicates: &HashMap<u64, Relation>) -> Result<()> {
        let mut kept = Vec::new();
        for p in std::mem::take(&mut self.patterns) {
            let Term::Id(predicate) = p.predicate else {
                kept.push(p);
                continue;
            };
            let Some(&relation) = predicates.get(&predicate) else {
                kept.push(p);
                continue;
            };
            if p.link.is_some() || p.graph.is_some() || p.author.is_some() {
                return Err(Error::InvalidArgument(
                    "an aggregate's link has no id, subgraph or author to match",
                ));
            }
            if p.as_of.is_some() {
                return Err(Error::InvalidArgument(
                    "an aggregate's link can't be matched 'as of'",
                ));
            }
            self.calls.push(Call {
                relation,
                args: vec![p.subject, p.object],
                mark: None,
            });
        }
        self.patterns = kept;
        for g in self
            .unions
            .iter_mut()
            .flatten()
            .chain(&mut self.nots)
            .chain(&mut self.optionals)
        {
            g.call_aggregates(predicates)?;
        }
        Ok(())
    }
}

/// Rewrites `body`, the group of `aggregates`, as rules: the relation of
/// each, defined by an aggregate rule over its base, and `body` calling
/// them where it matches their links.  Returns the relations.
fn rewrite(
    q: &mut Query,
    body: &mut Group,
    aggregates: &[LinkAggregate],
) -> Result<Vec<(Relation, LinkAggregate)>> {
    let mut predicates = HashMap::new();
    let mut out = Vec::new();
    // The relation each aggregate's links are matched by.
    let mut bases = Vec::new();
    for a in aggregates {
        if a.predicate == 0 {
            return Err(Error::InvalidArgument(
                "an aggregate's link needs a predicate",
            ));
        }
        let name = format!("{}(#{})", a.function.name(), a.predicate);
        let arity = 2 + usize::from(a.scope.is_some());
        let r = q.relation(&name, arity);
        // The links, matched: the tuples without their scopes, once each.
        let links = match a.scope {
            None => r,
            Some(_) => {
                let links = q.relation(&format!("{name} links"), 2);
                let n = q.names.len();
                let (s, v, scope) = (
                    q.hidden_var(&format!("_s{n}")),
                    q.hidden_var(&format!("_v{n}")),
                    q.hidden_var(&format!("_scope{n}")),
                );
                let mut body = Group::new();
                body.call(r, [s, v, scope]);
                q.rule(links, [s, v], body);
                links
            }
        };
        bases.push(links);
        if predicates.insert(a.predicate, links).is_some() {
            return Err(Error::InvalidArgument(
                "two aggregates derive links with the same predicate",
            ));
        }
        if a.distinct && a.function.ranks() {
            return Err(Error::InvalidArgument("a ranking can't be distinct"));
        }
        out.push((r, a.clone()));
    }
    body.call_aggregates(&predicates)?;
    // Each aggregate's base leaves out the patterns matching its links, and
    // those of the aggregates after it: an aggregate reads the links of
    // those before it, which are evaluated first.
    let groups: Vec<Group> = (0..bases.len())
        .map(|i| body.base(&bases[i..].iter().copied().collect()))
        .collect();
    for ((r, a), base) in out.iter().zip(groups) {
        let bound = base.maybe_vars();
        if !a.head().iter().all(|v| bound.contains(v)) {
            return Err(Error::InvalidArgument(
                "an aggregate's subject, object and scope must be bound by the rest of its group",
            ));
        }
        q.rules.push(Rule {
            relation: *r,
            head: a.head(),
            body: base,
            assertions: Vec::new(),
            link_aggregates: Vec::new(),
            aggregate: Some(a.clone()),
        });
    }
    Ok(out)
}

impl Query {
    /// The query with its link aggregates, and its rules', planned as rules
    /// (see the [module](self) docs), if it has any.
    pub(crate) fn with_link_aggregates(&self) -> Result<Option<Query>> {
        if self.link_aggregates.is_empty()
            && self.rules.iter().all(|r| r.link_aggregates.is_empty())
        {
            return Ok(None);
        }
        let mut q = self.clone();
        let aggregates = std::mem::take(&mut q.link_aggregates);
        let mut root = std::mem::take(&mut q.root);
        let aggregated = rewrite(&mut q, &mut root, &aggregates)?;
        q.root = root;
        q.aggregated = aggregated;
        for i in 0..q.rules.len() {
            let aggregates = std::mem::take(&mut q.rules[i].link_aggregates);
            if aggregates.is_empty() {
                continue;
            }
            let mut body = std::mem::take(&mut q.rules[i].body);
            rewrite(&mut q, &mut body, &aggregates)?;
            q.rules[i].body = body;
        }
        Ok(Some(q))
    }
}
