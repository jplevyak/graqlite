//! A graph database: nodes, interned values and links, stored in a [`Local`]
//! database and indexed by [`tuple_index`].
//!
//! Supports only single-valued properties ([`set_link`](Graph::set_link)), unique keys
//! ([`node_by_key`](Graph::node_by_key)), and importing entries numbered
//! elsewhere ([`import`](Graph::import)).
//!
//! # Entries
//!
//! A *node* is an entity with no content.  A *value* is a blob of data;
//! values are interned, so each distinct data is one value.  A *link* is a
//! tuple of subject, predicate, object, author, creation time (`when`) and
//! any number of subgraphs (see [`LinkFields`]), each field the id of an
//! earlier entry (or 0) except `when`.  The subject, predicate and object
//! must be non-zero; any of them may be a link, to make statements about
//! links.
//!
//! # Link state
//!
//! A link's subject, predicate and object (its SPO) identify it, and the
//! latest link with an SPO says whether it holds: live, or dead.
//! [`delete_link`](Graph::delete_link) appends a dead link with the SPO, and
//! linking it again appends a live one.  Appending a link whose SPO's latest
//! link already has the same state returns that link instead.
//!
//! Queries that find links by index return them all, live and dead;
//! [`live`](Graph::live) keeps those that are the latest, live link of their
//! SPO.  It needs every link of each SPO in the set, which every set found by
//! subject, predicate or object (or a pair of them) has.
//!
//! # Subgraphs
//!
//! A subgraph is a set of tags over the live facts: a fact is in subgraph
//! `g` if the latest link of its SPO is live and lists `g` among its
//! subgraphs, so every subgraph is part of the main graph, and membership
//! has history as liveness does.  [`add_to_subgraph`](Graph::add_to_subgraph)
//! appends a live link with the latest's subgraphs and `g` (linking the
//! fact, if it isn't live), [`remove_from_subgraph`](Graph::remove_from_subgraph)
//! one without `g`, and [`delete_link`](Graph::delete_link) a dead link,
//! which takes the fact out of every subgraph.  Linking a live fact again
//! keeps its subgraphs.  Each change appends a link, so the fact's link id
//! changes with its subgraphs.  [`subgraph_links`](Graph::subgraph_links)
//! and [`subgraphs_of`](Graph::subgraphs_of) read them, as of an id.
//!
//! # Keys
//!
//! An entity may have a unique key: a value linked from it by the predicate
//! whose data is [`KEY_PREDICATE`].  Each key names at most one entity, and
//! each entity has at most one key.
//!
//! # Time
//!
//! Ids increase as entries are appended, so an id is also a time: queries
//! taking `as_of` see the graph as it was when that id was appended.  Each
//! commit also records its wall-clock time (see [`times`]),
//! so [`id_at_time`](Graph::id_at_time) gives the last id committed at a
//! time, and [`time_of`](Graph::time_of) the time an id was committed.
//!
//! The graph keeps all of its history unless [compacted](Graph::compact),
//! by hand or by [`tick`](Graph::tick) given a [`history`](Options::history)
//! to keep: then queries as of before the horizon fail.
//!
//! # Commits
//!
//! Appends are visible at once and become durable on
//! [`commit`](Graph::commit); [`batch`](Graph::batch) appends all or nothing.
//! After a crash or [`rollback`](Graph::rollback) the indexes may still name
//! entries that were lost, which the log has replaced with filler nodes, so
//! the graph checks what the ids it finds name.
//!
//! # Statistics
//!
//! The graph keeps [`Stats`] of its links for estimating query costs, saved
//! to `{prefix}stats` by [`tick`](Graph::tick) and when the graph is dropped.
//! Opening a graph counts the entries committed since they were last saved,
//! or all of them if they were never saved.
//!
//! # Threads
//!
//! A `Graph` is `Send` and `Sync`.  Queries read it through `&Graph`, so
//! many threads can run them at once, and writes take `&mut Graph`: share
//! one as an `RwLock<Graph>`, whose readers run in parallel and whose writer
//! waits for them; or give other threads readers, which run beside the
//! writer.
//!
//! # Readers
//!
//! [`reader`](Graph::reader) makes a *reader*: a read-only `Graph` that
//! sees the graph as of its last commit, for any thread, while the graph
//! goes on being written and committed.  Run any query on it.  It takes no
//! locks: it shares the log's mappings, reading only below the committed
//! end (the log never rewrites an entry, nor reuses an id), and reads the
//! indexes through their lock-free readers, dropping the ids past its end;
//! it has the statistics and commit times of its commit.  Its writes fail.
//!
//! [`readers`](Graph::readers) gives a handle, [`Readers`], to pass to
//! other threads: [`latest`](Readers::latest) is a reader as of the latest
//! commit, which the writer publishes at each one from then on.
//!
//! A reader can't see an index level the writer has retired: a lookup
//! re-reads the current level, and one still on a retired level holds it
//! until it returns, when it is unmapped (see
//! [`dynamic_table`](crate::dynamic_table)).  Compaction builds large
//! index entries new trees, and the old trees' pages are reused once every
//! reader made before has been dropped (see
//! [`postings`](crate::postings#compaction)).

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use crate::local::{self, Local};
pub use crate::local::{Options, Synchronous};
use crate::local_record_log::{self, Records};
use crate::local_table::IdSet;
use crate::record::{Link, LinkFields, LinkState, Linkage, Record, now};
use crate::record_log::Entry;
use crate::stats::{self, Stats};
use crate::times::{self, Times};
use crate::tuple_index::{self, BuiltinIndex, Intersection, NUM_BUILTIN_INDEXES};

/// The data of the value that is the predicate of key links.
pub const KEY_PREDICATE: &[u8] = b"graqlite:key";

/// Errors from a graph.
#[derive(Debug)]
pub enum Error {
    Local(local::Error),
    Index(tuple_index::Error),
    /// Encoding or reading an entry failed, or a link refers forward.
    Record(crate::Error),
    InvalidArgument(&'static str),
    /// The key is already the key of the entity with this id.
    KeyInUse(u64),
    /// An imported link refers to this id, which is neither in the graph nor
    /// an earlier imported entry that was written.
    Unresolved(u64),
    /// A query reads the graph as of an id before the horizon its indexes
    /// are compacted to (see [`Graph::compact`]).
    Compacted {
        as_of: u64,
        horizon: u64,
    },
    /// A link's object, with a predicate that has a [vector
    /// index](Graph::create_vector_index), isn't a vector of its dimension.
    NotAVector {
        value: u64,
        dims: usize,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Local(e) => write!(f, "{e}"),
            Error::Index(e) => write!(f, "{e}"),
            Error::Record(e) => write!(f, "record: {e}"),
            Error::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Error::KeyInUse(id) => write!(f, "key is in use by {id}"),
            Error::Unresolved(id) => write!(f, "unresolved reference to {id}"),
            Error::Compacted { as_of, horizon } => write!(
                f,
                "as of {as_of} is before {horizon}, the horizon the indexes are compacted to"
            ),
            Error::NotAVector { value, dims } => write!(
                f,
                "#{value} isn't a vector of {dims} dimensions, as its predicate's index needs"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Local(e) => Some(e),
            Error::Index(e) => Some(e),
            Error::Record(e) => Some(e),
            Error::InvalidArgument(_)
            | Error::KeyInUse(_)
            | Error::Unresolved(_)
            | Error::Compacted { .. }
            | Error::NotAVector { .. } => None,
        }
    }
}

impl From<local::Error> for Error {
    fn from(e: local::Error) -> Self {
        Error::Local(e)
    }
}

impl From<local_record_log::Error> for Error {
    fn from(e: local_record_log::Error) -> Self {
        Error::Local(local::Error::Log(e))
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Local(local::Error::Io(e))
    }
}

impl From<tuple_index::Error> for Error {
    fn from(e: tuple_index::Error) -> Self {
        Error::Index(e)
    }
}

impl From<crate::Error> for Error {
    fn from(e: crate::Error) -> Self {
        Error::Record(e)
    }
}

/// Whether an entry starts at an id; see [`Graph::check_entry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryCheck {
    /// The indexes confirm an entry starts there.
    Entry,
    /// None does.
    NotEntry,
    /// What is there reads as a node that has no key and is in no link.
    /// It may be one, or a zero byte within another entry.
    Unconfirmed,
}

