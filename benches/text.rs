//! Full-text search: `GRAQLITE_BENCH_TEXTS` short texts (default 200 000) of
//! about twelve words, each a value linked by `text`, on disk.
//!
//! Most words are from a vocabulary of 5 000; `common` is in every other
//! text, `rare` in one of 2 000.  Before the benchmarks, it prints the time
//! to load them and the disk space of the text index against the whole
//! database.  Then it times, per query:
//!
//! - `index/rare`, `index/common`, `index/both`: `(match …)`, by the index;
//! - `scan/rare`, `scan/common`: `contains(?v, …)`, reading every text.
//!
//! ```sh
//! cargo bench --bench text
//! GRAQLITE_BENCH_TEXTS=20000 cargo bench --bench text
//! ```

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use graqlite::graph::{Graph, Options};
use graqlite::graql;

fn texts() -> usize {
    std::env::var("GRAQLITE_BENCH_TEXTS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(200_000)
}

/// A small deterministic random number generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 20) % n
    }
}

fn text(rng: &mut Rng, i: usize) -> String {
    let mut t = format!("note{i}");
    for _ in 0..10 {
        t.push_str(&format!(" w{}", rng.below(5_000)));
    }
    if i.is_multiple_of(2) {
        t.push_str(" common");
    }
    if i % 2_000 == 7 {
        t.push_str(" rare");
    }
    t
}

fn load(prefix: &Path, n: usize) -> Graph {
    let mut g = Graph::open(Some(prefix), Options::default()).unwrap();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let p = g.value(b"text").unwrap();
    let start = Instant::now();
    for i in 0..n {
        let s = g.node().unwrap();
        let v = g.value(text(&mut rng, i).as_bytes()).unwrap();
        g.link(s, p, v).unwrap();
    }
    g.commit().unwrap();
    println!("loaded {n} texts in {:.1?}", start.elapsed());
    g
}

fn directory_bytes(dir: &PathBuf) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.metadata().unwrap().blocks() * 512)
        .sum()
}

fn bench(c: &mut Criterion) {
    let n = texts();
    let dir = std::env::temp_dir().join(format!("graqlite-bench-text-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut g = load(&dir.join("db."), n);
    let text_bytes = g.text_index_bytes().unwrap();
    println!(
        "text index {:.1} MiB of {:.1} MiB on disk",
        text_bytes as f64 / (1 << 20) as f64,
        directory_bytes(&dir) as f64 / (1 << 20) as f64
    );

    let mut group = c.benchmark_group("text");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));
    let mut query = |name: &str, q: &str| {
        let statement = graql::parse(q).unwrap().remove(0);
        group.bench_function(name, |b| {
            b.iter(|| black_box(graql::run(&mut g, &statement).unwrap()))
        });
    };
    query("index/rare", r#"select ?s where ?s text (match "rare")"#);
    query(
        "index/common",
        r#"select ?s where ?s text (match "common")"#,
    );
    query(
        "index/both",
        r#"select ?s where ?s text (match "common rare")"#,
    );
    query("scan/rare", r#"select ?s where ?s text (contains("rare"))"#);
    query(
        "scan/common",
        r#"select ?s where ?s text (contains("common"))"#,
    );
    group.finish();
    drop(g);
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(benches, bench);
criterion_main!(benches);
