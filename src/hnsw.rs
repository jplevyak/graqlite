//! A hierarchical navigable small world graph: approximate nearest-neighbour
//! search over vectors (Malkov and Yashunin, 2016).
//!
//! Each item is a vector with an id and the ranges of ids over which it is
//! *live*, so that a search can be as of an id: it finds the items live then
//! (see [`Hnsw::search`]).  An item stays in the graph once it isn't live,
//! as a tombstone that searches pass through but don't return, until the
//! graph is built again without it.
//!
//! - **Layers:** an item's level is drawn at random, with each level `m`
//!   times rarer than the one below; it is linked on each layer up to its
//!   level to at most `m` near items (`2m` on layer 0), chosen by the
//!   paper's heuristic, which keeps links in different directions.
//! - **Search:** greedy from the top layer down to layer 1, then a beam of
//!   `ef` candidates on layer 0.  If too few of the items found are live,
//!   the beam is widened.
//! - **Deterministic:** the levels come from a seeded generator, so the same
//!   insertions build the same graph.

use std::cmp::Ordering;
#[cfg(test)]
use std::collections::HashSet;
use std::collections::{BinaryHeap, HashMap};

use crate::vector::{Metric, distance, dot, l2_squared};

/// An id beyond any: the end of a range still live.
pub const LIVE: u64 = u64::MAX;

/// The graph; see the [module](self) docs.
#[derive(Debug, Clone)]
pub struct Hnsw {
    dims: usize,
    metric: Metric,
    m: usize,
    ef: usize,
    /// One over the log of `m`: a level's scale.
    level_scale: f64,
    items: Vec<Item>,
    /// Each item's index, by id.
    by_id: HashMap<u64, u32>,
    /// The item searches start from, on the top layer.
    entry: Option<u32>,
    rng: u64,
}

#[derive(Debug, Clone)]
struct Item {
    id: u64,
    vector: Box<[f32]>,
    /// Its Euclidean norm, for the cosine.
    norm: f32,
    /// The ranges of ids `[from, to)` over which it is live, in order.
    live: Vec<(u64, u64)>,
    /// Its neighbours on each layer, up to its level.
    links: Vec<Vec<u32>>,
}

impl Item {
    /// Whether it is live as of `at`: in a range `from <= at < to`.
    fn live_at(&self, at: u64) -> bool {
        self.live.iter().any(|&(from, to)| from <= at && at < to)
    }

    fn is_live(&self) -> bool {
        self.live.last().is_some_and(|r| r.1 == LIVE)
    }
}

/// A candidate: its distance and index, ordered by distance (then index).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Near(f64, u32);

impl Eq for Near {}

