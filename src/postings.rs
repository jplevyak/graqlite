//! Large index entries, as append-only B+ trees of ids in posting files.
//!
//! A [`LocalTable`](crate::local_table::LocalTable) key holding many ids is
//! *promoted* from its hash table bucket to a tree here (see issue 011).  A
//! key's ids only increase, since entries are appended in log order and ids
//! are never reused, so a tree grows only at its right edge: no splits in the
//! middle, and no page rewritten before its end.
//!
//! # Layout
//!
//! The postings of a table are pages of [`PAGE`] bytes in segment files,
//! `{prefix}postings.{n}`, each twice the size of the one before, mapped with
//! their whole size reserved so that pages never move.  Page 0 is the file
//! header: the magic, the version, the pages in use at the last sync, and the
//! [horizon](Postings::horizon) of the table's compaction.
//! Every page starts with its kind:
//!
//! - **A header page**, one per tree: its count of ids, last id, root page
//!   and height (0 when the root is a leaf), a durable copy of those four
//!   written at each [`sync`](Postings::sync), and the table key it holds
//!   the ids of (see [`set_key`](Postings::set_key)).
//! - **A leaf page:** its count of ids, its first id, the bytes of the
//!   varint deltas of its other ids, and its rank (the count of ids before
//!   it).
//! - **An internal page:** its count of children, then for each child its
//!   first id, its rank, and its page.
//!
//! # Concurrency and crashes
//!
//! There is one writer, and readers take no locks, reading every word
//! atomically.  The writer stores a tree's new pages and the words that
//! link them first, and its count last (a release); a reader loads the
//! count first (an acquire) and reads only that many ids.  Each sync flushes
//! the pages, then copies each changed tree's live words to its durable ones
//! and flushes again.  Opening a table resets every tree to its durable
//! words, so a crash leaves each tree as of its last sync: its pages up to
//! then are never written again, and what was written after is ignored, then
//! overwritten.
//!
//! # Compaction
//!
//! Compacting a key (see [`LocalTable::compact`](crate::local_table::LocalTable::compact))
//! builds it a new tree rather than changing the old one, which its readers
//! may still hold.  The old tree's pages are not yet reclaimed.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::mmap::{self, Mapping, suffixed};

/// The size of a page, in bytes.
pub const PAGE: u64 = 4096;
const WORDS: usize = (PAGE / 8) as usize;
const MAGIC: u64 = u64::from_le_bytes(*b"grqlpost");
const VERSION: u64 = 1;
/// The pages of the first segment; each later one has twice as many.
const FIRST_SEGMENT_PAGES: u64 = 256;
const MAX_SEGMENTS: usize = 40;

const KIND_HEADER: u64 = 1;
const KIND_LEAF: u64 = 2;
const KIND_INTERNAL: u64 = 3;

// The words of a header page.
const H_COUNT: usize = 1;
const H_LAST: usize = 2;
const H_ROOT: usize = 3;
const H_HEIGHT: usize = 4;
/// The durable copy of words 1 to 4.
const H_DURABLE: usize = 5;
/// The table key of the tree: its two halves.
const H_KEY: usize = 9;
const H_KEY2: usize = 10;

// The words of the file header page, after the magic and version.
const F_USED: usize = 2;
const F_HORIZON: usize = 3;

// The words of a leaf page.
const L_COUNT: usize = 1;
const L_FIRST: usize = 2;
const L_BYTES: usize = 3;
const L_RANK: usize = 4;
const LEAF_DATA: usize = 40;
const LEAF_CAPACITY: usize = PAGE as usize - LEAF_DATA;

// The words of an internal page.
const I_COUNT: usize = 1;
const I_ENTRIES: usize = 2;
const FANOUT: usize = (WORDS - I_ENTRIES) / 3;

fn segment_of(page: u64) -> usize {
    (63 - (page / FIRST_SEGMENT_PAGES + 1).leading_zeros()) as usize
}

fn segment_start(s: usize) -> u64 {
    FIRST_SEGMENT_PAGES * ((1u64 << s) - 1)
}

fn segment_pages(s: usize) -> u64 {
    FIRST_SEGMENT_PAGES << s
}

