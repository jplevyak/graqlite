//! The vector indexes' searches: an [`Hnsw`] of each indexed predicate's
//! objects, in memory, shared by the graph and its readers.
//!
//! - **Fed at commits,** from the records committed since: each link with an
//!   indexed predicate makes its object live from its id, if a live link
//!   with the predicate has it then, or ends the range it was live over, if
//!   none has.  So a search sees what a query as of an id would, for any id
//!   since the search was built ([`VectorSearch::floor`]).  Entries not yet
//!   committed are not in it: a predicate the writer has appended links of
//!   since its last commit is searched exactly, by scanning, until the next
//!   commit or rollback.
//! - **Built** from the live links when the graph is opened, or a
//!   declaration committed, and again when items no longer live are a
//!   quarter of it (at [`Graph::tick`]), keeping those live as of the
//!   oldest reader's snapshot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use super::{Graph, Result, VectorIndex};
use crate::hnsw::Hnsw;
use crate::record::Linkage;
use crate::vector;

/// An index's search.
#[derive(Debug, Clone)]
pub struct VectorSearch {
    pub index: VectorIndex,
    pub hnsw: Hnsw,
    /// The least id a search may be as of: it was built then, so it may lack
    /// what was live only before.
    pub floor: u64,
}

impl VectorSearch {
    /// The ids of the `k` values live as of `at` nearest `query`, with their
    /// distances, nearest first (then by id); `None` if `at` is before the
    /// floor.
    pub fn search(&self, query: &[f32], k: usize, at: u64) -> Option<Vec<(u64, f64)>> {
        if at < self.floor {
            return None;
        }
        let mut found = self.hnsw.search(query, k, self.index.ef.max(k), at);
        found.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        Some(found)
    }
}

/// The searches, by predicate, and the id the commits fed them reach.
#[derive(Debug, Default)]
pub(super) struct Searches {
    by_predicate: HashMap<u64, Arc<VectorSearch>>,
    fed: u64,
}

/// Shared by a graph and its readers.
pub(super) type SharedSearches = Arc<RwLock<Searches>>;

impl Graph {
    /// The search of `predicate`'s vector index, if it has one that a
    /// search of the graph may use: its declaration is the graph's, and the
    /// writer hasn't appended links with it since its last commit.
    pub fn vector_search(&self, predicate: u64) -> Option<Arc<VectorSearch>> {
        let index = self.vector_index(predicate)?;
        if self.vector_dirty.contains(&predicate) {
            return None;
        }
        let searches = self.searches.read().unwrap_or_else(|e| e.into_inner());
        let search = searches.by_predicate.get(&predicate)?;
        (search.index == *index).then(|| Arc::clone(search))
    }

    /// The id a search of the graph is as of: a reader's last, or for the
    /// writer, any (its uncommitted links aren't searched).
    pub fn vector_bound(&self) -> u64 {
        self.snapshot
            .map_or(u64::MAX - 1, |end| end.saturating_sub(1))
    }

    /// Notes that `record` was appended: a link with an indexed predicate
    /// makes its searches exact until the next commit.
    pub(super) fn vector_appended(&mut self, record: &crate::record::Record<'_>) {
        if let Some(l) = record.as_link()
            && self.vector_index(l.predicate()).is_some()
        {
            self.vector_dirty.insert(l.predicate());
        }
    }

    /// Feeds the searches the records committed since, building those of
    /// indexes declared since (and dropping those of indexes dropped).
    pub(super) fn feed_vector_searches(&mut self) -> Result<()> {
        self.vector_dirty.clear();
        let committed = self.local.log().metadata().next_id;
        let indexes: HashMap<u64, VectorIndex> = self
            .vector_indexes()
            .iter()
            .map(|i| (i.predicate, i.clone()))
            .collect();
        let shared = Arc::clone(&self.searches);
        let mut searches = shared.write().unwrap_or_else(|e| e.into_inner());
        let fed = searches.fed;
        searches
            .by_predicate
            .retain(|p, s| indexes.get(p) == Some(&s.index));
        // The links of the predicates searched already, since fed.
        let fed_predicates: HashSet<u64> = searches.by_predicate.keys().copied().collect();
        if !fed_predicates.is_empty() && fed < committed {
            let mut changes: Vec<(u64, u64, u64)> = Vec::new();
            for record in self.records(fed) {
                let record = record?;
                if record.id() >= committed {
                    break;
                }
                if let Some(l) = record.as_link()
                    && fed_predicates.contains(&l.predicate())
                {
                    changes.push((record.id(), l.predicate(), l.object()));
                }
            }
            for (id, p, o) in changes {
                let live = !self
                    .query_property([o], Linkage::Object, p, Some(id))?
                    .0
                    .is_empty();
                let search = Arc::make_mut(searches.by_predicate.get_mut(&p).expect("searched"));
                if live {
                    if !search.hnsw.contains(o) {
                        match self.data(o).and_then(|d| vector::decode(&d)) {
                            Some(v) => search.hnsw.insert(o, &v, id),
                            None => false,
                        };
                    } else {
                        search.hnsw.insert(o, &[], id);
                    }
                } else {
                    search.hnsw.remove(o, id);
                }
            }
        }
        for (p, index) in indexes {
            if let std::collections::hash_map::Entry::Vacant(e) = searches.by_predicate.entry(p) {
                e.insert(Arc::new(
                    self.build_vector_search(&index, committed.saturating_sub(1)),
                ));
            }
        }
        searches.fed = committed;
        Ok(())
    }

    /// A search of `index`'s predicate's live links' objects, as of `at`.
    fn build_vector_search(&self, index: &VectorIndex, at: u64) -> VectorSearch {
        let mut hnsw = Hnsw::new(index.dims, index.metric, index.m, index.ef);
        let links = self.live(&self.links_at(Linkage::Predicate, [index.predicate]), None);
        let mut objects: Vec<u64> = links
            .iter()
            .filter_map(|&l| self.with_link(l, |l| l.object()))
            .collect();
        objects.sort_unstable();
        objects.dedup();
        for o in objects {
            if let Some(v) = self.data(o).and_then(|d| vector::decode(&d)) {
                hnsw.insert(o, &v, at);
            }
        }
        VectorSearch {
            index: index.clone(),
            hnsw,
            floor: at,
        }
    }

    /// Builds again each search whose items no longer live are a quarter of
    /// it, keeping those live as of the oldest reader's snapshot.
    pub(super) fn rebuild_vector_searches(&mut self) {
        let committed = self.local.log().metadata().next_id;
        let oldest = self
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .next()
            .copied()
            .unwrap_or(committed);
        let at = oldest.min(committed).saturating_sub(1);
        let shared = Arc::clone(&self.searches);
        let mut searches = shared.write().unwrap_or_else(|e| e.into_inner());
        for search in searches.by_predicate.values_mut() {
            let (len, live) = (search.hnsw.len(), search.hnsw.live_len());
            if len >= 64 && (len - live) * 4 >= len {
                let at = at.max(search.floor);
                *search = Arc::new(VectorSearch {
                    index: search.index.clone(),
                    hnsw: search.hnsw.rebuilt(at),
                    floor: at,
                });
            }
        }
    }
}
