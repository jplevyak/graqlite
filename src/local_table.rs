//! An index from ids or [`Uuid`]s to sets of ids, stored in a
//! [`dynamic_table`].
//!
//! A dynamic table key holds any
//! number of values, and an in-memory dynamic table serves transient indexes.
//!
//! An id key `k` is the same key as the [`Uuid`] `{ uuid0: k, uuid1: 0 }`.
//! Keys and ids must be non-zero.
//!
//! A key holds either a set of ids, built up by [`insert`](LocalTable::insert),
//! or a single id, set by [`set`](LocalTable::set); a table should use each
//! key one way.  Sets are not checked for duplicates when inserting, since
//! that would cost a lookup per insert; duplicates are dropped when reading.

use std::collections::HashMap;
use std::io;
use std::ops::{Bound, RangeBounds};
use std::path::Path;

use crate::dynamic_table::{self, Inserted, Lookup, MAX_VALUES, Reader, Writer};
pub use crate::dynamic_table::{Error, Options, Result};
use crate::postings::{self, Cursor, Postings, PostingsReader};
use crate::record::Uuid;

/// A set of ids, in order: a sorted vector of distinct ids.  Ids mostly
/// arrive in order (index entries give them so), so adding one is a push,
/// and a set operation a merge or a binary search; a set built from ids in
/// any order is sorted once (see [`from_unsorted`](IdSet::from_unsorted)).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct IdSet(Vec<u64>);

impl IdSet {
    pub fn new() -> Self {
        IdSet(Vec::new())
    }

    /// The set of `ids`, which must be increasing.
    pub fn from_sorted(ids: Vec<u64>) -> Self {
        debug_assert!(ids.windows(2).all(|w| w[0] < w[1]), "sorted ids");
        IdSet(ids)
    }

    /// The set of `ids`, in any order, with repeats.
    pub fn from_unsorted(mut ids: Vec<u64>) -> Self {
        if !ids.windows(2).all(|w| w[0] < w[1]) {
            ids.sort_unstable();
            ids.dedup();
        }
        IdSet(ids)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The ids, increasing.
    pub fn iter(&self) -> std::slice::Iter<'_, u64> {
        self.0.iter()
    }

    pub fn as_slice(&self) -> &[u64] {
        &self.0
    }

    pub fn contains(&self, id: &u64) -> bool {
        self.0.binary_search(id).is_ok()
    }

    pub fn first(&self) -> Option<&u64> {
        self.0.first()
    }

    pub fn last(&self) -> Option<&u64> {
        self.0.last()
    }

    /// Adds `id`, returning whether it is new: a push, if it is the
    /// greatest.
    pub fn insert(&mut self, id: u64) -> bool {
        match self.0.last() {
            Some(&last) if last >= id => match self.0.binary_search(&id) {
                Ok(_) => false,
                Err(i) => {
                    self.0.insert(i, id);
                    true
                }
            },
            _ => {
                self.0.push(id);
                true
            }
        }
    }

    /// Removes `id`, returning whether it was here.
    pub fn remove(&mut self, id: &u64) -> bool {
        match self.0.binary_search(id) {
            Ok(i) => {
                self.0.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    /// The ids within `range`, increasing (or, from the back, decreasing).
    pub fn range(&self, range: impl RangeBounds<u64>) -> std::slice::Iter<'_, u64> {
        let lo = match range.start_bound() {
            Bound::Included(x) => self.0.partition_point(|id| id < x),
            Bound::Excluded(x) => self.0.partition_point(|id| id <= x),
            Bound::Unbounded => 0,
        };
        let hi = match range.end_bound() {
            Bound::Included(x) => self.0.partition_point(|id| id <= x),
            Bound::Excluded(x) => self.0.partition_point(|id| id < x),
            Bound::Unbounded => self.0.len(),
        };
        self.0[lo..hi.max(lo)].iter()
    }

    /// Keeps the ids for which `keep` holds.
    pub fn retain(&mut self, keep: impl FnMut(&u64) -> bool) {
        self.0.retain(keep);
    }

    /// The ids in both.
    pub fn intersection<'a>(&'a self, other: &'a IdSet) -> impl Iterator<Item = &'a u64> + 'a {
        let (small, large) = if self.len() <= other.len() {
            (self, other)
        } else {
            (other, self)
        };
        small.iter().filter(move |id| large.contains(id))
    }

    /// The ids, as a vector.
    pub fn into_vec(self) -> Vec<u64> {
        self.0
    }
}

impl Extend<u64> for IdSet {
    /// Adds the ids: appended, if they follow these in order, else merged.
    fn extend<I: IntoIterator<Item = u64>>(&mut self, ids: I) {
        let start = self.0.len();
        let mut sorted = true;
        for id in ids {
            if let Some(&last) = self.0.last() {
                sorted &= last < id;
            }
            self.0.push(id);
        }
        if !sorted && self.0.len() > start {
            self.0.sort_unstable();
            self.0.dedup();
        }
    }
}

impl<'a> Extend<&'a u64> for IdSet {
    fn extend<I: IntoIterator<Item = &'a u64>>(&mut self, ids: I) {
        self.extend(ids.into_iter().copied());
    }
}

impl FromIterator<u64> for IdSet {
    fn from_iter<I: IntoIterator<Item = u64>>(ids: I) -> Self {
        IdSet::from_unsorted(ids.into_iter().collect())
    }
}

impl<'a> FromIterator<&'a u64> for IdSet {
    fn from_iter<I: IntoIterator<Item = &'a u64>>(ids: I) -> Self {
        ids.into_iter().copied().collect()
    }
}

