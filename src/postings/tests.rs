use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// A temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("graqlite-postings-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn prefix(&self) -> PathBuf {
        self.0.join("t.")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `n` increasing ids from `start`, with gaps from a small generator (gaps of
/// up to `max_gap`).
fn ids(n: usize, start: u64, max_gap: u64) -> Vec<u64> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut id = start;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            id += 1 + x % max_gap;
            id
        })
        .collect()
}

/// Checks every way of reading `tree` against `expected`.
fn check(tree: &Tree, expected: &[u64]) {
    assert_eq!(tree.count(), expected.len() as u64);
    assert_eq!(tree.iter().collect::<Vec<_>>(), expected);
    let mut rev = expected.to_vec();
    rev.reverse();
    assert_eq!(tree.iter_rev().collect::<Vec<_>>(), rev);
    if expected.is_empty() {
        return;
    }
    let (lo, hi) = (expected[0], expected[expected.len() - 1]);
    // Seeks at, between, before and after the ids.
    let step = (expected.len() / 97).max(1);
    let mut targets: Vec<u64> = vec![0, lo - 1, lo, hi, hi + 1, u64::MAX];
    for i in (0..expected.len()).step_by(step) {
        targets.extend([expected[i], expected[i] + 1, expected[i].saturating_sub(1)]);
    }
    for t in targets {
        let from = expected.partition_point(|&id| id < t);
        let got: Vec<u64> = tree.seek(t).take(5).collect();
        assert_eq!(
            got,
            &expected[from..(from + 5).min(expected.len())],
            "seek {t}"
        );
        let to = expected.partition_point(|&id| id <= t);
        let got: Vec<u64> = tree.seek_rev(t).take(5).collect();
        let want: Vec<u64> = expected[to.saturating_sub(5)..to]
            .iter()
            .rev()
            .copied()
            .collect();
        assert_eq!(got, want, "seek_rev {t}");
    }
}

#[test]
fn trees_of_every_height() {
    let mut p = Postings::open(None).unwrap();
    // Small gaps pack about 4000 ids to a leaf; large ones about 1000, so
    // 400 000 of them make a tree of height 2.
    for (n, gap) in [
        (1, 10),
        (2, 10),
        (64, 10),
        (5_000, 3),
        (20_000, 1_000_000),
        (400_000, 1_000_000),
    ] {
        let ids = ids(n, 100, gap);
        let h = p.create(&ids).unwrap();
        check(&p.tree(h), &ids);
    }
    let big = p.create(&ids(400_000, 1, 1_000_000)).unwrap();
    assert_eq!(p.shared.load(big, H_HEIGHT), 2);
}

#[test]
fn appending() {
    let mut p = Postings::open(None).unwrap();
    let all = ids(250_000, 7, 1_000_000);
    let h = p.create(&all[..10]).unwrap();
    // Uneven batches, with ids already in the tree among them.
    let mut at = 10;
    let mut size = 1;
    while at < all.len() {
        let end = (at + size).min(all.len());
        let batch: Vec<u64> = all[at.saturating_sub(2)..end].to_vec();
        p.append(h, &batch).unwrap();
        at = end;
        size = size * 3 % 5_000 + 1;
        if at % 7 == 0 {
            check(&p.tree(h), &all[..at]);
        }
    }
    check(&p.tree(h), &all);
    // A tree read before an append keeps what it read.
    let before = p.tree(h);
    p.append(h, &[all[all.len() - 1] + 5]).unwrap();
    check(&before, &all);
    assert_eq!(p.tree(h).count(), all.len() as u64 + 1);
}

