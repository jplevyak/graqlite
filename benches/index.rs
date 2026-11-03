//! Benchmarks of large index entries: the baseline for issue 011, which
//! stores large entries as B+ trees to count, stream and search them.
//!
//! The graph is `GRAQLITE_BENCH_PEOPLE` people, each with `type person`, a
//! `city` (one of ten) and an `age` (one of 90).  The default is 200 000.  That makes
//! the predicate entries for `type`, `city` and `age`, and the
//! predicate+object entry for `type person`, hold one id per person, and
//! `city paris` a tenth of them.
//!
//! ```sh
//! cargo bench --bench index
//! GRAQLITE_BENCH_PEOPLE=1000000 cargo bench --bench index
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use graqlite::Linkage;
use graqlite::graph::{Graph, Options};
use graqlite::query::{Query, Term};

/// The ids of the graph's values.
struct Data {
    g: Graph,
    people: u64,
    type_: u64,
    person: u64,
    city: u64,
    paris: u64,
    age: u64,
    /// An id a tenth of the way through the people.
    early: u64,
}

fn people() -> u64 {
    std::env::var("GRAQLITE_BENCH_PEOPLE")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(200_000)
}

fn load() -> Data {
    let people = people();
    let start = Instant::now();
    let mut g = Graph::open(None, Options::for_tuples(people * 4)).unwrap();
    let v = |g: &mut Graph, d: &str| g.value(d.as_bytes()).unwrap();
    let (type_, person, city, age) = (
        v(&mut g, "type"),
        v(&mut g, "person"),
        v(&mut g, "city"),
        v(&mut g, "age"),
    );
    let cities: Vec<u64> = [
        "paris", "rome", "oslo", "lima", "pune", "kyiv", "nice", "bern", "riga", "baku",
    ]
    .iter()
    .map(|c| v(&mut g, c))
    .collect();
    let ages: Vec<u64> = (1..=90).map(|a| v(&mut g, &a.to_string())).collect();
    let mut early = 0;
    for i in 0..people {
        let p = g.node().unwrap();
        g.link(p, type_, person).unwrap();
        g.link(p, city, cities[(i % 10) as usize]).unwrap();
        g.link(p, age, ages[(i % 90) as usize]).unwrap();
        if i == people / 10 {
            early = g.next_id();
        }
        if i % 100_000 == 99_999 {
            g.commit().unwrap();
        }
    }
    g.commit().unwrap();
    eprintln!(
        "index bench: {people} people, {} links, loaded in {:.1?}",
        g.stats().links(),
        start.elapsed()
    );
    Data {
        g,
        people,
        type_,
        person,
        city,
        paris: cities[0],
        age,
        early,
    }
}

/// The default reduction budget.
fn reduce_budget() -> u64 {
    graqlite::query::DEFAULT_REDUCTION_BUDGET
}

/// The rows of `q`, counted.
fn rows(q: &Query, g: &Graph) -> usize {
    q.plan(g).unwrap().execute(g).count()
}

fn bench(c: &mut Criterion) {
    let d = load();
    let g = &d.g;
    let mut group = c.benchmark_group("index");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));
    let n = d.people;

    // Counting a large entry: what the planner pays to estimate a pattern
    // with a large constant.
    group.bench_function(BenchmarkId::new("count predicate type", n), |b| {
        b.iter(|| black_box(g.link_count(Linkage::Predicate, d.type_)))
    });
    group.bench_function(BenchmarkId::new("count type person", n), |b| {
        b.iter(|| {
            black_box(
                g.property_count(d.person, Linkage::Object, d.type_)
                    .unwrap(),
            )
        })
    });

    // Reading a whole entry, as a lookup does today.
    group.bench_function(BenchmarkId::new("find predicate type", n), |b| {
        b.iter(|| black_box(g.links_at(Linkage::Predicate, [d.type_]).len()))
    });

    // A query that needs only its first rows.
    let mut first = Query::new();
    let p = first.var("p");
    first.pattern(p, d.type_, d.person).limit(10);
    group.bench_function(BenchmarkId::new("limit 10 of type person", n), |b| {
        b.iter(|| black_box(rows(&first, g)))
    });

    // Two large entries joined: people in paris.
    let mut both = Query::new();
    let p = both.var("p");
    both.pattern(p, d.type_, d.person)
        .pattern(p, d.city, d.paris);
    group.bench_function(BenchmarkId::new("type person and city paris", n), |b| {
        b.iter(|| black_box(rows(&both, g)))
    });

    // Planning alone, with large constants.
    let mut planned = Query::new();
    let (p, a) = (planned.var("p"), planned.var("a"));
    planned
        .pattern(p, d.type_, d.person)
        .pattern(p, d.age, Term::Var(a))
        .reduction_budget(0);
    group.bench_function(BenchmarkId::new("plan type person and age", n), |b| {
        b.iter(|| black_box(planned.plan(g).unwrap().estimated_cost()))
    });

    // `as of` early in the log: a tenth of the people.
    let mut early = Query::new();
    let p = early.var("p");
    early.pattern(p, d.type_, d.person).as_of(d.early);
    let expected = rows(&early, g);
    assert!(expected > 0 && (expected as u64) < n, "{expected}");
    group.bench_function(BenchmarkId::new("as of a tenth, type person", n), |b| {
        b.iter(|| black_box(rows(&early, g)))
    });
    group.bench_function(
        BenchmarkId::new("plan as of a tenth, type person", n),
        |b| b.iter(|| black_box(early.plan(g).unwrap().estimated_cost())),
    );
    let mut both_planned = both.clone();
    both_planned.reduction_budget(reduce_budget());
    group.bench_function(
        BenchmarkId::new("plan type person and city paris", n),
        |b| b.iter(|| black_box(both_planned.plan(g).unwrap().estimated_cost())),
    );

    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