impl IntoIterator for IdSet {
    type Item = u64;
    type IntoIter = std::vec::IntoIter<u64>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a IdSet {
    type Item = &'a u64;
    type IntoIter = std::slice::Iter<'a, u64>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<const N: usize> From<[u64; N]> for IdSet {
    fn from(ids: [u64; N]) -> Self {
        ids.into_iter().collect()
    }
}

/// The ids at which a key's set is promoted from its bucket to a tree in the
/// table's postings (see [`postings`]).
pub const PROMOTE_AT: usize = 64;

/// A table key: an id or a [`Uuid`].
pub trait Key: Copy {
    /// The dynamic table key and second key.
    fn parts(self) -> (u64, u64);
}

impl Key for u64 {
    fn parts(self) -> (u64, u64) {
        (self, 0)
    }
}

impl Key for Uuid {
    fn parts(self) -> (u64, u64) {
        (self.uuid0, self.uuid1)
    }
}

/// Whether a table exists at `prefix`.
pub fn exists(prefix: &Path) -> bool {
    dynamic_table::exists(prefix)
}

/// Deletes the table at `prefix`, and its postings.
pub fn remove(prefix: &Path) -> io::Result<()> {
    dynamic_table::remove(prefix)?;
    postings::remove(prefix)
}

/// What a key holds, as a lookup found it.
enum Entry {
    None,
    /// The key's ids in its bucket, in the order inserted, perhaps repeated.
    Inline(Vec<u64>),
    /// The header page of the key's tree.
    Promoted(u64),
}

fn entry(lookup: Option<Lookup>) -> Entry {
    match lookup {
        None => Entry::None,
        Some(l) if l.bit => Entry::Promoted(l.values[0]),
        Some(l) => Entry::Inline(l.values),
    }
}

/// The ids of one key, in increasing or decreasing order, read as needed.
pub enum Ids {
    /// Sorted, and from `pos` (forward) or before it (backward).
    Inline {
        ids: Vec<u64>,
        pos: usize,
        forward: bool,
    },
    Tree(Cursor),
}

impl Iterator for Ids {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        match self {
            Ids::Inline {
                ids,
                pos,
                forward: true,
            } => {
                let id = ids.get(*pos).copied();
                *pos += usize::from(id.is_some());
                id
            }
            Ids::Inline {
                ids,
                pos,
                forward: false,
            } => {
                *pos = pos.checked_sub(1)?;
                Some(ids[*pos])
            }
            Ids::Tree(c) => c.next(),
        }
    }
}

/// Lookups shared by a table and its readers.
trait Lookups {
    fn lookup(&self, k: u64, k2: u64) -> Option<Lookup>;
    /// The same, into `values`, returning the extra bit.
    fn lookup_into(&self, k: u64, k2: u64, values: &mut Vec<u64>) -> Option<bool>;
    fn postings(&self) -> &dyn Trees;

    fn entry<K: Key>(&self, key: K) -> Entry {
        let (k, k2) = key.parts();
        entry(self.lookup(k, k2))
    }

    /// The ids at `key` from the first at least `from` (forward), or from the
    /// last at most `from` (backward).
    fn ids<K: Key>(&self, key: K, from: u64, forward: bool) -> Ids {
        match self.entry(key) {
            Entry::None => Ids::Inline {
                ids: Vec::new(),
                pos: 0,
                forward,
            },
            Entry::Inline(mut ids) => {
                ids.sort_unstable();
                ids.dedup();
                let pos = if forward {
                    ids.partition_point(|&id| id < from)
                } else {
                    ids.partition_point(|&id| id <= from)
                };
                Ids::Inline { ids, pos, forward }
            }
            Entry::Promoted(h) => {
                let tree = self.postings().tree(h);
                Ids::Tree(if forward {
                    tree.seek(from)
                } else {
                    tree.seek_rev(from)
                })
            }
        }
    }