fn segment_path(prefix: &Path, s: usize) -> PathBuf {
    suffixed(prefix, &format!("postings.{s}"))
}

/// Whether postings exist at `prefix`.
pub fn exists(prefix: &Path) -> bool {
    segment_path(prefix, 0).exists()
}

/// Deletes the postings at `prefix`.
pub fn remove(prefix: &Path) -> io::Result<()> {
    for s in 0..MAX_SEGMENTS {
        match fs::remove_file(segment_path(prefix, s)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            r => r?,
        }
    }
    Ok(())
}

/// The mapped segments, shared with readers.
struct Shared {
    segments: [AtomicPtr<Mapping>; MAX_SEGMENTS],
}

impl Drop for Shared {
    fn drop(&mut self) {
        for s in &self.segments {
            let p = s.load(Ordering::Acquire);
            if !p.is_null() {
                // SAFETY: made by `Box::into_raw` and dropped once, here.
                drop(unsafe { Box::from_raw(p) });
            }
        }
    }
}

impl Shared {
    /// Word `w` of `page`, which must be in a mapped segment within its
    /// file.
    fn word(&self, page: u64, w: usize) -> &AtomicU64 {
        let s = segment_of(page);
        let map = self.segments[s].load(Ordering::Acquire);
        assert!(!map.is_null(), "posting page {page} is not mapped");
        let offset = (page - segment_start(s)) * PAGE + 8 * w as u64;
        // SAFETY: segments stay mapped while `self` lives, and the writer
        // extends a segment's file before handing out its pages.
        unsafe { (*map).word(offset) }
    }

    fn load(&self, page: u64, w: usize) -> u64 {
        self.word(page, w).load(Ordering::Acquire)
    }

    /// The bytes `range` of `page`, copied a word at a time.
    fn bytes(&self, page: u64, start: usize, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        let mut w = start / 8;
        while out.len() < len + start % 8 {
            out.extend_from_slice(&self.load(page, w).to_le_bytes());
            w += 1;
        }
        out.drain(..start % 8);
        out.truncate(len);
        out
    }

    /// The ids of the leaf at `page`, the first `n` of them.
    fn leaf_ids(&self, page: u64, n: u64) -> Vec<u64> {
        let mut ids = Vec::with_capacity(n as usize);
        if n == 0 {
            return ids;
        }
        let mut id = self.load(page, L_FIRST);
        ids.push(id);
        let len = self.load(page, L_BYTES) as usize;
        let bytes = self.bytes(page, LEAF_DATA, len.min(LEAF_CAPACITY));
        let mut pos = 0;
        while (ids.len() as u64) < n {
            let Some((delta, used)) = read_varint(&bytes[pos..]) else {
                break;
            };
            pos += used;
            id += delta;
            ids.push(id);
        }
        ids
    }

    /// Entry `i` of the internal `page`: its first id, rank and child.
    fn entry(&self, page: u64, i: usize) -> (u64, u64, u64) {
        let w = I_ENTRIES + 3 * i;
        (
            self.load(page, w),
            self.load(page, w + 1),
            self.load(page, w + 2),
        )
    }

    /// The tree whose header is at `header`, as it was published.
    fn tree(self: &Arc<Self>, header: u64) -> Tree {
        let count = self.load(header, H_COUNT);
        Tree {
            shared: Arc::clone(self),
            count,
            root: self.load(header, H_ROOT),
            height: self.load(header, H_HEIGHT),
        }
    }
}

fn write_varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in bytes.iter().enumerate().take(10) {
        v |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            return Some((v, i + 1));
        }
    }
    None
}

/// The writer's view of the right edge of a tree, where it grows.
struct Spine {
    count: u64,
    first: u64,
    last: u64,
    root: u64,
    height: u64,
    leaf: u64,
    leaf_count: u64,
    /// The deltas written to the leaf so far.
    leaf_bytes: Vec<u8>,
    /// The rightmost internal pages, from the leaf's parent up to the root,
    /// and their counts of children.
    path: Vec<(u64, usize)>,
}