#[test]
fn reopening_and_crashing() {
    let dir = TempDir::new("reopen");
    let prefix = dir.prefix();
    let all = ids(60_000, 1, 1_000_000);
    let (a, b);
    {
        let mut p = Postings::open(Some(&prefix)).unwrap();
        a = p.create(&all[..30_000]).unwrap();
        b = p.create(&all[..100]).unwrap();
        p.sync().unwrap();
        // Not synced: lost in the "crash" of dropping without a sync.
        p.append(a, &all[30_000..45_000]).unwrap();
        p.create(&all[..5]).unwrap();
    }
    {
        let mut p = Postings::open(Some(&prefix)).unwrap();
        check(&p.tree(a), &all[..30_000]);
        check(&p.tree(b), &all[..100]);
        // Appending again after the crash overwrites what was lost.
        p.append(a, &all[30_000..]).unwrap();
        p.append(b, &all[100..200]).unwrap();
        check(&p.tree(a), &all);
        p.sync().unwrap();
    }
    let p = Postings::open(Some(&prefix)).unwrap();
    check(&p.tree(a), &all);
    check(&p.tree(b), &all[..200]);
    // A snapshot restores the same trees.
    let mut chunks = Vec::new();
    p.snapshot(|c| {
        chunks.push(c.to_vec());
        Ok(())
    })
    .unwrap();
    let mut chunks = chunks.into_iter();
    let restored = Postings::restore(None, || Ok(chunks.next().unwrap())).unwrap();
    check(&restored.tree(a), &all);
    check(&restored.tree(b), &all[..200]);
    assert_eq!(restored.pages(), p.pages());
    remove(&prefix).unwrap();
    assert!(!exists(&prefix));
}

#[test]
fn reading_while_appending() {
    let mut p = Postings::open(None).unwrap();
    let all = ids(200_000, 1, 1_000_000);
    let h = p.create(&all[..1]).unwrap();
    let reader = p.reader();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut reads = 0;
            while !done.load(Ordering::Acquire) || reads == 0 {
                let tree = reader.tree(h);
                let n = tree.count() as usize;
                // A tree as published: exactly a prefix of the ids.
                let got: Vec<u64> = tree.iter().collect();
                assert_eq!(got, &all[..n]);
                let back: Vec<u64> = tree.iter_rev().take(3).collect();
                assert_eq!(
                    back,
                    all[n.saturating_sub(3)..n]
                        .iter()
                        .rev()
                        .copied()
                        .collect::<Vec<_>>()
                );
                reads += 1;
            }
        });
        for chunk in all[1..].chunks(997) {
            p.append(h, chunk).unwrap();
        }
        done.store(true, Ordering::Release);
    });
    check(&p.tree(h), &all);
}

#[test]
fn releasing_frees_every_page_of_a_tree_once_unpinned() {
    let mut p = Postings::open(None).unwrap();
    // A tree of height 2, grown by appends, and a small one kept.
    let big = ids(400_000, 1, 1_000_000);
    let h = p.create(&big[..1]).unwrap();
    for chunk in big[1..].chunks(10_007) {
        p.append(h, chunk).unwrap();
    }
    assert_eq!(p.shared.load(h, H_HEIGHT), 2);
    let small = ids(5_000, 1, 1_000);
    let s = p.create(&small).unwrap();
    let small_pages = p.shared.tree_pages(s, p.used).unwrap().len() as u64;
    let pages = p.pages();
    let reader = p.reader();
    let held = reader.tree(h);
    p.release(&[h]);
    // Every page but the file header's and the small tree's.
    assert_eq!(p.free_pages(), pages - 1 - small_pages);
    assert_eq!(p.trees(), vec![(s, (0, 0))]);
    // Held by a reader made before, and by its tree: none is reused.
    drop(reader);
    let again = p.create(&big[..50_000]).unwrap();
    assert!(p.pages() > pages);
    check(&held, &big);
    // A reader made after the release doesn't hold them.
    let after = p.reader();
    drop(held);
    let pages = p.pages();
    let more = p.create(&big[..50_000]).unwrap();
    assert_eq!(p.pages(), pages);
    check(&after.tree(more), &big[..50_000]);
    check(&p.tree(again), &big[..50_000]);
    check(&p.tree(s), &small);
    // Releasing a tree twice, or a page that isn't a tree's, does nothing.
    let free = p.free_pages();
    p.release(&[h, 3]);
    assert_eq!(p.free_pages(), free);
}
