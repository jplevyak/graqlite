//! Traversals of a graph shaped like a program's: path queries, one step
//! from a set, loading and editing, each beside the same work on adjacency
//! arrays (CSR) built from the same edges.  See issue 040.
//!
//! The graph has `GRAQLITE_BENCH_DEFS` definitions (default 2 000) in 20
//! layers, each calling 3 of the next layer (`calls`), and 36 nodes per
//! definition joined by `dep`: each node to the one before it and to an
//! earlier one of its definition, and across each call (the caller's node
//! to the callee's last, the callee's first back to the caller's).  So
//! `dep`'s closure from most nodes is most of the graph, as a program's
//! dependence graph's is.  7 500 definitions gives about the graph of the
//! issue's measurement.
//!
//! ```sh
//! cargo bench --bench traversal
//! GRAQLITE_BENCH_DEFS=7500 cargo bench --bench traversal
//! ```

use std::collections::{BTreeSet, VecDeque};
use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use graqlite::graph::{Graph, Options};
use graqlite::query::{PathExpr, Query};

const LAYERS: u64 = 20;
const NODES: u64 = 36;
const SAMPLE: usize = 100;

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// The edges, by index: definitions `0..defs`, and node `k` of definition
/// `d` at `d * NODES + k`.
struct Edges {
    defs: u64,
    calls: Vec<(u32, u32)>,
    dep: Vec<(u32, u32)>,
}

fn edges() -> Edges {
    let defs: u64 = std::env::var("GRAQLITE_BENCH_DEFS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(2_000);
    let per_layer = defs / LAYERS;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut calls, mut dep) = (BTreeSet::new(), BTreeSet::new());
    for d in 0..defs {
        let node = |k: u64| (d * NODES + k) as u32;
        for k in 1..NODES {
            dep.insert((node(k), node(k - 1)));
            dep.insert((node(k), node(rng.below(k))));
        }
        let layer = d / per_layer;
        if layer + 1 < LAYERS {
            for _ in 0..3 {
                let callee = (layer + 1) * per_layer + rng.below(per_layer);
                calls.insert((d as u32, callee as u32));
                let site = node(1 + rng.below(NODES - 1));
                dep.insert((site, (callee * NODES + NODES - 1) as u32));
                dep.insert(((callee * NODES) as u32, site));
            }
        }
    }
    Edges {
        defs,
        calls: calls.into_iter().collect(),
        dep: dep.into_iter().collect(),
    }
}