impl Ord for Near {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hnsw {
    /// An empty graph of vectors of `dims` elements, measured by `metric`,
    /// with `m` links per item per layer and a beam of `ef` (at least `m`)
    /// while building.
    pub fn new(dims: usize, metric: Metric, m: usize, ef: usize) -> Self {
        let m = m.max(2);
        Hnsw {
            dims,
            metric,
            m,
            ef: ef.max(m),
            level_scale: 1.0 / (m as f64).ln(),
            items: Vec::new(),
            by_id: HashMap::new(),
            entry: None,
            rng: 0x9e37_79b9_7f4a_7c15,
        }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// The items, live or not.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items live now.
    pub fn live_len(&self) -> usize {
        self.items.iter().filter(|i| i.is_live()).count()
    }

    /// Whether `id` is an item.
    pub fn contains(&self, id: u64) -> bool {
        self.by_id.contains_key(&id)
    }

    /// Makes `id`, with `vector`, live from id `from`: added, if it isn't an
    /// item yet (and `vector` has the graph's dimension), or live again.
    /// Returns whether it was added or made live.
    pub fn insert(&mut self, id: u64, vector: &[f32], from: u64) -> bool {
        if let Some(&i) = self.by_id.get(&id) {
            let item = &mut self.items[i as usize];
            if item.is_live() {
                return false;
            }
            item.live.push((from, LIVE));
            return true;
        }
        if vector.len() != self.dims || self.distance(vector, vector).is_none() {
            return false;
        }
        let i = self.items.len() as u32;
        let level = self.level();
        self.items.push(Item {
            id,
            vector: vector.into(),
            norm: dot(vector, vector).sqrt(),
            live: vec![(from, LIVE)],
            links: vec![Vec::new(); level + 1],
        });
        self.by_id.insert(id, i);
        let Some(entry) = self.entry else {
            self.entry = Some(i);
            return true;
        };
        let top = self.items[entry as usize].links.len() - 1;
        let q = Query::new(vector);
        let mut nearest = Near(self.dist(&q, entry), entry);
        for layer in (level + 1..=top).rev() {
            nearest = self.greedy(&q, nearest, layer);
        }
        let mut entries = vec![nearest];
        for layer in (0..=level.min(top)).rev() {
            let found = self.search_layer(&q, &entries, self.ef, layer);
            let cap = self.cap(layer);
            let chosen = self.select(&found, self.m);
            self.items[i as usize].links[layer] = chosen.iter().map(|n| n.1).collect();
            for n in &chosen {
                let links = &mut self.items[n.1 as usize].links[layer];
                links.push(i);
                if links.len() > cap {
                    self.prune(n.1, layer, cap);
                }
            }
            entries = found;
        }
        if level > top {
            self.entry = Some(i);
        }
        true
    }

    /// Ends the range over which `id` is live at `to`: a search as of `to`
    /// or later doesn't find it.  Returns whether it was live.
    pub fn remove(&mut self, id: u64, to: u64) -> bool {
        let Some(&i) = self.by_id.get(&id) else {
            return false;
        };
        let item = &mut self.items[i as usize];
        match item.live.last_mut() {
            Some(r) if r.1 == LIVE => {
                r.1 = to.max(r.0);
                true
            }
            _ => false,
        }
    }

    /// The ids of the `k` items live as of `at` nearest `query`, nearest
    /// first, with their distances.  Approximate: a beam of `ef` (at least
    /// `k`), widened while it finds too few live items.
    pub fn search(&self, query: &[f32], k: usize, ef: usize, at: u64) -> Vec<(u64, f64)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 || query.len() != self.dims || self.distance(query, query).is_none() {
            return Vec::new();
        }
        let top = self.items[entry as usize].links.len() - 1;
        let q = Query::new(query);
        let mut nearest = Near(self.dist(&q, entry), entry);
        for layer in (1..=top).rev() {
            nearest = self.greedy(&q, nearest, layer);
        }
        let mut ef = ef.max(k);
        loop {
            let found = self.search_layer(&q, &[nearest], ef, 0);
            // Measured again exactly, as a scan would.
            let mut live: Vec<(u64, f64)> = found
                .iter()
                .filter(|n| self.items[n.1 as usize].live_at(at))
                .take(k)
                .map(|n| {
                    let item = &self.items[n.1 as usize];
                    let d = self.distance(query, &item.vector).unwrap_or(f64::INFINITY);
                    (item.id, d)
                })
                .collect();
            live.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            if live.len() >= k || ef >= self.items.len() {
                return live;
            }
            ef = (ef * 2).min(self.items.len());
        }
    }

    /// The graph built again of the items live as of `at` and later, so that
    /// those dead before are dropped.
    pub fn rebuilt(&self, at: u64) -> Hnsw {
        let mut out = Hnsw::new(self.dims, self.metric, self.m, self.ef);
        for item in &self.items {
            let live: Vec<(u64, u64)> = item.live.iter().copied().filter(|r| r.1 > at).collect();
            let Some(&(from, _)) = live.first() else {
                continue;
            };
            out.insert(item.id, &item.vector, from);
            let i = out.by_id[&item.id] as usize;
            out.items[i].live = live;
        }
        out
    }

    /// The links per item on `layer`, at most.
    fn cap(&self, layer: usize) -> usize {
        if layer == 0 { 2 * self.m } else { self.m }
    }

