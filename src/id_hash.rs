//! A fast hash for keys made of ids.
//!
//! Sets and maps keyed by ids are built and probed once per entry a search
//! reaches, where the standard hasher (SipHash, which resists chosen keys)
//! costs more than the rest of the step.  Ids are the database's own, not an
//! adversary's, so a multiply and a rotate per word is enough.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// A hasher of words: each is mixed in with a rotate, an xor and a multiply.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct IdHasher(u64);

impl IdHasher {
    const K: u64 = 0xf135_7aea_2e62_a9c5;

    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(Self::K);
    }
}

impl Hasher for IdHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, x: u8) {
        self.add(u64::from(x));
    }

    #[inline]
    fn write_u32(&mut self, x: u32) {
        self.add(u64::from(x));
    }

    #[inline]
    fn write_u64(&mut self, x: u64) {
        self.add(x);
    }

    #[inline]
    fn write_usize(&mut self, x: usize) {
        self.add(x as u64);
    }

    /// The high bits of a product are the well mixed ones, and a table
    /// takes its bucket from the low ones.
    #[inline]
    fn finish(&self) -> u64 {
        self.0.rotate_left(26)
    }
}

pub(crate) type IdBuildHasher = BuildHasherDefault<IdHasher>;
/// A hash map keyed by ids, or by tuples of them.
pub(crate) type IdMap<K, V> = HashMap<K, V, IdBuildHasher>;
/// A hash set of ids, or of tuples of them.
pub(crate) type IdHashSet<K> = HashSet<K, IdBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    #[test]
    fn spreads_ids() {
        // Ids a few bytes apart, as a log's are, fall in distinct buckets
        // of a small table about as often as chance allows.
        let build = IdBuildHasher::default();
        let mut buckets = [0u32; 256];
        for i in 0..25_600u64 {
            buckets[(build.hash_one((1000 + i * 7, 3usize)) & 255) as usize] += 1;
        }
        assert!(
            buckets.iter().all(|&n| (50..200).contains(&n)),
            "{buckets:?}"
        );
        let mut set: IdHashSet<(u64, usize)> = IdHashSet::default();
        assert!(set.insert((1, 2)));
        assert!(!set.insert((1, 2)));
        assert!(set.insert((2, 1)));
    }
}
