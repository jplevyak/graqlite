//! A local graph database: a [`LocalRecordLog`] of entries plus a number of
//! [`LocalTable`] indexes, committed together.
//!
//! # Files
//!
//! A database at `prefix` is the log at `{prefix}record_log.` and the index
//! tables at `{prefix}index0.`, `{prefix}index1.`, ...  With no prefix the
//! database is in memory.
//!
//! # Commits
//!
//! [`commit`](Local::commit) runs the log's two-phase commit with the indexes
//! flushed in between, so a completed commit's index entries are on disk.  If
//! a crash interrupts a commit, opening the database abandons it, since its
//! index entries may not be.
//!
//! An index may refer to entries that were never committed: its pages can
//! reach disk at any time.  The log never reuses an id, so such a reference
//! names a filler node rather than a different entry (see
//! [`local_record_log`](crate::local_record_log#ids-are-never-reused)).
//!
//! # Durability
//!
//! [`Options::synchronous`] says what a commit waits for.  Under
//! [`Normal`](Synchronous::Normal) and [`Off`](Synchronous::Off) a commit
//! leaves the indexes to reach the disk when they do, and marks them stale
//! in the log's metadata with its first such commit; dropping the database
//! flushes them and clears the mark.
//!
//! An index whose pages reached the disk in no particular order can't be
//! repaired by replaying the entries since some earlier point: a bucket
//! may link to one that was lost, or a level's count of buckets be older
//! than the buckets.  So opening a database marked stale deletes its index
//! files, and the caller rebuilds them from the whole log and calls
//! [`indexes_rebuilt`](Local::indexes_rebuilt), as
//! [`Graph::open`](crate::graph::Graph::open) does for the built-in indexes.
//! A crash during that leaves the mark, and the next open starts again.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::dynamic_table;
use crate::local_record_log::{self, LocalRecordLog};
use crate::local_table::{self, LocalTable, TableReader, TableRef};
use crate::mmap::{self, suffixed};
use crate::tuple_index::NUM_BUILTIN_INDEXES;

/// Rough sizes for preallocating a database from its expected tuple count.
const LOG_BYTES_PER_TUPLE: u64 = 30;
const INDEX_BYTES_PER_TUPLE: u64 = 10;

const SNAPSHOT_MAGIC: u64 = u64::from_le_bytes(*b"grql-snp");
/// Version 2 adds each index's postings; version 1 snapshots still restore.
const SNAPSHOT_VERSION: u64 = 2;
/// The largest snapshot frame accepted, well above any component's chunks.
const MAX_FRAME: u64 = 1 << 24;