/// What [`Graph::import`] does with a link it cannot import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnError {
    /// Fail the import, leaving what it already appended.
    #[default]
    Stop,
    /// Skip the link, and the links that refer to it.
    Skip,
}

/// The result of [`Graph::import`].
#[derive(Debug, Default)]
pub struct Imported {
    /// The local id of each imported entry that was written or matched.
    pub ids: BTreeMap<u64, u64>,
    /// The ids of the links skipped, with why.
    pub skipped: Vec<(u64, Error)>,
}

/// Whether a graph exists at `prefix`.
pub fn exists(prefix: &Path) -> bool {
    local::exists(prefix)
}

/// Deletes the graph at `prefix`.
pub fn remove(prefix: &Path) -> io::Result<()> {
    local::remove(prefix)?;
    stats::remove(prefix)?;
    vectors::remove(prefix)?;
    times::remove(prefix)
}

/// A graph database; see the [module](self) docs.
pub struct Graph {
    local: Local,
    /// The statistics of every entry appended.
    stats: Arc<Stats>,
    /// The statistics of the committed entries.
    committed: Arc<Stats>,
    /// The id the saved statistics cover entries before.
    saved_id: u64,
    /// When each id was committed.
    times: Times,
    /// Values computed by queries that the graph doesn't have: see
    /// [`ephemeral_value`](Self::ephemeral_value).
    /// Shared with the graph's readers, so that ids agree.
    ephemeral: Arc<std::sync::Mutex<Ephemeral>>,
    /// The latest reader, which the writer replaces at each commit, once
    /// [`readers`](Graph::readers) asked for it.
    published: Option<Arc<std::sync::Mutex<Arc<Graph>>>>,
    /// The snapshots of the readers alive (their ends, counted), shared by
    /// the writer and its readers, so that compaction never passes one.
    snapshots: Arc<std::sync::Mutex<BTreeMap<u64, usize>>>,
    /// A reader's snapshot, registered there while it lives.
    snapshot: Option<u64>,
    /// The vector indexes declared (see
    /// [`create_vector_index`](Self::create_vector_index)).
    vector_indexes: indexes::VectorIndexes,
    /// Their searches, shared with the readers; and the predicates the
    /// writer has appended links of since its last commit, searched exactly.
    searches: vectors::SharedSearches,
    vector_dirty: HashSet<u64>,
}

/// The bit set in the id of a [ephemeral value](Graph::ephemeral_value):
/// no entry's id has it.
pub const EPHEMERAL: u64 = 1 << 63;

/// What an ephemeral id is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EphemeralKind {
    Value,
    Key,
    /// A node, or a link of an overlay: data of its own it hasn't.
    Anonymous,
}

/// Values, keys and nodes held in memory, never written: each one's kind,
/// data and holds, by id, and the id of each value and key.  Ids aren't
/// given again, so one freed names nothing rather than other data.
#[derive(Default)]
struct Ephemeral {
    /// The ids given, so far: the next one's, without [`EPHEMERAL`].
    next: u64,
    entries: HashMap<u64, EphemeralEntry>,
    ids: HashMap<(EphemeralKind, Box<[u8]>), u64>,
}

struct EphemeralEntry {
    kind: EphemeralKind,
    data: Box<[u8]>,
    /// The [holds](EphemeralHold) holding it, freed once there are none; or
    /// `None` if kept while the graph is open, once given with no hold.
    holds: Option<usize>,
}

impl Ephemeral {
    /// The id of the `data` of `kind` (a new one, for an anonymous node),
    /// added to `hold`'s, or kept for good if `hold` is `None`.
    fn id(&mut self, kind: EphemeralKind, data: &[u8], hold: Option<&mut HashSet<u64>>) -> u64 {
        let found = if kind == EphemeralKind::Anonymous {
            None
        } else {
            self.ids.get(&(kind, data.into())).copied()
        };
        let id = found.unwrap_or_else(|| {
            let id = EPHEMERAL | self.next;
            self.next += 1;
            let data: Box<[u8]> = data.into();
            if kind != EphemeralKind::Anonymous {
                self.ids.insert((kind, data.clone()), id);
            }
            self.entries.insert(
                id,
                EphemeralEntry {
                    kind,
                    data,
                    holds: Some(0),
                },
            );
            id
        });
        self.hold(id, hold);
        id
    }

    /// Adds `id` to `hold`'s, or keeps it for good if `hold` is `None`.
    fn hold(&mut self, id: u64, hold: Option<&mut HashSet<u64>>) {
        let Some(e) = self.entries.get_mut(&id) else {
            return;
        };
        match hold {
            None => e.holds = None,
            Some(ids) => {
                if let Some(n) = &mut e.holds
                    && ids.insert(id)
                {
                    *n += 1;
                }
            }
        }
    }

    /// Drops a hold of each of `ids`, freeing those it was the last of.
    fn release(&mut self, ids: impl IntoIterator<Item = u64>) {
        for id in ids {
            let Some(e) = self.entries.get_mut(&id) else {
                continue;
            };
            let Some(n) = &mut e.holds else {
                continue;
            };
            *n -= 1;
            if *n == 0 {
                let e = self.entries.remove(&id).expect("an entry");
                if e.kind != EphemeralKind::Anonymous {
                    self.ids.remove(&(e.kind, e.data));
                }
            }
        }
    }
}

/// A hold of [ephemeral](Graph::ephemeral_value) ids, which are freed once
/// no hold has them, rather than kept while the graph is open: of a query's
/// constants and the values it computes, or of an overlay's nodes.
/// Cloning it shares it; it lets go of its ids when the last clone drops.
#[derive(Clone)]
pub struct EphemeralHold(Arc<HoldInner>);

struct HoldInner {
    table: Arc<std::sync::Mutex<Ephemeral>>,
    ids: std::sync::Mutex<HashSet<u64>>,
}

impl Drop for HoldInner {
    fn drop(&mut self) {
        let ids = std::mem::take(self.ids.get_mut().unwrap_or_else(|e| e.into_inner()));
        if !ids.is_empty() {
            let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
            t.release(ids);
        }
    }
}

impl fmt::Debug for EphemeralHold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EphemeralHold")
    }
}

impl EphemeralHold {
    /// [`Graph::ephemeral_value`], held by this hold rather than kept.
    pub fn value(&self, g: &Graph, data: &[u8]) -> u64 {
        if let Some(id) = g.find_value(data) {
            return id;
        }
        self.id(g, EphemeralKind::Value, data)
    }

    /// [`Graph::ephemeral_key`], held by this hold rather than kept.
    pub fn key(&self, g: &Graph, key: &[u8]) -> u64 {
        if let Some(id) = g.find_by_key(key) {
            return id;
        }
        self.id(g, EphemeralKind::Key, key)
    }

    /// [`Graph::ephemeral_node`], held by this hold rather than kept.
    pub fn node(&self, g: &Graph) -> u64 {
        self.id(g, EphemeralKind::Anonymous, &[])
    }