/// The posting trees of a table, for the single writer.
pub struct Postings {
    shared: Arc<Shared>,
    prefix: Option<PathBuf>,
    files: Vec<File>,
    /// The bytes allocated in each segment's file.
    allocated: Vec<u64>,
    /// The pages handed out, including the file header.
    used: u64,
    spines: HashMap<u64, Spine>,
    /// The header pages changed since the last sync.
    dirty: BTreeSet<u64>,
    /// Every header page, in order.
    headers: Vec<u64>,
}

impl Postings {
    /// Opens or creates the postings at `prefix`, or in memory, resetting
    /// each tree to its durable state.
    pub fn open(prefix: Option<&Path>) -> io::Result<Self> {
        let mut p = Postings {
            shared: Arc::new(Shared {
                segments: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            }),
            prefix: prefix.map(Path::to_path_buf),
            files: Vec::new(),
            allocated: Vec::new(),
            used: 1,
            spines: HashMap::new(),
            dirty: BTreeSet::new(),
            headers: Vec::new(),
        };
        let existing = prefix.is_some_and(exists);
        p.open_segment(0, existing)?;
        if !existing {
            p.ensure(0)?;
            p.store(0, 0, MAGIC);
            p.store(0, 1, VERSION);
            p.store(0, F_USED, 1);
            return Ok(p);
        }
        if p.shared.load(0, 0) != MAGIC || p.shared.load(0, 1) != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad postings magic or version",
            ));
        }
        p.used = p.shared.load(0, F_USED).max(1);
        for s in 1..MAX_SEGMENTS {
            if segment_start(s) >= p.used {
                break;
            }
            p.open_segment(s, true)?;
        }
        // Reset each tree to its durable state.
        for page in 1..p.used {
            if p.shared.load(page, 0) == KIND_HEADER {
                for w in 0..4 {
                    let durable = p.shared.load(page, H_DURABLE + w);
                    p.store(page, H_COUNT + w, durable);
                }
                p.headers.push(page);
            }
        }
        Ok(p)
    }

    /// A reader of these postings, for any thread.
    pub fn reader(&self) -> PostingsReader {
        PostingsReader {
            shared: Arc::clone(&self.shared),
        }
    }

    fn open_segment(&mut self, s: usize, existing: bool) -> io::Result<()> {
        let file = match &self.prefix {
            Some(prefix) => OpenOptions::new()
                .read(true)
                .write(true)
                .create(!existing)
                .truncate(false)
                .open(segment_path(prefix, s))?,
            None => mmap::ephemeral_file(&format!("graqlite-postings-{s}"))?,
        };
        let len = file.metadata()?.len();
        let map = Mapping::new(&file, segment_pages(s) * PAGE)?;
        self.shared.segments[s].store(Box::into_raw(Box::new(map)), Ordering::Release);
        while self.files.len() <= s {
            self.files.push(file.try_clone()?);
            self.allocated.push(0);
        }
        self.files[s] = file;
        self.allocated[s] = len;
        Ok(())
    }

    /// Makes `page` writable: its segment mapped and its file long enough.
    fn ensure(&mut self, page: u64) -> io::Result<()> {
        let s = segment_of(page);
        if s >= MAX_SEGMENTS {
            return Err(io::Error::other("postings are full"));
        }
        if self.shared.segments[s].load(Ordering::Acquire).is_null() {
            self.open_segment(s, false)?;
        }
        let needed = (page - segment_start(s) + 1) * PAGE;
        if needed > self.allocated[s] {
            let grow = (self.allocated[s] / 4).max(64 * PAGE);
            let len = (needed.max(self.allocated[s] + grow)).min(segment_pages(s) * PAGE);
            mmap::allocate(&self.files[s], len)?;
            self.allocated[s] = len;
        }
        Ok(())
    }

    fn store(&self, page: u64, w: usize, v: u64) {
        self.shared.word(page, w).store(v, Ordering::Release);
    }

    /// A new page, zeroed, of `kind`.
    fn allocate(&mut self, kind: u64) -> io::Result<u64> {
        let page = self.used;
        self.ensure(page)?;
        self.used += 1;
        for w in 1..WORDS {
            self.store(page, w, 0);
        }
        self.store(page, 0, kind);
        Ok(page)
    }

    /// Stores the bytes `from..` of `bytes` into the leaf at `page`, at their
    /// offsets in its data, a word at a time.
    fn store_leaf_bytes(&self, page: u64, bytes: &[u8], from: usize) {
        if from >= bytes.len() {
            return;
        }
        let first = (LEAF_DATA + from) / 8;
        let last = (LEAF_DATA + bytes.len()).div_ceil(8);
        for w in first..last {
            let mut word = [0u8; 8];
            for (i, b) in word.iter_mut().enumerate() {
                let pos = w * 8 + i;
                if pos >= LEAF_DATA && pos - LEAF_DATA < bytes.len() {
                    *b = bytes[pos - LEAF_DATA];
                }
            }
            self.store(page, w, u64::from_le_bytes(word));
        }
    }

    /// A new leaf holding `id`, at `rank`.
    fn new_leaf(&mut self, id: u64, rank: u64) -> io::Result<u64> {
        let leaf = self.allocate(KIND_LEAF)?;
        self.store(leaf, L_FIRST, id);
        self.store(leaf, L_RANK, rank);
        self.store(leaf, L_COUNT, 1);
        Ok(leaf)
    }

    /// A new tree of `ids`, which must be increasing and not empty, returning
    /// its header page.
    pub fn create(&mut self, ids: &[u64]) -> io::Result<u64> {
        assert!(!ids.is_empty(), "a posting tree needs an id");
        let first = ids[0];
        let leaf = self.new_leaf(first, 0)?;
        let mut spine = Spine {
            count: 1,
            first,
            last: first,
            root: leaf,
            height: 0,
            leaf,
            leaf_count: 1,
            leaf_bytes: Vec::new(),
            path: Vec::new(),
        };
        self.extend(&mut spine, &ids[1..])?;
        let header = self.allocate(KIND_HEADER)?;
        self.publish(header, &spine);
        self.spines.insert(header, spine);
        self.headers.push(header);
        Ok(header)
    }

    /// Records that the tree at `header` holds the ids of the table key
    /// `(k, k2)`, so that [`trees`](Self::trees) can name it.  It is
    /// durable from the next [`sync`](Self::sync).
    pub fn set_key(&mut self, header: u64, (k, k2): (u64, u64)) {
        self.store(header, H_KEY, k);
        self.store(header, H_KEY2, k2);
    }

    /// Every tree made, by header page, with the key recorded for it, or
    /// `(0, 0)` if none was.  Trees left by [compaction](crate::local_table::LocalTable::compact)
    /// are here too: a key's tree is the one its bucket names.
    pub fn trees(&self) -> Vec<(u64, (u64, u64))> {
        self.headers
            .iter()
            .map(|&h| (h, (self.shared.load(h, H_KEY), self.shared.load(h, H_KEY2))))
            .collect()
    }

    /// Stops tracking the right edge of the tree at `header`, which is no
    /// longer appended to.  Its pages stay as they are, for its readers.
    pub fn forget(&mut self, header: u64) {
        self.spines.remove(&header);
    }

    /// The id that ids at or before have been compacted to, or 0.
    pub fn horizon(&self) -> u64 {
        self.shared.load(0, F_HORIZON)
    }

    /// Records `horizon` (see [`horizon`](Self::horizon)) durably.
    pub fn set_horizon(&mut self, horizon: u64) -> io::Result<()> {
        self.store(0, F_HORIZON, horizon);
        self.flush()
    }

    /// Appends `ids`, which must be increasing, to the tree at `header`; ids
    /// not past its last id are already in it, and are skipped.
    pub fn append(&mut self, header: u64, ids: &[u64]) -> io::Result<()> {
        let mut spine = match self.spines.remove(&header) {
            Some(s) => s,
            None => self.spine(header),
        };
        let result = self.extend(&mut spine, ids);
        self.publish(header, &spine);
        self.spines.insert(header, spine);
        result
    }

    /// Adds `ids` at the right edge of `spine`'s tree, without publishing.
    fn extend(&mut self, spine: &mut Spine, ids: &[u64]) -> io::Result<()> {
        let mut written = spine.leaf_bytes.len();
        for &id in ids {
            if id <= spine.last {
                continue;
            }
            let delta = id - spine.last;
            if spine.leaf_bytes.len() + varint_len(delta) <= LEAF_CAPACITY {
                write_varint(delta, &mut spine.leaf_bytes);
                spine.leaf_count += 1;
            } else {
                // Finish the full leaf, then start the next.
                self.finish_leaf(spine, written);
                let leaf = self.new_leaf(id, spine.count)?;
                self.add_child(spine, 0, (id, spine.count, leaf))?;
                spine.leaf = leaf;
                spine.leaf_count = 1;
                spine.leaf_bytes.clear();
                written = 0;
            }
            spine.count += 1;
            spine.last = id;
        }
        self.finish_leaf(spine, written);
        Ok(())
    }

    /// Stores the leaf's bytes from `written` on, and its counts.
    fn finish_leaf(&self, spine: &Spine, written: usize) {
        self.store_leaf_bytes(spine.leaf, &spine.leaf_bytes, written);
        self.store(spine.leaf, L_BYTES, spine.leaf_bytes.len() as u64);
        self.store(spine.leaf, L_COUNT, spine.leaf_count);
    }

    /// Adds `child` to the rightmost internal page at `level` (0: the leaves'
    /// parents), adding pages and a root as needed.
    fn add_child(
        &mut self,
        spine: &mut Spine,
        level: usize,
        child: (u64, u64, u64),
    ) -> io::Result<()> {
        if level == spine.path.len() {
            // The child's level is the root's: a new root above them both.
            let root = self.allocate(KIND_INTERNAL)?;
            self.store_entry(root, 0, (spine.first, 0, spine.root));
            self.store_entry(root, 1, child);
            self.store(root, I_COUNT, 2);
            spine.path.push((root, 2));
            spine.root = root;
            spine.height += 1;
            return Ok(());
        }
        let (page, n) = spine.path[level];
        if n < FANOUT {
            self.store_entry(page, n, child);
            self.store(page, I_COUNT, (n + 1) as u64);
            spine.path[level].1 = n + 1;
            return Ok(());
        }
        let page = self.allocate(KIND_INTERNAL)?;
        self.store_entry(page, 0, child);
        self.store(page, I_COUNT, 1);
        spine.path[level] = (page, 1);
        self.add_child(spine, level + 1, (child.0, child.1, page))
    }

    fn store_entry(&self, page: u64, i: usize, (first, rank, child): (u64, u64, u64)) {
        let w = I_ENTRIES + 3 * i;
        self.store(page, w, first);
        self.store(page, w + 1, rank);
        self.store(page, w + 2, child);
    }

    /// Publishes `spine`'s tree at `header`: its root and height, then its
    /// count and last id.
    fn publish(&mut self, header: u64, spine: &Spine) {
        self.store(header, H_ROOT, spine.root);
        self.store(header, H_HEIGHT, spine.height);
        self.store(header, H_LAST, spine.last);
        self.store(header, H_COUNT, spine.count);
        self.dirty.insert(header);
    }

    /// The right edge of the tree at `header`, read from its pages, with
    /// the words past what is published (left by a crash) corrected.
    fn spine(&mut self, header: u64) -> Spine {
        let s = &self.shared;
        let (count, last, root, height) = (
            s.load(header, H_COUNT),
            s.load(header, H_LAST),
            s.load(header, H_ROOT),
            s.load(header, H_HEIGHT),
        );
        let mut path = Vec::new();
        let mut page = root;
        let mut first = None;
        for _ in 0..height {
            let stored = s.load(page, I_COUNT) as usize;
            // The children within the published count.
            let n = (0..stored.min(FANOUT))
                .take_while(|&i| s.entry(page, i).1 < count)
                .count()
                .max(1);
            first.get_or_insert(s.entry(page, 0).0);
            path.push((page, n));
            page = s.entry(page, n - 1).2;
        }
        let leaf = page;
        let rank = s.load(leaf, L_RANK);
        let leaf_count = count - rank;
        let ids = s.leaf_ids(leaf, leaf_count);
        let mut leaf_bytes = Vec::new();
        for w in ids.windows(2) {
            write_varint(w[1] - w[0], &mut leaf_bytes);
        }
        let first = first.unwrap_or(s.load(leaf, L_FIRST));
        path.reverse();
        // Correct what a crash may have left past the published state.
        for &(page, n) in &path {
            self.store(page, I_COUNT, n as u64);
        }
        self.store(leaf, L_BYTES, leaf_bytes.len() as u64);
        self.store(leaf, L_COUNT, leaf_count);
        Spine {
            count,
            first,
            last,
            root,
            height,
            leaf,
            leaf_count,
            leaf_bytes,
            path,
        }
    }

    /// The pages handed out.
    pub fn pages(&self) -> u64 {
        self.used
    }

    /// Flushes the postings, then records each changed tree's state as
    /// durable, and flushes that.
    pub fn sync(&mut self) -> io::Result<()> {
        self.flush()?;
        for &header in &self.dirty {
            for w in 0..4 {
                let live = self.shared.load(header, H_COUNT + w);
                self.store(header, H_DURABLE + w, live);
            }
        }
        self.store(0, F_USED, self.used);
        self.flush()?;
        self.dirty.clear();
        if let Some(p) = &self.prefix {
            mmap::sync_dir(p)?;
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        for (s, &len) in self.allocated.iter().enumerate() {
            let map = self.shared.segments[s].load(Ordering::Acquire);
            if !map.is_null() && len > 0 {
                // SAFETY: mapped while `self.shared` lives.
                unsafe { (*map).sync(len)? };
            }
        }
        Ok(())
    }

    /// Writes the postings to `sink` as chunks: a header of the magic, the
    /// version and the pages, then the pages, at most 256 to a chunk.
    pub fn snapshot(&self, mut sink: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        let mut header = Vec::new();
        for w in [MAGIC, VERSION, self.used] {
            header.extend_from_slice(&w.to_le_bytes());
        }
        sink(&header)?;
        let mut page = 0;
        while page < self.used {
            let end = (page + 256).min(self.used);
            let mut chunk = Vec::with_capacity(((end - page) * PAGE) as usize);
            for p in page..end {
                for w in 0..WORDS {
                    chunk.extend_from_slice(&self.shared.load(p, w).to_le_bytes());
                }
            }
            sink(&chunk)?;
            page = end;
        }
        Ok(())
    }

    /// Creates postings at `prefix`, or in memory, from the chunks of a
    /// [`snapshot`](Self::snapshot), returned in order by `source`.
    pub fn restore(
        prefix: Option<&Path>,
        mut source: impl FnMut() -> io::Result<Vec<u8>>,
    ) -> io::Result<Self> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
        let header = source()?;
        if header.len() != 24 {
            return Err(invalid("postings snapshot header"));
        }
        let word = |i: usize| u64::from_le_bytes(header[i * 8..i * 8 + 8].try_into().unwrap());
        if word(0) != MAGIC || word(1) != VERSION {
            return Err(invalid("bad postings magic or version"));
        }
        let used = word(2);
        let mut p = Postings::open(prefix)?;
        let mut page = 0;
        while page < used {
            let chunk = source()?;
            let pages = chunk.len() as u64 / PAGE;
            if !(chunk.len() as u64).is_multiple_of(PAGE) || pages == 0 || page + pages > used {
                return Err(invalid("postings snapshot chunk"));
            }
            for (i, words) in chunk.as_chunks::<8>().0.iter().enumerate() {
                let (pg, w) = (page + i as u64 / WORDS as u64, i % WORDS);
                p.ensure(pg)?;
                p.store(pg, w, u64::from_le_bytes(*words));
            }
            page += pages;
        }
        p.used = used.max(1);
        // The restored state is consistent: make it durable.
        for page in 1..p.used {
            if p.shared.load(page, 0) == KIND_HEADER {
                p.dirty.insert(page);
                p.headers.push(page);
            }
        }
        p.sync()?;
        Ok(p)
    }
}