    /// A random level: 0 with chance `1 - 1/m`, and so on up.
    fn level(&mut self) -> usize {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let u = ((self.rng >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        ((-u.ln() * self.level_scale) as usize).min(16)
    }

    fn distance(&self, a: &[f32], b: &[f32]) -> Option<f64> {
        distance(a, b, self.metric)
    }

    /// A distance from `q` to item `i` that orders as the metric's does,
    /// measured fast: the squared distance, for L2.
    fn dist(&self, q: &Query, i: u32) -> f64 {
        let item = &self.items[i as usize];
        let d = match self.metric {
            Metric::Cosine => 1.0 - dot(q.vector, &item.vector) / (q.norm * item.norm),
            Metric::L2 => l2_squared(q.vector, &item.vector),
            Metric::Dot => -dot(q.vector, &item.vector),
        };
        if d.is_nan() {
            f64::INFINITY
        } else {
            f64::from(d)
        }
    }

    /// The same, between two items.
    fn dist_items(&self, i: u32, j: u32) -> f64 {
        let item = &self.items[i as usize];
        self.dist(
            &Query {
                vector: &item.vector,
                norm: item.norm,
            },
            j,
        )
    }

    /// The item nearest `v` found by moving to nearer neighbours on `layer`.
    fn greedy(&self, v: &Query, mut nearest: Near, layer: usize) -> Near {
        loop {
            let mut moved = false;
            let links = self.items[nearest.1 as usize].links.get(layer);
            for &n in links.into_iter().flatten() {
                let d = Near(self.dist(v, n), n);
                if d < nearest {
                    nearest = d;
                    moved = true;
                }
            }
            if !moved {
                return nearest;
            }
        }
    }

    /// The `ef` items nearest `v` found from `entries` on `layer`, nearest
    /// first.
    fn search_layer(&self, v: &Query, entries: &[Near], ef: usize, layer: usize) -> Vec<Near> {
        let mut visited = Visited::new(self.items.len());
        for n in entries {
            visited.insert(n.1);
        }
        // Candidates to expand, nearest first; and the nearest found, the
        // farthest first.
        let mut candidates: BinaryHeap<std::cmp::Reverse<Near>> =
            entries.iter().map(|&n| std::cmp::Reverse(n)).collect();
        let mut found: BinaryHeap<Near> = entries.iter().copied().collect();
        while found.len() > ef {
            found.pop();
        }
        while let Some(std::cmp::Reverse(c)) = candidates.pop() {
            if found.len() >= ef && found.peek().is_some_and(|f| c > *f) {
                break;
            }
            let links = self.items[c.1 as usize].links.get(layer);
            for &n in links.into_iter().flatten() {
                if !visited.insert(n) {
                    continue;
                }
                let d = Near(self.dist(v, n), n);
                if found.len() < ef || found.peek().is_some_and(|f| d < *f) {
                    candidates.push(std::cmp::Reverse(d));
                    found.push(d);
                    if found.len() > ef {
                        found.pop();
                    }
                }
            }
        }
        found.into_sorted_vec()
    }

    /// At most `m` of `candidates` (nearest first) to link to: each nearer
    /// the item than to any chosen before, so that the links spread out;
    /// then the nearest of those passed over, to fill them.
    fn select(&self, candidates: &[Near], m: usize) -> Vec<Near> {
        let mut chosen: Vec<Near> = Vec::with_capacity(m);
        let mut passed = Vec::new();
        for &c in candidates {
            if chosen.len() >= m {
                break;
            }
            if chosen.iter().all(|s| c.0 < self.dist_items(c.1, s.1)) {
                chosen.push(c);
            } else {
                passed.push(c);
            }
        }
        for c in passed {
            if chosen.len() >= m {
                break;
            }
            chosen.push(c);
        }
        chosen
    }

    /// Cuts item `i`'s links on `layer` to `cap`, chosen as `select` does.
    fn prune(&mut self, i: u32, layer: usize, cap: usize) {
        let mut near: Vec<Near> = self.items[i as usize].links[layer]
            .iter()
            .map(|&n| Near(self.dist_items(i, n), n))
            .collect();
        near.sort();
        let kept = self.select(&near, cap);
        self.items[i as usize].links[layer] = kept.iter().map(|n| n.1).collect();
    }
}

/// A vector searched near, and its norm.
struct Query<'a> {
    vector: &'a [f32],
    norm: f32,
}

impl<'a> Query<'a> {
    fn new(vector: &'a [f32]) -> Self {
        Query {
            vector,
            norm: dot(vector, vector).sqrt(),
        }
    }
}

/// The items a search has visited: a bit each.
struct Visited(Vec<u64>);

impl Visited {
    fn new(n: usize) -> Self {
        Visited(vec![0; n.div_ceil(64)])
    }

    /// Marks `i`, returning whether it wasn't.
    fn insert(&mut self, i: u32) -> bool {
        let (w, b) = (i as usize / 64, 1u64 << (i % 64));
        let new = self.0[w] & b == 0;
        self.0[w] |= b;
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic random number generator (xorshift64*).
    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let x = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
            (x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }

        fn vector(&mut self, dims: usize) -> Vec<f32> {
            (0..dims).map(|_| self.unit()).collect()
        }
    }

