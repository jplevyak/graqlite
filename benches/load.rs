//! Loading a CSV of `GRAQLITE_BENCH_ROWS` (default 100 000) rows of three
//! columns, by `insert … from csv`, in memory and on disk, against the same
//! data as `insert` statements in one transaction.
//!
//! ```sh
//! GRAQLITE_BENCH_ROWS=1000000 cargo bench --bench load
//! ```

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use graqlite::graph::{self, Graph, Options};
use graqlite::graql::Session;

fn from_csv(g: &mut Graph, path: &str) {
    let mut session = Session::new();
    session.allow_files(true);
    let text = format!("insert ?p a person ; name ?name ; city ?city from csv {path:?}");
    session.execute(g, &text).unwrap();
    g.commit().unwrap();
}

fn bench(c: &mut Criterion) {
    let rows: u64 = std::env::var("GRAQLITE_BENCH_ROWS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(100_000);
    let dir = std::env::temp_dir().join(format!("graqlite-bench-load-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("people.csv");
    let mut text = String::from("p,name,city\n");
    let mut statements = String::new();
    for i in 0..rows {
        writeln!(text, "/p{i},name {i},/c{}", i % 1000).unwrap();
        writeln!(
            statements,
            "insert /p{i} a person ; name \"name {i}\" ; city /c{}",
            i % 1000
        )
        .unwrap();
    }
    fs::write(&csv, text).unwrap();
    let path = csv.to_str().unwrap().to_string();
    let disk = |g: &str| dir.join(g);
    let fresh = |prefix: &Path| {
        if graph::exists(prefix) {
            graph::remove(prefix).unwrap();
        }
        Graph::open(Some(prefix), Options::default()).unwrap()
    };

    let mut group = c.benchmark_group("load");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));
    group.bench_function(BenchmarkId::new("csv in memory", rows), |b| {
        b.iter(|| {
            let mut g = Graph::open(None, Options::default()).unwrap();
            from_csv(&mut g, &path);
        })
    });
    group.bench_function(BenchmarkId::new("csv on disk", rows), |b| {
        b.iter(|| from_csv(&mut fresh(&disk("csv.")), &path))
    });
    group.bench_function(BenchmarkId::new("statements in memory", rows), |b| {
        b.iter(|| {
            let mut g = Graph::open(None, Options::default()).unwrap();
            Session::new().execute(&mut g, &statements).unwrap();
            g.commit().unwrap();
        })
    });
    group.finish();
    fs::remove_dir_all(&dir).unwrap();
}

criterion_group!(benches, bench);
criterion_main!(benches);