/// Reads the posting trees of a table, on any thread.
#[derive(Clone)]
pub struct PostingsReader {
    shared: Arc<Shared>,
}

impl PostingsReader {
    /// The tree whose header is at `header`.
    pub fn tree(&self, header: u64) -> Tree {
        self.shared.tree(header)
    }

    /// The horizon the table was last compacted to (see
    /// [`Postings::horizon`]).
    pub fn horizon(&self) -> u64 {
        self.shared.load(0, F_HORIZON)
    }
}

impl Postings {
    /// The tree whose header is at `header`.
    pub fn tree(&self, header: u64) -> Tree {
        self.shared.tree(header)
    }
}

/// A tree as it was published when read: its ids are fixed.
pub struct Tree {
    shared: Arc<Shared>,
    count: u64,
    root: u64,
    height: u64,
}

impl Tree {
    /// The number of ids.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The ids in increasing order.
    pub fn iter(&self) -> Cursor {
        self.cursor(0, true)
    }

    /// The ids in decreasing order.
    pub fn iter_rev(&self) -> Cursor {
        self.cursor(u64::MAX, false)
    }

    /// The ids from the first at least `id`, increasing.
    pub fn seek(&self, id: u64) -> Cursor {
        self.cursor(id, true)
    }

    /// The ids from the last at most `id`, decreasing.
    pub fn seek_rev(&self, id: u64) -> Cursor {
        self.cursor(id, false)
    }