    /// Read into a buffer of the thread's, so that counting allocates
    /// nothing; the ids are sorted only if they aren't increasing already, as
    /// they are unless a key had ids inserted out of order or repeated.
    fn count<K: Key>(&self, key: K) -> u64 {
        thread_local! {
            static VALUES: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let (k, k2) = key.parts();
        VALUES.with(|values| {
            let mut fresh = Vec::new();
            let mut held = values.try_borrow_mut();
            let values = match &mut held {
                Ok(v) => &mut **v,
                Err(_) => &mut fresh,
            };
            match self.lookup_into(k, k2, values) {
                None => 0,
                Some(true) => self.postings().tree(values[0]).count(),
                Some(false) => distinct(values),
            }
        })
    }

    fn find_one<K: Key>(&self, key: K) -> Option<u64> {
        match self.entry(key) {
            Entry::None => None,
            Entry::Inline(ids) => ids.first().copied(),
            Entry::Promoted(h) => self.postings().tree(h).iter().next(),
        }
    }
}

/// The number of distinct ids of `values`, sorting them if they aren't
/// increasing.
fn distinct(values: &mut Vec<u64>) -> u64 {
    if !values.windows(2).all(|w| w[0] < w[1]) {
        values.sort_unstable();
        values.dedup();
    }
    values.len() as u64
}

/// Whether the bucket of `key` names the tree at `header`.
fn names(writer: &Writer, header: u64, (k, k2): (u64, u64)) -> bool {
    matches!(entry(writer.get(k, k2)), Entry::Promoted(h) if h == header)
}

/// Where a table's trees are read.
trait Trees {
    fn tree(&self, header: u64) -> postings::Tree;
}

impl Trees for Postings {
    fn tree(&self, header: u64) -> postings::Tree {
        Postings::tree(self, header)
    }
}

impl Trees for PostingsReader {
    fn tree(&self, header: u64) -> postings::Tree {
        PostingsReader::tree(self, header)
    }
}

/// The bounds of `range` as a first and last id.
fn bounds(range: impl RangeBounds<u64>) -> Option<(u64, u64)> {
    let lo = match range.start_bound() {
        Bound::Included(&x) => x,
        Bound::Excluded(&x) => x.checked_add(1)?,
        Bound::Unbounded => 0,
    };
    let hi = match range.end_bound() {
        Bound::Included(&x) => x,
        Bound::Excluded(&x) => x.checked_sub(1)?,
        Bound::Unbounded => u64::MAX,
    };
    (lo <= hi).then_some((lo, hi))
}

/// The methods a table and its readers share, written once.
macro_rules! lookups {
    () => {
        /// The set at `key`, empty if there is none.
        pub fn find<K: Key>(&self, key: K) -> IdSet {
            match self.iter(key) {
                // Sorted and distinct already.
                Ids::Inline {
                    ids,
                    pos: 0,
                    forward: true,
                } => IdSet::from_sorted(ids),
                ids => IdSet::from_sorted(ids.collect()),
            }
        }

        /// The number of ids at `key`; for a large set, without reading it.
        pub fn count<K: Key>(&self, key: K) -> u64 {
            Lookups::count(self, key)
        }

        /// The single id at `key` (or the first inserted, if it holds a set).
        pub fn find_one<K: Key>(&self, key: K) -> Option<u64> {
            Lookups::find_one(self, key)
        }

        /// The ids at `key`, increasing.
        pub fn iter<K: Key>(&self, key: K) -> Ids {
            self.ids(key, 0, true)
        }

        /// The ids at `key`, decreasing.
        pub fn iter_rev<K: Key>(&self, key: K) -> Ids {
            self.ids(key, u64::MAX, false)
        }

        /// The ids at `key` from the first at least `id`, increasing.
        pub fn seek<K: Key>(&self, key: K, id: u64) -> Ids {
            self.ids(key, id, true)
        }

        /// The ids at `key` from the last at most `id`, decreasing.
        pub fn seek_rev<K: Key>(&self, key: K, id: u64) -> Ids {
            self.ids(key, id, false)
        }

        /// The ids at `key` within `range`, increasing.
        pub fn range<K: Key>(
            &self,
            key: K,
            range: impl RangeBounds<u64>,
        ) -> impl Iterator<Item = u64> {
            let b = bounds(range);
            let (lo, hi) = b.unwrap_or((1, 0));
            self.seek(key, lo)
                .take_while(move |&id| b.is_some() && id <= hi)
        }

        /// The ids at `key` within `range`, decreasing.
        pub fn range_rev<K: Key>(
            &self,
            key: K,
            range: impl RangeBounds<u64>,
        ) -> impl Iterator<Item = u64> {
            let b = bounds(range);
            let (lo, hi) = b.unwrap_or((1, 0));
            self.seek_rev(key, hi)
                .take_while(move |&id| b.is_some() && id >= lo)
        }
    };
}

/// What [`LocalTable::compact`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Compacted {
    /// The keys whose ids were rebuilt.
    pub trees: u64,
    /// The ids dropped from them.
    pub dropped: u64,
}

impl std::ops::AddAssign for Compacted {
    fn add_assign(&mut self, o: Compacted) {
        self.trees += o.trees;
        self.dropped += o.dropped;
    }
}

/// The single writer of a table.
pub struct LocalTable {
    writer: Writer,
    postings: Postings,
    /// The header pages of keys found promoted.
    promoted: HashMap<(u64, u64), u64>,
    /// Whether a promotion flushes its tree before a bucket names it; see
    /// [`set_ordered`](Self::set_ordered).
    ordered: bool,
}

impl Lookups for LocalTable {
    fn lookup(&self, k: u64, k2: u64) -> Option<Lookup> {
        self.writer.get(k, k2)
    }

    fn lookup_into(&self, k: u64, k2: u64, values: &mut Vec<u64>) -> Option<bool> {
        self.writer.get_into(k, k2, values)
    }

    fn postings(&self) -> &dyn Trees {
        &self.postings
    }
}

impl LocalTable {
    /// Opens or creates the table whose files start with `prefix`, or an
    /// in-memory table if `prefix` is `None`.
    pub fn open(prefix: Option<&Path>, options: Options) -> Result<Self> {
        let mut t = LocalTable {
            writer: Writer::open(prefix, options)?,
            postings: Postings::open(prefix)?,
            promoted: HashMap::new(),
            ordered: true,
        };
        t.reclaim_unnamed();
        Ok(t)
    }

    /// Whether a promotion flushes its tree before a bucket names it, as it
    /// must unless a crash would have the table rebuilt.  It is by default.
    pub fn set_ordered(&mut self, ordered: bool) {
        self.ordered = ordered;
    }

    /// Frees the pages of every tree no bucket names: left by compaction,
    /// by keys set or deleted, or by a crash during promotion.
    fn reclaim_unnamed(&mut self) {
        let writer = &self.writer;
        self.postings
            .reclaim_unnamed(|h, key| names(writer, h, key));
    }

    /// A reader for looking up keys, on any thread, while this table changes.
    pub fn reader(&self) -> TableReader {
        TableReader {
            reader: self.writer.reader(),
            postings: self.postings.reader(),
        }
    }

    /// Adds `ids` to the set at `key`, promoting the set to a tree once it
    /// holds [`PROMOTE_AT`] ids.
    pub fn insert<K: Key>(&mut self, key: K, ids: impl IntoIterator<Item = u64>) -> Result<()> {
        let (k, k2) = key.parts();
        let ids: Vec<u64> = ids.into_iter().collect::<IdSet>().into_vec();
        if ids.is_empty() {
            return Ok(());
        }
        if ids[0] == 0 {
            return Err(Error::InvalidArgument("values must be non-zero"));
        }
        if let Some(&h) = self.promoted.get(&(k, k2)) {
            return Ok(self.postings.append(h, &ids)?);
        }
        let mut count = 0;
        for (i, chunk) in ids.chunks(MAX_VALUES).enumerate() {
            match self.writer.insert_unless_flagged(k, k2, chunk)? {
                Inserted::Values(n) => count = n,
                Inserted::Flagged(values) => {
                    let h = values[0];
                    self.promoted.insert((k, k2), h);
                    return Ok(self.postings.append(h, &ids[i * MAX_VALUES..])?);
                }
            }
        }
        if count >= PROMOTE_AT {
            self.promote(k, k2)?;
        }
        Ok(())
    }

