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
//! - **Built** from the live links when a declaration is committed, or the
//!   graph is opened without them saved, and again when items no longer
//!   live are a quarter of it (at [`Graph::tick`]), keeping those live as
//!   of the oldest reader's snapshot.
//! - **Saved** to `{prefix}vectors` (see [`save`]) when the graph is
//!   dropped, and at a tick once the changes since are an eighth of the
//!   items; on opening, loaded and fed the records committed since, or
//!   built if missing or not valid.  The file is a cache of the log: losing
//!   it costs only the time to build the searches again.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use super::{Graph, Result, VectorIndex};
use crate::hnsw::Hnsw;
use crate::mmap::{suffixed, sync_dir};
use crate::record::{Linkage, Uuid};
use crate::vector::{self, Metric};

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
    /// The items added or changed since saved.
    changes: usize,
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
        let before = searches.by_predicate.len();
        searches
            .by_predicate
            .retain(|p, s| indexes.get(p) == Some(&s.index));
        searches.changes += before - searches.by_predicate.len();
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
                searches.changes += 1;
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
        let mut built = 0;
        for (p, index) in indexes {
            if let std::collections::hash_map::Entry::Vacant(e) = searches.by_predicate.entry(p) {
                let search = self.build_vector_search(&index, committed.saturating_sub(1));
                built += search.hnsw.len().max(1);
                e.insert(Arc::new(search));
            }
        }
        searches.changes += built;
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
        let mut changes = 0;
        for search in searches.by_predicate.values_mut() {
            let (len, live) = (search.hnsw.len(), search.hnsw.live_len());
            if len >= 64 && (len - live) * 4 >= len {
                let at = at.max(search.floor);
                changes += len.max(1);
                *search = Arc::new(VectorSearch {
                    index: search.index.clone(),
                    hnsw: search.hnsw.rebuilt(at),
                    floor: at,
                });
            }
        }
        searches.changes += changes;
    }

    /// Loads the searches saved for the graph, if they are valid and of its
    /// log, to be fed the records committed since.
    pub(super) fn load_vector_searches(&mut self) {
        let (Some(prefix), meta) = (self.local.prefix(), self.local.log().metadata()) else {
            return;
        };
        let start = self.local.log().start_id();
        if let Some(loaded) = load(prefix, meta.uuid)
            && (start..=meta.next_id).contains(&loaded.fed)
        {
            *self.searches.write().unwrap_or_else(|e| e.into_inner()) = loaded;
        }
    }

    /// Saves the searches, if they have changed since saved: at all, or if
    /// not `all`, by an eighth of their items.
    pub(super) fn save_vector_searches(&self, all: bool) -> io::Result<()> {
        let Some(prefix) = self.local.prefix() else {
            return Ok(());
        };
        if self.is_reader() {
            return Ok(());
        }
        let mut searches = self.searches.write().unwrap_or_else(|e| e.into_inner());
        let items: usize = searches.by_predicate.values().map(|s| s.hnsw.len()).sum();
        if searches.changes == 0 || (!all && searches.changes * 8 < items.max(1024)) {
            return Ok(());
        }
        if searches.by_predicate.is_empty() {
            remove(prefix)?;
        } else {
            save(prefix, self.local.log().metadata().uuid, &searches)?;
        }
        searches.changes = 0;
        Ok(())
    }
}

const FILE_MAGIC: u64 = u64::from_le_bytes(*b"grqlvect");
const FILE_VERSION: u64 = 1;

/// The searches' file of the graph at `prefix`.
pub(crate) fn path(prefix: &Path) -> PathBuf {
    suffixed(prefix, "vectors")
}

fn metric_code(m: Metric) -> u64 {
    match m {
        Metric::Cosine => 0,
        Metric::L2 => 1,
        Metric::Dot => 2,
    }
}

/// Saves `searches`, fed the entries before `searches.fed` of the log with
/// `uuid`, to the file at `prefix`, replacing it atomically: a header, then
/// each search's declaration, floor and graph ([`Hnsw::write`]).
fn save(prefix: &Path, uuid: Uuid, searches: &Searches) -> io::Result<()> {
    let tmp = suffixed(prefix, "vectors.tmp");
    let mut out = BufWriter::new(File::create(&tmp)?);
    let mut all: Vec<&Arc<VectorSearch>> = searches.by_predicate.values().collect();
    all.sort_by_key(|s| s.index.predicate);
    for word in [
        FILE_MAGIC,
        FILE_VERSION,
        uuid.uuid0,
        uuid.uuid1,
        searches.fed,
        all.len() as u64,
    ] {
        out.write_all(&word.to_le_bytes())?;
    }
    for s in all {
        let i = &s.index;
        for word in [
            i.predicate,
            i.dims as u64,
            metric_code(i.metric),
            i.m as u64,
            i.ef as u64,
            s.floor,
        ] {
            out.write_all(&word.to_le_bytes())?;
        }
        s.hnsw.write(&mut out)?;
    }
    out.into_inner()?.sync_all()?;
    fs::rename(&tmp, path(prefix))?;
    sync_dir(prefix)
}

/// The searches saved at `prefix` for the log with `uuid`, or `None` if
/// there are none, or they are not valid.
fn load(prefix: &Path, uuid: Uuid) -> Option<Searches> {
    let mut input = BufReader::new(File::open(path(prefix)).ok()?);
    let result = (|| -> io::Result<Option<Searches>> {
        let input = &mut input;
        let mut word = || -> io::Result<u64> {
            let mut b = [0u8; 8];
            input.read_exact(&mut b)?;
            Ok(u64::from_le_bytes(b))
        };
        let header: Vec<u64> = (0..6).map(|_| word()).collect::<io::Result<_>>()?;
        let [magic, version, uuid0, uuid1, fed, count] = header[..] else {
            unreachable!()
        };
        if (magic, version) != (FILE_MAGIC, FILE_VERSION)
            || (uuid0, uuid1) != (uuid.uuid0, uuid.uuid1)
        {
            return Ok(None);
        }
        let mut out = Searches {
            fed,
            ..Searches::default()
        };
        for _ in 0..count {
            let mut word = || -> io::Result<u64> {
                let mut b = [0u8; 8];
                input.read_exact(&mut b)?;
                Ok(u64::from_le_bytes(b))
            };
            let (predicate, dims, metric, m, ef, floor) =
                (word()?, word()?, word()?, word()?, word()?, word()?);
            let metric = match metric {
                0 => Metric::Cosine,
                1 => Metric::L2,
                2 => Metric::Dot,
                _ => return Ok(None),
            };
            let index = VectorIndex {
                predicate,
                dims: dims as usize,
                metric,
                m: m as usize,
                ef: ef as usize,
            };
            let hnsw = Hnsw::read(input)?;
            if hnsw.dims() != index.dims || hnsw.metric() != metric || floor > fed {
                return Ok(None);
            }
            let search = VectorSearch { index, hnsw, floor };
            if out
                .by_predicate
                .insert(predicate, Arc::new(search))
                .is_some()
            {
                return Ok(None);
            }
        }
        if input.read(&mut [0])? != 0 {
            return Ok(None);
        }
        Ok(Some(out))
    })();
    result.ok().flatten()
}

/// Deletes the searches' file at `prefix`, if any.
pub(crate) fn remove(prefix: &Path) -> io::Result<()> {
    for p in [path(prefix), suffixed(prefix, "vectors.tmp")] {
        match fs::remove_file(p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}