    fn cursor(&self, target: u64, forward: bool) -> Cursor {
        let mut c = Cursor {
            shared: Arc::clone(&self.shared),
            count: self.count,
            forward,
            stack: Vec::new(),
            leaf: Vec::new(),
            pos: 0,
        };
        if self.count == 0 {
            return c;
        }
        // Descend to the leaf that would hold `target`.
        let mut page = self.root;
        for _ in 0..self.height {
            let n = c.children(page);
            let i = (0..n)
                .rev()
                .find(|&i| c.shared.entry(page, i).0 <= target)
                .unwrap_or(0);
            c.stack.push((page, i, n));
            page = c.shared.entry(page, i).2;
        }
        c.load_leaf(page);
        if forward {
            c.pos = c.leaf.partition_point(|&id| id < target);
        } else {
            c.pos = c.leaf.partition_point(|&id| id <= target);
        }
        c
    }
}

/// Ids of a tree, in one direction, read a leaf at a time.
pub struct Cursor {
    shared: Arc<Shared>,
    count: u64,
    forward: bool,
    /// The internal pages above the leaf: each page, the child taken, and
    /// its children within the count.
    stack: Vec<(u64, usize, usize)>,
    /// The leaf's ids within the count.
    leaf: Vec<u64>,
    /// Forward, the next id's index; backward, one past it.
    pos: usize,
}

