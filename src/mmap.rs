//! Memory mapping and file helpers.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

pub fn round_up_to_page(n: u64) -> u64 {
    n.div_ceil(page_size()).saturating_mul(page_size())
}

/// A shared read-write mapping of a file.  The mapping may be longer than the
/// file: the pages past its end fault until the file is extended, after which
/// they are valid at the same address.
pub struct Mapping {
    ptr: *mut u8,
    len: usize,
    /// The lowest offset [marked](Self::mark_dirty) since the last
    /// [`sync_dirty`](Self::sync_dirty), `u64::MAX` if none is.
    dirty_low: AtomicU64,
    /// The end of the highest range marked, 0 if none is.
    dirty_high: AtomicU64,
}

// SAFETY: the mapping is only accessed through atomics or by the single writer.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn new(file: &File, len: u64) -> io::Result<Self> {
        let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        // SAFETY: a fresh mapping at an address of the kernel's choosing.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping {
            ptr: ptr.cast(),
            len,
            dirty_low: AtomicU64::new(u64::MAX),
            dirty_high: AtomicU64::new(0),
        })
    }

    pub fn len(&self) -> u64 {
        self.len as u64
    }

    /// The 8 byte word at `offset`.
    ///
    /// # Safety
    ///
    /// `offset` must be a multiple of 8 within the part of the mapping backed
    /// by the file.
    pub unsafe fn word(&self, offset: u64) -> &AtomicU64 {
        debug_assert!(offset.is_multiple_of(8) && offset + 8 <= self.len as u64);
        // SAFETY: the mapping is page aligned, so the word is aligned, and the
        // caller guarantees it is in bounds.
        unsafe { &*(self.ptr.add(offset as usize) as *const AtomicU64) }
    }

    /// The `N` words at `offset`, with the same requirements as
    /// [`word`](Self::word) for all of them.
    pub unsafe fn words<const N: usize>(&self, offset: u64) -> &[AtomicU64; N] {
        debug_assert!(offset.is_multiple_of(8) && offset + 8 * N as u64 <= self.len as u64);
        // SAFETY: as for `word`.
        unsafe { &*(self.ptr.add(offset as usize) as *const [AtomicU64; N]) }
    }

    /// The `len` bytes at `offset`.
    ///
    /// # Safety
    ///
    /// The range must be within the part of the mapping backed by the file,
    /// and must not be written while the slice is alive.
    pub unsafe fn bytes(&self, offset: u64, len: u64) -> &[u8] {
        debug_assert!(offset + len <= self.len as u64);
        // SAFETY: the caller guarantees the range is backed and unwritten.
        unsafe { std::slice::from_raw_parts(self.ptr.add(offset as usize), len as usize) }
    }

    /// The `len` bytes at `offset`, for writing.
    ///
    /// # Safety
    ///
    /// The range must be within the part of the mapping backed by the file.
    pub unsafe fn bytes_mut(&mut self, offset: u64, len: u64) -> &mut [u8] {
        // SAFETY: as for `bytes_mut_shared`, with `&mut self` keeping out
        // other slices.
        unsafe { self.bytes_mut_shared(offset, len) }
    }

    /// The `len` bytes at `offset`, for writing, through a shared mapping:
    /// the single writer's, while readers read other ranges of it.
    ///
    /// # Safety
    ///
    /// The range must be within the part of the mapping backed by the file,
    /// and no other slice of it may be alive while this one is.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn bytes_mut_shared(&self, offset: u64, len: u64) -> &mut [u8] {
        debug_assert!(offset + len <= self.len as u64);
        // SAFETY: the caller guarantees the range is backed, and `&mut self`
        // keeps out other slices of this mapping.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(offset as usize), len as usize) }
    }

    /// Flushes the bytes of `range` to the file, with the rest of the pages
    /// they are on.  An empty range makes no system call.
    pub fn sync(&self, range: Range<u64>) -> io::Result<()> {
        let end = range.end.min(self.len as u64);
        if range.start >= end {
            return Ok(());
        }
        let start = range.start - range.start % page_size();
        // SAFETY: the range lies within this mapping, from a page boundary.
        check(unsafe {
            libc::msync(
                self.ptr.add(start as usize).cast(),
                (end - start) as usize,
                libc::MS_SYNC,
            )
        })
    }

    /// Records that the `len` bytes at `offset` were written, for the next
    /// [`sync_dirty`](Self::sync_dirty).  For the mapping's single writer:
    /// the two words are loaded and stored, not updated atomically.
    pub fn mark_dirty(&self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        if offset < self.dirty_low.load(Ordering::Relaxed) {
            self.dirty_low.store(offset, Ordering::Relaxed);
        }
        if offset + len > self.dirty_high.load(Ordering::Relaxed) {
            self.dirty_high.store(offset + len, Ordering::Relaxed);
        }
    }

    /// Whether anything was [marked](Self::mark_dirty) since the last
    /// [`sync_dirty`](Self::sync_dirty).
    pub fn is_dirty(&self) -> bool {
        self.dirty_low.load(Ordering::Relaxed) < self.dirty_high.load(Ordering::Relaxed)
    }

    /// Flushes from the lowest offset marked to the end of the highest, and
    /// forgets them if that succeeds.  With none marked, it makes no system
    /// call.
    ///
    /// One range, however far apart the writes, as each `msync` that writes
    /// waits for the device: one call over a hash table's scattered buckets
    /// is cheaper than one per page.
    pub fn sync_dirty(&self) -> io::Result<()> {
        if !self.is_dirty() {
            return Ok(());
        }
        self.sync(self.dirty_low.load(Ordering::Relaxed)..self.dirty_high.load(Ordering::Relaxed))?;
        self.dirty_low.store(u64::MAX, Ordering::Relaxed);
        self.dirty_high.store(0, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly this mapping, which nothing references now.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// `prefix` with `suffix` appended to its last component.
pub fn suffixed(prefix: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(prefix.as_os_str());
    s.push(suffix);
    s.into()
}

/// Flushes the directory holding the files that start with `prefix`, so that
/// files created there survive a crash.
pub fn sync_dir(prefix: &Path) -> io::Result<()> {
    let dir = match prefix.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

/// Extends `file` to at least `len` bytes, allocating its blocks so that a full
/// disk is reported here rather than as a fault on a later store.
pub fn allocate(file: &File, len: u64) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain syscall on an open file.
        let r = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len as libc::off_t) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if file.metadata()?.len() < len {
            file.set_len(len)?;
        }
        Ok(())
    }
}

