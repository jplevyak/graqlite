//! A GraQL point query, `select ?c where $p a person ; city ?c`, for 1 000
//! people of `GRAQLITE_BENCH_PEOPLE` (default 40 000): parsed and run each
//! time, parsed once and bound each time, and prepared once; and lookups,
//! of many links and of a few.
//!
//! ```sh
//! cargo bench --bench graql
//! ```

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use graqlite::graph::{Graph, Options};
use graqlite::graql::{Param, Params, Session, parse};

const SAMPLE: u64 = 1_000;

fn bench(c: &mut Criterion) {
    let n: u64 = std::env::var("GRAQLITE_BENCH_PEOPLE")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(40_000);
    let mut g = Graph::open(None, Options::for_tuples(n * 3)).unwrap();
    let [type_, person, city] = ["type", "person", "city"].map(|d| g.value(d.as_bytes()).unwrap());
    let cities: Vec<u64> = (0..10)
        .map(|c| g.value(format!("c{c}").as_bytes()).unwrap())
        .collect();
    let mut people = Vec::new();
    for i in 0..n {
        let p = g.node().unwrap();
        g.link(p, type_, person).unwrap();
        g.link(p, city, cities[(i % 10) as usize]).unwrap();
        people.push(p);
    }
    g.commit().unwrap();
    let sample: Vec<u64> = (0..SAMPLE)
        .map(|i| people[(i * 7919 % n) as usize])
        .collect();
    let params: Vec<Params> = sample
        .iter()
        .map(|&p| Params::from([("p".to_string(), Param::Id(p))]))
        .collect();
    let text = "select ?c where $p a person ; city ?c";
    let statement = &parse(text).unwrap()[0];
    let mut s = Session::new();

    let mut group = c.benchmark_group("graql point query of 1000");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(3));
    group.bench_function(BenchmarkId::new("text", n), |b| {
        b.iter(|| {
            for &p in &sample {
                let text = format!("select ?c where #{p} a person ; city ?c");
                s.execute(&mut g, &text).unwrap();
            }
        })
    });
    group.bench_function(BenchmarkId::new("bound", n), |b| {
        b.iter(|| {
            for p in &params {
                s.run_with(&mut g, statement, p).unwrap();
            }
        })
    });
    let mut prepared = s.prepare(&g, statement).unwrap();
    group.bench_function(BenchmarkId::new("prepared", n), |b| {
        b.iter(|| {
            for p in &params {
                s.run_prepared(&mut g, &mut prepared, p).unwrap();
            }
        })
    });
    group.finish();

    // A lookup of a city's people: their city and type links, 4 000 rows.
    let mut group = c.benchmark_group("graql lookup");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    group.bench_function(BenchmarkId::new("a city's people", n), |b| {
        b.iter(|| s.execute(&mut g, "lookup ?p a person ; city c0").unwrap())
    });
    // A lookup per person, of 1 000.
    group.bench_function(BenchmarkId::new("1000 people's links", n), |b| {
        b.iter(|| {
            for &p in &sample {
                s.execute(&mut g, &format!("lookup #{p} a person ; city ?c"))
                    .unwrap();
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
