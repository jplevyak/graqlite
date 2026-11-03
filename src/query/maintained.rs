//! Subgraphs kept equal to a query's results as the graph changes: the
//! facts of a template, for each row of a [`StandingQuery`] (see
//! [Subgraphs](crate::graph#subgraphs)).

use std::collections::{BTreeMap, HashMap};

use super::{Query, StandingQuery, Term, Var};
use crate::graph::{Error, Graph, Result};

/// A field of a template's triple: a constant, or a column of the rows.
#[derive(Debug, Clone, Copy)]
enum Field {
    Id(u64),
    Column(usize),
}

/// The facts a maintained subgraph gained and lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubgraphChanges {
    pub added: Vec<[u64; 3]>,
    pub removed: Vec<[u64; 3]>,
}

impl SubgraphChanges {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// A subgraph kept equal to the facts of a template for each row of a
/// query: what `insert T into g where P` writes once, written again as the
/// rows of `P` change.
///
/// [`update`](Self::update) brings the query's rows up to date with the
/// entries committed since (see [`StandingQuery`]), and adds to the
/// subgraph each fact some row now gives and none did, and removes each
/// that no row gives any more.  Each fact is counted by the rows giving it,
/// and the changes of an update are netted, fact by fact, before any is
/// written.  So a change that leaves a fact's rows giving it, such as the
/// subgraph's own tag changing the link of a fact the query matches, writes
/// nothing, and maintaining a subgraph doesn't feed on itself.
///
/// The subgraph is the maintainer's: [`new`](Self::new) removes the facts
/// in it that the query doesn't give.  A fact given that is deleted (which
/// takes it out of every subgraph) stays out until its rows change.  The
/// maintainer lives in memory: it isn't saved with the graph.
#[derive(Debug, Clone)]
pub struct MaintainedSubgraph {
    standing: StandingQuery,
    template: Vec<[Field; 3]>,
    subgraph: u64,
    /// The rows giving each fact.
    counts: BTreeMap<[u64; 3], u64>,
}

impl MaintainedSubgraph {
    /// Maintains `subgraph` as the facts of `template` for each row of
    /// `query`, on the committed entries of `g`, and writes it so now: the
    /// changes are those to the subgraph, not yet committed.  The template's
    /// variables must be ones `query` selects.  Fails as
    /// [`StandingQuery::new`] does.
    pub fn new(
        query: &Query,
        template: impl IntoIterator<Item = [Term; 3]>,
        subgraph: u64,
        g: &mut Graph,
    ) -> Result<(Self, SubgraphChanges)> {
        if subgraph == 0 {
            return Err(Error::InvalidArgument("a subgraph is an entry, not 0"));
        }
        let mut q = query.clone();
        q.distinct = false;
        let selected: Vec<Var> = q.selected();
        let template: Vec<[Field; 3]> = template
            .into_iter()
            .map(|triple| {
                let mut fields = [Field::Id(0); 3];
                for (f, t) in fields.iter_mut().zip(triple) {
                    *f = match t {
                        Term::Id(id) => Field::Id(id),
                        Term::Var(v) => {
                            Field::Column(selected.iter().position(|&s| s == v).ok_or(
                                Error::InvalidArgument(
                                    "a template's variable is not one the query selects",
                                ),
                            )?)
                        }
                    };
                }
                Ok(fields)
            })
            .collect::<Result<_>>()?;
        let standing = StandingQuery::new(&q, g)?;
        let mut this = MaintainedSubgraph {
            standing,
            template,
            subgraph,
            counts: BTreeMap::new(),
        };
        for row in this.standing.rows() {
            for fact in this.facts(&row) {
                *this.counts.entry(fact).or_default() += 1;
            }
        }
        // The subgraph as it is, and as it should be.
        let mut changes = SubgraphChanges::default();
        for id in g.subgraph_links(subgraph, None) {
            let Some(f) = g.get(id).ok().and_then(|r| r.as_link().map(|l| l.fields())) else {
                continue;
            };
            let fact = [f.subject, f.predicate, f.object];
            if !this.counts.contains_key(&fact) {
                g.remove_from_subgraph(fact[0], fact[1], fact[2], subgraph)?;
                changes.removed.push(fact);
            }
        }
        for &fact in this.counts.keys() {
            if !g
                .subgraphs_of(fact[0], fact[1], fact[2], None)
                .contains(&subgraph)
            {
                g.add_to_subgraph(fact[0], fact[1], fact[2], subgraph)?;
                changes.added.push(fact);
            }
        }
        Ok((this, changes))
    }

    pub fn subgraph(&self) -> u64 {
        self.subgraph
    }

    /// The facts the rows give, in order.
    pub fn facts_given(&self) -> impl Iterator<Item = [u64; 3]> + '_ {
        self.counts.keys().copied()
    }

    /// The facts of the template for `row`: those with no field unbound.
    fn facts(&self, row: &[u64]) -> Vec<[u64; 3]> {
        self.template
            .iter()
            .map(|fields| {
                fields.map(|f| match f {
                    Field::Id(id) => id,
                    Field::Column(k) => row[k],
                })
            })
            .filter(|fact| !fact.contains(&0))
            .collect()
    }

    /// Brings the subgraph up to date with the entries committed to `g`
    /// since the last update, writing (not committing) and returning its
    /// changes.
    pub fn update(&mut self, g: &mut Graph) -> Result<SubgraphChanges> {
        let changes = self.standing.update(g)?;
        // Each fact's change, netted.
        let mut net: HashMap<[u64; 3], i64> = HashMap::new();
        for (rows, sign) in [(&changes.added, 1i64), (&changes.removed, -1)] {
            for (row, n) in rows {
                for fact in self.facts(row) {
                    *net.entry(fact).or_default() += sign * *n as i64;
                }
            }
        }
        let mut net: Vec<([u64; 3], i64)> = net.into_iter().filter(|&(_, d)| d != 0).collect();
        net.sort_unstable();
        let mut out = SubgraphChanges::default();
        for (fact, delta) in net {
            let before = self.counts.get(&fact).copied().unwrap_or(0);
            let after = before
                .checked_add_signed(delta)
                .expect("a fact's rows are never fewer than none");
            let [s, p, o] = fact;
            if after == 0 {
                self.counts.remove(&fact);
                if g.remove_from_subgraph(s, p, o, self.subgraph)?.is_some() {
                    out.removed.push(fact);
                }
            } else {
                self.counts.insert(fact, after);
                if before == 0 {
                    g.add_to_subgraph(s, p, o, self.subgraph)?;
                    out.added.push(fact);
                }
            }
        }
        Ok(out)
    }
}