    /// Holds `id`, if it is an ephemeral one not yet freed, as long as this
    /// hold: an id given by another, which something of this one's names.
    pub fn retain(&self, id: u64) {
        if id & EPHEMERAL == 0 {
            return;
        }
        let mut t = self.0.table.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids = self.0.ids.lock().unwrap_or_else(|e| e.into_inner());
        t.hold(id, Some(&mut ids));
    }

    /// Whether `self` and `other` are the same hold.
    pub fn same(&self, other: &EphemeralHold) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn id(&self, g: &Graph, kind: EphemeralKind, data: &[u8]) -> u64 {
        debug_assert!(
            Arc::ptr_eq(&self.0.table, &g.ephemeral),
            "a hold of another graph"
        );
        let mut t = self.0.table.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids = self.0.ids.lock().unwrap_or_else(|e| e.into_inner());
        t.id(kind, data, Some(&mut ids))
    }
}

/// The [holds](EphemeralHold) of the ephemeral ids a result names, which it
/// keeps while it lives.  They compare equal to any others, so that results
/// compare by what they name.
#[derive(Clone, Default)]
pub struct Holds(Vec<EphemeralHold>);

impl Holds {
    /// The holds of what `query` gives, and of the overlay it reads, if any.
    pub fn of(query: &crate::query::Query, overlay: Option<&crate::query::Overlay>) -> Holds {
        let mut out = overlay.map(|o| o.holds()).unwrap_or_default();
        if let Some(h) = query.ephemeral_hold() {
            out.push(h);
        }
        out
    }

    /// Adds `hold`, if not already here.
    pub fn push(&mut self, hold: &EphemeralHold) {
        if !self.0.iter().any(|h| h.same(hold)) {
            self.0.push(hold.clone());
        }
    }

    /// Adds `other`'s.
    pub fn extend(&mut self, other: &Holds) {
        for h in &other.0 {
            self.push(h);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for Holds {
    fn eq(&self, _: &Holds) -> bool {
        true
    }
}

impl fmt::Debug for Holds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Holds({})", self.0.len())
    }
}

impl Graph {
    /// Opens or creates the graph whose files start with `prefix`, or an
    /// in-memory graph if `prefix` is `None`.
    pub fn open(prefix: Option<&Path>, mut options: Options) -> Result<Self> {
        options.tables = options.tables.max(NUM_BUILTIN_INDEXES);
        let mut local = Local::open(prefix, options)?;
        if local.indexes_stale() {
            tuple_index::reindex(&mut local)?;
            local.indexes_rebuilt()?;
        }
        Self::cover_text(&mut local)?;
        Self::with_stats(local)
    }

    /// Indexes the text of a database made before the text index: its table
    /// is new and empty.  A crash while that is under way has every index
    /// rebuilt, as the indexes are marked stale.
    fn cover_text(local: &mut Local) -> Result<()> {
        if tuple_index::text_covered(local) {
            return Ok(());
        }
        if local.log().next_id() != local.log().start_id() {
            local.begin_bulk()?;
            tuple_index::index_text(local)?;
            tuple_index::mark_text_covered(local)?;
            local.indexes_rebuilt()?;
        } else {
            tuple_index::mark_text_covered(local)?;
        }
        Ok(())
    }

    /// Rebuilds the [text index](crate::text) from the log.  Commit first.
    pub fn reindex_text(&mut self) -> Result<()> {
        if self.is_reader() {
            return Err(Error::InvalidArgument("a reader can't rebuild an index"));
        }
        if !self.local.log().is_clean() {
            return Err(Error::InvalidArgument(
                "commit before rebuilding the text index",
            ));
        }
        self.local.begin_bulk()?;
        self.local.reset_table(BuiltinIndex::Text.table())?;
        tuple_index::index_text(&mut self.local)?;
        tuple_index::mark_text_covered(&mut self.local)?;
        Ok(self.local.indexes_rebuilt()?)
    }

    /// Makes dropping this graph like a crash: nothing more is flushed.
    #[cfg(test)]
    pub(crate) fn crash_for_test(&mut self) {
        self.local.crash();
    }

    /// The index of the values containing each term.
    pub(crate) fn text_table(&self) -> crate::local_table::TableRef<'_> {
        self.local.table(BuiltinIndex::Text.table())
    }

    /// The disk space the text index takes, in bytes: `None` in memory.
    pub fn text_index_bytes(&self) -> Option<u64> {
        self.local.table_bytes(BuiltinIndex::Text.table())
    }

    /// The graph of `local`, loading its statistics and counting the entries
    /// they do not cover.
    fn with_stats(local: Local) -> Result<Self> {
        let log = local.log();
        let meta = log.metadata();
        let loaded = local
            .prefix()
            .and_then(|p| stats::load(p, meta.uuid))
            .filter(|&(_, id)| (log.start_id()..=meta.next_id).contains(&id));
        let (mut stats, saved_id) = loaded.unwrap_or_else(|| (Stats::default(), log.start_id()));
        for record in log.records(saved_id) {
            let record = record?;
            let replaces = replaces_live(&local, &record);
            stats.add(&record, replaces);
        }
        let times = Times::open(local.prefix(), meta.uuid, log)?;
        let mut g = Graph {
            local,
            committed: Arc::new(stats.clone()),
            stats: Arc::new(stats),
            saved_id,
            times,
            ephemeral: Default::default(),
            published: None,
            snapshots: Default::default(),
            snapshot: None,
            vector_indexes: Arc::new([]),
            searches: Default::default(),
            vector_dirty: HashSet::new(),
        };
        g.load_vector_indexes()?;
        if !g.is_reader() {
            g.load_vector_searches();
            g.feed_vector_searches()?;
        }
        Ok(g)
    }

    /// Creates a graph at `prefix`, or in memory, from a
    /// [`snapshot`](Self::snapshot) read from `input`.
    pub fn restore(
        prefix: Option<&Path>,
        mut options: Options,
        input: &mut impl Read,
    ) -> Result<Self> {
        options.tables = options.tables.max(NUM_BUILTIN_INDEXES);
        let mut local = Local::restore(prefix, options, input)?;
        Self::cover_text(&mut local)?;
        Self::with_stats(local)
    }

    /// Writes a snapshot of the graph, including entries not yet committed,
    /// to `out`.
    pub fn snapshot(&mut self, out: &mut impl Write) -> Result<()> {
        Ok(self.local.snapshot(out)?)
    }

    /// Makes the appended entries durable, recording the time of the
    /// commit (see [`id_at_time`](Self::id_at_time)).
    pub fn commit(&mut self) -> Result<()> {
        self.commit_at(now())
    }

    /// The same, recording `time` as the time of the commit, such as the
    /// original time of entries being imported.  Recorded times never
    /// decrease: an earlier time than the last commit's is taken as that.
    pub fn commit_at(&mut self, time: crate::record::LinkTime) -> Result<()> {
        self.local.commit()?;
        self.committed = Arc::new((*self.stats).clone());
        self.feed_vector_searches()?;
        let next_id = self.next_id();
        self.times.record(time, next_id);
        self.publish();
        Ok(())
    }

    /// A reader of the graph, as of its last commit: a graph, for any
    /// thread, that sees what was committed then, while this one goes on
    /// being written and committed (see [Readers](self#readers)).  Its
    /// writes fail.
    pub fn reader(&self) -> Graph {
        let local = self.local.reader();
        let end = local.log().next_id();
        *self
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(end)
            .or_default() += 1;
        let mut r = Graph {
            local,
            stats: Arc::clone(&self.committed),
            committed: Arc::clone(&self.committed),
            saved_id: self.saved_id,
            times: self.times.reader(),
            ephemeral: Arc::clone(&self.ephemeral),
            published: None,
            snapshots: Arc::clone(&self.snapshots),
            snapshot: Some(end),
            vector_indexes: Arc::new([]),
            searches: Arc::clone(&self.searches),
            vector_dirty: HashSet::new(),
        };
        // As committed: the writer's may include some since.
        if r.load_vector_indexes().is_err() {
            r.vector_indexes = Arc::clone(&self.vector_indexes);
        }
        r
    }