/// Adjacency arrays: the targets of `i` are `targets[offsets[i]..offsets[i + 1]]`.
struct Csr {
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

impl Csr {
    fn new(n: usize, mut edges: Vec<(u32, u32)>) -> Csr {
        edges.sort_unstable();
        let mut offsets = vec![0u32; n + 1];
        for &(a, _) in &edges {
            offsets[a as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        Csr {
            offsets,
            targets: edges.into_iter().map(|(_, b)| b).collect(),
        }
    }

    fn of(&self, i: u32) -> &[u32] {
        &self.targets[self.offsets[i as usize] as usize..self.offsets[i as usize + 1] as usize]
    }

    /// The nodes reached from `start` by one or more edges, within `depth`.
    fn reach(&self, start: u32, depth: u32) -> usize {
        let mut seen = vec![false; self.offsets.len() - 1];
        let mut queue = VecDeque::from([(start, 0)]);
        let mut count = 0;
        while let Some((i, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for &j in self.of(i) {
                if !seen[j as usize] {
                    seen[j as usize] = true;
                    count += 1;
                    queue.push_back((j, d + 1));
                }
            }
        }
        count
    }
}

struct Arrays {
    calls: Csr,
    called: Csr,
    dep: Csr,
}

fn arrays(e: &Edges) -> Arrays {
    let rev = |v: &[(u32, u32)]| v.iter().map(|&(a, b)| (b, a)).collect();
    let (d, n) = (e.defs as usize, (e.defs * NODES) as usize);
    Arrays {
        calls: Csr::new(d, e.calls.clone()),
        called: Csr::new(d, rev(&e.calls)),
        dep: Csr::new(n, e.dep.clone()),
    }
}

struct Data {
    g: Graph,
    defs: Vec<u64>,
    nodes: Vec<u64>,
    calls: u64,
    dep: u64,
}

fn load(e: &Edges) -> Data {
    let links = (e.calls.len() + e.dep.len()) as u64;
    let mut g = Graph::open(None, Options::for_tuples(links)).unwrap();
    let calls = g.value(b"calls").unwrap();
    let dep = g.value(b"dep").unwrap();
    let defs: Vec<u64> = (0..e.defs).map(|_| g.node().unwrap()).collect();
    let nodes: Vec<u64> = (0..e.defs * NODES).map(|_| g.node().unwrap()).collect();
    for &(a, b) in &e.calls {
        g.link(defs[a as usize], calls, defs[b as usize]).unwrap();
    }
    for &(a, b) in &e.dep {
        g.link(nodes[a as usize], dep, nodes[b as usize]).unwrap();
    }
    g.commit().unwrap();
    Data {
        g,
        defs,
        nodes,
        calls,
        dep,
    }
}

/// The entries a path from `start` reaches.
fn reach(g: &Graph, start: u64, expr: PathExpr) -> usize {
    let mut q = Query::new();
    let x = q.var("x");
    q.group().path_expr(start, expr, x);
    q.select([x]);
    q.run(g).unwrap().len()
}

fn link(p: u64) -> Box<PathExpr> {
    Box::new(PathExpr::Link(p))
}

fn bench(c: &mut Criterion) {
    let e = edges();
    let links = e.calls.len() + e.dep.len();
    let a = arrays(&e);
    let data = load(&e);
    let g = &data.g;
    let mut rng = Rng(42);
    let some: Vec<usize> = (0..SAMPLE).map(|_| rng.below(e.defs) as usize).collect();
    // A definition of the first layer, and its first node: most of the
    // graph is below them.
    let top = 0usize;

    let mut group = c.benchmark_group("traversal");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(5));
    let id = |name: &str| BenchmarkId::new(name, links);

    group.bench_function(id("build, graqlite"), |b| {
        b.iter(|| black_box(load(&e).defs.len()))
    });
    group.bench_function(id("build, arrays"), |b| {
        b.iter(|| black_box(arrays(&e).dep.targets.len()))
    });

    // Callers within 3 links of each of 100 definitions: small results.
    let callers = || PathExpr::Repeat(Box::new(PathExpr::Inverse(link(data.calls))), 1, Some(3));
    let want: usize = some.iter().map(|&d| a.called.reach(d as u32, 3)).sum();
    let got: usize = some
        .iter()
        .map(|&d| reach(g, data.defs[d], callers()))
        .sum();
    assert_eq!(got, want);
    group.bench_function(id("100 x callers within 3, graqlite"), |b| {
        b.iter(|| {
            for &d in &some {
                black_box(reach(g, data.defs[d], callers()));
            }
        })
    });
    group.bench_function(id("100 x callers within 3, arrays"), |b| {
        b.iter(|| {
            for &d in &some {
                black_box(a.called.reach(d as u32, 3));
            }
        })
    });

    // The closure of `dep` from one node: a large result.
    let closure = || PathExpr::Repeat(link(data.dep), 1, None);
    let start = (top as u64 * NODES + NODES - 1) as usize;
    let want = a.dep.reach(start as u32, u32::MAX);
    assert_eq!(reach(g, data.nodes[start], closure()), want);
    println!("dep closure reaches {want} of {} nodes", data.nodes.len());
    group.bench_function(id("dep closure, graqlite"), |b| {
        b.iter(|| black_box(reach(g, data.nodes[start], closure())))
    });
    group.bench_function(id("dep closure, arrays"), |b| {
        b.iter(|| black_box(a.dep.reach(start as u32, u32::MAX)))
    });

    // Level by level over `calls` in both directions, a set at a time, as
    // a caller wanting each node's distance must.
    let levels = || {
        let mut seen = BTreeSet::from([data.defs[top]]);
        let mut frontier = vec![data.defs[top]];
        while !frontier.is_empty() {
            let mut next: Vec<u64> = g
                .follow_forward(data.calls, frontier.iter().copied())
                .iter()
                .copied()
                .collect();
            next.extend(
                g.follow_backward(data.calls, frontier.iter().copied())
                    .iter(),
            );
            frontier = next.into_iter().filter(|i| seen.insert(*i)).collect();
        }
        seen.len()
    };
    let array_levels = || {
        let mut seen = vec![false; e.defs as usize];
        seen[top] = true;
        let mut frontier = vec![top as u32];
        let mut count = 1;
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for &i in &frontier {
                for &j in a.calls.of(i).iter().chain(a.called.of(i)) {
                    if !seen[j as usize] {
                        seen[j as usize] = true;
                        count += 1;
                        next.push(j);
                    }
                }
            }
            frontier = next;
        }
        count
    };
    assert_eq!(levels(), array_levels());
    println!(
        "levels over calls reach {} of {} definitions",
        levels(),
        e.defs
    );
    group.bench_function(id("levels over calls, graqlite"), |b| {
        b.iter(|| black_box(levels()))
    });
    group.bench_function(id("levels over calls, arrays"), |b| {
        b.iter(|| black_box(array_levels()))
    });
    group.finish();

    // Replacing one definition's calls: delete, commit, link, commit.
    let mut group = c.benchmark_group("traversal edit");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    let mut data = load(&e);
    let callees: Vec<u64> = data
        .g
        .follow_forward(data.calls, [data.defs[top]])
        .iter()
        .copied()
        .collect();
    group.bench_function(id("replace one definition's calls"), |b| {
        b.iter_batched(
            || (),
            |()| {
                for &o in &callees {
                    data.g.delete_link(data.defs[top], data.calls, o).unwrap();
                }
                data.g.commit().unwrap();
                for &o in &callees {
                    data.g.link(data.defs[top], data.calls, o).unwrap();
                }
                data.g.commit().unwrap();
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
