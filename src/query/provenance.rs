//! What a row of a query recording its links matched: its links, the paths
//! it followed, and the tuples of the relations it called; and the stored
//! links a path or a tuple was derived from (see
//! [`Query::record_links`](super::Query::record_links)).

use std::collections::HashMap;

use super::paths::{Automaton, Step};
use super::{Call, Group, Overlay, PathExpr, PathPattern, Query, Relation, Term};
use crate::graph::Graph;

/// A path a row followed: the pattern, and the ends and predicate it
/// matched.
#[derive(Debug, Clone)]
pub(crate) struct PathMatch {
    pub path: PathPattern,
    pub subject: u64,
    pub predicate: u64,
    pub object: u64,
}

/// What a row matched.
#[derive(Debug, Clone, Default)]
pub(crate) struct RowProvenance {
    pub links: Vec<u64>,
    pub paths: Vec<PathMatch>,
    pub calls: Vec<(Relation, Vec<u64>)>,
}

/// The first derivation of each tuple of the relations of a query recording
/// its links: what the row of a rule's body that found it matched.
pub(crate) type Derivations = HashMap<(Relation, Vec<u64>), RowProvenance>;

/// The rules (by their index in the query) asserting triples that derive
/// each tuple of the relations of a query recording its links.
pub(crate) type Asserting = HashMap<(Relation, Vec<u64>), Vec<usize>>;

/// The paths and calls of `group` that a row may follow (not in negations),
/// with marks.
fn marked<'g>(group: &'g Group, paths: &mut Vec<&'g PathPattern>, calls: &mut Vec<&'g Call>) {
    paths.extend(group.paths.iter().filter(|p| p.mark.is_some()));
    calls.extend(group.calls.iter().filter(|c| c.mark.is_some()));
    for g in group.unions.iter().flatten().chain(&group.optionals) {
        marked(g, paths, calls);
    }
}

/// What reads a row's provenance: the link variables, and the marked paths
/// and calls, of a query recording its links.
pub(crate) struct Recorder<'q> {
    links: Vec<usize>,
    paths: Vec<&'q PathPattern>,
    calls: Vec<&'q Call>,
}

impl<'q> Recorder<'q> {
    pub fn new(q: &'q Query) -> Self {
        let (mut paths, mut calls) = (Vec::new(), Vec::new());
        marked(&q.root, &mut paths, &mut calls);
        Recorder {
            links: q.recorded.iter().map(|v| v.0).collect(),
            paths,
            calls,
        }
    }

    /// Whether a row can follow paths or call relations, beyond its links.
    pub fn derives(&self) -> bool {
        !self.paths.is_empty() || !self.calls.is_empty()
    }

    /// The links `row` matched, added to `out`.
    pub fn links_into(&self, row: &[u64], out: &mut std::collections::BTreeSet<u64>) {
        out.extend(self.links.iter().map(|&k| row[k]).filter(|&id| id != 0));
    }

    /// What `row`, with a slot for each variable, matched.
    pub fn row(&self, row: &[u64]) -> RowProvenance {
        let value = |t: Term| match t {
            Term::Id(id) => id,
            Term::Var(v) => row[v.0],
        };
        let marked = |mark: Option<super::Var>| mark.is_some_and(|v| row[v.0] != 0);
        RowProvenance {
            links: self
                .links
                .iter()
                .map(|&k| row[k])
                .filter(|&id| id != 0)
                .collect(),
            paths: self
                .paths
                .iter()
                .filter(|p| marked(p.mark))
                .map(|p| PathMatch {
                    path: (*p).clone(),
                    subject: value(p.subject),
                    predicate: value(p.predicate),
                    object: value(p.object),
                })
                .collect(),
            calls: self
                .calls
                .iter()
                .filter(|c| marked(c.mark))
                .map(|c| (c.relation, c.args.iter().map(|&t| value(t)).collect()))
                .collect(),
        }
    }
}

/// The links of a shortest path from `m`'s subject to its object that its
/// pattern follows, as the graph is as of `as_of` (or the pattern's own).
pub(crate) fn witness(
    g: &Graph,
    m: &PathMatch,
    as_of: Option<u64>,
    overlay: Option<&Overlay>,
) -> Option<Vec<u64>> {
    let as_of = m.path.as_of.or(as_of);
    let transitive;
    let expr = match &m.path.expr {
        Some(e) => &**e,
        None => {
            transitive = PathExpr::Repeat(Box::new(PathExpr::Link(m.predicate)), 1, None);
            &transitive
        }
    };
    let next = |step: Step, x: u64| -> Vec<(u64, u64)> {
        super::overlay::step_links(g, overlay, step.predicate, x, step.forward, as_of)
            .into_iter()
            .map(|(id, [s, _, o])| (id, if step.forward { o } else { s }))
            .collect()
    };
    Automaton::new(expr, true).witness(m.subject, m.object, next)
}

/// The stored links supporting the tuples of relations, found through
/// their derivations, once each.
pub(crate) struct Support<'d> {
    pub derivations: &'d Derivations,
    pub as_of: Option<u64>,
    pub overlay: Option<std::sync::Arc<Overlay>>,
    known: HashMap<(Relation, Vec<u64>), Vec<u64>>,
}

impl<'d> Support<'d> {
    pub fn new(
        derivations: &'d Derivations,
        as_of: Option<u64>,
        overlay: Option<std::sync::Arc<Overlay>>,
    ) -> Self {
        Support {
            derivations,
            as_of,
            overlay,
            known: HashMap::new(),
        }
    }

    /// The stored links supporting `row`'s provenance: its links, its
    /// paths' witnesses, and its calls' tuples' support.
    pub fn of_row(&mut self, g: &Graph, row: &RowProvenance) -> Vec<u64> {
        let mut out = row.links.clone();
        for m in &row.paths {
            out.extend(
                witness(g, m, self.as_of, self.overlay.as_deref())
                    .into_iter()
                    .flatten(),
            );
        }
        for (r, t) in &row.calls {
            out.extend(self.of_tuple(g, *r, t));
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The stored links supporting the tuple `t` of `r`: its first
    /// derivation's, which used only tuples found before it.
    pub fn of_tuple(&mut self, g: &Graph, r: Relation, t: &[u64]) -> Vec<u64> {
        let key = (r, t.to_vec());
        if let Some(s) = self.known.get(&key) {
            return s.clone();
        }
        let support = match self.derivations.get(&key) {
            Some(row) => self.of_row(g, row),
            None => Vec::new(),
        };
        self.known.insert(key, support.clone());
        support
    }
}