impl Cursor {
    /// The children of the internal `page` within the count.
    fn children(&self, page: u64) -> usize {
        let stored = (self.shared.load(page, I_COUNT) as usize).min(FANOUT);
        let mut n = 0;
        while n < stored && self.shared.entry(page, n).1 < self.count {
            n += 1;
        }
        n.max(1)
    }

    fn load_leaf(&mut self, page: u64) {
        let rank = self.shared.load(page, L_RANK);
        self.leaf = self.shared.leaf_ids(page, self.count.saturating_sub(rank));
    }

    /// Moves to the next leaf in the cursor's direction, if there is one.
    fn next_leaf(&mut self) -> bool {
        // Up to the first page with a sibling to take.
        let mut depth = self.stack.len();
        loop {
            if depth == 0 {
                return false;
            }
            let (_, i, n) = self.stack[depth - 1];
            let next = if self.forward {
                (i + 1 < n).then_some(i + 1)
            } else {
                i.checked_sub(1)
            };
            if let Some(next) = next {
                self.stack[depth - 1].1 = next;
                break;
            }
            depth -= 1;
        }
        self.stack.truncate(depth);
        // Down the nearest edge to a leaf.
        let (page, i, _) = self.stack[depth - 1];
        let mut child = self.shared.entry(page, i).2;
        while self.shared.load(child, 0) == KIND_INTERNAL {
            let n = self.children(child);
            let i = if self.forward { 0 } else { n - 1 };
            self.stack.push((child, i, n));
            child = self.shared.entry(child, i).2;
        }
        self.load_leaf(child);
        self.pos = if self.forward { 0 } else { self.leaf.len() };
        true
    }
}

impl Iterator for Cursor {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        loop {
            if self.forward {
                if self.pos < self.leaf.len() {
                    self.pos += 1;
                    return Some(self.leaf[self.pos - 1]);
                }
            } else if self.pos > 0 {
                self.pos -= 1;
                return Some(self.leaf[self.pos]);
            }
            if !self.next_leaf() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests;