    /// Moves the set at a key from its bucket to a tree.
    ///
    /// The tree is made durable before the bucket's records are replaced by
    /// one naming it, since the bucket may reach the disk at any time: a
    /// bucket never names a tree that a crash could lose, and a crash before
    /// the replacement leaves the key's records as they were.
    fn promote(&mut self, k: u64, k2: u64) -> Result<()> {
        let Entry::Inline(mut ids) = entry(self.writer.get(k, k2)) else {
            return Ok(());
        };
        ids.sort_unstable();
        ids.dedup();
        let h = self.postings.create(&ids)?;
        self.postings.set_key(h, (k, k2));
        if self.ordered {
            self.postings.sync()?;
        }
        self.writer.replace(k, k2, true, &[h])?;
        self.promoted.insert((k, k2), h);
        Ok(())
    }

    /// Makes `id` the single id at `key`.
    pub fn set<K: Key>(&mut self, key: K, id: u64) -> Result<()> {
        let (k, k2) = key.parts();
        self.promoted.remove(&(k, k2));
        self.writer.replace(k, k2, false, &[id])
    }

    /// Removes `key` and its ids.
    pub fn delete<K: Key>(&mut self, key: K) -> Result<()> {
        let (k, k2) = key.parts();
        self.promoted.remove(&(k, k2));
        self.writer.delete(k, k2)
    }

    lookups!();

    /// Rebuilds each promoted key's tree with only the ids `keep` accepts,
    /// returning the trees rebuilt and the ids dropped.  A key left with ids
    /// that fit one bucket record ([`MAX_VALUES`]) goes back to its bucket,
    /// replaced in one step so that readers see the old ids or the new, and
    /// one left with none is deleted.
    ///
    /// The new trees are made durable before the buckets name them, as in
    /// promotion.  Once the buckets are durable too, the old trees are
    /// [released](Postings::release), with those of keys set or deleted
    /// since they were promoted: their pages are reused once no reader
    /// made before can be reading them.
    pub fn compact(&mut self, mut keep: impl FnMut(u64) -> bool) -> Result<Compacted> {
        let mut done = Compacted::default();
        let mut changes = Vec::new();
        // Trees no bucket names: of keys set or deleted since.
        let mut unnamed = Vec::new();
        for (h, key) in self.postings.trees() {
            if key == (0, 0) {
                continue;
            }
            if !names(&self.writer, h, key) {
                unnamed.push(h);
                continue;
            }
            let tree = self.postings.tree(h);
            let kept: Vec<u64> = tree.iter().filter(|&id| keep(id)).collect();
            if kept.len() as u64 == tree.count() {
                continue;
            }
            done.trees += 1;
            done.dropped += tree.count() - kept.len() as u64;
            let new = if kept.len() > MAX_VALUES {
                let n = self.postings.create(&kept)?;
                self.postings.set_key(n, key);
                Ok(n)
            } else {
                Err(kept)
            };
            changes.push((key, h, new));
        }
        if changes.is_empty() && unnamed.is_empty() {
            return Ok(done);
        }
        if !changes.is_empty() {
            self.postings.sync()?;
        }
        for ((k, k2), old, new) in changes {
            match new {
                Ok(n) => {
                    self.writer.replace(k, k2, true, &[n])?;
                    self.promoted.insert((k, k2), n);
                }
                Err(ids) => {
                    if ids.is_empty() {
                        self.writer.delete(k, k2)?;
                    } else {
                        self.writer.replace(k, k2, false, &ids)?;
                    }
                    self.promoted.remove(&(k, k2));
                }
            }
            unnamed.push(old);
        }
        self.writer.sync()?;
        self.postings.release(&unnamed);
        Ok(done)
    }

    /// The id that this table's large entries have been compacted to; see
    /// [`compact`](Self::compact).
    pub fn horizon(&self) -> u64 {
        self.postings.horizon()
    }

    /// Records the horizon durably, before compacting to it.
    pub fn set_horizon(&mut self, horizon: u64) -> Result<()> {
        Ok(self.postings.set_horizon(horizon)?)
    }

    /// Does periodic maintenance; see [`Writer::tick`].
    pub fn tick(&mut self) -> Result<()> {
        self.writer.tick()
    }

    /// Flushes the table to disk: its postings first, so that a promoted key
    /// never names a tree that is not on disk.
    pub fn sync(&mut self) -> Result<()> {
        self.postings.sync()?;
        self.writer.sync()
    }

    /// Whether anything was written since the last [`sync`](Self::sync).
    pub fn is_dirty(&self) -> bool {
        self.writer.is_dirty() || self.postings.is_dirty()
    }

    /// Whether the directory needs flushing for this table's files.
    pub fn dir_dirty(&self) -> bool {
        self.writer.dir_dirty() || self.postings.dir_dirty()
    }

    /// Notes that the caller flushed the directory.
    pub fn dir_synced(&mut self) {
        self.writer.dir_synced();
        self.postings.dir_synced();
    }

    /// Writes a snapshot of the table to `sink`: the dynamic table's chunks
    /// (see [`Writer::snapshot`]), then the postings'.
    pub fn snapshot(&mut self, mut sink: impl FnMut(&[u8]) -> io::Result<()>) -> Result<()> {
        self.writer.snapshot(&mut sink)?;
        Ok(self.postings.snapshot(sink)?)
    }