    /// A handle that gives readers of the graph as of its latest commit, on
    /// any thread, while this graph is written and committed by another:
    /// from now on, each commit publishes a new reader (see
    /// [Readers](self#readers)).
    pub fn readers(&mut self) -> Readers {
        if self.is_reader() {
            let reader = Arc::new(self.reader());
            return Readers {
                latest: Arc::new(std::sync::Mutex::new(reader)),
            };
        }
        let latest = match &self.published {
            Some(p) => Arc::clone(p),
            None => {
                let p = Arc::new(std::sync::Mutex::new(Arc::new(self.reader())));
                self.published = Some(Arc::clone(&p));
                p
            }
        };
        Readers { latest }
    }

    /// Whether this is a reader (see [`reader`](Self::reader)).
    pub fn is_reader(&self) -> bool {
        self.local.is_reader()
    }

    /// Replaces the latest reader, if anyone may want one.
    fn publish(&self) {
        if let Some(p) = &self.published {
            let reader = Arc::new(self.reader());
            *p.lock().unwrap_or_else(|e| e.into_inner()) = reader;
        }
    }

    /// The last id committed at or before `time`, in milliseconds since
    /// 1970, so that querying as of it sees the graph as it was then; just
    /// before the first id if nothing was.  Entries appended but not yet
    /// committed have no time.
    pub fn id_at_time(&self, time: crate::record::LinkTime) -> u64 {
        self.times
            .id_at(time)
            .unwrap_or(self.start_id().saturating_sub(1))
    }

    /// When the entry at `id` was committed, or `None` if it is not yet.
    pub fn time_of(&self, id: u64) -> Option<crate::record::LinkTime> {
        self.times.time_of(id)
    }

    /// The times of the graph's commits.
    pub fn times(&self) -> &Times {
        &self.times
    }

    /// Starts a bulk load: a crash before [`end_bulk`](Self::end_bulk) has
    /// the indexes rebuilt from the log, so nothing need be flushed for
    /// them until then.  Commit before ending it.
    pub fn begin_bulk(&mut self) -> Result<()> {
        Ok(self.local.begin_bulk()?)
    }

    /// Ends a bulk load, after it is committed or rolled back.
    pub fn end_bulk(&mut self) -> Result<()> {
        Ok(self.local.end_bulk()?)
    }

    /// Sizes a graph with nothing in it for about `tuples` tuples, as
    /// [`Options::for_tuples`] does on creating one; a graph with entries
    /// is left as it is.
    pub fn presize(&mut self, tuples: u64) -> Result<()> {
        Ok(self.local.presize(tuples)?)
    }

    /// Discards the entries appended since the last commit.  Their ids are not
    /// reused.
    pub fn rollback(&mut self) -> Result<()> {
        self.local.log_mut().abandon_commit()?;
        self.stats = Arc::new((*self.committed).clone());
        self.load_vector_indexes()?;
        self.vector_dirty.clear();
        // The ids abandoned are committed as nodes.
        self.publish();
        Ok(())
    }

    /// Runs `f`, then commits if it succeeds or rolls back if it fails.
    /// Rolling back discards everything appended since the last commit, not
    /// just what `f` appended.
    pub fn batch<R>(&mut self, f: impl FnOnce(&mut Graph) -> Result<R>) -> Result<R> {
        match f(self) {
            Ok(r) => {
                self.commit()?;
                Ok(r)
            }
            Err(e) => {
                self.rollback()?;
                Err(e)
            }
        }
    }

    /// Does periodic maintenance on the indexes, and saves the statistics.
    /// With a [`history`](Options::history) to keep, it also compacts the
    /// large index entries to the last id committed that long ago, once the
    /// entries since the horizon they were last compacted to are at least
    /// an eighth of the log, so that compacting costs a constant amount per
    /// entry appended.
    pub fn tick(&mut self) -> Result<()> {
        if self.is_reader() {
            return Ok(());
        }
        self.local.tick()?;
        if let Some(history) = self.local.options().history {
            let horizon = self.id_at_time(now().saturating_sub(history));
            let (start, done) = (self.start_id(), self.horizon().max(self.start_id()));
            if horizon > done && (horizon - done) * 8 >= self.next_id() - start {
                self.compact(horizon)?;
            }
        }
        self.rebuild_vector_searches();
        self.save_vector_searches(false)?;
        self.save_stats()
    }

    /// The id the large index entries are compacted to, or 0: queries as of
    /// earlier fail (see [`compact`](Self::compact)).
    pub fn horizon(&self) -> u64 {
        self.local.horizon()
    }

    /// Drops from the large index entries the history at or before
    /// `horizon`: the links that later links at or before it supersede, and
    /// those dead as of it.  Queries as of `horizon` or later find the same
    /// links as before; those as of earlier fail with [`Error::Compacted`],
    /// as do [standing queries](crate::query::StandingQuery) last refreshed
    /// before it.  `horizon` must be committed; one before the current
    /// horizon is taken as that, to compact the entries made large since.
    ///
    /// The pages of the old entries are reused once the readers made before
    /// are dropped.
    pub fn compact(&mut self, horizon: u64) -> Result<crate::local_table::Compacted> {
        let committed = self.local.log().metadata().next_id;
        if horizon >= committed {
            return Err(Error::InvalidArgument(
                "the compaction horizon must be committed",
            ));
        }
        // Not past a reader's snapshot: what it sees must stay.
        let oldest = self
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .next()
            .copied();
        let horizon = match oldest {
            Some(end) => horizon.min(end.saturating_sub(1)),
            None => horizon,
        };
        let horizon = horizon.max(self.horizon());
        Ok(tuple_index::compact(&mut self.local, horizon)?)
    }

    /// Saves the statistics of the committed entries, if they have changed.
    pub fn save_stats(&mut self) -> Result<()> {
        if self.is_reader() {
            return Ok(());
        }
        let meta = self.local.log().metadata();
        if let Some(prefix) = self.local.prefix()
            && meta.next_id != self.saved_id
        {
            stats::save(prefix, meta.uuid, meta.next_id, &self.committed)?;
            self.saved_id = meta.next_id;
        }
        Ok(())
    }