    /// The `k` ids live as of `at` nearest `query`, exactly.
    fn exact(
        items: &[(u64, Vec<f32>)],
        live: impl Fn(u64) -> bool,
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Vec<u64> {
        let mut all: Vec<(f64, u64)> = items
            .iter()
            .filter(|(id, _)| live(*id))
            .map(|(id, v)| (distance(query, v, metric).unwrap(), *id))
            .collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        all.into_iter().take(k).map(|(_, id)| id).collect()
    }

    /// The share of `want` that `got` has.
    fn recall(got: &[(u64, f64)], want: &[u64]) -> f64 {
        let got: HashSet<u64> = got.iter().map(|g| g.0).collect();
        want.iter().filter(|w| got.contains(w)).count() as f64 / want.len() as f64
    }

    #[test]
    fn finds_most_of_the_nearest() {
        for metric in [Metric::Cosine, Metric::L2, Metric::Dot] {
            let mut rng = Rng(42);
            let mut h = Hnsw::new(16, metric, 12, 100);
            let items: Vec<(u64, Vec<f32>)> = (0..1500).map(|i| (i, rng.vector(16))).collect();
            for (id, v) in &items {
                assert!(h.insert(*id, v, 1));
            }
            assert_eq!(h.len(), 1500);
            let mut total = 0.0;
            let queries = 50;
            for _ in 0..queries {
                let q = rng.vector(16);
                let got = h.search(&q, 10, 100, 1);
                assert_eq!(got.len(), 10);
                // Nearest first, with the distances measured.
                assert!(got.windows(2).all(|w| w[0].1 <= w[1].1));
                total += recall(&got, &exact(&items, |_| true, &q, 10, metric));
            }
            let recall = total / queries as f64;
            assert!(recall >= 0.95, "{metric:?}: recall {recall}");
        }
    }

    #[test]
    fn searches_as_of_an_id() {
        let mut rng = Rng(7);
        let mut h = Hnsw::new(8, Metric::L2, 8, 64);
        let items: Vec<(u64, Vec<f32>)> = (0..500).map(|i| (i, rng.vector(8))).collect();
        // Each added at id 10 + its id; the even ones removed at 1000, and
        // the multiples of four live again from 2000.
        for (id, v) in &items {
            h.insert(*id, v, 10 + id);
        }
        for id in (0..500).step_by(2) {
            assert!(h.remove(id, 1000));
            assert!(!h.remove(id, 1001));
        }
        for id in (0..500).step_by(4) {
            assert!(h.insert(id, &[], 2000));
        }
        assert_eq!(h.live_len(), 250 + 125);
        let live_at = |id: u64, at: u64| {
            (10 + id <= at && at < 1000)
                || id % 2 == 1 && 10 + id <= at
                || id.is_multiple_of(4) && at >= 2000
        };
        for at in [100, 999, 1000, 1500, 2000, 5000] {
            let q = rng.vector(8);
            let got = h.search(&q, 20, 64, at);
            assert!(got.iter().all(|&(id, _)| live_at(id, at)), "{at}");
            let want = exact(&items, |id| live_at(id, at), &q, 20, Metric::L2);
            assert_eq!(got.len(), want.len());
            assert!(recall(&got, &want) >= 0.9, "{at}");
        }
        // Built again as of 1500, the items dead then are dropped, and
        // searches as of then and later find the same.
        let r = h.rebuilt(1500);
        assert_eq!(r.len(), 250 + 125);
        let q = rng.vector(8);
        let want = exact(&items, |id| live_at(id, 5000), &q, 10, Metric::L2);
        assert!(recall(&r.search(&q, 10, 64, 5000), &want) >= 0.9);
    }

    #[test]
    fn refuses_what_it_cannot_measure() {
        let mut h = Hnsw::new(2, Metric::Cosine, 4, 10);
        assert!(!h.insert(1, &[1.0, 2.0, 3.0], 1));
        assert!(!h.insert(2, &[0.0, 0.0], 1));
        assert!(h.is_empty());
        assert!(h.search(&[1.0, 0.0], 3, 10, 1).is_empty());
        assert!(h.insert(3, &[1.0, 0.0], 1));
        assert!(!h.insert(3, &[1.0, 0.0], 1));
        assert!(h.search(&[0.0, 0.0], 3, 10, 1).is_empty());
        assert_eq!(
            h.search(&[1.0, 0.1], 3, 10, 1),
            vec![(3, h.search(&[1.0, 0.1], 1, 1, 1)[0].1)]
        );
        // Not live before it was added.
        assert!(h.search(&[1.0, 0.0], 3, 10, 0).is_empty());
    }
}
