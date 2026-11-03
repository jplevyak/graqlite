//! The layers of a bind join's lookup of one row, timed apart: the table
//! lookup alone (a count), the entry read and collected, the log record,
//! and the live links of the entry; a point query, planned each time and
//! prepared; then a join of two large entries, with and without reduction,
//! and its planning alone.
//!
//! The graph is that of `benches/index.rs`: `GRAQLITE_BENCH_PEOPLE` people
//! (default 40 000), each with a `type`, a `city` and an `age`.  Each
//! benchmark does its work for 1 000 people, picked at random, so its time
//! divided by 1 000 is the time per row.
//!
//! ```sh
//! cargo bench --bench lookups
//! ```

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use graqlite::graph::{Graph, Options};
use graqlite::query::Query;
use graqlite::tuple_index::{self, BuiltinIndex, LinkKey, spo_key};

const SAMPLE: usize = 1_000;

struct Data {
    g: Graph,
    people: Vec<u64>,
    type_: u64,
    person: u64,
    city: u64,
    paris: u64,
    /// The link of `type person` of each person sampled.
    links: Vec<u64>,
}

fn load() -> Data {
    let n: u64 = std::env::var("GRAQLITE_BENCH_PEOPLE")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(40_000);
    let mut g = Graph::open(None, Options::for_tuples(n * 4)).unwrap();
    let v = |g: &mut Graph, d: &str| g.value(d.as_bytes()).unwrap();
    let (type_, person, city, age) = (
        v(&mut g, "type"),
        v(&mut g, "person"),
        v(&mut g, "city"),
        v(&mut g, "age"),
    );
    let cities: Vec<u64> = (0..10).map(|c| v(&mut g, &format!("c{c}"))).collect();
    let ages: Vec<u64> = (1..=90).map(|a| v(&mut g, &a.to_string())).collect();
    let mut all = Vec::new();
    for i in 0..n {
        let p = g.node().unwrap();
        g.link(p, type_, person).unwrap();
        g.link(p, city, cities[(i % 10) as usize]).unwrap();
        g.link(p, age, ages[(i % 90) as usize]).unwrap();
        all.push(p);
    }
    g.commit().unwrap();
    // A fixed pseudo-random sample.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let people: Vec<u64> = (0..SAMPLE)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            all[(x % n) as usize]
        })
        .collect();
    let links = people
        .iter()
        .map(|&p| g.find_link(p, type_, person).unwrap())
        .collect();
    Data {
        g,
        people,
        type_,
        person,
        city,
        paris: cities[0],
        links,
    }
}

fn bench(c: &mut Criterion) {
    let d = load();
    let g = &d.g;
    let n = g.stats().links();
    let spo = g.local().table(BuiltinIndex::Spo.table());
    let keys: Vec<_> = d
        .people
        .iter()
        .map(|&p| spo_key(p, d.type_, d.person))
        .collect();
    let mut group = c.benchmark_group("lookups of 1000");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(3));

    // The table's count of a key: one bucket read, nothing collected.
    group.bench_function(BenchmarkId::new("local table count", n), |b| {
        b.iter(|| keys.iter().map(|&k| spo.count(k)).sum::<u64>())
    });
    // The entry as `Ids`, as a bind join reads it.
    group.bench_function(BenchmarkId::new("local table seek_rev", n), |b| {
        b.iter(|| {
            keys.iter()
                .map(|&k| spo.seek_rev(k, u64::MAX).count())
                .sum::<usize>()
        })
    });
    // The same, collected into a set, as `find` does.
    group.bench_function(BenchmarkId::new("local table find", n), |b| {
        b.iter(|| keys.iter().map(|&k| spo.find(k).len()).sum::<usize>())
    });
    // Reading each link's record and its subject, predicate and object.
    group.bench_function(BenchmarkId::new("log record", n), |b| {
        b.iter(|| {
            d.links
                .iter()
                .map(|&id| {
                    let r = g.get(id).unwrap();
                    let l = r.as_link().unwrap();
                    l.subject() ^ l.predicate() ^ l.object() ^ u64::from(l.dead())
                })
                .fold(0, |a, x| a ^ x)
        })
    });
    // The whole lookup of the SPO's latest link: the table and the log.
    group.bench_function(BenchmarkId::new("find_link", n), |b| {
        b.iter(|| {
            d.people
                .iter()
                .filter_map(|&p| g.find_link(p, d.type_, d.person))
                .count()
        })
    });
    // The entry's live links, as a bind join finds them for a row.
    group.bench_function(BenchmarkId::new("live links of entry", n), |b| {
        b.iter(|| {
            d.people
                .iter()
                .map(|&p| {
                    let ids = tuple_index::links_up_to(
                        g.local(),
                        LinkKey::Spo(p, d.type_, d.person),
                        None,
                    );
                    g.live(&ids, None).len()
                })
                .sum::<usize>()
        })
    });
    // A small query per person, planned and run: what a point query costs.
    group.bench_function(BenchmarkId::new("query per person", n), |b| {
        let queries: Vec<Query> = d
            .people
            .iter()
            .map(|&p| {
                let mut q = Query::new();
                let c = q.var("c");
                q.pattern(p, d.type_, d.person).pattern(p, d.city, c);
                q
            })
            .collect();
        b.iter(|| {
            queries
                .iter()
                .map(|q| q.run(g).unwrap().len())
                .sum::<usize>()
        })
    });
    // The same query prepared once, with the person a parameter: what a
    // point query costs without planning.
    let mut q = Query::new();
    let (who, city) = (q.param("p"), q.var("c"));
    q.pattern(who, d.type_, d.person).pattern(who, d.city, city);
    let mut prepared = q.prepare(g).unwrap();
    eprintln!("{}", prepared.plan().explain());
    group.bench_function(BenchmarkId::new("prepared query per person", n), |b| {
        b.iter(|| {
            d.people
                .iter()
                .map(|&p| prepared.run(g, &[p]).unwrap().len())
                .sum::<usize>()
        })
    });
    group.finish();

    // The join itself, per row: people in paris and of type person.
    let mut both = Query::new();
    let p = both.var("p");
    both.pattern(p, d.type_, d.person)
        .pattern(p, d.city, d.paris);
    let plan = both.plan(g).unwrap();
    eprintln!("{}", plan.explain());
    let rows = plan.execute(g).count();
    let mut group = c.benchmark_group("join");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    group.bench_function(BenchmarkId::new(format!("{rows} rows"), n), |b| {
        b.iter(|| black_box(both.plan(g).unwrap().execute(g).count()))
    });
    group.bench_function(BenchmarkId::new("planning alone", n), |b| {
        b.iter(|| black_box(both.plan(g).unwrap().estimated_cost()))
    });
    let mut bind = both.clone();
    bind.reduction_budget(0);
    let plan = bind.plan(g).unwrap();
    eprintln!("{}", plan.explain());
    group.bench_function(
        BenchmarkId::new(format!("{rows} rows, not reduced"), n),
        |b| b.iter(|| black_box(bind.plan(g).unwrap().execute(g).count())),
    );
    group.bench_function(BenchmarkId::new("planning alone, not reduced", n), |b| {
        b.iter(|| black_box(bind.plan(g).unwrap().estimated_cost()))
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