/// A file that exists only in memory and disappears when closed.
pub fn ephemeral_file(name: &str) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;
        let name = std::ffi::CString::new(name).map_err(io::Error::other)?;
        // SAFETY: plain syscall with a valid C string.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        check(fd)?;
        // SAFETY: `fd` is a freshly created descriptor that we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        std::fs::remove_file(&path)?;
        Ok(file)
    }
}

/// Takes an exclusive lock on `file`, failing with `WouldBlock` if another
/// open table holds it.
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    // SAFETY: plain syscall on an open file.
    check(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_ranges() {
        let file = ephemeral_file("graqlite-dirty").unwrap();
        let size = 4 * page_size();
        allocate(&file, size).unwrap();
        let map = Mapping::new(&file, size).unwrap();
        assert!(!map.is_dirty());
        map.sync_dirty().unwrap();
        map.mark_dirty(100, 0);
        assert!(!map.is_dirty());
        // Neither end on a page boundary, which msync would refuse.
        map.mark_dirty(page_size() + 8, 8);
        map.mark_dirty(2 * page_size() + 8, 8);
        map.mark_dirty(page_size() + 16, 8);
        assert!(map.is_dirty());
        map.sync_dirty().unwrap();
        assert!(!map.is_dirty());
        map.sync(page_size() + 1..page_size() + 2).unwrap();
        map.sync(5..5).unwrap();
        // Past the mapping is cut off at its end.
        map.sync(page_size()..u64::MAX).unwrap();
    }
}