    /// The statistics of the graph's links, including those not yet
    /// committed.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn is_transient(&self) -> bool {
        self.local.is_transient()
    }

    /// The underlying database.
    pub fn local(&self) -> &Local {
        &self.local
    }

    /// The id of the first entry.
    pub fn start_id(&self) -> u64 {
        self.local.log().start_id()
    }

    /// The id the next entry appended will have.
    pub fn next_id(&self) -> u64 {
        self.local.log().next_id()
    }

    /// Whether `id` is within the graph.  It may not start an entry.
    pub fn contains(&self, id: u64) -> bool {
        (self.start_id()..self.next_id()).contains(&id)
    }

    /// The entry at `id`.
    pub fn get(&self, id: u64) -> Result<Record<'_>> {
        Ok(self.local.log().get(id)?)
    }

    /// Whether an entry starts at `id`, as far as the indexes can tell.
    /// [`get`](Self::get) decodes whatever is at `id`, which within an entry
    /// may look like another one: a zero byte, for one, reads as a node.
    /// The indexes confirm a value by its data, a link by its SPO, and a
    /// node by its key or a link naming it.
    pub fn check_entry(&self, id: u64) -> EntryCheck {
        if !self.contains(id) {
            return EntryCheck::NotEntry;
        }
        let Ok(record) = self.get(id) else {
            return EntryCheck::NotEntry;
        };
        let confirmed = if let Some(link) = record.as_link() {
            self.find_links(link.subject(), link.predicate(), link.object())
                .contains(&id)
        } else if record.is_node() {
            if self.key(id).is_some() || self.is_linked(id) {
                true
            } else {
                return EntryCheck::Unconfirmed;
            }
        } else {
            record
                .data()
                .is_some_and(|data| self.find_value(data) == Some(id))
        };
        if confirmed {
            EntryCheck::Entry
        } else {
            EntryCheck::NotEntry
        }
    }

    /// Whether a link has `id` as its subject, predicate or object.
    fn is_linked(&self, id: u64) -> bool {
        [Linkage::Subject, Linkage::Predicate, Linkage::Object]
            .into_iter()
            .any(|linkage| {
                tuple_index::find_links(&self.local, linkage, id)
                    .into_iter()
                    .any(|l| self.with_link(l, |l| l.get(linkage) == id) == Some(true))
            })
    }

    /// The entries from `id` (which must start an entry) on.
    pub fn records(&self, id: u64) -> Records<'_> {
        self.local.log().records(id)
    }

    /// The data of the value at `id`: borrowed from the log, or a copy of
    /// an ephemeral value's (see [`ephemeral_value`](Self::ephemeral_value)).
    pub fn data(&self, id: u64) -> Option<Cow<'_, [u8]>> {
        if id & EPHEMERAL != 0 {
            return self.ephemeral_data(id, false).map(Cow::Owned);
        }
        self.stored_data(id).map(Cow::Borrowed)
    }

    /// The data of the value at `id` in the log.
    fn stored_data(&self, id: u64) -> Option<&[u8]> {
        match self.local.log().entry(id) {
            Ok(Entry::Value(v)) if !v.is_empty() => Some(v.as_bytes()),
            _ => None,
        }
    }

    /// Calls `f` with the link at `id`, if it is one.
    fn with_link<T>(&self, id: u64, f: impl FnOnce(&Link<'_>) -> T) -> Option<T> {
        let record = self.local.log().get(id).ok()?;
        record.as_link().map(|l| f(&l))
    }

    fn append(&mut self, record: &Record<'_>) -> Result<u64> {
        self.append_replacing(record, false)
    }

    /// Appends `record`, which, if `replaces`, is a live link replacing the
    /// live link of its subject, predicate and object.
    fn append_replacing(&mut self, record: &Record<'_>, replaces: bool) -> Result<u64> {
        self.local.log_mut().append(record)?;
        tuple_index::index_records(&mut self.local, std::slice::from_ref(record))?;
        Arc::make_mut(&mut self.stats).add(record, replaces);
        self.vector_appended(record);
        Ok(record.id())
    }

    /// Appends a node.
    pub fn node(&mut self) -> Result<u64> {
        let id = self.next_id();
        self.append(&Record::node(id))
    }

    /// The value holding `data`, appending it if there is none.  Empty data
    /// is a node, which is not interned.
    pub fn value(&mut self, data: &[u8]) -> Result<u64> {
        if data.is_empty() {
            return self.node();
        }
        if let Some(id) = self.find_value(data) {
            return Ok(id);
        }
        let id = self.next_id();
        self.append(&Record::value(id, data))
    }

    /// The value holding `data`.
    pub fn find_value(&self, data: &[u8]) -> Option<u64> {
        tuple_index::find_value(&self.local, data).filter(|&id| self.stored_data(id) == Some(data))
    }

    /// Links `subject` to `object` by `predicate`, now, returning the live
    /// link.
    pub fn link(&mut self, subject: u64, predicate: u64, object: u64) -> Result<u64> {
        self.add_link(&LinkFields {
            subject,
            predicate,
            object,
            when: now(),
            ..LinkFields::default()
        })
    }

    /// Checks that `fields` make a link here: a live one with a predicate
    /// that has a vector index has a vector of its dimension.
    fn check_link(&self, fields: &LinkFields) -> Result<()> {
        if fields.subject == 0 || fields.predicate == 0 || fields.object == 0 {
            return Err(Error::InvalidArgument(
                "a link needs a subject, predicate and object",
            ));
        }
        if fields.state == LinkState::Live
            && let Some(index) = self.vector_index(fields.predicate)
        {
            self.check_vector(fields.object, index.dims)?;
        }
        let ids = [
            fields.subject,
            fields.predicate,
            fields.object,
            fields.author,
        ];
        for &id in ids.iter().chain(&fields.subgraphs) {
            if id != 0 && id < self.start_id() {
                return Err(Error::InvalidArgument("a link field is before the graph"));
            }
            if id >= self.next_id() {
                return Err(crate::Error::ForwardReference.into());
            }
        }
        Ok(())
    }

    /// Appends a link with `fields`, unless the latest link with its SPO
    /// already has its state, which is returned instead.  The subject,
    /// predicate and object must be the ids of earlier entries, and the
    /// other fields but `when` 0 or such ids.
    ///
    /// A live link of a fact that is live already takes the union of the
    /// latest link's subgraphs and `fields`'; it is appended only if that
    /// adds one, and else the latest is returned (see
    /// [Subgraphs](#subgraphs)).
    pub fn add_link(&mut self, fields: &LinkFields) -> Result<u64> {
        self.check_link(fields)?;
        let (s, p, o) = (fields.subject, fields.predicate, fields.object);
        let Some((id, state)) = self.latest_link(s, p, o) else {
            let record = Record::link(self.next_id(), fields)?;
            return self.append(&record);
        };
        if state != fields.state {
            let record = Record::link(self.next_id(), fields)?;
            return self.append(&record);
        }
        if state == LinkState::Dead {
            return Ok(id);
        }
        // Live, and live again: the union of the subgraphs.
        let mut subgraphs = self.with_link(id, |l| l.subgraphs()).unwrap_or_default();
        let before = subgraphs.len();
        for &g in &fields.subgraphs {
            if g != 0 && !subgraphs.contains(&g) {
                subgraphs.push(g);
            }
        }
        if subgraphs.len() == before {
            return Ok(id);
        }
        let record = Record::link(
            self.next_id(),
            &LinkFields {
                subgraphs,
                ..fields.clone()
            },
        )?;
        self.append_replacing(&record, true)
    }

    /// Adds the fact `subject predicate object` to subgraph `subgraph`, now,
    /// returning its live link: if the fact isn't live, it is linked (into
    /// the main graph too); if it is and isn't in `subgraph`, a live link
    /// with its subgraphs and `subgraph` replaces its latest.
    pub fn add_to_subgraph(
        &mut self,
        subject: u64,
        predicate: u64,
        object: u64,
        subgraph: u64,
    ) -> Result<u64> {
        if subgraph == 0 {
            return Err(Error::InvalidArgument("a subgraph is an entry, not 0"));
        }
        self.add_link(&LinkFields {
            subject,
            predicate,
            object,
            when: now(),
            subgraphs: vec![subgraph],
            ..LinkFields::default()
        })
    }

    /// Removes the fact `subject predicate object` from subgraph `subgraph`,
    /// now, returning the live link appended without it, or `None` if the
    /// fact isn't live in `subgraph`.  The fact stays live, in the main graph
    /// and its other subgraphs.
    pub fn remove_from_subgraph(
        &mut self,
        subject: u64,
        predicate: u64,
        object: u64,
        subgraph: u64,
    ) -> Result<Option<u64>> {
        let Some(id) = self.find_link(subject, predicate, object) else {
            return Ok(None);
        };
        let Some(mut fields) = self.with_link(id, |l| l.fields()) else {
            return Ok(None);
        };
        if !fields.subgraphs.contains(&subgraph) {
            return Ok(None);
        }
        fields.subgraphs.retain(|&g| g != subgraph);
        fields.author = 0;
        fields.when = now();
        let record = Record::link(self.next_id(), &fields)?;
        self.append_replacing(&record, true).map(Some)
    }

    /// The latest link of `subject predicate object` appended by `as_of` (or
    /// at all), and whether it is live.
    pub(crate) fn latest_link_as_of(
        &self,
        subject: u64,
        predicate: u64,
        object: u64,
        as_of: Option<u64>,
    ) -> Option<(u64, LinkState)> {
        self.newest_link(subject, predicate, object, as_of)
    }

    /// The latest link of `subject predicate object` appended by `as_of` (or
    /// at all), and its state: read newest first, so usually one record.  An
    /// id of the entry whose record is another fact's (its key's hash
    /// collided) is passed over.
    fn newest_link(
        &self,
        subject: u64,
        predicate: u64,
        object: u64,
        as_of: Option<u64>,
    ) -> Option<(u64, LinkState)> {
        let key = tuple_index::LinkKey::Spo(subject, predicate, object);
        tuple_index::links_newest_first(&self.local, key, as_of).find_map(|id| {
            self.with_link(id, |l| {
                ((l.subject(), l.predicate(), l.object()) == (subject, predicate, object))
                    .then(|| l.state())
            })
            .flatten()
            .map(|state| (id, state))
        })
    }

    /// Whether `link` is the latest, live link of its fact as of `as_of` (or
    /// now), and in subgraph `subgraph`.
    pub fn in_subgraph(&self, link: u64, subgraph: u64, as_of: Option<u64>) -> bool {
        let Some(f) = self.with_link(link, |l| l.fields()) else {
            return false;
        };
        f.subgraphs.contains(&subgraph)
            && self.latest_link_as_of(f.subject, f.predicate, f.object, as_of)
                == Some((link, LinkState::Live))
    }

    /// The links of the facts in subgraph `subgraph` as of `as_of` (or now):
    /// of the links naming it, those that are their fact's latest and live.
    /// The Subgraph index's entry for it isn't every link of its facts, so
    /// each is checked by looking its fact up.
    pub fn subgraph_links(&self, subgraph: u64, as_of: Option<u64>) -> IdSet {
        let candidates = tuple_index::find_links(&self.local, Linkage::Subgraph, subgraph);
        candidates
            .range(..=as_of.unwrap_or(u64::MAX))
            .copied()
            .filter(|&id| self.in_subgraph(id, subgraph, as_of))
            .collect()
    }

    /// The subgraphs the fact `subject predicate object` is in as of `as_of`
    /// (or now): none if it isn't live.
    pub fn subgraphs_of(
        &self,
        subject: u64,
        predicate: u64,
        object: u64,
        as_of: Option<u64>,
    ) -> Vec<u64> {
        match self.latest_link_as_of(subject, predicate, object, as_of) {
            Some((id, LinkState::Live)) => {
                self.with_link(id, |l| l.subgraphs()).unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    /// Deletes the link with this SPO, now, returning the dead link appended,
    /// or `None` if there is no live link to delete.
    pub fn delete_link(
        &mut self,
        subject: u64,
        predicate: u64,
        object: u64,
    ) -> Result<Option<u64>> {
        match self.latest_link(subject, predicate, object) {
            Some((_, LinkState::Live)) => self
                .add_link(&LinkFields {
                    subject,
                    predicate,
                    object,
                    when: now(),
                    state: LinkState::Dead,
                    ..LinkFields::default()
                })
                .map(Some),
            _ => Ok(None),
        }
    }

    /// Links `subject` to `object` by `predicate`, now, deleting any other
    /// live links from `subject` by `predicate`, so that `object` is the
    /// property's only value.  Returns the live link.
    pub fn set_link(&mut self, subject: u64, predicate: u64, object: u64) -> Result<u64> {
        self.check_link(&LinkFields {
            subject,
            predicate,
            object,
            ..LinkFields::default()
        })?;
        for other in self.follow_forward(predicate, [subject]) {
            if other != object {
                self.delete_link(subject, predicate, other)?;
            }
        }
        self.link(subject, predicate, object)
    }

    /// The object of the latest live link from `subject` by `predicate`.
    pub fn property(&self, subject: u64, predicate: u64) -> Option<u64> {
        let (links, _) = self
            .query_property([subject], Linkage::Subject, predicate, None)
            .expect("subject");
        self.with_link(*links.last()?, |l| l.object())
    }

    /// The id of the value with `data`: the graph's, or else a *ephemeral*
    /// value's, held in memory and never written, whose id has the
    /// [`EPHEMERAL`] bit set.  [`data`](Self::data) reads it; it is in no
    /// link, has no key and is no entry.  So a query can compute values (a
    /// count, a rank) and compare them by id to the graph's, or to
    /// constants that aren't in the graph.  A data's ephemeral id stays the
    /// same while it is held, even if the value is added to the graph later
    /// (then it has two ids).  One given here is kept while the graph is
    /// open; one given by an [`EphemeralHold`] (see
    /// [`ephemeral_hold`](Self::ephemeral_hold)) only while a hold has it.
    pub fn ephemeral_value(&self, data: &[u8]) -> u64 {
        if let Some(id) = self.find_value(data) {
            return id;
        }
        let mut t = self.ephemeral.lock().unwrap_or_else(|e| e.into_inner());
        t.id(EphemeralKind::Value, data, None)
    }

    /// A new ephemeral id with no data or key: a node, or a link, of an
    /// overlay (see [`query::Overlay`](crate::query::Overlay)).
    pub fn ephemeral_node(&self) -> u64 {
        let mut t = self.ephemeral.lock().unwrap_or_else(|e| e.into_inner());
        t.id(EphemeralKind::Anonymous, &[], None)
    }

    /// The node with `key`: the graph's, or else an ephemeral one's, as
    /// [`ephemeral_value`](Self::ephemeral_value) gives a value's, which
    /// [`key`](Self::key) reads.
    pub fn ephemeral_key(&self, key: &[u8]) -> u64 {
        if let Some(id) = self.find_by_key(key) {
            return id;
        }
        let mut t = self.ephemeral.lock().unwrap_or_else(|e| e.into_inner());
        t.id(EphemeralKind::Key, key, None)
    }

    /// A new hold of ephemeral ids: those it gives, and those it
    /// [retains](EphemeralHold::retain), are freed once no hold has them.
    pub fn ephemeral_hold(&self) -> EphemeralHold {
        EphemeralHold(Arc::new(HoldInner {
            table: Arc::clone(&self.ephemeral),
            ids: Default::default(),
        }))
    }

    /// The number of ephemeral values, keys and nodes the graph and its
    /// readers hold: those kept while the graph is open, and those a hold
    /// has.
    pub fn ephemeral_count(&self) -> usize {
        self.ephemeral
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .len()
    }

    /// A copy of the data of an ephemeral value, or the key of an ephemeral
    /// node.
    fn ephemeral_data(&self, id: u64, key: bool) -> Option<Vec<u8>> {
        let t = self.ephemeral.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = t.entries.get(&id) else {
            // Something naming an id should hold it.
            debug_assert!(
                id & !EPHEMERAL >= t.next,
                "ephemeral id {id:#x} read once freed"
            );
            return None;
        };
        let want = if key {
            EphemeralKind::Key
        } else {
            EphemeralKind::Value
        };
        (e.kind == want).then(|| e.data.to_vec())
    }

    /// The entity whose key is `key`.
    pub fn find_by_key(&self, key: &[u8]) -> Option<u64> {
        let predicate = self.find_value(KEY_PREDICATE)?;
        let (links, _) = self
            .query_property([self.find_value(key)?], Linkage::Object, predicate, None)
            .expect("object");
        self.with_link(*links.last()?, |l| l.subject())
    }

    /// The key of `id`: borrowed from the log, or a copy of an ephemeral
    /// node's (see [`ephemeral_key`](Self::ephemeral_key)).
    pub fn key(&self, id: u64) -> Option<Cow<'_, [u8]>> {
        if id & EPHEMERAL != 0 {
            return self.ephemeral_data(id, true).map(Cow::Owned);
        }
        self.stored_data(self.property(id, self.find_value(KEY_PREDICATE)?)?)
            .map(Cow::Borrowed)
    }

    /// The node whose key is `key`, appending one if there is none.
    pub fn node_by_key(&mut self, key: &[u8]) -> Result<u64> {
        if key.is_empty() {
            return Err(Error::InvalidArgument("empty key"));
        }
        if let Some(id) = self.find_by_key(key) {
            return Ok(id);
        }
        let node = self.node()?;
        self.set_key(node, key)?;
        Ok(node)
    }

    /// Makes `key` the key of `id`, replacing any key it has.  Fails if `key`
    /// is another entity's.
    pub fn set_key(&mut self, id: u64, key: &[u8]) -> Result<()> {
        if key.is_empty() {
            return Err(Error::InvalidArgument("empty key"));
        }
        match self.find_by_key(key) {
            Some(owner) if owner == id => return Ok(()),
            Some(owner) => return Err(Error::KeyInUse(owner)),
            None => {}
        }
        if !self.contains(id) {
            return Err(Error::InvalidArgument("not an entity in the graph"));
        }
        let predicate = self.value(KEY_PREDICATE)?;
        let key = self.value(key)?;
        self.set_link(id, predicate, key)?;
        Ok(())
    }

    /// Removes the key of `id`, returning whether it had one.
    pub fn remove_key(&mut self, id: u64) -> Result<bool> {
        let Some(predicate) = self.find_value(KEY_PREDICATE) else {
            return Ok(false);
        };
        let Some(key) = self.property(id, predicate) else {
            return Ok(false);
        };
        Ok(self.delete_link(id, predicate, key)?.is_some())
    }

    /// Imports `records`, numbered elsewhere, in increasing id order.
    ///
    /// A reference below the first record's id is to an entry already in
    /// this graph; one at or above it is to an earlier record.  Values are
    /// interned, links deduplicated as by [`add_link`](Self::add_link), and
    /// nodes and values written only when a link first refers to them, so
    /// that ones nothing refers to are not written.
    ///
    /// The import does not commit; run it in a [`batch`](Self::batch) to
    /// import all or nothing.
    pub fn import<'a>(
        &mut self,
        records: impl IntoIterator<Item = Record<'a>>,
        on_error: OnError,
    ) -> Result<Imported> {
        let local_end = self.next_id();
        let mut start = None;
        let mut last = None;
        // Nodes and values not yet referred to, by id: the data of each value.
        let mut pending: HashMap<u64, Option<Vec<u8>>> = HashMap::new();
        let mut imported = Imported::default();
        for record in records {
            let id = record.id();
            if last.is_some_and(|last| id <= last) {
                return Err(Error::InvalidArgument(
                    "records are not in increasing id order",
                ));
            }
            last = Some(id);
            let start = *start.get_or_insert(id);
            let Some(link) = record.as_link() else {
                pending.insert(
                    id,
                    record.data().filter(|d| !d.is_empty()).map(<[u8]>::to_vec),
                );
                continue;
            };
            let result = self.import_link(
                link.fields(),
                start,
                local_end,
                &mut pending,
                &mut imported.ids,
            );
            match result {
                Ok(local) => {
                    imported.ids.insert(id, local);
                }
                Err(e) if on_error == OnError::Skip => imported.skipped.push((id, e)),
                Err(e) => return Err(e),
            }
        }
        Ok(imported)
    }

    /// Imports a link with `fields` in the import's numbering, writing the
    /// nodes and values it is the first to refer to.
    fn import_link(
        &mut self,
        mut fields: LinkFields,
        start: u64,
        local_end: u64,
        pending: &mut HashMap<u64, Option<Vec<u8>>>,
        ids: &mut BTreeMap<u64, u64>,
    ) -> Result<u64> {
        let resolvable = |id: u64| {
            id == 0
                || (id < start && (self.start_id()..local_end).contains(&id))
                || ids.contains_key(&id)
                || pending.contains_key(&id)
        };
        let refs = [
            fields.subject,
            fields.predicate,
            fields.object,
            fields.author,
        ];
        if let Some(&id) = refs
            .iter()
            .chain(&fields.subgraphs)
            .find(|&&id| !resolvable(id))
        {
            return Err(Error::Unresolved(id));
        }
        if fields.subject == 0 || fields.predicate == 0 || fields.object == 0 {
            return Err(Error::InvalidArgument(
                "a link needs a subject, predicate and object",
            ));
        }
        let mut resolve = |g: &mut Graph, id: u64| -> Result<u64> {
            if id == 0 || id < start {
                return Ok(id);
            }
            if let Some(&local) = ids.get(&id) {
                return Ok(local);
            }
            let local = match pending.remove(&id).expect("resolvable") {
                Some(data) => g.value(&data)?,
                None => g.node()?,
            };
            ids.insert(id, local);
            Ok(local)
        };
        fields.subject = resolve(self, fields.subject)?;
        fields.predicate = resolve(self, fields.predicate)?;
        fields.object = resolve(self, fields.object)?;
        fields.author = resolve(self, fields.author)?;
        for subgraph in &mut fields.subgraphs {
            *subgraph = resolve(self, *subgraph)?;
        }
        self.add_link(&fields)
    }

    /// Every link, live or dead, with this SPO.
    pub fn find_links(&self, subject: u64, predicate: u64, object: u64) -> IdSet {
        tuple_index::find_links_by_spo(&self.local, subject, predicate, object)
            .into_iter()
            .filter(|&id| {
                self.with_link(id, |l| (l.subject(), l.predicate(), l.object()))
                    == Some((subject, predicate, object))
            })
            .collect()
    }

    fn latest_link(&self, subject: u64, predicate: u64, object: u64) -> Option<(u64, LinkState)> {
        self.newest_link(subject, predicate, object, None)
    }

    /// The live link with this SPO.
    pub fn find_link(&self, subject: u64, predicate: u64, object: u64) -> Option<u64> {
        match self.latest_link(subject, predicate, object) {
            Some((id, LinkState::Live)) => Some(id),
            _ => None,
        }
    }

    /// Every link, live or dead, with any of `ids` at `linkage` (any of its
    /// subgraphs, for `Subgraph`).
    pub fn links_at(&self, linkage: Linkage, ids: impl IntoIterator<Item = u64>) -> IdSet {
        if linkage == Linkage::When {
            return IdSet::new();
        }
        let mut links = Vec::new();
        for id in ids {
            links.extend(tuple_index::find_links(&self.local, linkage, id));
        }
        let mut links = IdSet::from_unsorted(links);
        links.retain(|&id| self.with_link(id, |_| ()).is_some());
        links
    }

    /// The number of links, live or dead, with `id` at `linkage` (any of
    /// their subgraphs, for `Subgraph`).  It may count links lost in a crash,
    /// so it is an upper bound.  It reads the ids but does not build a set.
    pub fn link_count(&self, linkage: Linkage, id: u64) -> u64 {
        if linkage == Linkage::When {
            return 0;
        }
        tuple_index::count_links(&self.local, linkage, id)
    }

    /// The number of links, live or dead, with `predicate` and `id` at
    /// `linkage`, which must be `Subject` or `Object`; an upper bound like
    /// [`link_count`](Self::link_count).
    pub fn property_count(&self, id: u64, linkage: Linkage, predicate: u64) -> Result<u64> {
        let (intersection, x, y) = match linkage {
            Linkage::Subject => (Intersection::SubjectPredicate, id, predicate),
            Linkage::Object => (Intersection::PredicateObject, predicate, id),
            _ => return Err(Error::InvalidArgument("linkage must be subject or object")),
        };
        Ok(tuple_index::count_links_by_intersection(
            &self.local,
            intersection,
            x,
            y,
        ))
    }

    /// The links of `links` that are the latest, live link of their SPO
    /// among those appended by `as_of` (or at all, if `None`).  `links` must
    /// hold every such link of each SPO it holds.
    pub fn live(&self, links: &IdSet, as_of: Option<u64>) -> IdSet {
        // Newest first, the first link seen of a fact is its latest.
        let mut seen = Facts::with_capacity(links.len());
        let mut live = Vec::new();
        for &id in links.range(..=as_of.unwrap_or(u64::MAX)).rev() {
            if let Some((spo, dead)) =
                self.with_link(id, |l| ([l.subject(), l.predicate(), l.object()], l.dead()))
                && seen.insert(spo)
                && !dead
            {
                live.push(id);
            }
        }
        live.reverse();
        IdSet::from_sorted(live)
    }

    /// The non-zero ids at `linkage` (every subgraph, for `Subgraph`) of
    /// `links`.
    pub fn project(&self, links: &IdSet, linkage: Linkage) -> IdSet {
        let mut out = Vec::with_capacity(links.len());
        for &id in links {
            self.with_link(id, |l| match linkage {
                Linkage::Subgraph => out.extend(l.subgraphs()),
                _ => out.push(l.get(linkage)),
            });
        }
        let mut out = IdSet::from_unsorted(out);
        out.remove(&0);
        out
    }

    /// The id at `linkage` (the first subgraph, for `Subgraph`) of each of
    /// `links`, if non-zero.
    pub fn project_map(&self, links: &IdSet, linkage: Linkage) -> BTreeMap<u64, u64> {
        links
            .iter()
            .filter_map(|&id| Some((id, self.with_link(id, |l| l.get(linkage))?)))
            .filter(|&(_, v)| v != 0)
            .collect()
    }

    /// The live links (as of `as_of`) with `predicate` and any of `ids` at
    /// `linkage`, which must be `Subject` or `Object`, and the entities at the
    /// other of the two.
    pub fn query_property(
        &self,
        ids: impl IntoIterator<Item = u64>,
        linkage: Linkage,
        predicate: u64,
        as_of: Option<u64>,
    ) -> Result<(IdSet, IdSet)> {
        let (intersection, other) = match linkage {
            Linkage::Subject => (Intersection::SubjectPredicate, Linkage::Object),
            Linkage::Object => (Intersection::PredicateObject, Linkage::Subject),
            _ => return Err(Error::InvalidArgument("linkage must be subject or object")),
        };
        // Gathered, then sorted once: the links of one id needn't follow
        // those of the id before.
        let mut links = Vec::new();
        for id in ids {
            let (x, y) = match intersection {
                Intersection::SubjectPredicate => (id, predicate),
                Intersection::PredicateObject => (predicate, id),
            };
            links.extend(tuple_index::find_links_by_intersection(
                &self.local,
                intersection,
                x,
                y,
            ));
        }
        let links = self.live(&IdSet::from_unsorted(links), as_of);
        let entities = self.project(&links, other);
        Ok((links, entities))
    }

    /// The objects of the live links with `predicate` from any of `subjects`.
    pub fn follow_forward(&self, predicate: u64, subjects: impl IntoIterator<Item = u64>) -> IdSet {
        self.query_property(subjects, Linkage::Subject, predicate, None)
            .expect("subject")
            .1
    }

    /// The subjects of the live links with `predicate` to any of `objects`.
    pub fn follow_backward(&self, predicate: u64, objects: impl IntoIterator<Item = u64>) -> IdSet {
        self.query_property(objects, Linkage::Object, predicate, None)
            .expect("object")
            .1
    }

    /// The ids at `output` of the live links (as of `as_of`) with `id` at
    /// `input`.
    pub fn entities_with(
        &self,
        input: Linkage,
        id: u64,
        output: Linkage,
        as_of: Option<u64>,
    ) -> IdSet {
        let links = self.live(&self.links_at(input, [id]), as_of);
        self.project(&links, output)
    }

    /// The id at `output` of the latest live link (as of `as_of`) with `id`
    /// at `input`.
    pub fn entity_with(
        &self,
        input: Linkage,
        id: u64,
        output: Linkage,
        as_of: Option<u64>,
    ) -> Option<u64> {
        let links = self.live(&self.links_at(input, [id]), as_of);
        let latest = *links.last()?;
        self.with_link(latest, |l| l.get(output))
            .filter(|&v| v != 0)
    }
}

/// Readers of a graph, as of its latest commit, for any thread: see
/// [`Graph::readers`].
#[derive(Clone)]
pub struct Readers {
    latest: Arc<std::sync::Mutex<Arc<Graph>>>,
}

impl Readers {
    /// A reader of the graph as of its latest commit, shared: run queries
    /// on it as on any graph.  Take another to see later commits.
    pub fn latest(&self) -> Arc<Graph> {
        Arc::clone(&self.latest.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl fmt::Debug for Readers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Readers")
    }
}

/// The subjects, predicates and objects seen, as links are read newest
/// first: a few, in a vector searched in order, or more, in a hash set.
pub(crate) struct Facts {
    few: Vec<[u64; 3]>,
    many: Option<HashSet<[u64; 3]>>,
}

impl Facts {
    /// The most kept in the vector.
    const FEW: usize = 32;

    /// For about `n` links.
    pub(crate) fn with_capacity(n: usize) -> Self {
        if n <= Self::FEW {
            Facts {
                few: Vec::with_capacity(n),
                many: None,
            }
        } else {
            Facts {
                few: Vec::new(),
                many: Some(HashSet::with_capacity(n)),
            }
        }
    }

    /// Adds `spo`, returning whether it is new.
    pub(crate) fn insert(&mut self, spo: [u64; 3]) -> bool {
        if let Some(many) = &mut self.many {
            return many.insert(spo);
        }
        if self.few.contains(&spo) {
            return false;
        }
        if self.few.len() == Self::FEW {
            let mut many: HashSet<[u64; 3]> = self.few.drain(..).collect();
            many.insert(spo);
            self.many = Some(many);
        } else {
            self.few.push(spo);
        }
        true
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        if let Some(end) = self.snapshot {
            let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = snapshots.get_mut(&end) {
                *n -= 1;
                if *n == 0 {
                    snapshots.remove(&end);
                }
            }
        }
        let _ = self.save_stats();
        let _ = self.save_vector_searches(true);
    }
}

mod indexes;
#[cfg(test)]
mod tests;
mod vectors;

pub use vectors::VectorSearch;

pub use indexes::{
    DIMS_PREDICATE, EF_PREDICATE, INDEXES_KEY, M_PREDICATE, METRIC_PREDICATE,
    VECTOR_INDEX_PREDICATE, VectorIndex,
};

/// Whether `record` is a live link replacing a live link of its subject,
/// predicate and object, as a change of its subgraphs does: for counting
/// the statistics of a log read again.
fn replaces_live(local: &Local, record: &Record<'_>) -> bool {
    let Some(link) = record.as_link() else {
        return false;
    };
    if link.dead() {
        return false;
    }
    let links =
        tuple_index::find_links_by_spo(local, link.subject(), link.predicate(), link.object());
    let log = local.log();
    // The link before it of the same fact, if live.
    links
        .range(..record.id())
        .rev()
        .find_map(|&id| {
            let r = log.get(id).ok()?;
            let l = r.as_link()?;
            ((l.subject(), l.predicate(), l.object())
                == (link.subject(), link.predicate(), link.object()))
                .then(|| !l.dead())
        })
        .unwrap_or(false)
}
