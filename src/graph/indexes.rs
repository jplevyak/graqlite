//! Vector indexes declared on predicates, saved in the graph.
//!
//! A declaration is a link from the node with key [`INDEXES_KEY`] by
//! [`VECTOR_INDEX_PREDICATE`] to the predicate indexed, and links from that
//! link (as a node) to its parameters: [`DIMS_PREDICATE`],
//! [`METRIC_PREDICATE`], [`M_PREDICATE`] and [`EF_PREDICATE`], each to a
//! value of its text.  So a declaration commits and rolls back with the
//! entries around it, and travels in snapshots and dumps.  While one is
//! live, a live link with the predicate must have a vector of its
//! dimension as its object.

use std::sync::Arc;

use super::{Error, Graph, Result};
use crate::record::Linkage;
use crate::vector::{self, Metric};

/// The key of the node that index declarations are linked from.
pub const INDEXES_KEY: &[u8] = b"/graqlite/indexes";
/// The predicate linking [`INDEXES_KEY`] to each predicate with a vector
/// index.
pub const VECTOR_INDEX_PREDICATE: &[u8] = b"graqlite:vector_index";
/// The predicates linking a declaration's link to its parameters.
pub const DIMS_PREDICATE: &[u8] = b"graqlite:dims";
pub const METRIC_PREDICATE: &[u8] = b"graqlite:metric";
pub const M_PREDICATE: &[u8] = b"graqlite:m";
pub const EF_PREDICATE: &[u8] = b"graqlite:ef";

/// A vector index of a predicate's objects (see [`Graph::create_vector_index`]):
/// their dimension, the metric searches use by default, and the HNSW's
/// parameters, the links per node (`m`) and the candidates kept while
/// building it (`ef`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorIndex {
    pub predicate: u64,
    pub dims: usize,
    pub metric: Metric,
    pub m: usize,
    pub ef: usize,
}

impl VectorIndex {
    /// The parameters given none: `m` 16 and `ef` 200.
    pub const M: usize = 16;
    pub const EF: usize = 200;

    pub fn new(predicate: u64, dims: usize, metric: Metric) -> Self {
        VectorIndex {
            predicate,
            dims,
            metric,
            m: Self::M,
            ef: Self::EF,
        }
    }

    fn check(&self) -> Result<()> {
        if self.dims == 0 || self.dims > vector::MAX_DIMS {
            return Err(Error::InvalidArgument(
                "a vector index's dimension must be from 1 to 65536",
            ));
        }
        if self.m < 2 || self.ef == 0 {
            return Err(Error::InvalidArgument(
                "a vector index's m must be at least 2, and its ef at least 1",
            ));
        }
        Ok(())
    }
}

impl Graph {
    /// The vector indexes declared, by predicate.
    pub fn vector_indexes(&self) -> &[VectorIndex] {
        &self.vector_indexes
    }

    /// The vector index of `predicate`, if it has one.
    pub fn vector_index(&self, predicate: u64) -> Option<&VectorIndex> {
        self.vector_indexes
            .iter()
            .find(|i| i.predicate == predicate)
    }

    /// Declares `index`, replacing the predicate's, if it has another.
    /// Returns whether it changed anything.  Fails if a live link with the
    /// predicate hasn't a vector of the dimension.
    pub fn create_vector_index(&mut self, index: &VectorIndex) -> Result<bool> {
        index.check()?;
        if self.vector_index(index.predicate) == Some(index) {
            return Ok(false);
        }
        for &link in &self.links_by_predicate(index.predicate) {
            let object = self.with_link(link, |l| l.object()).unwrap_or(0);
            self.check_vector(object, index.dims)?;
        }
        self.drop_vector_index(index.predicate)?;
        let node = self.node_by_key(INDEXES_KEY)?;
        let p = self.value(VECTOR_INDEX_PREDICATE)?;
        let link = self.link(node, p, index.predicate)?;
        for (name, value) in [
            (DIMS_PREDICATE, index.dims.to_string()),
            (METRIC_PREDICATE, index.metric.name().to_string()),
            (M_PREDICATE, index.m.to_string()),
            (EF_PREDICATE, index.ef.to_string()),
        ] {
            let p = self.value(name)?;
            let v = self.value(value.as_bytes())?;
            self.link(link, p, v)?;
        }
        self.load_vector_indexes()?;
        Ok(true)
    }

    /// Drops the vector index of `predicate`, returning whether it had one.
    pub fn drop_vector_index(&mut self, predicate: u64) -> Result<bool> {
        let (Some(node), Some(p)) = (
            self.find_by_key(INDEXES_KEY),
            self.find_value(VECTOR_INDEX_PREDICATE),
        ) else {
            return Ok(false);
        };
        let Some(link) = self.find_link(node, p, predicate) else {
            return Ok(false);
        };
        for name in [DIMS_PREDICATE, METRIC_PREDICATE, M_PREDICATE, EF_PREDICATE] {
            if let Some(q) = self.find_value(name) {
                let (links, _) = self.query_property([link], Linkage::Subject, q, None)?;
                for &l in &links {
                    let o = self.with_link(l, |l| l.object()).unwrap_or(0);
                    self.delete_link(link, q, o)?;
                }
            }
        }
        self.delete_link(node, p, predicate)?;
        self.load_vector_indexes()?;
        Ok(true)
    }

    /// Reads the declarations again, as the graph has them now.
    pub(super) fn load_vector_indexes(&mut self) -> Result<()> {
        self.vector_indexes = self.saved_vector_indexes()?.into();
        Ok(())
    }

    /// The declarations the graph has; one whose parameters don't read is
    /// left out.
    fn saved_vector_indexes(&self) -> Result<Vec<VectorIndex>> {
        let (Some(node), Some(p)) = (
            self.find_by_key(INDEXES_KEY),
            self.find_value(VECTOR_INDEX_PREDICATE),
        ) else {
            return Ok(Vec::new());
        };
        let (links, _) = self.query_property([node], Linkage::Subject, p, None)?;
        let mut out = Vec::new();
        for &link in &links {
            let Some(predicate) = self.with_link(link, |l| l.object()) else {
                continue;
            };
            let param = |name: &[u8]| -> Option<String> {
                let id = self.property(link, self.find_value(name)?)?;
                Some(String::from_utf8_lossy(&self.data(id)?).into_owned())
            };
            let number = |name: &[u8]| param(name)?.parse::<usize>().ok();
            let (Some(dims), Some(metric), Some(m), Some(ef)) = (
                number(DIMS_PREDICATE),
                param(METRIC_PREDICATE).and_then(|m| Metric::of_name(&m)),
                number(M_PREDICATE),
                number(EF_PREDICATE),
            ) else {
                continue;
            };
            out.push(VectorIndex {
                predicate,
                dims,
                metric,
                m,
                ef,
            });
        }
        out.sort_by_key(|i| i.predicate);
        Ok(out)
    }

    /// The live links with `predicate`.
    fn links_by_predicate(&self, predicate: u64) -> crate::local_table::IdSet {
        self.live(&self.links_at(Linkage::Predicate, [predicate]), None)
    }

    /// Fails unless `object` is a vector of `dims` elements.
    pub(super) fn check_vector(&self, object: u64, dims: usize) -> Result<()> {
        let ok = self
            .data(object)
            .is_some_and(|d| vector::is_vector(&d) && (d.len() - 1) / 4 == dims);
        if ok {
            Ok(())
        } else {
            Err(Error::NotAVector {
                value: object,
                dims,
            })
        }
    }
}

/// The declarations, shared by the graph's clones of them.
pub(super) type VectorIndexes = Arc<[VectorIndex]>;