/// Errors from a local database.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Log(local_record_log::Error),
    Index(dynamic_table::Error),
    /// A snapshot is not a valid local database snapshot.
    InvalidFormat(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Log(e) => write!(f, "record log: {e}"),
            Error::Index(e) => write!(f, "index: {e}"),
            Error::InvalidFormat(what) => write!(f, "invalid snapshot: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Log(e) => Some(e),
            Error::Index(e) => Some(e),
            Error::InvalidFormat(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<local_record_log::Error> for Error {
    fn from(e: local_record_log::Error) -> Self {
        Error::Log(e)
    }
}

impl From<dynamic_table::Error> for Error {
    fn from(e: dynamic_table::Error) -> Self {
        Error::Index(e)
    }
}

/// What a [commit](Local::commit) waits for, as SQLite's `synchronous`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Synchronous {
    /// The log and the indexes are flushed: a commit survives losing power,
    /// and opening the database costs nothing more after a crash.
    #[default]
    Full,
    /// The log is flushed and the indexes are not.  A commit survives
    /// losing power, but opening the database after a crash rebuilds the
    /// indexes from the log.
    Normal,
    /// Nothing is flushed, for bulk loads and tests.  A commit survives the
    /// process being killed, with the indexes rebuilt as under `Normal`, but
    /// losing power may lose or corrupt the database.
    Off,
}

impl Synchronous {
    /// The level called `name`: `full`, `normal` or `off`.
    pub fn of_name(name: &str) -> Option<Self> {
        match name {
            "full" => Some(Synchronous::Full),
            "normal" => Some(Synchronous::Normal),
            "off" => Some(Synchronous::Off),
            _ => None,
        }
    }
}

/// Options for opening a database.
#[derive(Debug, Clone)]
pub struct Options {
    pub log: local_record_log::Options,
    /// The options for each index.
    pub index: dynamic_table::Options,
    /// The number of index tables a database has at least: an existing
    /// database with fewer is given more.
    pub tables: usize,
    /// How much history, in milliseconds, a graph's large index entries keep
    /// when [`Graph::tick`](crate::graph::Graph::tick) compacts them; `None`
    /// keeps it all, so every `as of` can be answered.
    pub history: Option<u64>,
    /// What a commit waits for.
    pub synchronous: Synchronous,
}

impl Default for Options {
    /// Room for the [built-in indexes](crate::tuple_index::BuiltinIndex).
    fn default() -> Self {
        Options {
            log: local_record_log::Options::default(),
            index: dynamic_table::Options::default(),
            tables: NUM_BUILTIN_INDEXES,
            history: None,
            synchronous: Synchronous::Full,
        }
    }
}

impl Options {
    /// Options preallocating room for about `tuples` tuples.
    pub fn for_tuples(tuples: u64) -> Self {
        let mut options = Options::default();
        options.log.create_size = tuples.saturating_mul(LOG_BYTES_PER_TUPLE);
        options.index.create_size = tuples.saturating_mul(INDEX_BYTES_PER_TUPLE);
        options
    }
}

fn log_prefix(prefix: &Path) -> PathBuf {
    suffixed(prefix, "record_log.")
}

fn index_prefix(prefix: &Path, i: usize) -> PathBuf {
    suffixed(prefix, &format!("index{i}."))
}

/// Whether a database exists at `prefix`.
pub fn exists(prefix: &Path) -> bool {
    local_record_log::exists(&log_prefix(prefix))
}

/// Deletes the database at `prefix`.
pub fn remove(prefix: &Path) -> io::Result<()> {
    local_record_log::remove(&log_prefix(prefix))?;
    for i in 0.. {
        let p = index_prefix(prefix, i);
        if !local_table::exists(&p) {
            break;
        }
        local_table::remove(&p)?;
    }
    Ok(())
}

/// A local graph database; see the [module](self) docs.
pub struct Local {
    prefix: Option<PathBuf>,
    options: Options,
    log: LocalRecordLog,
    indexes: Vec<LocalTable>,
    /// A reader's (see [`reader`](Local::reader)) readers of the tables, in
    /// place of `indexes`.
    readers: Vec<TableReader>,
    /// Whether this session's commits left the indexes to be flushed when
    /// it is dropped.
    unsynced: bool,
}

impl Drop for Local {
    /// Flushes indexes that commits left unflushed, so that the next open
    /// need not rebuild them.  If that fails, it will.
    fn drop(&mut self) {
        if self.unsynced {
            let _ = self.sync_indexes();
        }
    }
}

impl Local {
    /// Opens or creates the database whose files start with `prefix`, or an
    /// in-memory database if `prefix` is `None`.  If its indexes are
    /// [stale](Self::indexes_stale), it has none until they are rebuilt.
    pub fn open(prefix: Option<&Path>, mut options: Options) -> Result<Self> {
        options.log.flush = options.synchronous != Synchronous::Off;
        let mut log = LocalRecordLog::open(prefix.map(log_prefix).as_deref(), options.log.clone())?;
        if log.has_pending_commit() {
            log.abandon_commit()?;
        }
        if log.metadata().indexes_stale
            && let Some(p) = prefix
        {
            // Before opening them: they may not even parse.
            let mut i = 0;
            while i < options.tables || local_table::exists(&index_prefix(p, i)) {
                local_table::remove(&index_prefix(p, i))?;
                i += 1;
            }
        }
        let mut local = Local {
            prefix: prefix.map(Path::to_path_buf),
            options,
            log,
            indexes: Vec::new(),
            readers: Vec::new(),
            unsynced: false,
        };
        while local.indexes.len() < local.options.tables
            || local
                .prefix
                .as_ref()
                .is_some_and(|p| local_table::exists(&index_prefix(p, local.indexes.len())))
        {
            local.new_table()?;
        }
        Ok(local)
    }

    /// A reader of the database, for any thread, while this one is written
    /// and committed: it sees what was committed when it was made, sharing
    /// the log's mappings and reading the tables through their readers
    /// (which see later entries too, which the log bounds).  Its writes
    /// fail.
    pub fn reader(&self) -> Local {
        Local {
            prefix: self.prefix.clone(),
            options: self.options.clone(),
            log: self.log.reader(),
            indexes: Vec::new(),
            readers: if self.is_reader() {
                self.readers.clone()
            } else {
                self.indexes.iter().map(LocalTable::reader).collect()
            },
            unsynced: false,
        }
    }

    /// Whether a promotion in a table must flush its tree: not if a crash
    /// would have the table rebuilt.
    fn tables_ordered(&self) -> bool {
        self.options.synchronous == Synchronous::Full && !self.log.metadata().indexes_stale
    }

    /// Whether the indexes may not cover the log, after a crash under
    /// [`Normal`](Synchronous::Normal) or [`Off`](Synchronous::Off): they
    /// must be rebuilt from the log.
    pub fn indexes_stale(&self) -> bool {
        self.log.metadata().indexes_stale
    }

    /// Records that the indexes, rebuilt from the log, cover it again.
    pub fn indexes_rebuilt(&mut self) -> Result<()> {
        self.sync_indexes()?;
        let ordered = self.tables_ordered();
        for index in &mut self.indexes {
            index.set_ordered(ordered);
        }
        Ok(())
    }

    /// Flushes the indexes and clears the mark that they may not cover the
    /// log.  In that order: a crash between them only rebuilds them.
    fn sync_indexes(&mut self) -> Result<()> {
        for index in &mut self.indexes {
            index.sync()?;
        }
        self.log.clear_indexes_stale()?;
        self.unsynced = false;
        Ok(())
    }

    /// Starts a bulk load: until [`end_bulk`](Self::end_bulk), the indexes
    /// are marked stale, so a crash has them rebuilt, and nothing need be
    /// flushed for them in the meantime.
    pub fn begin_bulk(&mut self) -> Result<()> {
        self.check_writer()?;
        self.log.mark_indexes_stale()?;
        self.unsynced = !self.is_transient();
        for index in &mut self.indexes {
            index.set_ordered(false);
        }
        Ok(())
    }

    /// Ends a bulk load, after it is committed or rolled back.  Under
    /// [`Full`](Synchronous::Full), the indexes are flushed and no longer
    /// marked stale.
    pub fn end_bulk(&mut self) -> Result<()> {
        if self.options.synchronous == Synchronous::Full {
            self.sync_indexes()?;
            for index in &mut self.indexes {
                index.set_ordered(true);
            }
        }
        Ok(())
    }

    /// Sizes a database with nothing in it for about `tuples` tuples, as
    /// [`Options::for_tuples`] does on creating one.  A database with
    /// entries is left as it is.
    pub fn presize(&mut self, tuples: u64) -> Result<()> {
        self.check_writer()?;
        if self.log.next_id() != self.log.start_id() {
            return Ok(());
        }
        self.log
            .reserve(tuples.saturating_mul(LOG_BYTES_PER_TUPLE))?;
        let size = tuples.saturating_mul(INDEX_BYTES_PER_TUPLE);
        for index in &mut self.indexes {
            index.presize(size)?;
        }
        Ok(())
    }

    /// Makes dropping this session like a crash: nothing more is flushed.
    #[cfg(test)]
    pub(crate) fn crash(&mut self) {
        self.unsynced = false;
    }

    /// Whether this is a reader (see [`reader`](Self::reader)).
    pub fn is_reader(&self) -> bool {
        self.log.is_reader()
    }

    fn check_writer(&self) -> Result<()> {
        if self.is_reader() {
            return Err(Error::Log(local_record_log::Error::InvalidArgument(
                "the database is read only: a reader",
            )));
        }
        Ok(())
    }

    /// Adds an index table, returning its number.
    pub fn new_table(&mut self) -> Result<usize> {
        self.check_writer()?;
        let i = self.indexes.len();
        let prefix = self.prefix.as_ref().map(|p| index_prefix(p, i));
        let mut table = LocalTable::open(prefix.as_deref(), self.options.index.clone())?;
        table.set_ordered(self.tables_ordered());
        self.indexes.push(table);
        Ok(i)
    }

    /// Empties index table `i`: its files are replaced, and readers made
    /// before go on reading the old one.  Have the indexes
    /// [marked stale](Self::begin_bulk) first, so that a crash rebuilds them.
    pub fn reset_table(&mut self, i: usize) -> Result<()> {
        self.check_writer()?;
        let options = self.options.index.clone();
        // Closed before its files go: a table may flush when dropped.
        self.indexes[i] = LocalTable::open(None, options.clone())?;
        let prefix = self.prefix.as_ref().map(|p| index_prefix(p, i));
        if let Some(p) = &prefix {
            local_table::remove(p)?;
        }
        let mut table = LocalTable::open(prefix.as_deref(), options)?;
        table.set_ordered(self.tables_ordered());
        self.indexes[i] = table;
        Ok(())
    }

    /// The disk space index table `i` takes, in bytes: `None` in memory.
    pub fn table_bytes(&self, i: usize) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        let path = index_prefix(self.prefix.as_ref()?, i);
        let name = path.file_name()?.to_str()?;
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        let mut bytes = 0;
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            if entry.file_name().to_string_lossy().starts_with(name) {
                bytes += entry.metadata().ok()?.blocks() * 512;
            }
        }
        Some(bytes)
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    pub fn is_transient(&self) -> bool {
        self.prefix.is_none()
    }

    /// The path prefix of the database's files, if it is not in memory.
    pub fn prefix(&self) -> Option<&Path> {
        self.prefix.as_deref()
    }

    pub fn log(&self) -> &LocalRecordLog {
        &self.log
    }

    /// The log, for appending.  Commit with [`commit`](Self::commit), not the
    /// log's own commit, so that the indexes are flushed too.
    pub fn log_mut(&mut self) -> &mut LocalRecordLog {
        &mut self.log
    }

    pub fn table_count(&self) -> usize {
        self.indexes.len().max(self.readers.len())
    }

    /// Index table `i`, or a reader's reader of it.  Panics if there is no
    /// such table.
    pub fn table(&self, i: usize) -> TableRef<'_> {
        match self.readers.get(i) {
            Some(r) => TableRef::Reader(r),
            None => TableRef::Table(&self.indexes[i]),
        }
    }

    /// Index table `i`, for inserting.  Panics if there is no such table,
    /// or this is a reader.
    pub fn table_mut(&mut self, i: usize) -> &mut LocalTable {
        assert!(!self.is_reader(), "a reader's tables are read only");
        &mut self.indexes[i]
    }

    /// Commits the log's appended entries.  Under
    /// [`Full`](Synchronous::Full) the indexes are flushed so that they
    /// cover them; otherwise they are left stale.  A commit with nothing to
    /// do makes no system call.
    pub fn commit(&mut self) -> Result<()> {
        self.check_writer()?;
        if self.options.synchronous != Synchronous::Full {
            if self.log.is_clean() {
                return Ok(());
            }
            // Not if they were stale already: then they were found so, and
            // only rebuilding them can clear the mark.
            if !self.indexes_stale() {
                self.unsynced = !self.is_transient();
            }
            self.log.set_indexes_stale();
            self.log.commit()?;
            return Ok(());
        }
        if self.log.is_clean() && !self.indexes.iter().any(LocalTable::is_dirty) {
            return Ok(());
        }
        // Once, and first: a commit's metadata must not reach the disk
        // before the names of the files it needs.
        if self.log.dir_dirty() || self.indexes.iter().any(LocalTable::dir_dirty) {
            if let Some(p) = &self.prefix {
                mmap::sync_dir(&log_prefix(p))?;
            }
            self.log.dir_synced();
            self.indexes.iter_mut().for_each(LocalTable::dir_synced);
        }
        self.log.start_commit()?;
        for index in &mut self.indexes {
            index.sync()?;
        }
        self.log.complete_commit()?;
        Ok(())
    }

    /// The id that the large index entries have been compacted to, or 0:
    /// the latest horizon of any table (see [`compact`](Self::compact)).
    pub fn horizon(&self) -> u64 {
        (0..self.table_count())
            .map(|i| self.table(i).horizon())
            .max()
            .unwrap_or(0)
    }

    /// The pages of the large index entries' trees, over all the tables,
    /// and of those the pages free for reuse (see
    /// [`LocalTable::postings_pages`]); both 0 for a reader.
    pub fn postings_pages(&self) -> (u64, u64) {
        self.indexes.iter().fold((0, 0), |(n, f), t| {
            let (pages, free) = t.postings_pages();
            (n + pages, f + free)
        })
    }

    /// Compacts the large entries of `tables` to `horizon`, keeping each id
    /// after it and each at or before it that `keep` accepts, given the log.
    /// Every table records the horizon durably first, so a crash part way
    /// leaves it recorded.
    pub fn compact(
        &mut self,
        horizon: u64,
        tables: &[usize],
        mut keep: impl FnMut(&LocalRecordLog, u64) -> bool,
    ) -> Result<local_table::Compacted> {
        self.check_writer()?;
        for index in &mut self.indexes {
            if index.horizon() < horizon {
                index.set_horizon(horizon)?;
            }
        }
        let mut done = local_table::Compacted::default();
        for &t in tables {
            let log = &self.log;
            done += self.indexes[t].compact(|id| id > horizon || keep(log, id))?;
        }
        Ok(done)
    }

    /// Does periodic maintenance on the indexes.
    pub fn tick(&mut self) -> Result<()> {
        if self.is_reader() {
            return Ok(());
        }
        for index in &mut self.indexes {
            index.tick()?;
        }
        Ok(())
    }

    /// Writes a snapshot of the database, including entries not yet
    /// committed, to `out`.
    pub fn snapshot(&mut self, out: &mut impl Write) -> Result<()> {
        self.check_writer()?;
        for word in [SNAPSHOT_MAGIC, SNAPSHOT_VERSION, self.indexes.len() as u64] {
            out.write_all(&word.to_le_bytes())?;
        }
        let mut sink = |chunk: &[u8]| {
            out.write_all(&(chunk.len() as u64).to_le_bytes())?;
            out.write_all(chunk)
        };
        self.log.snapshot(&mut sink)?;
        for index in &mut self.indexes {
            index.snapshot(&mut sink)?;
        }
        Ok(())
    }

    /// Creates a database at `prefix`, or in memory if `prefix` is `None`,
    /// from a [`snapshot`](Self::snapshot) read from `input`.  Fails if a
    /// database exists at `prefix`.
    pub fn restore(
        prefix: Option<&Path>,
        mut options: Options,
        input: &mut impl Read,
    ) -> Result<Self> {
        options.log.flush = options.synchronous != Synchronous::Off;
        if prefix.is_some_and(exists) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }
        let result = (|| {
            let mut word = || -> io::Result<u64> {
                let mut b = [0u8; 8];
                input.read_exact(&mut b)?;
                Ok(u64::from_le_bytes(b))
            };
            let (magic, version) = (word()?, word()?);
            if magic != SNAPSHOT_MAGIC || !(1..=SNAPSHOT_VERSION).contains(&version) {
                return Err(Error::InvalidFormat("bad magic or version"));
            }
            let count = word()?;
            let mut source = || -> io::Result<Vec<u8>> {
                let mut b = [0u8; 8];
                input.read_exact(&mut b)?;
                let len = u64::from_le_bytes(b);
                if len > MAX_FRAME {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "snapshot frame too long",
                    ));
                }
                let mut chunk = vec![0; len as usize];
                input.read_exact(&mut chunk)?;
                Ok(chunk)
            };
            let log = LocalRecordLog::restore(
                prefix.map(log_prefix).as_deref(),
                options.log.clone(),
                &mut source,
            )?;
            let mut indexes = Vec::new();
            for i in 0..count as usize {
                let p = prefix.map(|p| index_prefix(p, i));
                indexes.push(LocalTable::restore(
                    p.as_deref(),
                    options.index.clone(),
                    &mut source,
                    version >= 2,
                )?);
            }
            if input.read(&mut [0])? != 0 {
                return Err(Error::InvalidFormat("trailing data"));
            }
            let mut local = Local {
                prefix: prefix.map(Path::to_path_buf),
                options,
                log,
                indexes,
                readers: Vec::new(),
                unsynced: false,
            };
            let ordered = local.tables_ordered();
            for index in &mut local.indexes {
                index.set_ordered(ordered);
            }
            while local.indexes.len() < local.options.tables {
                local.new_table()?;
            }
            Ok(local)
        })();
        if result.is_err()
            && let Some(p) = prefix
        {
            let _ = remove(p);
        }
        result
    }
}

#[cfg(test)]
mod tests;