    /// Creates a table at `prefix`, or in memory if `prefix` is `None`, from
    /// the chunks of a [`snapshot`](Self::snapshot).  A snapshot made before
    /// postings existed has none (`with_postings` false).
    pub fn restore(
        prefix: Option<&Path>,
        options: Options,
        mut source: impl FnMut() -> io::Result<Vec<u8>>,
        with_postings: bool,
    ) -> Result<Self> {
        let writer = Writer::restore(prefix, options, &mut source)?;
        let postings = if with_postings {
            Postings::restore(prefix, source)?
        } else {
            Postings::open(prefix)?
        };
        let mut t = LocalTable {
            writer,
            postings,
            promoted: HashMap::new(),
            ordered: true,
        };
        t.reclaim_unnamed();
        Ok(t)
    }

    /// The pages of the table's postings, and of those the pages free for
    /// reuse (or waiting for readers made before they were freed).
    pub fn postings_pages(&self) -> (u64, u64) {
        (self.postings.pages(), self.postings.free_pages())
    }
}

/// Looks up keys in a [`LocalTable`]; see [`LocalTable::reader`].
#[derive(Clone)]
pub struct TableReader {
    reader: Reader,
    postings: PostingsReader,
}

impl Lookups for TableReader {
    fn lookup(&self, k: u64, k2: u64) -> Option<Lookup> {
        self.reader.get(k, k2)
    }

    fn lookup_into(&self, k: u64, k2: u64, values: &mut Vec<u64>) -> Option<bool> {
        self.reader.get_into(k, k2, values)
    }

    fn postings(&self) -> &dyn Trees {
        &self.postings
    }
}

impl TableReader {
    lookups!();

    /// The horizon the table was last compacted to (see
    /// [`LocalTable::horizon`]).
    pub fn horizon(&self) -> u64 {
        self.postings.horizon()
    }
}

/// A table, or a reader of one: what a database's lookups read (see
/// [`Local::table`](crate::local::Local::table)).
#[derive(Clone, Copy)]
pub enum TableRef<'a> {
    Table(&'a LocalTable),
    Reader(&'a TableReader),
}

impl Lookups for TableRef<'_> {
    fn lookup(&self, k: u64, k2: u64) -> Option<Lookup> {
        match self {
            TableRef::Table(t) => t.lookup(k, k2),
            TableRef::Reader(r) => r.lookup(k, k2),
        }
    }

    fn lookup_into(&self, k: u64, k2: u64, values: &mut Vec<u64>) -> Option<bool> {
        match self {
            TableRef::Table(t) => t.lookup_into(k, k2, values),
            TableRef::Reader(r) => r.lookup_into(k, k2, values),
        }
    }

    fn postings(&self) -> &dyn Trees {
        match self {
            TableRef::Table(t) => Lookups::postings(*t),
            TableRef::Reader(r) => Lookups::postings(*r),
        }
    }
}

