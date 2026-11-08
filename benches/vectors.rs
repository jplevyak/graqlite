//! Nearest-neighbour search: `GRAQLITE_BENCH_VECTORS` vectors (default
//! 100 000) of `GRAQLITE_BENCH_DIMS` dimensions (default 384), each a value
//! linked by `embedding`, with a vector index declared, by cosine.
//!
//! The vectors are clustered, as embeddings are: each is one of 1 000
//! centres moved a little.  Before the benchmarks, it prints the time to
//! build the index (fed at the commit), to save it and to load it, and the
//! recall at 10 of the index's search against an exact one, for 100
//! queries.  Then it times, per query:
//!
//! - `index`: the index's search alone (`VectorSearch::search`);
//! - `nearest`: a prepared query `nearest(embedding $q 10 ?v ?d)`, which
//!   uses the index;
//! - `scan`: the same by l2, which the index (by cosine) can't answer, so
//!   it is a scan of every vector.
//!
//! Queries per second are one over the time.
//!
//! ```sh
//! cargo bench --bench vectors
//! GRAQLITE_BENCH_VECTORS=20000 cargo bench --bench vectors
//! ```

use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use graqlite::graph::{Graph, Options, VectorIndex};
use graqlite::query::aggregate::Cell;
use graqlite::query::{Nearest, Query};
use graqlite::vector::{self, Metric};

const K: usize = 10;
const QUERIES: usize = 100;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(default)
}

/// A small deterministic random number generator (xorshift64*), giving
/// numbers in [-1, 1).
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

    /// A vector near one of `centres`.
    fn near(&mut self, centres: &[Vec<f32>]) -> Vec<f32> {
        let c =
            &centres[((self.unit() + 1.0) / 2.0 * centres.len() as f32) as usize % centres.len()];
        c.iter().map(|x| x + 0.3 * self.unit()).collect()
    }
}

/// A database directory removed when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bench(c: &mut Criterion) {
    let n = env("GRAQLITE_BENCH_VECTORS", 100_000);
    let dims = env("GRAQLITE_BENCH_DIMS", 384);
    let mut rng = Rng(0x5eed);
    let centres: Vec<Vec<f32>> = (0..1000).map(|_| rng.vector(dims)).collect();
    let dir = TempDir(
        std::env::temp_dir().join(format!("graqlite-bench-vectors-{}", std::process::id())),
    );
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let prefix = dir.0.join("g.");

    let mut g = Graph::open(Some(&prefix), Options::for_tuples(2 * n as u64)).unwrap();
    let p = g.value(b"embedding").unwrap();
    g.create_vector_index(&VectorIndex::new(p, dims, Metric::Cosine))
        .unwrap();
    g.commit().unwrap();
    let mut vectors = Vec::with_capacity(n);
    for _ in 0..n {
        let v = rng.near(&centres);
        let s = g.node().unwrap();
        let id = g.value(&vector::encode(&v)).unwrap();
        g.link(s, p, id).unwrap();
        vectors.push((id, v));
    }
    let t = Instant::now();
    g.commit().unwrap();
    eprintln!(
        "{n} vectors of {dims}: index built in {:.1?} ({:.0?} a vector)",
        t.elapsed(),
        t.elapsed() / n as u32
    );
    let t = Instant::now();
    drop(g);
    eprintln!("saved, with the graph dropped, in {:.1?}", t.elapsed());
    let t = Instant::now();
    let g = Graph::open(Some(&prefix), Options::for_tuples(2 * n as u64)).unwrap();
    eprintln!("opened, the index loaded, in {:.1?}", t.elapsed());
    let search = g.vector_search(p).expect("an index");

    // Recall at 10 against the exact search.
    let queries: Vec<Vec<f32>> = (0..QUERIES).map(|_| rng.near(&centres)).collect();
    let at = g.vector_bound();
    let mut found = 0;
    for q in &queries {
        let got = search.search(q, K, at).unwrap();
        let mut exact: Vec<(f64, u64)> = vectors
            .iter()
            .map(|(id, v)| (vector::distance(q, v, Metric::Cosine).unwrap(), *id))
            .collect();
        exact.select_nth_unstable_by(K, |a, b| a.0.total_cmp(&b.0));
        found += exact[..K]
            .iter()
            .filter(|e| got.iter().any(|g| g.0 == e.1))
            .count();
    }
    eprintln!(
        "recall at {K}, ef {}: {:.3}",
        search.index.ef,
        found as f64 / (QUERIES * K) as f64
    );

    let prepared = |metric: Metric| {
        let mut q = Query::new();
        let (v, d, s) = (q.var("v"), q.var("d"), q.var("s"));
        let qv = q.param("q");
        q.group()
            .nearest(Nearest::new(p, qv, K, v, d).metric(metric), s);
        q.select([v, d]);
        q.prepare(&g).unwrap()
    };
    let cells: Vec<Vec<Cell>> = queries
        .iter()
        .map(|q| vec![Cell::Text(vector::format(q))])
        .collect();
    let mut group = c.benchmark_group("vectors");
    group.measurement_time(Duration::from_secs(5));
    let mut i = 0;
    group.bench_function("index", |b| {
        b.iter(|| {
            i = (i + 1) % QUERIES;
            black_box(search.search(&queries[i], K, at))
        })
    });
    let mut nearest = prepared(Metric::Cosine);
    group.bench_function("nearest", |b| {
        b.iter(|| {
            i = (i + 1) % QUERIES;
            black_box(nearest.run_values(&g, &cells[i]).unwrap())
        })
    });
    let mut scan = prepared(Metric::L2);
    group.sample_size(10);
    group.bench_function("scan", |b| {
        b.iter(|| {
            i = (i + 1) % QUERIES;
            black_box(scan.run_values(&g, &cells[i]).unwrap())
        })
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
