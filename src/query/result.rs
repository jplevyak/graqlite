//! Graphs as results: the links a query's rows match, those derived from
//! them by paths and rules, and the entries the rows bind (see
//! [`Query::graph`](super::Query::graph)).

use std::sync::Arc;

use super::PathExpr;
use super::aggregate::Cell;

/// A link of a [`ResultGraph`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ResultLink {
    /// A link in the graph: its id.
    Stored(u64),
    /// A link derived from stored ones: by a path, or a rule.
    Derived {
        /// Its subject, predicate and object.
        fields: [ResultNode; 3],
        by: Derivation,
        /// The stored links it was derived from, in order for a path: one
        /// derivation, not every one.
        support: Vec<u64>,
    },
}

impl ResultLink {
    /// The link's id, if it is stored.
    pub fn id(&self) -> Option<u64> {
        match self {
            ResultLink::Stored(id) => Some(*id),
            ResultLink::Derived { .. } => None,
        }
    }
}

/// A field of a derived link.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ResultNode {
    /// An entry in the graph.
    Entry(u64),
    /// A value, which needn't be in the graph: a rule's name, as the
    /// predicate of the links it derives.
    Value(Cell),
    /// A path expression, as the predicate of the links it derives.
    Path(Arc<PathExpr>),
    /// The node with this key, which needn't be in the graph: an asserted
    /// triple's.
    Key(String),
    /// A new node, from a blank node of an asserted triple: the same number
    /// is the same node, within the result.
    Blank(u32),
}

/// How a link was derived.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Derivation {
    /// By a path pattern: `s p++ o` (its predicate an entry), or a path
    /// expression (its predicate the expression).
    Path,
    /// By the rules of a relation, named: its tuples, if it has two
    /// columns, are links from the first to the second; and the triples its
    /// rules assert (see [`Query::rule_asserts`](super::Query::rule_asserts)).
    Rule(String),
    /// Asserted by the query (see [`Assertion`](super::Assertion)).
    Asserted,
    /// By an aggregate (see [`LinkAggregate`](super::LinkAggregate)),
    /// named: `COUNT`, `RANK`, ...
    Aggregate(String),
    /// A link of the overlay read (see [`Overlay`](super::Overlay)): not
    /// stored, and not derived from stored links.
    Overlay,
}

/// A graph returned by a query: the links its rows match, stored first in
/// id order, then those derived, in the order found; and the entries its
/// selected variables bind that aren't the subject, predicate or object of
/// one of them, in id order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultGraph {
    pub links: Vec<ResultLink>,
    pub nodes: Vec<u64>,
    /// The holds of the ephemeral ids it names, which it keeps.
    pub holds: crate::graph::Holds,
}

impl ResultGraph {
    /// The ids of the stored links.
    pub fn stored(&self) -> impl Iterator<Item = u64> + '_ {
        self.links.iter().filter_map(ResultLink::id)
    }

    /// The derived links.
    pub fn derived(&self) -> impl Iterator<Item = &ResultLink> + '_ {
        self.links.iter().filter(|l| l.id().is_none())
    }
}