impl TableRef<'_> {
    lookups!();

    pub fn horizon(&self) -> u64 {
        match self {
            TableRef::Table(t) => t.horizon(),
            TableRef::Reader(r) => r.horizon(),
        }
    }

    /// A reader of the table, for any thread.
    pub fn reader(&self) -> TableReader {
        match self {
            TableRef::Table(t) => t.reader(),
            TableRef::Reader(r) => (*r).clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    /// A temporary directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("graqlite-lt-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn prefix(&self, name: &str) -> PathBuf {
            self.0.join(format!("{name}."))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn set(ids: &[u64]) -> IdSet {
        ids.iter().copied().collect()
    }

    #[test]
    fn id_sets_are_sorted_and_distinct() {
        let mut s = IdSet::from_unsorted(vec![9, 3, 7, 3, 1]);
        assert_eq!(s.as_slice(), [1, 3, 7, 9]);
        // Pushed in order, inserted out of it, or there already.
        assert!(s.insert(12));
        assert!(s.insert(5));
        assert!(!s.insert(7));
        assert_eq!(s.as_slice(), [1, 3, 5, 7, 9, 12]);
        assert!(s.contains(&5) && !s.contains(&6));
        assert!(s.remove(&3) && !s.remove(&3));
        assert_eq!((s.first(), s.last()), (Some(&1), Some(&12)));
        // Ranges of each kind of bound, either way.
        let r = |s: &IdSet, r: (Bound<u64>, Bound<u64>)| s.range(r).copied().collect::<Vec<_>>();
        use Bound::*;
        assert_eq!(r(&s, (Included(5), Excluded(9))), [5, 7]);
        assert_eq!(r(&s, (Excluded(5), Included(9))), [7, 9]);
        assert_eq!(r(&s, (Unbounded, Included(4))), [1]);
        assert_eq!(r(&s, (Included(13), Unbounded)), [] as [u64; 0]);
        assert_eq!(r(&s, (Included(9), Excluded(5))), [] as [u64; 0]);
        assert_eq!(s.range(..=9).next_back(), Some(&9));
        // Extended in order (appended) and out of it (merged).
        s.extend([20, 30]);
        assert_eq!(s.as_slice(), [1, 5, 7, 9, 12, 20, 30]);
        s.extend([2, 30, 8]);
        assert_eq!(s.as_slice(), [1, 2, 5, 7, 8, 9, 12, 20, 30]);
        let other = IdSet::from([2, 3, 30]);
        assert_eq!(s.intersection(&other).copied().collect::<Vec<_>>(), [2, 30]);
        s.retain(|&id| id % 2 == 0);
        assert_eq!(s.into_iter().collect::<Vec<_>>(), [2, 8, 12, 20, 30]);
    }

    #[test]
    fn counts_ids_inserted_out_of_order_once() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        t.insert(7u64, [5, 3]).unwrap();
        t.insert(7u64, [3, 9]).unwrap();
        assert_eq!(t.count(7u64), 3);
        assert_eq!(t.find(7u64).as_slice(), [3, 5, 9]);
        assert_eq!(t.reader().count(7u64), 3);
        assert_eq!(t.count(8u64), 0);
    }

    #[test]
    fn sets() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        assert!(t.find(1u64).is_empty());
        t.insert(1u64, [5, 3, 9]).unwrap();
        t.insert(1u64, [3, 1]).unwrap();
        t.insert(1u64, []).unwrap();
        assert_eq!(t.find(1u64), set(&[1, 3, 5, 9]));
        assert_eq!(t.count(1u64), 4);
        assert_eq!(t.reader().count(1u64), 4);

        // More than fit in one dynamic table operation, with duplicates.
        let many: Vec<u64> = (1..=100).rev().chain(1..=50).collect();
        t.insert(2u64, many).unwrap();
        assert_eq!(t.find(2u64), (1..=100).collect());
        assert_eq!(t.count(2u64), 100);
        assert_eq!(t.find_one(2u64), Some(1));

        t.delete(1u64).unwrap();
        assert!(t.find(1u64).is_empty());
        assert_eq!(t.find_one(1u64), None);
        assert_eq!(t.count(1u64), 0);

        assert!(matches!(
            t.insert(0u64, [1]),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            t.insert(3u64, [1, 0]),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn single() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        t.set(7u64, 70).unwrap();
        assert_eq!(t.find_one(7u64), Some(70));
        t.set(7u64, 71).unwrap();
        assert_eq!(t.find_one(7u64), Some(71));
        assert_eq!(t.find(7u64), set(&[71]));
        assert!(matches!(t.set(7u64, 0), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn uuid_keys() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        let u = Uuid::of(b"alice");
        t.set(u, 10).unwrap();
        t.insert(Uuid { uuid0: 4, uuid1: 1 }, [1, 2]).unwrap();
        t.insert(4u64, [3]).unwrap();
        assert_eq!(t.find_one(u), Some(10));
        assert_eq!(t.find(Uuid { uuid0: 4, uuid1: 1 }), set(&[1, 2]));
        assert_eq!(t.find(4u64), set(&[3]));
        // An id key is the uuid with a zero second half.
        assert_eq!(t.find(Uuid { uuid0: 4, uuid1: 0 }), set(&[3]));
    }

    #[test]
    fn reader() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        let r = t.reader();
        t.insert(1u64, [1, 2]).unwrap();
        t.set(2u64, 9).unwrap();
        std::thread::spawn(move || {
            assert_eq!(r.find(1u64), set(&[1, 2]));
            assert_eq!(r.find_one(2u64), Some(9));
            assert!(r.find(3u64).is_empty());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn persistent() {
        let dir = TempDir::new("persistent");
        let prefix = dir.prefix("index");
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        for k in 1..=2000u64 {
            t.insert(k, (1..=k % 20 + 1).map(|i| i * k)).unwrap();
            t.tick().unwrap();
        }
        t.set(Uuid::of(b"v"), 42).unwrap();
        t.sync().unwrap();
        drop(t);

        let check = |t: &LocalTable| {
            for k in 1..=2000u64 {
                assert_eq!(
                    t.find(k),
                    (1..=k % 20 + 1).map(|i| i * k).collect(),
                    "key {k}"
                );
            }
            assert_eq!(t.find_one(Uuid::of(b"v")), Some(42));
        };
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        check(&t);

        let mut chunks = VecDeque::new();
        t.snapshot(|c| {
            chunks.push_back(c.to_vec());
            Ok(())
        })
        .unwrap();
        let copy = dir.prefix("copy");
        let restored = LocalTable::restore(
            Some(&copy),
            Options::default(),
            || Ok(chunks.pop_front().unwrap_or_default()),
            true,
        )
        .unwrap();
        check(&restored);
        drop(t);
        remove(&prefix).unwrap();
        assert!(!exists(&prefix));
        assert!(exists(&copy));
    }

    /// Checks every lookup of `key` in `t` and its reader against `expected`.
    fn check_key(t: &LocalTable, key: u64, expected: &[u64]) {
        let r = t.reader();
        assert_eq!(t.find(key), expected.iter().copied().collect(), "key {key}");
        assert_eq!(t.count(key), expected.len() as u64);
        assert_eq!(r.count(key), expected.len() as u64);
        assert_eq!(t.find_one(key), expected.first().copied());
        assert_eq!(t.iter(key).collect::<Vec<_>>(), expected);
        assert_eq!(r.iter(key).collect::<Vec<_>>(), expected);
        let mut rev = expected.to_vec();
        rev.reverse();
        assert_eq!(t.iter_rev(key).collect::<Vec<_>>(), rev);
        if let (Some(&lo), Some(&hi)) = (expected.first(), expected.last()) {
            let mid = expected[expected.len() / 2];
            let want: Vec<u64> = expected.iter().copied().filter(|&id| id >= mid).collect();
            assert_eq!(t.seek(key, mid).collect::<Vec<_>>(), want);
            let want: Vec<u64> = expected
                .iter()
                .rev()
                .copied()
                .filter(|&id| id <= mid)
                .collect();
            assert_eq!(r.seek_rev(key, mid).collect::<Vec<_>>(), want);
            let want: Vec<u64> = expected
                .iter()
                .copied()
                .filter(|&id| id > lo && id < hi)
                .collect();
            assert_eq!(t.range(key, lo + 1..hi).collect::<Vec<_>>(), want);
            let mut want = want;
            want.reverse();
            assert_eq!(t.range_rev(key, lo + 1..hi).collect::<Vec<_>>(), want);
            assert_eq!(t.range(key, hi + 1..).count(), 0);
            assert_eq!(t.range(key, ..lo).count(), 0);
        }
    }

    #[test]
    fn promotion() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        // Ids added a few at a time, as indexing appends them, through the
        // promotion and past it.
        let mut expected = Vec::new();
        for i in 1..=5000u64 {
            t.insert(1u64, [i * 3]).unwrap();
            expected.push(i * 3);
            if [1, 2, 63, 64, 65, 100, 1000, 5000].contains(&i) {
                check_key(&t, 1, &expected);
            }
        }
        assert!(t.promoted.contains_key(&(1, 0)));
        // Ids already there are skipped, in batches too.
        t.insert(1u64, [3, 6, 15_000, 15_003]).unwrap();
        expected.push(15_003);
        check_key(&t, 1, &expected);
        // Many keys, some promoted, sharing buckets: each is still right.
        for k in 2..=300u64 {
            t.insert(k, (1..=k).map(|i| i * 7 + k)).unwrap();
        }
        for k in 2..=300u64 {
            let ids: Vec<u64> = (1..=k).map(|i| i * 7 + k).collect();
            check_key(&t, k, &ids);
        }
        // A promoted key can be replaced or deleted like any other.
        t.set(200u64, 5).unwrap();
        check_key(&t, 200, &[5]);
        t.delete(300u64).unwrap();
        check_key(&t, 300, &[]);
        // And a deleted key used again.
        t.insert(300u64, [4, 8]).unwrap();
        check_key(&t, 300, &[4, 8]);
    }

    #[test]
    fn promoted_keys_persist() {
        let dir = TempDir::new("promoted");
        let prefix = dir.prefix("index");
        let ids = |k: u64, n: u64| -> Vec<u64> { (1..=n).map(|i| i * 10 + k).collect() };
        {
            let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
            for k in 1..=40u64 {
                t.insert(k, ids(k, k * 10)).unwrap();
            }
            t.sync().unwrap();
            // Promoting key 1 makes its tree durable before its bucket names
            // it, so the bucket, which may reach the disk without a sync,
            // never names a tree a crash lost.
            t.insert(1u64, ids(1, 500)).unwrap();
            // Not synced: appended to the trees, then lost in the "crash" of
            // dropping without a sync.
            t.insert(1u64, ids(1, 900)).unwrap();
            t.insert(39u64, ids(39, 900)).unwrap();
        }
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        check_key(&t, 1, &ids(1, 500));
        for k in 2..=40u64 {
            check_key(&t, k, &ids(k, k * 10));
        }
        // Appending to a key promoted before the reopen.
        t.insert(40u64, ids(40, 1000)).unwrap();
        check_key(&t, 40, &ids(40, 1000));
        t.sync().unwrap();
        let mut chunks = VecDeque::new();
        t.snapshot(|c| {
            chunks.push_back(c.to_vec());
            Ok(())
        })
        .unwrap();
        let restored = LocalTable::restore(
            None,
            Options::default(),
            || Ok(chunks.pop_front().unwrap()),
            true,
        )
        .unwrap();
        check_key(&restored, 40, &ids(40, 1000));
        check_key(&restored, 7, &ids(7, 70));
        drop(t);
        remove(&prefix).unwrap();
        assert!(!postings::exists(&prefix));
    }

    #[test]
    fn compaction() {
        let dir = TempDir::new("compaction");
        let prefix = dir.prefix("index");
        let range = |a: u64, b: u64| -> Vec<u64> { (a..=b).collect() };
        let keep = |id: u64| match id {
            1..=1000 => id.is_multiple_of(2),
            2001..=2100 => id <= 2005,
            3001..=3100 => false,
            _ => true,
        };
        let evens: Vec<u64> = (1..=500).map(|i| i * 2).collect();
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        for (k, ids) in [
            (1u64, range(1, 1000)),
            (2, range(2001, 2100)),
            (3, range(3001, 3100)),
            (4, range(4001, 4100)),
            (5, range(5001, 5010)),
        ] {
            t.insert(k, ids).unwrap();
        }
        assert_eq!(t.horizon(), 0);
        t.set_horizon(4000).unwrap();
        // A reader of key 1 from before keeps reading its old tree.
        let old = t.reader().iter(1u64);
        let done = t.compact(keep).unwrap();
        assert_eq!(
            done,
            Compacted {
                trees: 3,
                dropped: 500 + 95 + 100
            }
        );
        assert_eq!(old.collect::<Vec<_>>(), range(1, 1000));
        let check = |t: &LocalTable| {
            check_key(t, 1, &evens);
            // Few enough ids for one record: back in the bucket.
            check_key(t, 2, &range(2001, 2005));
            assert!(matches!(t.entry(2u64), Entry::Inline(_)));
            check_key(t, 3, &[]);
            check_key(t, 4, &range(4001, 4100));
            check_key(t, 5, &range(5001, 5010));
        };
        check(&t);
        // Compacting again finds nothing more, skipping the old trees.
        assert_eq!(t.compact(keep).unwrap(), Compacted::default());
        drop(t);
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        check(&t);
        assert_eq!(t.horizon(), 4000);
        // The compacted keys grow again, and key 2 is promoted again.
        t.insert(1u64, [1002, 1004]).unwrap();
        t.insert(2u64, range(6001, 6100)).unwrap();
        t.insert(3u64, [7001]).unwrap();
        let mut ones = evens.clone();
        ones.extend([1002, 1004]);
        check_key(&t, 1, &ones);
        let mut twos = range(2001, 2005);
        twos.extend(range(6001, 6100));
        check_key(&t, 2, &twos);
        assert!(matches!(t.entry(2u64), Entry::Promoted(_)));
        check_key(&t, 3, &[7001]);
        drop(t);
        remove(&prefix).unwrap();
    }

    /// Adds ids `from..from + n` to key 1 of `t`, then compacts it to the
    /// last `keep` of them.
    fn grow_and_compact(t: &mut LocalTable, from: u64, n: u64, keep: u64) {
        t.insert(1u64, from..from + n).unwrap();
        let first = (from + n).saturating_sub(keep);
        t.compact(|id| id >= first).unwrap();
    }

    #[test]
    fn repeated_compaction_reuses_pages() {
        let dir = TempDir::new("reuse");
        let prefix = dir.prefix("index");
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        let mut next = 1;
        let mut pages = Vec::new();
        for _ in 0..40 {
            grow_and_compact(&mut t, next, 20_000, 30_000);
            next += 20_000;
            pages.push(t.postings_pages().0);
        }
        // A tree of the last 30 000 ids and the next 20 000, a new tree beside
        // it while compacting, and the pages freed in the round before.
        assert_eq!(pages[39], pages[9], "{pages:?}");
        check_key(&t, 1, &(next - 30_000..next).collect::<Vec<_>>());
        let len = fs::metadata(dir.0.join("index.postings.0")).unwrap().len();
        drop(t);
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        check_key(&t, 1, &(next - 30_000..next).collect::<Vec<_>>());
        grow_and_compact(&mut t, next, 20_000, 30_000);
        assert_eq!(t.postings_pages().0, pages[39]);
        assert_eq!(
            fs::metadata(dir.0.join("index.postings.0")).unwrap().len(),
            len
        );
    }

    #[test]
    fn a_reader_holds_back_reuse_of_the_pages_it_may_read() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        grow_and_compact(&mut t, 1, 20_000, 20_000);
        let before = t.reader();
        let old: Vec<u64> = (1..=20_000).collect();
        let mut cursor = t.reader().iter(1u64);
        assert_eq!(cursor.next(), Some(1));
        grow_and_compact(&mut t, 20_001, 20_000, 1_000);
        let freed = t.postings_pages().1;
        assert!(freed > 0);
        // A reader made since doesn't hold the freed pages.
        let after = t.reader();
        // Readers made before do: nothing is reused.
        let pages = t.postings_pages().0;
        grow_and_compact(&mut t, 40_001, 20_000, 20_000);
        assert!(t.postings_pages().0 > pages);
        assert!(t.postings_pages().1 >= freed);
        assert_eq!(cursor.collect::<Vec<_>>(), old[1..]);
        assert_eq!(before.count(1u64), 20_000);
        drop(before);
        let latest: Vec<u64> = (40_001..60_001).collect();
        assert_eq!(after.iter(1u64).collect::<Vec<_>>(), latest);
        // With them gone, the next tree reuses the freed pages.
        let (pages, free) = t.postings_pages();
        t.insert(1u64, 60_001..65_001).unwrap();
        assert_eq!(t.postings_pages().0, pages);
        assert!(t.postings_pages().1 < free);
        let mut all = latest;
        all.extend(60_001..65_001);
        check_key(&t, 1, &all);
        assert_eq!(after.iter(1u64).collect::<Vec<_>>(), all);
    }

    #[test]
    fn opening_frees_the_pages_of_unnamed_trees() {
        let dir = TempDir::new("unnamed");
        let prefix = dir.prefix("index");
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        t.insert(1u64, 1..=5_000).unwrap();
        t.insert(2u64, 1..=5_000).unwrap();
        t.insert(3u64, 1..=5_000).unwrap();
        let pages = t.postings_pages().0;
        // Key 2's tree is left unnamed by `set`, and key 3's by `delete`.
        t.set(2u64, 7).unwrap();
        t.delete(3u64).unwrap();
        t.sync().unwrap();
        assert_eq!(t.postings_pages(), (pages, 0));
        drop(t);
        let mut t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        let (pages, free) = t.postings_pages();
        assert!(
            free * 3 >= (pages - 1) * 2 - 2,
            "{pages} pages, {free} free"
        );
        check_key(&t, 1, &(1..=5_000).collect::<Vec<_>>());
        assert_eq!(t.find_one(2u64), Some(7));
        check_key(&t, 3, &[]);
        // New trees take the free pages.
        t.insert(4u64, 1..=5_000).unwrap();
        t.insert(5u64, 1..=5_000).unwrap();
        assert_eq!(t.postings_pages().0, pages);
        check_key(&t, 4, &(1..=5_000).collect::<Vec<_>>());
        check_key(&t, 5, &(1..=5_000).collect::<Vec<_>>());
        check_key(&t, 1, &(1..=5_000).collect::<Vec<_>>());
        drop(t);
        let t = LocalTable::open(Some(&prefix), Options::default()).unwrap();
        for k in [1u64, 4, 5] {
            check_key(&t, k, &(1..=5_000).collect::<Vec<_>>());
        }
    }

    #[test]
    fn compaction_releases_the_trees_of_keys_set_since() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        t.insert(1u64, 1..=5_000).unwrap();
        t.set(1u64, 9).unwrap();
        assert_eq!(t.postings_pages().1, 0);
        assert_eq!(t.compact(|_| true).unwrap(), Compacted::default());
        assert!(t.postings_pages().1 > 0);
        assert_eq!(t.find_one(1u64), Some(9));
    }

    #[test]
    fn reading_promoted_keys_while_inserting() {
        let mut t = LocalTable::open(None, Options::default()).unwrap();
        let r = t.reader();
        let done = std::sync::atomic::AtomicBool::new(false);
        let done = &done;
        std::thread::scope(|s| {
            s.spawn(move || {
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    // Whatever is read is a prefix of the ids, in order.
                    let got: Vec<u64> = r.iter(9u64).collect();
                    assert!(
                        got.iter()
                            .enumerate()
                            .all(|(i, &id)| id == (i as u64 + 1) * 2),
                        "{got:?}"
                    );
                    assert!(r.count(9u64) >= got.len() as u64);
                }
            });
            for i in 1..=20_000u64 {
                t.insert(9u64, [i * 2]).unwrap();
            }
            done.store(true, std::sync::atomic::Ordering::Release);
        });
        assert_eq!(t.count(9u64), 20_000);
    }
}
