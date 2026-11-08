use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use super::plan::{self, Access, Join, Step};
use super::*;
use crate::graph::Options;
use crate::record::Linkage;

fn graph() -> Graph {
    Graph::open(None, Options::default()).unwrap()
}

/// A small deterministic random number generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// Every live link (as of `as_of`): its id, subject, predicate and object.
fn live_links(g: &Graph, as_of: Option<u64>) -> Vec<(u64, [u64; 3])> {
    let predicates: Vec<u64> = g.stats().predicates().map(|(p, _)| p).collect();
    let mut links = Vec::new();
    for p in predicates {
        for id in g.live(&g.links_at(Linkage::Predicate, [p]), as_of) {
            let l = g.get(id).unwrap().as_link().unwrap().fields();
            links.push((id, [l.subject, l.predicate, l.object]));
        }
    }
    // In id order, not the statistics' order of predicates, so that random
    // tests picking links from them are the same each run.
    links.sort_unstable();
    links
}

/// The live links beneath `overlay` (as of `as_of`, for the graph's): the
/// graph's the overlay has no link of, if it is over the graph, and the
/// overlay's live ones.
fn layered_links(g: &Graph, overlay: Option<&Overlay>, as_of: Option<u64>) -> Vec<(u64, [u64; 3])> {
    let Some(ov) = overlay else {
        return live_links(g, as_of);
    };
    let mut out: Vec<(u64, [u64; 3])> = if ov.over_database() {
        live_links(g, as_of)
            .into_iter()
            .filter(|&(_, spo)| ov.fact(spo).is_none())
            .collect()
    } else {
        Vec::new()
    };
    out.extend(ov.links().filter(|l| l.live).map(|l| (l.id, l.spo)));
    out.sort_unstable();
    out
}

/// Evaluates queries by trying every combination of links.
struct BruteForce<'a> {
    q: &'a Query,
    g: &'a Graph,
    /// The overlay read over the graph, if any.
    overlay: Option<&'a Overlay>,
    links: HashMap<Option<u64>, Vec<(u64, [u64; 3])>>,
    /// The tuples of each relation, once evaluated.
    relations: HashMap<Relation, HashSet<Vec<u64>>>,
}

type Partial = Vec<Option<u64>>;

impl BruteForce<'_> {
    fn links(&mut self, as_of: Option<u64>) -> Vec<(u64, [u64; 3])> {
        let (g, overlay) = (self.g, self.overlay);
        self.links
            .entry(as_of)
            .or_insert_with(|| layered_links(g, overlay, as_of))
            .clone()
    }

    fn full(row: &Partial) -> Vec<u64> {
        row.iter().map(|v| v.unwrap_or(0)).collect()
    }

    fn group(&mut self, group: &Group, row: &Partial) -> Vec<Partial> {
        let mut rows = vec![row.clone()];
        for p in &group.patterns {
            let links = self.links(p.as_of.or(self.q.as_of));
            let mut next = Vec::new();
            for row in &rows {
                for &(id, spo) in &links {
                    let mut new = row.clone();
                    let fields = [
                        (Some(p.subject), spo[0]),
                        (Some(p.predicate), spo[1]),
                        (Some(p.object), spo[2]),
                        (p.link.map(Term::Var), id),
                    ];
                    let ok = fields.iter().all(|&(term, value)| match term {
                        None => true,
                        Some(Term::Id(c)) => c == value,
                        Some(Term::Var(v)) => match new[v.0] {
                            Some(bound) => bound == value,
                            None => {
                                new[v.0] = Some(value);
                                true
                            }
                        },
                    });
                    if !ok {
                        continue;
                    }
                    // The link's author and subgraphs, read from its record,
                    // or the overlay's.
                    let (author, subgraphs) = match self.overlay.and_then(|o| o.link(id)) {
                        Some(l) => (l.author, l.subgraphs.clone()),
                        None => {
                            let record = self.g.get(id).unwrap();
                            let link = record.as_link().unwrap();
                            (link.author(), link.subgraphs())
                        }
                    };
                    if let Some(t) = p.author
                        && (author == 0 || !bind(&mut new, [(t, author)]))
                    {
                        continue;
                    }
                    match p.graph {
                        None => next.push(new),
                        Some(t) => {
                            for sg in subgraphs {
                                let mut with = new.clone();
                                if bind(&mut with, [(t, sg)]) {
                                    next.push(with);
                                }
                            }
                        }
                    }
                }
            }
            rows = next;
        }
        for path in &group.paths {
            let links = self.links(path.as_of.or(self.q.as_of));
            let mut next = Vec::new();
            if let Some(expr) = &path.expr {
                for row in &rows {
                    // The ends a row binds can be joined to themselves.
                    let known = [path.subject, path.object].map(|t| match t {
                        Term::Id(id) => Some(id),
                        Term::Var(v) => row[v.0],
                    });
                    for (s, o) in expr_pairs(&links, expr, known.into_iter().flatten()) {
                        let mut new = row.clone();
                        if bind(&mut new, [(path.subject, s), (path.object, o)]) {
                            next.push(new);
                        }
                    }
                }
                rows = next;
                continue;
            }
            for row in &rows {
                for (s, p, o) in closure(&links) {
                    let mut new = row.clone();
                    if bind(
                        &mut new,
                        [(path.subject, s), (path.predicate, p), (path.object, o)],
                    ) {
                        next.push(new);
                    }
                }
            }
            rows = next;
        }
        for call in &group.calls {
            let tuples: Vec<Vec<u64>> = self.relations[&call.relation].iter().cloned().collect();
            let mut next = Vec::new();
            for row in &rows {
                for t in &tuples {
                    let mut new = row.clone();
                    if bind(&mut new, call.args.iter().copied().zip(t.iter().copied())) {
                        next.push(new);
                    }
                }
            }
            rows = next;
        }
        for branches in &group.unions {
            let mut next = Vec::new();
            for row in &rows {
                for b in branches {
                    next.extend(self.group(b, row));
                }
            }
            rows = next;
        }
        // Each optional group extends a row by each match, or leaves its
        // variables unbound: 0, as the engine does.
        for optional in &group.optionals {
            let mut next = Vec::new();
            for row in &rows {
                let matches = self.group(optional, row);
                if matches.is_empty() {
                    let mut kept = row.clone();
                    for v in optional.maybe_vars() {
                        kept[v.0].get_or_insert(0);
                    }
                    next.push(kept);
                } else {
                    next.extend(matches);
                }
            }
            rows = next;
        }
        let g = self.g;
        rows.retain(|r| group.filters.iter().all(|f| f.eval(g, &Self::full(r), &[])));
        rows.retain(|r| group.nots.iter().all(|n| self.group(n, r).is_empty()));
        rows
    }

    /// The query's rows, selected and made distinct, but not ordered or
    /// limited.
    /// Evaluates the relations naively, in levels: a relation is above
    /// those it calls within a negation.
    fn evaluate(&mut self) {
        let q = self.q;
        let n = q.relations.len();
        let mut level = vec![0usize; n];
        for _ in 0..=n {
            for rule in &q.rules {
                let mut deps = Vec::new();
                rules_dependencies(&rule.body, false, &mut deps);
                for (r, negated) in deps {
                    let l = level[r.0] + usize::from(negated);
                    level[rule.relation.0] = level[rule.relation.0].max(l);
                }
            }
        }
        for r in 0..n {
            self.relations.insert(Relation(r), HashSet::new());
        }
        for l in 0..=n {
            loop {
                let mut added = false;
                for rule in q.rules.iter().filter(|r| level[r.relation.0] == l) {
                    for row in self.group(&rule.body, &vec![None; q.names.len()]) {
                        let t: Vec<u64> = rule.head.iter().map(|v| row[v.0].unwrap()).collect();
                        added |= self.relations.get_mut(&rule.relation).unwrap().insert(t);
                    }
                }
                if !added {
                    break;
                }
            }
        }
    }

    fn rows(q: &Query, g: &Graph) -> Vec<Vec<u64>> {
        Self::rows_over(q, g, None)
    }

    /// The same, beneath an overlay.
    fn rows_over(q: &Query, g: &Graph, overlay: Option<&Overlay>) -> Vec<Vec<u64>> {
        let mut bf = BruteForce {
            q,
            g,
            overlay,
            links: HashMap::new(),
            relations: HashMap::new(),
        };
        bf.evaluate();
        let rows = bf.group(&q.root, &vec![None; q.names.len()]);
        let mut out: Vec<Vec<u64>> = rows
            .iter()
            // A union branch not taken leaves its hidden link variables
            // unbound: 0, as optional groups' are.
            .map(|r| q.selected().iter().map(|v| r[v.0].unwrap_or(0)).collect())
            .collect();
        if q.distinct {
            out.sort();
            out.dedup();
        }
        out
    }
}

/// Binds `fields` in `row`: constants and variables already bound must
/// match.
fn bind(row: &mut Partial, fields: impl IntoIterator<Item = (Term, u64)>) -> bool {
    fields.into_iter().all(|(term, value)| match term {
        Term::Id(c) => c == value,
        Term::Var(v) => match row[v.0] {
            Some(bound) => bound == value,
            None => {
                row[v.0] = Some(value);
                true
            }
        },
    })
}

/// The pairs of entries a path expression joins, naively: each part as a
/// relation, composed.  An empty path joins each entry of the links, and
/// `extra`, to itself.
fn expr_pairs(
    links: &[(u64, [u64; 3])],
    expr: &PathExpr,
    extra: impl IntoIterator<Item = u64>,
) -> BTreeSet<(u64, u64)> {
    let mut universe: BTreeSet<u64> = links.iter().flat_map(|&(_, [s, _, o])| [s, o]).collect();
    universe.extend(extra);
    fn compose(a: &BTreeSet<(u64, u64)>, b: &BTreeSet<(u64, u64)>) -> BTreeSet<(u64, u64)> {
        let mut out = BTreeSet::new();
        for &(x, m) in a {
            for &(m2, y) in b.range((m, 0)..=(m, u64::MAX)) {
                debug_assert_eq!(m, m2);
                out.insert((x, y));
            }
        }
        out
    }
    fn pairs(
        links: &[(u64, [u64; 3])],
        universe: &BTreeSet<u64>,
        expr: &PathExpr,
    ) -> BTreeSet<(u64, u64)> {
        match expr {
            PathExpr::Link(p) => links
                .iter()
                .filter(|&&(_, [_, lp, _])| lp == *p)
                .map(|&(_, [s, _, o])| (s, o))
                .collect(),
            PathExpr::Inverse(e) => pairs(links, universe, e)
                .into_iter()
                .map(|(s, o)| (o, s))
                .collect(),
            PathExpr::Seq(es) => {
                let mut out: BTreeSet<(u64, u64)> = universe.iter().map(|&x| (x, x)).collect();
                for e in es {
                    out = compose(&out, &pairs(links, universe, e));
                }
                out
            }
            PathExpr::Alt(es) => es.iter().flat_map(|e| pairs(links, universe, e)).collect(),
            PathExpr::Repeat(e, min, max) => {
                let step = pairs(links, universe, e);
                let mut power: BTreeSet<(u64, u64)> = universe.iter().map(|&x| (x, x)).collect();
                let mut out = BTreeSet::new();
                let mut k = 0;
                loop {
                    if k >= *min {
                        let before = out.len();
                        out.extend(power.iter().copied());
                        // Unbounded: until no more pairs are found.
                        if max.is_none() && k > *min && out.len() == before {
                            break;
                        }
                    }
                    if max.is_some_and(|m| k >= m) || (max.is_none() && k > *min + 64) {
                        break;
                    }
                    power = compose(&power, &step);
                    k += 1;
                }
                out
            }
        }
    }
    pairs(links, &universe, expr)
}

/// Every (subject, predicate, object) joined by a chain of one or more
/// links with the predicate, naively.
fn closure(links: &[(u64, [u64; 3])]) -> BTreeSet<(u64, u64, u64)> {
    let mut pairs: BTreeSet<(u64, u64, u64)> =
        links.iter().map(|&(_, [s, p, o])| (s, p, o)).collect();
    loop {
        let mut next = pairs.clone();
        for &(s, p, m) in &pairs {
            for &(_, [m2, p2, o]) in links {
                if m2 == m && p2 == p {
                    next.insert((s, p, o));
                }
            }
        }
        if next.len() == pairs.len() {
            return pairs;
        }
        pairs = next;
    }
}

/// The relations `group` calls, and whether within a negation.
fn rules_dependencies(group: &Group, negated: bool, out: &mut Vec<(Relation, bool)>) {
    out.extend(group.calls.iter().map(|c| (c.relation, negated)));
    for g in group.unions.iter().flatten() {
        rules_dependencies(g, negated, out);
    }
    for g in &group.nots {
        rules_dependencies(g, true, out);
    }
}

fn sorted(mut rows: Vec<Vec<u64>>) -> Vec<Vec<u64>> {
    rows.sort();
    rows
}

/// Compares selected rows by the query's sort keys, which must be selected.
fn compare(q: &Query, g: &Graph, a: &[u64], b: &[u64]) -> Ordering {
    let selected = q.selected();
    for k in &q.order {
        let i = selected.iter().position(|&v| v == k.var).unwrap();
        let (x, y) = (a[i], b[i]);
        let mut ord = if k.by_data {
            aggregate::compare_cells(g, &aggregate::Cell::of_id(x), &aggregate::Cell::of_id(y))
                .then(x.cmp(&y))
        } else {
            x.cmp(&y)
        };
        if k.descending {
            ord = ord.reverse();
        }
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Checks `rows` of `q` against `expected`, all of its rows unordered and
/// without the limit.
fn check_rows(q: &Query, g: &Graph, rows: Vec<Vec<u64>>, expected: &[Vec<u64>], what: &str) {
    let ordered = |rows: &[Vec<u64>]| {
        rows.windows(2)
            .all(|w| compare(q, g, &w[0], &w[1]) != Ordering::Greater)
    };
    assert!(ordered(&rows), "not ordered: {q:?}\n{what}");
    match (q.limit, q.offset) {
        (None, 0) => assert_eq!(sorted(rows), sorted(expected.to_vec()), "{q:?}\n{what}"),
        (limit, offset) => {
            let n = expected
                .len()
                .saturating_sub(offset)
                .min(limit.unwrap_or(usize::MAX));
            assert_eq!(rows.len(), n, "{q:?}\n{what}");
            for r in &rows {
                assert!(expected.contains(r), "{q:?}\n{what}");
            }
            if !q.order.is_empty() {
                // The rows are those at their places in order, up to ties.
                let mut all = expected.to_vec();
                all.sort_by(|a, b| compare(q, g, a, b));
                for (r, e) in rows.iter().zip(&all[offset.min(all.len())..]) {
                    assert_eq!(compare(q, g, r, e), Ordering::Equal, "{q:?}\n{what}");
                }
            }
        }
    }
}

/// Checks `q` against brute force, as planned and with each join used
/// wherever it can be.
fn check(q: &Query, g: &Graph) {
    check_against(q, g, &BruteForce::rows(q, g));
}

/// Checks the rows of every plan of `q` against `expected`.
fn check_against(q: &Query, g: &Graph, expected: &[Vec<u64>]) {
    // With a full reduction, none, and one stopped early.
    for budget in [None, Some(0), Some(3)] {
        let mut q = q.clone();
        if let Some(b) = budget {
            q.reduction_budget(b);
        }
        for force in [None, Some(Join::Bind), Some(Join::Hash)] {
            let plan = plan::plan_with(&q, g, force).unwrap();
            let rows: Vec<Vec<u64>> = plan.execute(g).collect::<Result<_>>().unwrap();
            check_rows(&q, g, rows, expected, &plan.explain());
        }
    }
}

/// A random graph: nodes, predicates, and links among them and about links.
fn random_graph(rng: &mut Rng) -> (Graph, Vec<u64>, Vec<u64>) {
    random_graph_of(rng, 150)
}

/// The same, with `n` links appended or deleted.
fn random_graph_of(rng: &mut Rng, n: usize) -> (Graph, Vec<u64>, Vec<u64>) {
    let mut g = graph();
    let nodes: Vec<u64> = (0..7).map(|_| g.node().unwrap()).collect();
    let predicates: Vec<u64> = (0..4)
        .map(|i| g.value(format!("p{i}").as_bytes()).unwrap())
        .collect();
    let mut links = Vec::new();
    for _ in 0..n {
        let s = if rng.chance(10) && !links.is_empty() {
            rng.pick(&links)
        } else {
            rng.pick(&nodes)
        };
        let (p, o) = (rng.pick(&predicates), rng.pick(&nodes));
        if rng.chance(20) {
            g.delete_link(s, p, o).unwrap();
        } else {
            links.push(g.link(s, p, o).unwrap());
        }
    }
    (g, nodes, predicates)
}

/// Makes random queries over a graph.
struct Generator<'a> {
    rng: &'a mut Rng,
    nodes: &'a [u64],
    predicates: &'a [u64],
    ids: (u64, u64),
}

impl Generator<'_> {
    /// A term: one of `vars`, a new variable added to them, or a constant.
    fn term(&mut self, q: &mut Query, vars: &mut Vec<Var>, consts: &[u64], percent: u64) -> Term {
        if !self.rng.chance(percent) {
            return Term::Id(self.rng.pick(consts));
        }
        if vars.is_empty() || (vars.len() < 4 && self.rng.chance(40)) {
            vars.push(q.var(&format!("v{}", q.names.len())));
        }
        Term::Var(self.rng.pick(vars))
    }

    fn pattern(&mut self, q: &mut Query, group: &mut Group, vars: &mut Vec<Var>) {
        let (nodes, predicates) = (self.nodes, self.predicates);
        let s = self.term(q, vars, nodes, 70);
        let p = self.term(q, vars, predicates, 15);
        let o = self.term(q, vars, nodes, 70);
        if self.rng.chance(10) {
            group.path(s, p, o);
            return;
        }
        if self.rng.chance(10) {
            let expr = self.path_expr(2);
            group.path_expr(s, expr, o);
            return;
        }
        group.pattern(s, p, o);
        if self.rng.chance(10) {
            group.patterns.last_mut().unwrap().link = Some(q.var("link"));
        }
        if self.rng.chance(10) {
            let (start, end) = self.ids;
            group.at(start + self.rng.next() % (end - start));
        }
    }

    /// A random path expression that is never empty, nested to `depth`.
    fn path_expr(&mut self, depth: usize) -> PathExpr {
        let predicates = self.predicates;
        if depth == 0 || self.rng.chance(35) {
            return PathExpr::Link(self.rng.pick(predicates));
        }
        match self.rng.below(5) {
            0 => PathExpr::Inverse(Box::new(self.path_expr(depth - 1))),
            1 => PathExpr::Seq(vec![self.path_expr(depth - 1), self.path_expr(depth - 1)]),
            2 => PathExpr::Alt(vec![self.path_expr(depth - 1), self.path_expr(depth - 1)]),
            3 => PathExpr::Repeat(Box::new(self.path_expr(depth - 1)), 1, None),
            _ => {
                let min = 1 + self.rng.below(2) as u32;
                PathExpr::Repeat(
                    Box::new(self.path_expr(depth - 1)),
                    min,
                    Some(min + self.rng.below(2) as u32),
                )
            }
        }
    }

    /// Random recursive rules, and a call to them in `root`.
    fn rules(&mut self, q: &mut Query, root: &mut Group, vars: &mut Vec<Var>) {
        let (nodes, predicates) = (self.nodes, self.predicates);
        let r = q.relation("r", 2);
        let s = q.relation("s", 2);
        let (a, b, c) = (q.var("a"), q.var("b"), q.var("c"));
        let mut base = Group::new();
        base.pattern(a, self.rng.pick(predicates), b);
        q.rule(r, [a, b], base);
        // Recursion to the left, to the right, or both, possibly through s.
        let mut step = Group::new();
        match self.rng.below(3) {
            0 => step
                .call(r, [a, c])
                .pattern(c, self.rng.pick(predicates), b),
            1 => step
                .pattern(a, self.rng.pick(predicates), c)
                .call(r, [c, b]),
            _ => step.call(r, [a, c]).call(r, [c, b]),
        };
        if self.rng.chance(40) {
            q.rule(s, [a, b], step);
            let mut via = Group::new();
            via.call(s, [a, b]);
            q.rule(r, [a, b], via);
        } else {
            q.rule(r, [a, b], step);
        }
        // A relation defined by a negation of r.
        let t = q.relation("t", 1);
        let mut unreached = Group::new();
        let mut reached = Group::new();
        reached.call(r, [Term::Id(self.rng.pick(nodes)), a.into()]);
        unreached
            .pattern(a, self.rng.pick(predicates), b)
            .not(reached);
        q.rule(t, [a], unreached);
        let x = self.term(q, vars, nodes, 80);
        let y = self.term(q, vars, nodes, 90);
        root.call(if self.rng.chance(70) { r } else { s }, [x, y]);
        if self.rng.chance(30) {
            root.call(t, [y]);
        }
    }

    /// A group binding exactly `vars`.
    fn branch(&mut self, q: &mut Query, vars: &[Var]) -> Group {
        let mut b = Group::new();
        for _ in 0..1 + self.rng.below(2) {
            let mut within = vars.to_vec();
            let (s, o) = (self.rng.pick(&within), self.rng.pick(&within));
            let s = if self.rng.chance(70) {
                Term::Var(s)
            } else {
                Term::Id(self.rng.pick(self.nodes))
            };
            let o = if self.rng.chance(70) {
                Term::Var(o)
            } else {
                Term::Id(self.rng.pick(self.nodes))
            };
            b.pattern(s, self.rng.pick(self.predicates), o);
            within.clear();
        }
        for &v in vars {
            if !b.bound_vars().contains(&v) {
                b.pattern(v, self.rng.pick(self.predicates), self.rng.pick(self.nodes));
            }
        }
        if vars.len() == 2 && self.rng.chance(30) {
            b.filter(Filter::Ne(vars[0].into(), vars[1].into()));
        }
        if self.rng.chance(20) {
            b.not(self.negation(q, vars));
        }
        b
    }

    /// An optional group using some of `outer` and its own variables, with
    /// optional groups within it to `depth`.
    fn optional(&mut self, q: &mut Query, outer: &[Var], depth: usize) -> Group {
        let mut o = Group::new();
        let mut vars: Vec<Var> = outer.to_vec();
        vars.push(q.var("opt"));
        for _ in 0..1 + self.rng.below(2) {
            let s = self.term(q, &mut vars, self.nodes, 70);
            let p = self.rng.pick(self.predicates);
            let obj = self.term(q, &mut vars, self.nodes, 70);
            o.pattern(s, p, obj);
        }
        if self.rng.chance(20) && !outer.is_empty() {
            o.filter(Filter::Ne(
                self.rng.pick(outer).into(),
                self.rng.pick(self.nodes).into(),
            ));
        }
        if self.rng.chance(15) {
            let bound: Vec<Var> = o.bound_vars().into_iter().collect();
            o.not(self.negation(q, &bound));
        }
        if depth > 1 && self.rng.chance(30) {
            let bound: Vec<Var> = o.bound_vars().into_iter().collect();
            let inner = self.optional(q, &bound, depth - 1);
            o.optional(inner);
        }
        o
    }

    /// A negated group using some of `outer` and its own variable.
    fn negation(&mut self, q: &mut Query, outer: &[Var]) -> Group {
        let mut n = Group::new();
        let mut vars: Vec<Var> = outer.to_vec();
        if self.rng.chance(40) || vars.is_empty() {
            vars.push(q.var("own"));
        }
        for _ in 0..1 + self.rng.below(2) {
            let s = self.term(q, &mut vars.clone(), self.nodes, 60);
            let o = self.term(q, &mut vars.clone(), self.nodes, 60);
            n.pattern(s, self.rng.pick(self.predicates), o);
        }
        if self.rng.chance(20) && !outer.is_empty() {
            n.filter(Filter::Ne(
                self.rng.pick(outer).into(),
                self.rng.pick(self.nodes).into(),
            ));
        }
        n
    }

    fn query(&mut self) -> Query {
        let mut q = Query::new();
        let mut root = Group::new();
        let mut vars = Vec::new();
        for _ in 0..1 + self.rng.below(3) {
            self.pattern(&mut q, &mut root, &mut vars);
        }
        if self.rng.chance(30) {
            let mut union_vars: Vec<Var> = Vec::new();
            for _ in 0..1 + self.rng.below(2) {
                if vars.is_empty() || self.rng.chance(40) {
                    vars.push(q.var("u"));
                }
                let v = self.rng.pick(&vars);
                if !union_vars.contains(&v) {
                    union_vars.push(v);
                }
            }
            let branches = (0..2)
                .map(|_| self.branch(&mut q, &union_vars))
                .collect::<Vec<_>>();
            root.union(branches);
        }
        if self.rng.chance(20) {
            self.rules(&mut q, &mut root, &mut vars);
        }
        if self.rng.chance(30) {
            let bound: Vec<Var> = root.bound_vars().into_iter().collect();
            root.not(self.negation(&mut q, &bound));
        }
        if self.rng.chance(35) {
            let bound: Vec<Var> = root.bound_vars().into_iter().collect();
            let optional = self.optional(&mut q, &bound, 2);
            let maybe: Vec<Var> = optional
                .maybe_vars()
                .into_iter()
                .filter(|v| !bound.contains(v))
                .collect();
            root.optional(optional);
            // Filters and negations of the optional's variables, which may
            // be unbound.
            if !maybe.is_empty() && self.rng.chance(40) {
                let v = self.rng.pick(&maybe);
                root.filter(Filter::Ne(v.into(), self.rng.pick(self.nodes).into()));
            }
            if !maybe.is_empty() && self.rng.chance(30) {
                root.not(self.negation(&mut q, &maybe));
            }
        }
        let bound: Vec<Var> = root.bound_vars().into_iter().collect();
        if bound.len() >= 2 && self.rng.chance(30) {
            let (a, b) = (self.rng.pick(&bound), self.rng.pick(&bound));
            root.filter(if self.rng.chance(50) {
                Filter::Ne(a.into(), b.into())
            } else {
                Filter::Lt(a.into(), b.into())
            });
        }
        q.root = root;
        if !bound.is_empty() && self.rng.chance(30) {
            let selected: Vec<Var> = bound
                .iter()
                .copied()
                .filter(|_| self.rng.chance(50))
                .collect();
            if !selected.is_empty() {
                q.select(selected).distinct();
            }
        }
        if self.rng.chance(30) {
            let selected = q.selected();
            if !selected.is_empty() {
                let key = SortKey::id(self.rng.pick(&selected));
                q.order_by(if self.rng.chance(50) { key.desc() } else { key });
            }
        }
        if self.rng.chance(20) {
            q.limit(self.rng.below(5));
        }
        if self.rng.chance(15) {
            q.offset(self.rng.below(4));
        }
        if self.rng.chance(20) {
            let (start, end) = self.ids;
            q.as_of(start + self.rng.next() % (end - start));
        }
        q
    }
}

#[test]
fn random_queries() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut rows, mut hash) = (0, 0);
    // Queries whose optional groups left a variable unbound, and that were
    // joined each way when planned.
    let (mut unbound, mut optional_bind, mut optional_hash) = (0, 0, 0);
    // Queries with path expressions that matched something.
    let mut path_exprs = 0;
    for _ in 0..8 {
        let (g, nodes, predicates) = random_graph(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..120 {
            let q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            check(&q, &g);
            rows += usize::from(!BruteForce::rows(&q, &g).is_empty());
            let plan = q.plan(&g).unwrap();
            hash += usize::from(plan.explain().contains("hash join"));
            if q.root.paths.iter().any(|p| p.expr.is_some()) {
                path_exprs += usize::from(!BruteForce::rows(&q, &g).is_empty());
            }
            if q.root.has_optionals() {
                let expected = BruteForce::rows(&q, &g);
                unbound += usize::from(expected.iter().any(|r| r.contains(&0)));
                let explain = plan.explain();
                optional_bind += usize::from(explain.contains("optional (for the rows)"));
                optional_hash += usize::from(explain.contains("optional (once"));
            }
        }
    }
    // The queries are not trivial.
    assert!(rows > 200, "{rows}");
    assert!(hash > 20, "{hash}");
    assert!(unbound > 20, "{unbound}");
    assert!(path_exprs > 20, "{path_exprs}");
    assert!(
        optional_bind > 10 && optional_hash > 10,
        "{optional_bind} {optional_hash}"
    );
}

fn first_step(plan: &Plan) -> (&Pattern, Access) {
    match &plan.root.steps[0] {
        Step::Pattern {
            pattern, access, ..
        } => (pattern, *access),
        step => panic!("{step:?}"),
    }
}

#[test]
fn chain_and_triangle() {
    let mut g = graph();
    let knows = g.value(b"knows").unwrap();
    let n: Vec<u64> = (0..6).map(|_| g.node().unwrap()).collect();
    for i in 0..6 {
        g.link(n[i], knows, n[(i + 1) % 6]).unwrap();
    }
    g.link(n[0], knows, n[2]).unwrap();
    g.link(n[2], knows, n[0]).unwrap();

    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(n[0], knows, x).pattern(x, knows, y).select([y]);
    assert_eq!(sorted(q.run(&g).unwrap()), [[n[0]], [n[2]], [n[3]]]);
    check(&q, &g);

    let mut q = Query::new();
    let (x, y, z) = (q.var("x"), q.var("y"), q.var("z"));
    q.pattern(x, knows, y)
        .pattern(y, knows, z)
        .pattern(z, knows, x)
        .filter(Filter::Lt(x.into(), y.into()))
        .filter(Filter::Lt(x.into(), z.into()));
    assert_eq!(q.run(&g).unwrap(), [[n[0], n[1], n[2]]]);
    check(&q, &g);
}

#[test]
fn planner_picks_selective_pattern_first() {
    let mut g = graph();
    let (kind, person, name) = (
        g.value(b"type").unwrap(),
        g.value(b"Person").unwrap(),
        g.value(b"name").unwrap(),
    );
    let mut bob = 0;
    for i in 0..300 {
        let n = g.node().unwrap();
        g.link(n, kind, person).unwrap();
        let v = g.value(format!("person{i}").as_bytes()).unwrap();
        g.link(n, name, v).unwrap();
        if i == 123 {
            bob = n;
        }
    }
    let bob_name = g.find_value(b"person123").unwrap();
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(x, kind, person).pattern(x, name, bob_name);
    // Not reduced by default, as the plan below costs less than reducing.
    assert!(q.plan(&g).unwrap().reduction.is_none());
    // Reduced first: by name, then the one person's type.
    q.reduction_budget(reduce::DEFAULT_REDUCTION_BUDGET);
    let plan = q.plan(&g).unwrap();
    assert!(
        plan.reduction.as_ref().unwrap().entries < 10,
        "{}",
        plan.explain()
    );
    assert_eq!(plan.execute(&g).count(), 1);
    q.reduction_budget(0);
    let plan = q.plan(&g).unwrap();
    let (pattern, access) = first_step(&plan);
    assert_eq!(pattern.object, Term::Id(bob_name), "{}", plan.explain());
    assert_eq!(access, Access::PredicateObject);
    assert!(plan.estimated_rows() < 10.0, "{}", plan.explain());
    assert!(
        plan.explain().starts_with("scan (?x "),
        "{}",
        plan.explain()
    );
    assert_eq!(q.run(&g).unwrap(), [[bob]]);
}

#[test]
fn large_results_and_limit() {
    let mut g = graph();
    let (p, q_) = (g.value(b"p").unwrap(), g.value(b"q").unwrap());
    let hub = g.node().unwrap();
    for _ in 0..3000 {
        let n = g.node().unwrap();
        g.link(hub, p, n).unwrap();
        g.link(n, q_, hub).unwrap();
    }
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(hub, p, x).pattern(x, q_, y);
    for force in [None, Some(Join::Bind), Some(Join::Hash)] {
        let plan = plan::plan_with(&q, &g, force).unwrap();
        let rows: Vec<Vec<u64>> = plan.execute(&g).collect::<Result<_>>().unwrap();
        assert_eq!(rows.len(), 3000);
        assert!(rows.iter().all(|r| r[1] == hub));
    }

    // Distinct and ordered.
    let mut q2 = q.clone();
    q2.select([y]).distinct();
    assert_eq!(q2.run(&g).unwrap(), [[hub]]);
    let mut q2 = q.clone();
    q2.order_by(SortKey::id(x).desc()).limit(3);
    let rows = q2.run(&g).unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows[0][0] > rows[1][0] && rows[1][0] > rows[2][0]);

    q.limit(5);
    assert_eq!(q.run(&g).unwrap().len(), 5);
    q.limit(0);
    assert!(q.run(&g).unwrap().is_empty());
    // Taking a few rows stops early.
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(hub, p, x);
    assert_eq!(q.plan(&g).unwrap().execute(&g).take(3).count(), 3);
}

#[test]
fn constants_links_and_filters() {
    let mut g = graph();
    let (a, b) = (g.node().unwrap(), g.node().unwrap());
    let (p, said, age) = (
        g.value(b"p").unwrap(),
        g.value(b"said").unwrap(),
        g.value(b"age").unwrap(),
    );
    let l = g.link(a, p, b).unwrap();
    let carol = g.node().unwrap();
    g.link(carol, said, l).unwrap();
    let v40 = g.value(b"40").unwrap();
    let v7 = g.value(b"7").unwrap();
    g.link(a, age, v40).unwrap();
    g.link(b, age, v7).unwrap();

    // A query with no variables: does the link hold?
    let mut q = Query::new();
    q.pattern(a, p, b);
    assert_eq!(q.run(&g).unwrap(), [Vec::<u64>::new()]);
    let mut q = Query::new();
    q.pattern(b, p, a);
    assert!(q.run(&g).unwrap().is_empty());

    // Who said that a p b?
    let mut q = Query::new();
    let (link, who) = (q.var("link"), q.var("who"));
    q.link_pattern(link, a, p, b)
        .pattern(who, said, link)
        .select([who]);
    assert_eq!(q.run(&g).unwrap(), [[carol]]);
    check(&q, &g);

    // After deleting it, nobody: the link pattern matches live links only.
    g.delete_link(a, p, b).unwrap();
    assert!(q.run(&g).unwrap().is_empty());
    check(&q, &g);

    // Tests of values.
    let mut q = Query::new();
    let (x, v) = (q.var("x"), q.var("v"));
    let adult: Test = Arc::new(|g: &Graph, id| {
        g.data(id)
            .and_then(|d| std::str::from_utf8(&d).ok()?.parse::<u32>().ok())
            .is_some_and(|n| n >= 18)
    });
    q.pattern(x, age, v)
        .filter(Filter::Test(v, adult))
        .select([x]);
    assert_eq!(q.run(&g).unwrap(), [[a]]);
    let mut q2 = Query::new();
    let (x, v) = (q2.var("x"), q2.var("v"));
    q2.pattern(x, age, v)
        .filter(Filter::Eq(v.into(), v7.into()));
    assert_eq!(q2.run(&g).unwrap(), [[b, v7]]);
    // A filter without variables.
    q2.filter(Filter::Eq(Term::Id(1), Term::Id(2)));
    assert!(q2.run(&g).unwrap().is_empty());
    let explain = q2.plan(&g).unwrap().explain();
    assert!(explain.lines().any(|l| l == "filter 1 = 2"), "{explain}");
}

#[test]
fn repeated_variables_and_predicate_variables() {
    let mut g = graph();
    let (a, b) = (g.node().unwrap(), g.node().unwrap());
    let (p, r) = (g.value(b"p").unwrap(), g.value(b"r").unwrap());
    g.link(a, p, a).unwrap();
    g.link(a, p, b).unwrap();
    g.link(a, r, b).unwrap();

    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(x, p, x);
    assert_eq!(q.run(&g).unwrap(), [[a]]);
    check(&q, &g);

    // Which predicates link a to b?
    let mut q = Query::new();
    let pred = q.var("pred");
    q.pattern(a, pred, b);
    assert_eq!(sorted(q.run(&g).unwrap()), [[p], [r]]);
    // A scan of every link.
    let mut q = Query::new();
    let (s, pred, o) = (q.var("s"), q.var("pred"), q.var("o"));
    q.pattern(s, pred, o);
    assert_eq!(first_step(&q.plan(&g).unwrap()).1, Access::All);
    assert_eq!(q.run(&g).unwrap().len(), 3);
    check(&q, &g);
}

/// People with names, some with nicknames, some banned, some with ages.
fn people() -> (Graph, Vec<u64>, [u64; 5]) {
    let mut g = graph();
    let [name, nick, banned, age, yes] =
        [&b"name"[..], b"nick", b"banned", b"age", b"yes"].map(|d| g.value(d).unwrap());
    let mut people = Vec::new();
    for (i, n) in ["carol", "alice", "dave", "bob"].iter().enumerate() {
        let p = g.node().unwrap();
        let v = g.value(n.as_bytes()).unwrap();
        g.link(p, name, v).unwrap();
        if i % 2 == 0 {
            let v = g.value(format!("{n}-nick").as_bytes()).unwrap();
            g.link(p, nick, v).unwrap();
        }
        if i == 2 {
            g.link(p, banned, yes).unwrap();
        }
        if i < 3 {
            let v = g.value(format!("{}", 20 + i).as_bytes()).unwrap();
            g.link(p, age, v).unwrap();
        }
        people.push(p);
    }
    (g, people, [name, nick, banned, age, yes])
}

#[test]
fn unions() {
    let (g, people, [name, nick, ..]) = people();
    // Every name or nickname.
    let mut q = Query::new();
    let (x, n) = (q.var("x"), q.var("n"));
    let mut by_name = Group::new();
    by_name.pattern(x, name, n);
    let mut by_nick = Group::new();
    by_nick.pattern(x, nick, n);
    q.union([by_name, by_nick]).order_by(SortKey::data(n));
    let rows = q.run(&g).unwrap();
    let data: Vec<Vec<u8>> = rows
        .iter()
        .map(|r| g.data(r[1]).unwrap().into_owned())
        .collect();
    assert_eq!(
        data,
        [
            &b"alice"[..],
            b"bob",
            b"carol",
            b"carol-nick",
            b"dave",
            b"dave-nick"
        ]
    );
    check(&q, &g);

    // A union joined to a pattern; each person once.
    let mut q2 = q.clone();
    let full_name = q2.var("name");
    q2.pattern(x, name, full_name);
    check(&q2, &g);
    q2.select([x]).distinct();
    assert_eq!(
        sorted(q2.run(&g).unwrap()),
        people.iter().map(|&p| vec![p]).collect::<Vec<_>>()
    );

    // Branches must bind the same variables.
    let mut q = Query::new();
    let (x, n) = (q.var("x"), q.var("n"));
    let mut a = Group::new();
    a.pattern(x, name, n);
    let mut b = Group::new();
    b.pattern(x, nick, 1);
    q.union([a, b]);
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
    let mut q = Query::new();
    q.union([]);
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
}

#[test]
fn negation() {
    let (g, people, [name, nick, banned, age, yes]) = people();
    // People who are not banned.
    let mut q = Query::new();
    let (x, n) = (q.var("x"), q.var("n"));
    let mut is_banned = Group::new();
    is_banned.pattern(x, banned, yes);
    q.pattern(x, name, n).not(is_banned).select([x]);
    let expect: Vec<Vec<u64>> = [0, 1, 3].iter().map(|&i| vec![people[i]]).collect();
    assert_eq!(sorted(q.run(&g).unwrap()), expect);
    check(&q, &g);
    for force in [Join::Bind, Join::Hash] {
        let plan = plan::plan_with(&q, &g, Some(force)).unwrap();
        let how = if force == Join::Bind {
            "for the rows"
        } else {
            "once"
        };
        assert!(plan.explain().contains(how), "{}", plan.explain());
    }

    // People without a nickname, using the negation's own variable.
    let mut q = Query::new();
    let (x, n, any) = (q.var("x"), q.var("n"), q.var("any"));
    let mut has_nick = Group::new();
    has_nick.pattern(x, nick, any);
    q.pattern(x, name, n).not(has_nick).select([x]);
    let expect: Vec<Vec<u64>> = [1, 3].iter().map(|&i| vec![people[i]]).collect();
    assert_eq!(sorted(q.run(&g).unwrap()), expect);
    check(&q, &g);

    // A negation with a filter using an outer variable: people with no
    // younger person.  It can only be evaluated for the rows.
    let mut q = Query::new();
    let (x, a, y, b) = (q.var("x"), q.var("a"), q.var("y"), q.var("b"));
    let mut younger = Group::new();
    younger
        .pattern(y, age, b)
        .filter(Filter::Lt(b.into(), a.into()));
    q.pattern(x, age, a).not(younger).select([x]);
    // Values were appended in age order, so the youngest has the least id.
    assert_eq!(q.run(&g).unwrap(), [[people[0]]]);
    check(&q, &g);
    let plan = plan::plan_with(&q, &g, Some(Join::Hash)).unwrap();
    assert!(
        plan.explain().contains("for the rows"),
        "{}",
        plan.explain()
    );

    // A negation sharing no variables removes all rows or none.
    let mut q = Query::new();
    let x = q.var("x");
    let mut anyone_banned = Group::new();
    anyone_banned.pattern(Term::Var(q.var("y")), banned, yes);
    let n = q.var("n");
    q.pattern(x, name, n).not(anyone_banned);
    assert!(q.run(&g).unwrap().is_empty());
    check(&q, &g);
}

#[test]
fn pattern_as_of() {
    let mut g = graph();
    let (a, b, c) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
    let knows = g.value(b"knows").unwrap();
    g.link(a, knows, b).unwrap();
    let then = g.next_id() - 1;
    g.link(a, knows, c).unwrap();
    g.delete_link(a, knows, b).unwrap();

    // Who does a know now that it did not then?
    let mut q = Query::new();
    let x = q.var("x");
    let mut before = Group::new();
    before.pattern(a, knows, x).at(then);
    q.pattern(a, knows, x).not(before);
    assert_eq!(q.run(&g).unwrap(), [[c]]);
    check(&q, &g);
    // And who did a know then that it does not now?
    let mut q = Query::new();
    let x = q.var("x");
    let mut now = Group::new();
    now.pattern(a, knows, x);
    q.pattern(a, knows, x).at(then).not(now);
    assert_eq!(q.run(&g).unwrap(), [[b]]);
    assert!(
        q.plan(&g)
            .unwrap()
            .explain()
            .contains(&format!("as of {then}"))
    );
    check(&q, &g);
}

#[test]
fn many_patterns_are_planned_greedily() {
    let mut g = graph();
    let next = g.value(b"next").unwrap();
    let n: Vec<u64> = (0..20).map(|_| g.node().unwrap()).collect();
    for w in n.windows(2) {
        g.link(w[0], next, w[1]).unwrap();
    }
    let mut q = Query::new();
    let vars: Vec<Var> = (0..=EXHAUSTIVE_PATTERNS + 2)
        .map(|i| q.var(&format!("v{i}")))
        .collect();
    q.pattern(n[0], next, vars[0]);
    for w in vars.windows(2) {
        q.pattern(w[0], next, w[1]);
    }
    assert!(q.patterns().len() > EXHAUSTIVE_PATTERNS);
    let rows = q.run(&g).unwrap();
    assert_eq!(rows, [n[1..=vars.len()].to_vec()]);
}

#[test]
fn errors() {
    let g = graph();
    assert!(matches!(
        Query::new().plan(&g),
        Err(Error::InvalidArgument(_))
    ));
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, 1, 2).filter(Filter::Ne(x.into(), y.into()));
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, 1, 2).select([y]);
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, 1, 2).order_by(SortKey::id(y));
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
    // A negation must have a pattern.
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(x, 1, 2).not(Group::new());
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
}

#[test]
fn reduction() {
    let mut g = graph();
    let (kind, person, knows, city) = (
        g.value(b"type").unwrap(),
        g.value(b"Person").unwrap(),
        g.value(b"knows").unwrap(),
        g.value(b"city").unwrap(),
    );
    let paris = g.value(b"Paris").unwrap();
    // Many people, each knowing the next; only a few in Paris.
    let people: Vec<u64> = (0..500).map(|_| g.node().unwrap()).collect();
    for (i, &p) in people.iter().enumerate() {
        g.link(p, kind, person).unwrap();
        g.link(p, knows, people[(i + 1) % people.len()]).unwrap();
        if i % 100 == 0 {
            g.link(p, city, paris).unwrap();
        }
    }
    // People in Paris who know a person who knows a person.
    let mut q = Query::new();
    let (x, y, z) = (q.var("x"), q.var("y"), q.var("z"));
    q.pattern(x, city, paris)
        .pattern(x, knows, y)
        .pattern(y, knows, z)
        .pattern(z, kind, person);
    // Joining from Paris costs less than reducing: not reduced by default.
    assert!(q.plan(&g).unwrap().reduction.is_none());
    check(&q, &g);
    q.reduction_budget(reduce::DEFAULT_REDUCTION_BUDGET);
    let plan = q.plan(&g).unwrap();
    let explain = plan.explain();
    // The reduction narrows every variable to the 5 people in Paris and
    // those they reach, without reading the 500 type links.
    assert!(
        explain.starts_with("reduce: ?x 5, ?y 5, ?z 5 candidates"),
        "{explain}"
    );
    let r = plan.reduction.as_ref().unwrap();
    assert!(r.complete && r.entries < 100, "{explain}");
    assert!(explain.contains("5 reduced links"), "{explain}");
    assert_eq!(plan.execute(&g).count(), 5);
    check(&q, &g);

    // A small budget stops it early; the rows are the same.
    q.reduction_budget(8);
    let plan = q.plan(&g).unwrap();
    assert!(
        plan.explain().contains("stopped at budget"),
        "{}",
        plan.explain()
    );
    assert!(!plan.reduction.as_ref().unwrap().complete);
    assert_eq!(plan.execute(&g).count(), 5);

    // A plan whose graph has changed does not use its reduction.
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(x, city, paris);
    let plan = q.plan(&g).unwrap();
    assert_eq!(plan.execute(&g).count(), 5);
    g.link(people[1], city, paris).unwrap();
    assert_eq!(plan.execute(&g).count(), 6);

    // An empty reduction: nobody in Paris knows themselves.
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(x, city, paris)
        .pattern(x, knows, x)
        .reduction_budget(reduce::DEFAULT_REDUCTION_BUDGET);
    let plan = q.plan(&g).unwrap();
    assert!(plan.explain().contains("?x 0"), "{}", plan.explain());
    assert_eq!(plan.execute(&g).count(), 0);

    // Filters on one variable narrow its candidates.
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(x, city, paris)
        .pattern(x, kind, person)
        .filter(Filter::Ne(x.into(), people[0].into()))
        .reduction_budget(reduce::DEFAULT_REDUCTION_BUDGET);
    let explain = q.plan(&g).unwrap().explain();
    assert!(explain.starts_with("reduce: ?x 5"), "{explain}");
    assert_eq!(q.run(&g).unwrap().len(), 5);
    // One pattern alone is not reduced: there is nothing to narrow it by.
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(x, city, paris);
    assert!(!q.plan(&g).unwrap().explain().starts_with("reduce"));
    assert_eq!(q.run(&g).unwrap().len(), 6);
}

/// The rows of `q` on the entries of `g` committed before `at`.
fn committed_rows(q: &Query, g: &Graph, at: u64) -> Vec<Vec<u64>> {
    let mut q = q.clone();
    q.as_of(at - 1);
    sorted(BruteForce::rows(&q, g))
}

/// Random changes to a random graph: new links and deletions.
fn mutate(rng: &mut Rng, g: &mut Graph, nodes: &[u64], predicates: &[u64]) {
    for _ in 0..3 + rng.below(12) {
        let (s, p, o) = (rng.pick(nodes), rng.pick(predicates), rng.pick(nodes));
        if rng.chance(35) {
            g.delete_link(s, p, o).unwrap();
        } else {
            let l = g.link(s, p, o).unwrap();
            if rng.chance(10) {
                g.link(l, rng.pick(predicates), rng.pick(nodes)).unwrap();
            }
        }
    }
}

#[test]
fn standing_queries() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut changed, mut unions, mut nots) = (0, 0, 0);
    for _ in 0..6 {
        let (mut g, nodes, predicates) = random_graph(&mut rng);
        g.commit().unwrap();
        let ids = (g.start_id(), g.next_id());
        // Queries with optional groups or paths are skipped, so more are
        // made.
        for _ in 0..80 {
            let mut q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            q.limit = None;
            q.offset = 0;
            q.order.clear();
            q.as_of = None;
            if q.root.has_paths_or_calls() || q.root.has_optionals() {
                assert!(StandingQuery::new(&q, &g).is_err());
                continue;
            }
            let mut standing = StandingQuery::new(&q, &g).unwrap();
            assert_eq!(standing.rows(), committed_rows(&q, &g, standing.at()));
            for _ in 0..4 {
                let before = standing.rows();
                mutate(&mut rng, &mut g, &nodes, &predicates);
                let committed = rng.chance(75);
                if committed {
                    g.commit().unwrap();
                }
                let changes = standing.update(&g).unwrap();
                let after = standing.rows();
                assert_eq!(after, committed_rows(&q, &g, standing.at()), "{q:?}");
                if !committed {
                    assert!(changes.is_empty());
                }
                // The changes are the difference.
                let mut expect = before.clone();
                for (row, n) in &changes.removed {
                    for _ in 0..*n {
                        let i = expect.iter().position(|r| r == row).expect("removed row");
                        expect.remove(i);
                    }
                }
                for (row, n) in &changes.added {
                    expect.extend(std::iter::repeat_n(row.clone(), *n as usize));
                }
                assert_eq!(sorted(expect), after, "{q:?}");
                if !changes.is_empty() {
                    changed += 1;
                    unions += usize::from(!q.root.unions.is_empty());
                    nots += usize::from(!q.root.nots.is_empty());
                }
            }
            g.commit().unwrap();
        }
    }
    // The changes are not trivial.
    assert!(
        changed > 50 && unions > 5 && nots > 5,
        "{changed} {unions} {nots}"
    );
}

#[test]
fn standing_query_changes() {
    let mut g = graph();
    let knows = g.value(b"knows").unwrap();
    let blocked = g.value(b"blocked").unwrap();
    let [a, b, c, d] = [(); 4].map(|_| g.node().unwrap());
    g.link(a, knows, b).unwrap();
    g.commit().unwrap();

    // Friends of friends of a, not blocked by a.
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    let mut is_blocked = Group::new();
    is_blocked.pattern(a, blocked, y);
    q.pattern(a, knows, x)
        .pattern(x, knows, y)
        .not(is_blocked)
        .select([y]);
    let mut s = StandingQuery::new(&q, &g).unwrap();
    assert!(s.rows().is_empty());

    g.link(b, knows, c).unwrap();
    g.link(b, knows, d).unwrap();
    // Not committed yet.
    assert!(s.update(&g).unwrap().is_empty());
    g.commit().unwrap();
    let changes = s.update(&g).unwrap();
    assert_eq!(changes.added, [(vec![c], 1), (vec![d], 1)]);
    assert!(changes.removed.is_empty());

    // Blocking d removes it; deleting a's link to b removes c too.
    g.link(a, blocked, d).unwrap();
    g.commit().unwrap();
    assert_eq!(s.update(&g).unwrap().removed, [(vec![d], 1)]);
    g.delete_link(a, knows, b).unwrap();
    g.commit().unwrap();
    assert_eq!(s.update(&g).unwrap().removed, [(vec![c], 1)]);
    assert!(s.rows().is_empty());

    // Unblocking d while knowing b again brings back both.
    g.link(a, knows, b).unwrap();
    g.delete_link(a, blocked, d).unwrap();
    g.commit().unwrap();
    assert_eq!(s.update(&g).unwrap().added, [(vec![c], 1), (vec![d], 1)]);

    // A distinct query reports a row once, while any of its matches remain.
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, knows, y).select([x]).distinct();
    let mut s = StandingQuery::new(&q, &g).unwrap();
    assert_eq!(s.rows(), [[a], [b]]);
    g.delete_link(b, knows, c).unwrap();
    g.commit().unwrap();
    assert!(s.update(&g).unwrap().is_empty());
    g.delete_link(b, knows, d).unwrap();
    g.commit().unwrap();
    assert_eq!(s.update(&g).unwrap().removed, [(vec![b], 1)]);

    // What a standing query can not keep.
    let mut q2 = q.clone();
    q2.limit(1);
    assert!(StandingQuery::new(&q2, &g).is_err());
    let mut q2 = q.clone();
    q2.order_by(SortKey::id(x));
    assert!(StandingQuery::new(&q2, &g).is_err());
    let mut q2 = q.clone();
    q2.as_of(1);
    assert!(StandingQuery::new(&q2, &g).is_err());
}

/// A graph of `n` nodes with `knows` from each to the next, a cycle back
/// from the last to `back`, and `likes` from the first to the last.
fn chain(n: usize, back: usize) -> (Graph, Vec<u64>, u64, u64) {
    let mut g = graph();
    let (knows, likes) = (g.value(b"knows").unwrap(), g.value(b"likes").unwrap());
    let nodes: Vec<u64> = (0..n).map(|_| g.node().unwrap()).collect();
    for w in nodes.windows(2) {
        g.link(w[0], knows, w[1]).unwrap();
    }
    g.link(nodes[n - 1], knows, nodes[back]).unwrap();
    g.link(nodes[0], likes, nodes[n - 1]).unwrap();
    (g, nodes, knows, likes)
}

#[test]
fn paths() {
    let (g, n, knows, likes) = chain(5, 2);
    let ids = |rows: Vec<Vec<u64>>| sorted(rows);
    // From a bound subject: everyone reachable, including the cycle.
    let mut q = Query::new();
    let y = q.var("y");
    q.path(n[0], knows, y);
    assert_eq!(
        ids(q.run(&g).unwrap()),
        ids(n[1..].iter().map(|&x| vec![x]).collect())
    );
    check(&q, &g);
    // To a bound object: everyone who reaches it.
    let mut q = Query::new();
    let x = q.var("x");
    q.path(x, knows, n[1]);
    assert_eq!(q.run(&g).unwrap(), [[n[0]]]);
    check(&q, &g);
    // Both bound, and a node reaching itself only through the cycle.
    let mut q = Query::new();
    q.path(n[3], knows, n[2]);
    assert_eq!(q.run(&g).unwrap().len(), 1);
    let mut q = Query::new();
    let x = q.var("x");
    q.path(x, knows, x);
    assert_eq!(ids(q.run(&g).unwrap()), [[n[2]], [n[3]], [n[4]]]);
    check(&q, &g);
    // Neither bound, with a variable predicate, and joined to a pattern.
    let mut q = Query::new();
    let (x, p, y) = (q.var("x"), q.var("p"), q.var("y"));
    q.path(x, p, y);
    // n[0] reaches 4 nodes and the others 3 each, and one likes link.
    assert_eq!(q.run(&g).unwrap().len(), 4 + 4 * 3 + 1);
    check(&q, &g);
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, likes, y).path(y, knows, x);
    assert!(q.run(&g).unwrap().is_empty());
    q.root.patterns[0].object = Term::Id(n[4]);
    let plan = q.plan(&g).unwrap();
    assert!(
        plan.explain().contains("++ ?x), rows"),
        "{}",
        plan.explain()
    );
    check(&q, &g);
    // In a negation: who can not reach n[1]?
    let mut q = Query::new();
    let (x, z) = (q.var("x"), q.var("z"));
    let mut reaches = Group::new();
    reaches.path(x, knows, n[1]);
    q.pattern(x, knows, z).not(reaches).select([x]);
    assert_eq!(ids(q.run(&g).unwrap()), [[n[1]], [n[2]], [n[3]], [n[4]]]);
    check(&q, &g);
}

#[test]
fn rules() {
    let (mut g, n, knows, likes) = chain(5, 2);
    let mut q = Query::new();
    let reach = q.relation("reach", 2);
    let (a, b, c) = (q.var("a"), q.var("b"), q.var("c"));
    let mut base = Group::new();
    base.pattern(a, knows, b);
    let mut step = Group::new();
    step.call(reach, [a, c]).pattern(c, knows, b);
    q.rule(reach, [a, b], base).rule(reach, [a, b], step);
    let (x, y) = (q.var("x"), q.var("y"));
    q.call(reach, [x, y]);
    // The same as the transitive path.
    let mut p = Query::new();
    let (x2, y2) = (p.var("x"), p.var("y"));
    p.path(x2, knows, y2);
    assert_eq!(sorted(q.run(&g).unwrap()), sorted(p.run(&g).unwrap()));
    let plan = q.plan(&g).unwrap();
    assert!(
        plan.explain().starts_with("relation reach: 16 tuples"),
        "{}",
        plan.explain()
    );
    check(&q, &g);

    // Mutual recursion: nodes an even and odd number of steps away.
    let mut q = Query::new();
    let (even, odd) = (q.relation("even", 1), q.relation("odd", 1));
    let (a, b) = (q.var("a"), q.var("b"));
    let mut zero = Group::new();
    zero.pattern(a, likes, b);
    q.rule(even, [a], zero);
    let mut from_even = Group::new();
    from_even.call(even, [b]).pattern(b, knows, a);
    q.rule(odd, [a], from_even);
    let mut from_odd = Group::new();
    from_odd.call(odd, [b]).pattern(b, knows, a);
    q.rule(even, [a], from_odd);
    let x = q.var("x");
    q.call(even, [x]).call(odd, [x]);
    // n[0] is even; on the cycle n[2] n[3] n[4] (odd length) both are.
    assert_eq!(sorted(q.run(&g).unwrap()), [[n[2]], [n[3]], [n[4]]]);
    check(&q, &g);

    // Stratified negation: nodes no path from n[3] reaches.
    let mut q = Query::new();
    let (reach, far) = (q.relation("reach", 1), q.relation("far", 1));
    let (a, b) = (q.var("a"), q.var("b"));
    let mut base = Group::new();
    base.pattern(n[3], knows, a);
    let mut step = Group::new();
    step.call(reach, [b]).pattern(b, knows, a);
    q.rule(reach, [a], base).rule(reach, [a], step);
    let mut unreached = Group::new();
    let mut reached = Group::new();
    reached.call(reach, [a]);
    unreached.pattern(a, knows, b).not(reached);
    q.rule(far, [a], unreached);
    let x = q.var("x");
    q.call(far, [x]);
    assert_eq!(sorted(q.run(&g).unwrap()), [[n[0]], [n[1]]]);
    check(&q, &g);

    // A plan made before the graph changed evaluates its rules again.
    let plan = q.plan(&g).unwrap();
    g.link(n[4], knows, n[0]).unwrap();
    assert!(plan.execute(&g).next().is_none());

    // Errors.
    let mut q = Query::new();
    let r = q.relation("r", 1);
    let a = q.var("a");
    let mut not_r = Group::new();
    let mut body = Group::new();
    not_r.call(r, [a]);
    body.pattern(a, knows, n[0]).not(not_r);
    q.rule(r, [a], body).call(r, [a]);
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
    let mut q = Query::new();
    let r = q.relation("r", 2);
    let a = q.var("a");
    q.call(r, [a]);
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
    let mut q = Query::new();
    let r = q.relation("r", 1);
    let (a, b) = (q.var("a"), q.var("b"));
    let mut body = Group::new();
    body.pattern(a, knows, n[0]);
    q.rule(r, [b], body).call(r, [a]);
    assert!(matches!(q.plan(&g), Err(Error::InvalidArgument(_))));
}

#[test]
fn explain_with_names() {
    let mut g = Graph::open(None, Options::default()).unwrap();
    let knows = g.value(b"knows").unwrap();
    let age = g.value(b"age").unwrap();
    let (a, b) = (g.node().unwrap(), g.node().unwrap());
    g.link(a, knows, b).unwrap();
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(a, knows, x)
        .pattern(x, age, y)
        .filter(Filter::Ne(Term::Var(y), Term::Id(age)));
    let plan = q.plan(&g).unwrap();
    let raw = plan.explain();
    assert!(raw.contains(&format!("({a} {knows} ?x)")), "{raw}");
    let named = plan.explain_with(&|id| match id {
        id if id == a => "/a".into(),
        id if id == knows => "knows".into(),
        id if id == age => "age".into(),
        id => format!("#{id}"),
    });
    assert!(named.contains("(/a knows ?x)"), "{named}");
    assert!(named.contains("(?x age ?y)"), "{named}");
    assert!(named.contains("filter ?y != age"), "{named}");
    // Only the naming differs.
    assert_eq!(named.lines().count(), raw.lines().count());
}

#[test]
fn optional_groups() {
    let mut g = graph();
    let [person, email, verified, a] =
        [&b"person"[..], b"email", b"verified", b"type"].map(|d| g.value(d).unwrap());
    let yes = g.value(b"yes").unwrap();
    let (ann, ben, cat) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
    let (e1, e2) = (g.value(b"ann@x").unwrap(), g.value(b"cat@y").unwrap());
    for p in [ann, ben, cat] {
        g.link(p, a, person).unwrap();
    }
    g.link(ann, email, e1).unwrap();
    g.link(cat, email, e2).unwrap();
    g.link(e2, verified, yes).unwrap();
    // Everyone, with their email if they have one.
    let mut q = Query::new();
    let (p, e, v) = (q.var("p"), q.var("e"), q.var("v"));
    let mut opt = Group::new();
    opt.pattern(p, email, e);
    q.pattern(p, a, person).optional(opt.clone()).select([p, e]);
    check(&q, &g);
    assert_eq!(
        sorted(q.run(&g).unwrap()),
        sorted(vec![vec![ann, e1], vec![ben, 0], vec![cat, e2]])
    );
    let explain = q.plan(&g).unwrap().explain();
    assert!(explain.contains("optional ("), "{explain}");
    // Nested: and whether the email is verified.
    let mut inner = Group::new();
    inner.pattern(e, verified, v);
    let mut nested = opt.clone();
    nested.optional(inner);
    let mut q2 = Query::new();
    let (p2, e2v, v2) = (q2.var("p"), q2.var("e"), q2.var("v"));
    assert_eq!((p2, e2v, v2), (p, e, v));
    q2.pattern(p, a, person).optional(nested).select([p, e, v]);
    check(&q2, &g);
    assert_eq!(
        sorted(q2.run(&g).unwrap()),
        sorted(vec![vec![ann, e1, 0], vec![ben, 0, 0], vec![cat, e2, yes]])
    );
    // A filter of an unbound variable fails; a negation of one holds.
    let mut q3 = Query::new();
    let (p3, e3) = (q3.var("p"), q3.var("e"));
    q3.pattern(p3, a, person)
        .optional(opt.clone())
        .filter(Filter::Ne(e3.into(), e1.into()))
        .select([p3]);
    assert_eq!(q3.run(&g).unwrap(), [[cat]]);
    let mut unverified = Group::new();
    unverified.pattern(e, verified, yes);
    let mut q4 = Query::new();
    let (p4, _e4) = (q4.var("p"), q4.var("e"));
    q4.pattern(p4, a, person)
        .optional(opt.clone())
        .not(unverified)
        .select([p4]);
    check(&q4, &g);
    assert_eq!(
        sorted(q4.run(&g).unwrap()),
        sorted(vec![vec![ann], vec![ben]])
    );
}

#[test]
fn optional_groups_in_rules_and_standing_queries() {
    let mut g = graph();
    let [knows, email] = [&b"knows"[..], b"email"].map(|d| g.value(d).unwrap());
    let (a, b, c) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
    let mail = g.value(b"b@x").unwrap();
    g.link(a, knows, b).unwrap();
    g.link(a, knows, c).unwrap();
    g.link(b, email, mail).unwrap();
    g.commit().unwrap();
    // A rule whose head has an optional variable.
    let mut q = Query::new();
    let r = q.relation("contact", 2);
    let (x, m) = (q.var("x"), q.var("m"));
    let mut opt = Group::new();
    opt.pattern(x, email, m);
    let mut body = Group::new();
    body.pattern(a, knows, x).optional(opt.clone());
    q.rule(r, [x, m], body);
    q.call(r, [x, m]);
    check(&q, &g);
    assert_eq!(
        sorted(q.run(&g).unwrap()),
        sorted(vec![vec![b, mail], vec![c, 0]])
    );
    // A relation depending on itself through an optional group is not
    // stratified.
    let mut q = Query::new();
    let r = q.relation("r", 1);
    let x = q.var("x");
    let mut inner = Group::new();
    inner.call(r, [x]);
    let mut body = Group::new();
    body.pattern(x, knows, b).optional(inner);
    q.rule(r, [x], body);
    q.call(r, [x]);
    assert!(q.validate().is_err());
    // Standing queries don't support them yet.
    let mut q = Query::new();
    let x = q.var("x");
    q.pattern(a, knows, x).optional(opt);
    let err = StandingQuery::new(&q, &g).unwrap_err();
    assert!(err.to_string().contains("optional"), "{err}");
}

#[test]
fn random_queries_on_large_entries() {
    // Enough links that the predicate, subject and object entries, and some
    // pairs, are promoted to trees and streamed, newest first; with links
    // deleted and linked again, so that liveness is read from the stream.
    let mut rng = Rng(0x51_7cc1_b727_220a);
    let mut streamed = 0;
    for _ in 0..3 {
        let (g, nodes, predicates) = random_graph_of(&mut rng, 1_500);
        let promoted = predicates
            .iter()
            .filter(|&&p| {
                matches!(
                    crate::tuple_index::links_newest_first(
                        g.local(),
                        crate::tuple_index::LinkKey::Linkage(Linkage::Predicate, p),
                        None
                    ),
                    crate::local_table::Ids::Tree(_)
                )
            })
            .count();
        assert!(promoted > 0);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..40 {
            let q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            check(&q, &g);
            streamed += usize::from(
                q.root
                    .patterns
                    .iter()
                    .any(|p| matches!(p.subject, Term::Var(_))),
            );
        }
    }
    assert!(streamed > 20, "{streamed}");
}

#[test]
fn random_queries_after_compaction() {
    // Compacting the large entries to a horizon changes no answer as of it
    // or later, through a second compaction and a snapshot, and refuses
    // queries as of earlier.
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut dropped = 0;
    for _ in 0..3 {
        let (mut g, nodes, predicates) = random_graph_of(&mut rng, 1_500);
        g.commit().unwrap();
        let (start, end) = (g.start_id(), g.next_id());
        let first = start + (end - start) / 3;
        let second = start + 2 * (end - start) / 3;
        let queries: Vec<(Query, Vec<Vec<u64>>)> = (0..40)
            .map(|_| {
                let q = Generator {
                    rng: &mut rng,
                    nodes: &nodes,
                    predicates: &predicates,
                    ids: (second, end),
                }
                .query();
                let expected = BruteForce::rows(&q, &g);
                (q, expected)
            })
            .collect();
        let mut early = Query::new();
        let (s, o) = (early.var("s"), early.var("o"));
        early.pattern(s, predicates[0], o);
        early.as_of(first - 1);
        assert!(early.plan(&g).is_ok());

        for horizon in [first, second] {
            let done = g.compact(horizon).unwrap();
            assert_eq!(g.horizon(), horizon);
            dropped += done.dropped;
            for (q, expected) in &queries {
                check_against(q, &g, expected);
            }
        }
        // An earlier horizon keeps the later one.
        g.compact(first).unwrap();
        assert_eq!(g.horizon(), second);
        let err = early.plan(&g).unwrap_err();
        assert!(matches!(err, Error::Compacted { .. }), "{err}");
        assert!(g.compact(g.next_id()).is_err());

        let mut snapshot = Vec::new();
        g.snapshot(&mut snapshot).unwrap();
        let restored =
            Graph::restore(None, crate::graph::Options::default(), &mut &snapshot[..]).unwrap();
        assert_eq!(restored.horizon(), second);
        for (q, expected) in &queries {
            check_against(q, &restored, expected);
        }
    }
    assert!(dropped > 500, "{dropped}");
}

/// `q` with some of the constants of its groups, not its rules, made
/// parameters, and the parameters' values.
fn parameterize(rng: &mut Rng, q: &Query) -> (Query, Vec<u64>) {
    fn term(rng: &mut Rng, q: &mut Query, values: &mut Vec<u64>, t: &mut Term) {
        if let Term::Id(id) = *t
            && rng.chance(50)
        {
            let n = values.len();
            *t = Term::Var(q.param(&format!("p{n}")));
            values.push(id);
        }
    }
    fn group(rng: &mut Rng, q: &mut Query, values: &mut Vec<u64>, g: &mut Group) {
        for p in &mut g.patterns {
            for t in [&mut p.subject, &mut p.predicate, &mut p.object] {
                term(rng, q, values, t);
            }
        }
        for p in &mut g.paths {
            term(rng, q, values, &mut p.subject);
            term(rng, q, values, &mut p.object);
        }
        for c in &mut g.calls {
            for t in &mut c.args {
                term(rng, q, values, t);
            }
        }
        for f in &mut g.filters {
            match f {
                Filter::Eq(a, b)
                | Filter::Ne(a, b)
                | Filter::Lt(a, b)
                | Filter::Compare(a, _, b) => {
                    term(rng, q, values, a);
                    term(rng, q, values, b);
                }
                Filter::Test(..) | Filter::Expr { .. } | Filter::Holds(_) => {}
            }
        }
        for branches in &mut g.unions {
            for b in branches {
                group(rng, q, values, b);
            }
        }
        for inner in g.nots.iter_mut().chain(&mut g.optionals) {
            group(rng, q, values, inner);
        }
    }
    let mut prepared = q.clone();
    // The selected variables, as they were, since parameters are not.
    let selected = prepared.selected();
    let mut root = std::mem::take(&mut prepared.root);
    let mut values = Vec::new();
    group(rng, &mut prepared, &mut values, &mut root);
    prepared.root = root;
    prepared.select(selected);
    (prepared, values)
}

#[test]
fn random_prepared_queries() {
    // A query with some constants made parameters, prepared once, answers
    // as the query does for each of its values; and run with another
    // query's values, as the other does.
    let mut rng = Rng(0x6a09_e667_f3bc_c908);
    let (mut params, mut rows) = (0, 0);
    for _ in 0..6 {
        let (g, nodes, predicates) = random_graph(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..80 {
            let q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            let (pq, values) = parameterize(&mut rng, &q);
            params += usize::from(!values.is_empty());
            let expected = BruteForce::rows(&q, &g);
            rows += usize::from(!expected.is_empty());
            let mut prepared = pq.prepare(&g).unwrap();
            let explain = prepared.plan().explain();
            assert!(values.is_empty() || prepared.plan().reduction.is_none());
            for force in [None, Some(Join::Bind), Some(Join::Hash)] {
                let plan = plan::plan_with(&pq, &g, force).unwrap();
                let got = plan
                    .execute_with(&g, &values)
                    .collect::<Result<_>>()
                    .unwrap();
                check_rows(&pq, &g, got, &expected, &plan.explain());
            }
            let got = prepared.run(&g, &values).unwrap();
            check_rows(&pq, &g, got, &expected, &explain);
            // Other values: those of the constants they replace, shuffled.
            let mut other = values.clone();
            other.reverse();
            let mut by_constants = pq.clone();
            let mut root = std::mem::take(&mut by_constants.root);
            substitute(&mut root, &pq.params, &other);
            by_constants.root = root;
            by_constants.params.clear();
            let expected = BruteForce::rows(&by_constants, &g);
            let got = prepared.run(&g, &other).unwrap();
            check_rows(&pq, &g, got, &expected, &explain);
        }
    }
    assert!(params > 300, "{params}");
    assert!(rows > 150, "{rows}");
}

/// Replaces each of `params` in `g` by its value.
fn substitute(g: &mut Group, params: &[Var], values: &[u64]) {
    let term = |t: &mut Term| {
        if let Term::Var(v) = *t
            && let Some(k) = params.iter().position(|&p| p == v)
        {
            *t = Term::Id(values[k]);
        }
    };
    for p in &mut g.patterns {
        for t in [&mut p.subject, &mut p.predicate, &mut p.object] {
            term(t);
        }
    }
    for p in &mut g.paths {
        term(&mut p.subject);
        term(&mut p.object);
    }
    for c in &mut g.calls {
        c.args.iter_mut().for_each(term);
    }
    for f in &mut g.filters {
        if let Filter::Eq(a, b) | Filter::Ne(a, b) | Filter::Lt(a, b) | Filter::Compare(a, _, b) = f
        {
            term(a);
            term(b);
        }
    }
    for branches in &mut g.unions {
        for b in branches {
            substitute(b, params, values);
        }
    }
    for inner in g.nots.iter_mut().chain(&mut g.optionals) {
        substitute(inner, params, values);
    }
}

#[test]
fn prepared_queries() {
    let mut g = graph();
    let [knows, likes] = [&b"knows"[..], b"likes"].map(|d| g.value(d).unwrap());
    let people: Vec<u64> = (0..5).map(|_| g.node().unwrap()).collect();
    for (i, &p) in people.iter().enumerate() {
        for &f in &people[i + 1..] {
            g.link(p, knows, f).unwrap();
        }
    }
    let mut q = Query::new();
    let who = q.param("who");
    let f = q.var("f");
    q.pattern(who, knows, f);
    let mut prepared = q.prepare(&g).unwrap();
    assert!(
        prepared.plan().explain().contains("$who"),
        "{}",
        prepared.plan().explain()
    );
    for (i, &p) in people.iter().enumerate() {
        let expected: Vec<Vec<u64>> = people[i + 1..].iter().map(|&f| vec![f]).collect();
        assert_eq!(sorted(prepared.run(&g, &[p]).unwrap()), expected);
    }
    // The wrong number of values.
    assert!(prepared.run(&g, &[]).is_err());
    assert!(q.plan(&g).unwrap().execute(&g).next().unwrap().is_err());
    // A parameter can be selected.
    let mut selected = q.clone();
    selected.select([who, f]);
    let rows = selected.prepare(&g).unwrap().run(&g, &[people[0]]).unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|r| r[0] == people[0]));
    // Not in a rule's body, nor a standing query.
    let mut ruled = q.clone();
    let r = ruled.relation("r", 1);
    let mut body = Group::new();
    body.pattern(who, likes, f);
    ruled.rule(r, [f], body);
    assert!(ruled.prepare(&g).is_err());
    assert!(StandingQuery::new(&q, &g).is_err());
    // Planned again once the graph has grown enough, and not before.
    let before = prepared.plan().next_id;
    g.link(people[0], likes, people[1]).unwrap();
    prepared.run(&g, &[people[0]]).unwrap();
    assert_eq!(prepared.plan().next_id, before);
    for _ in 0..2 * g.stats().links() {
        let n = g.node().unwrap();
        g.link(people[0], likes, n).unwrap();
    }
    prepared.run(&g, &[people[0]]).unwrap();
    assert_ne!(prepared.plan().next_id, before);
}

#[test]
fn computed_values_and_aggregates() {
    use super::aggregate::{Cell, Function};
    use super::expr::{Expr, Func};
    let mut g = graph();
    let [city, age, paris, rome] =
        [&b"city"[..], b"age", b"paris", b"rome"].map(|d| g.value(d).unwrap());
    // (city, age) of five people; the last has no age.
    let mut people = Vec::new();
    for (c, a) in [
        (paris, Some(30)),
        (paris, Some(40)),
        (rome, Some(25)),
        (paris, Some(30)),
        (rome, None),
    ] {
        let p = g.node().unwrap();
        g.link(p, city, c).unwrap();
        if let Some(a) = a {
            let v = g.value(a.to_string().as_bytes()).unwrap();
            g.link(p, age, v).unwrap();
        }
        people.push(p);
    }
    let base = |q: &mut Query| {
        let (p, c, a) = (q.var("p"), q.var("c"), q.var("a"));
        q.pattern(p, city, c);
        let mut o = Group::new();
        o.pattern(p, age, a);
        q.optional(o);
        (p, c, a)
    };

    // A value computed per row, ordered by.
    let mut q = Query::new();
    let (p, _, a) = base(&mut q);
    let next = q.compute("next", Expr::from(a) + 1);
    q.select([p, next]).order_by(SortKey::data(next).desc());
    let rows = q.cells(&g).unwrap();
    let nexts: Vec<Cell> = rows.iter().map(|r| r[1].clone()).collect();
    assert_eq!(
        nexts,
        [
            Cell::Int(41),
            Cell::Int(31),
            Cell::Int(31),
            Cell::Int(26),
            Cell::Unbound
        ]
    );
    // It needs cells, and a filter can't read it.
    assert!(q.plan(&g).unwrap().execute(&g).next().unwrap().is_err());
    let mut bad = q.clone();
    bad.filter(Filter::Holds(Expr::from(next).gt(30)));
    assert!(bad.plan(&g).is_err());

    // Aggregates, grouped by the variables selected by default.
    let mut q = Query::new();
    let (_, c, a) = base(&mut q);
    let n = q.aggregate("n", Function::Count, None, false);
    let ages = q.aggregate("ages", Function::Count, Some(a.into()), true);
    let mean = q.aggregate("mean", Function::Avg, Some(a.into()), false);
    q.select([c, n, ages, mean]).order_by(SortKey::data(c));
    let explain = q.plan(&g).unwrap().explain();
    assert!(explain.contains("aggregate ?n = count(*)"), "{explain}");
    assert!(explain.contains("group by ?c"), "{explain}");
    assert_eq!(
        q.cells(&g).unwrap(),
        [
            vec![
                Cell::Id(paris),
                Cell::Int(3),
                Cell::Int(2),
                Cell::Float(100.0 / 3.0)
            ],
            vec![
                Cell::Id(rome),
                Cell::Int(2),
                Cell::Int(1),
                Cell::Float(25.0)
            ],
        ]
    );
    // Having, of an aggregate and a parameter.
    let mut h = q.clone();
    let least = h.param("least");
    h.having(Expr::from(n).ge(least));
    let mut prepared = h.prepare(&g).unwrap();
    let two = g.value(b"2").unwrap();
    let three = g.value(b"3").unwrap();
    assert_eq!(prepared.cells(&g, &[two]).unwrap().len(), 2);
    assert_eq!(prepared.cells(&g, &[three]).unwrap().len(), 1);
    // Over every row, one group; even of none.
    let mut total = Query::new();
    let (_, _, a) = base(&mut total);
    let sum = total.aggregate("sum", Function::Sum, Some(a.into()), false);
    total.select([sum]);
    assert_eq!(total.cells(&g).unwrap(), [vec![Cell::Int(125)]]);
    let mut none = Query::new();
    let x = none.var("x");
    none.pattern(x, rome, x);
    let k = none.aggregate("k", Function::Count, None, false);
    none.select([k]);
    assert_eq!(none.cells(&g).unwrap(), [vec![Cell::Int(0)]]);
    // Grouped by a computed value, limited after.
    let mut q = Query::new();
    let (p, _, a) = base(&mut q);
    let decade = q.compute("decade", Expr::call(Func::Floor, [Expr::from(a) / 10]));
    let n = q.aggregate("n", Function::Count, Some(p.into()), false);
    q.group_by([decade])
        .select([decade, n])
        .order_by(SortKey::data(n).desc())
        .limit(1);
    // Decades 3, 3, 4, 2 and unbound.
    assert_eq!(q.cells(&g).unwrap(), [vec![Cell::Int(3), Cell::Int(2)]]);
    // Selecting what is neither grouped nor aggregated, or having without
    // an aggregate, are errors.
    let mut bad = q.clone();
    bad.select([decade, n, p]);
    assert!(bad.plan(&g).is_err());
    let mut bad = Query::new();
    base(&mut bad);
    bad.having(Expr::from(true));
    assert!(bad.plan(&g).is_err());
    assert!(StandingQuery::new(&q, &g).is_err());

    // Filters of expressions, and distinct cells.
    let mut q = Query::new();
    let (_, c, a) = base(&mut q);
    q.filter(Filter::Holds(
        Expr::from(a)
            .ge(30)
            .or(Expr::call(Func::Bound, [a.into()]).not()),
    ));
    let upper = q.compute("upper", Expr::call(Func::Upper, [c.into()]));
    q.select([upper]).distinct();
    let mut got: Vec<Option<String>> = q.cells(&g).unwrap().iter().map(|r| r[0].text(&g)).collect();
    got.sort();
    assert_eq!(got, [Some("PARIS".to_string()), Some("ROME".to_string())]);
    let explain = q.plan(&g).unwrap().explain();
    assert!(
        explain.contains("filter ?a >= 30 or not bound(?a)"),
        "{explain}"
    );
    // A query that computes nothing can be run as cells too.
    let mut plain = Query::new();
    let (p, ..) = base(&mut plain);
    plain.select([p]);
    assert_eq!(plain.cells(&g).unwrap().len(), 5);
}

#[test]
fn parameters_given_values() {
    use super::aggregate::{Cell, Function};
    use super::expr::Expr;
    let mut g = graph();
    let [age, name] = [&b"age"[..], b"name"].map(|d| g.value(d).unwrap());
    let mut people = Vec::new();
    for (n, a) in [("ann", 17), ("ben", 29), ("cat", 41)] {
        let p = g.node().unwrap();
        let [n, a] = [n.to_string(), a.to_string()].map(|d| g.value(d.as_bytes()).unwrap());
        g.link(p, name, n).unwrap();
        g.link(p, age, a).unwrap();
        people.push(p);
    }
    // A value compared, which is not an entry: 21 is no one's age.
    let mut q = Query::new();
    let (p, a) = (q.var("p"), q.var("a"));
    let min = q.param("min");
    q.pattern(p, age, a)
        .filter(Filter::Holds(Expr::from(a).ge(min)))
        .select([p]);
    let mut prepared = q.prepare(&g).unwrap();
    let mut older = |v: Cell| sorted(prepared.run_values(&g, &[v]).unwrap());
    assert_eq!(older(Cell::Int(21)), [[people[1]], [people[2]]]);
    assert_eq!(older(Cell::Text("30".into())), [[people[2]]]);
    assert_eq!(older(Cell::Float(16.5)).len(), 3);
    // As an entry, by its data.
    let seventeen = g.find_value(b"17").unwrap();
    assert_eq!(prepared.run(&g, &[seventeen]).unwrap().len(), 3);
    // In a pattern, a value is the entry with its data, or matches nothing.
    let mut q = Query::new();
    let p = q.var("p");
    let who = q.param("who");
    q.pattern(p, name, who).select([p]);
    let mut by_name = q.prepare(&g).unwrap();
    assert_eq!(
        by_name.run_values(&g, &[Cell::Text("ben".into())]).unwrap(),
        [[people[1]]]
    );
    assert!(
        by_name
            .run_values(&g, &[Cell::Text("dan".into())])
            .unwrap()
            .is_empty()
    );
    // In a computed value and `having`.
    let mut q = Query::new();
    let (p, a) = (q.var("p"), q.var("a"));
    let (k, least) = (q.param("k"), q.param("least"));
    q.pattern(p, age, a);
    let later = q.compute("later", Expr::from(a) + k);
    let n = q.aggregate("n", Function::Count, Some(p.into()), false);
    q.select([later, n])
        .having(Expr::from(n).ge(least))
        .order_by(SortKey::data(later));
    let mut prepared = q.prepare(&g).unwrap();
    let rows = prepared
        .cells_values(&g, &[Cell::Int(10), Cell::Int(1)])
        .unwrap();
    assert_eq!(
        rows,
        [
            vec![Cell::Int(27), Cell::Int(1)],
            vec![Cell::Int(39), Cell::Int(1)],
            vec![Cell::Int(51), Cell::Int(1)]
        ]
    );
    assert!(
        prepared
            .cells_values(&g, &[Cell::Int(10), Cell::Int(2)])
            .unwrap()
            .is_empty()
    );
    let explain = prepared.plan().explain();
    assert!(explain.contains("compute ?later = ?a + $k"), "{explain}");
    assert!(explain.contains("having ?n >= $least"), "{explain}");
}

#[test]
fn random_queries_recording_links() {
    // Recording a query's links leaves its rows as they are, and gives the
    // links brute force binds to the same link variables.
    let mut rng = Rng(0xbb67_ae85_84ca_a73b);
    let (mut graphs, mut links, mut derived) = (0, 0, 0);
    for _ in 0..6 {
        let (g, nodes, predicates) = random_graph(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..80 {
            let mut q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            q.record_links();
            check(&q, &g);
            let recorded = q.with_link_vars().expect("records its links");
            // The oracle: the query with its own link variables, added by
            // the test, not by with_link_vars.
            let (mut by_links, link_vars) = with_own_link_vars(&q);
            by_links.select(link_vars);
            by_links.limit = None;
            by_links.offset = 0;
            let expected: BTreeSet<u64> = BruteForce::rows(&by_links, &g)
                .into_iter()
                .flatten()
                .filter(|&id| id != 0)
                .collect();
            if q.limit.is_some() || q.offset > 0 {
                // Some of the rows: their links are some of these.
                let graph = q.graph(&g).unwrap();
                let supporting: BTreeSet<u64> = graph
                    .derived()
                    .flat_map(|l| match l {
                        ResultLink::Derived { support, .. } => support.clone(),
                        _ => Vec::new(),
                    })
                    .collect();
                if !q.relations.iter().any(|(_, arity)| *arity != 2) {
                    assert!(
                        graph
                            .stored()
                            .all(|id| expected.contains(&id) || supporting.contains(&id)),
                        "{q:?}"
                    );
                }
                continue;
            }
            let graph = q.graph(&g).unwrap();
            let got: BTreeSet<u64> = graph.stored().collect();
            // The links the patterns match, and those supporting derived
            // links (and calls of relations of other arities, which give
            // support but no link).
            assert!(expected.is_subset(&got), "{q:?}");
            let supporting: BTreeSet<u64> = graph
                .derived()
                .flat_map(|l| match l {
                    ResultLink::Derived { support, .. } => support.clone(),
                    _ => Vec::new(),
                })
                .collect();
            let other_arities = q.relations.iter().any(|(_, arity)| *arity != 2);
            if !other_arities {
                let extra: BTreeSet<u64> = got.difference(&expected).copied().collect();
                assert!(extra.is_subset(&supporting), "{q:?}");
            }
            check_derived(&q, &g, &graph);
            derived += graph.derived().count();
            // The nodes are the selected entries not in a link.
            let mut in_links: BTreeSet<u64> = got
                .iter()
                .flat_map(|&id| {
                    let f = g.get(id).unwrap().as_link().unwrap().fields();
                    [f.subject, f.predicate, f.object]
                })
                .collect();
            for l in graph.derived() {
                if let ResultLink::Derived { fields, .. } = l {
                    for f in fields {
                        if let ResultNode::Entry(e) = f {
                            in_links.insert(*e);
                        }
                    }
                }
            }
            // Of the selected variables but the link variables, whose values
            // are links.
            let mut of_nodes = recorded.clone();
            of_nodes.select(
                recorded
                    .selected()
                    .into_iter()
                    .filter(|v| !recorded.recorded.contains(v)),
            );
            let selected: BTreeSet<u64> = BruteForce::rows(&of_nodes, &g)
                .into_iter()
                .flatten()
                .filter(|id| *id != 0 && !in_links.contains(id))
                .collect();
            let nodes: BTreeSet<u64> = graph.nodes.iter().copied().collect();
            assert_eq!(nodes, selected, "{q:?}");
            graphs += usize::from(!got.is_empty());
            links += got.len();
        }
    }
    assert!(graphs > 100, "{graphs}");
    assert!(links > 500, "{links}");
    assert!(derived > 100, "{derived}");
}

/// Checks the derived links of `graph`, of `q`: a path's support is a walk
/// of live links from its subject to its object (forward links of its
/// predicate, for `p++`), and a rule's link is a tuple of its relation.  And
/// each row's matches of the top group's paths, and calls of relations of
/// two columns, have their derived links.
fn check_derived(q: &Query, g: &Graph, graph: &ResultGraph) {
    let mut bf = BruteForce {
        q,
        g,
        overlay: None,
        links: HashMap::new(),
        relations: HashMap::new(),
    };
    bf.evaluate();
    let mut paths = HashSet::new();
    let mut rules = HashSet::new();
    for l in graph.derived() {
        let ResultLink::Derived {
            fields,
            by,
            support,
        } = l
        else {
            unreachable!()
        };
        let [ResultNode::Entry(s), predicate, ResultNode::Entry(o)] = fields else {
            panic!("{l:?}")
        };
        match by {
            Derivation::Path => {
                let mut at = *s;
                for &id in support {
                    let f = g.get(id).unwrap().as_link().unwrap().fields();
                    assert!(
                        g.find_link(f.subject, f.predicate, f.object).is_some()
                            || q.as_of.is_some(),
                        "{l:?}"
                    );
                    match predicate {
                        ResultNode::Entry(p) => {
                            assert_eq!((f.predicate, f.subject), (*p, at), "{l:?} {q:?}");
                            at = f.object;
                        }
                        _ if f.subject == at => at = f.object,
                        _ if f.object == at => at = f.subject,
                        _ => panic!("not a walk: {l:?} {q:?}"),
                    }
                }
                assert_eq!(at, *o, "{l:?} {q:?}");
                if let ResultNode::Entry(p) = predicate {
                    paths.insert((*s, Some(*p), *o));
                }
                paths.insert((*s, None, *o));
            }
            Derivation::Asserted | Derivation::Aggregate(_) | Derivation::Overlay => {
                unreachable!("random queries assert and aggregate nothing")
            }
            Derivation::Rule(name) => {
                let r = q.relations.iter().position(|(n, _)| n == name).unwrap();
                assert!(
                    bf.relations[&Relation(r)].contains(&vec![*s, *o]),
                    "{l:?} {q:?}"
                );
                // Every derivation matches a stored link, unless by a path
                // that may be empty.
                let empty_paths = q.rules.iter().any(|rule| {
                    rule.body
                        .paths
                        .iter()
                        .any(|p| p.expr.as_ref().is_some_and(|e| e.nullable()))
                });
                assert!(empty_paths || !support.is_empty(), "{l:?} {q:?}");
                for &id in support {
                    let f = g.get(id).unwrap().as_link().unwrap().fields();
                    assert!(
                        g.find_link(f.subject, f.predicate, f.object).is_some()
                            || q.as_of.is_some(),
                        "{l:?}"
                    );
                }
                rules.insert((r, *s, *o));
            }
        }
    }
    // Every row's paths and calls of the top group.
    let value = |t: Term, row: &Partial| match t {
        Term::Id(id) => id,
        Term::Var(v) => row[v.0].unwrap_or(0),
    };
    for row in bf.group(&q.root, &vec![None; q.names.len()]) {
        for p in &q.root.paths {
            let (s, o) = (value(p.subject, &row), value(p.object, &row));
            if p.expr.is_none() {
                let pr = value(p.predicate, &row);
                assert!(paths.contains(&(s, Some(pr), o)), "{s} {pr} {o} {q:?}");
            } else if s != o {
                assert!(paths.contains(&(s, None, o)), "{s} {o} {q:?}");
            }
        }
        for c in q.root.calls.iter().filter(|c| c.args.len() == 2) {
            let (a, b) = (value(c.args[0], &row), value(c.args[1], &row));
            assert!(rules.contains(&(c.relation.0, a, b)), "{a} {b} {q:?}");
        }
    }
}

#[test]
fn recording_links() {
    let (g, people, [name, nick, ..]) = people();
    let mut q = Query::new();
    let (p, n, k) = (q.var("p"), q.var("n"), q.var("k"));
    q.pattern(p, name, n);
    let mut o = Group::new();
    o.pattern(p, nick, k);
    q.optional(o).select([n]);
    // A query must record its links to give a graph.
    assert!(q.plan(&g).unwrap().execute_graph(&g, &[]).is_err());
    let graph = q.graph(&g).unwrap();
    // A name link for each person, and a nick link for those with one.
    assert_eq!(graph.links.len(), 4 + 2);
    assert!(graph.nodes.is_empty());
    // The rows are as they were, and the link variables are not selected.
    let mut recording = q.clone();
    recording.record_links();
    assert_eq!(
        sorted(recording.run(&g).unwrap()),
        sorted(q.run(&g).unwrap())
    );
    let explain = recording.plan(&g).unwrap().explain();
    assert!(explain.contains("?_link0 = "), "{explain}");
    // Unions' branches record their own links.
    let mut q = Query::new();
    let (x, v) = (q.var("x"), q.var("v"));
    let (mut by_name, mut by_nick) = (Group::new(), Group::new());
    by_name.pattern(x, name, v);
    by_nick.pattern(x, nick, v);
    q.union([by_name, by_nick]).select([x, v]);
    assert_eq!(q.graph(&g).unwrap().links.len(), 4 + 2);
    // A path gives the links it follows, and the link it derives.
    let mut q = Query::new();
    let y = q.var("y");
    q.path(people[0], name, y).select([y]);
    let graph = q.graph(&g).unwrap();
    assert_eq!(graph.stored().count(), 1);
    let derived: Vec<&ResultLink> = graph.derived().collect();
    assert!(
        matches!(derived[..], [ResultLink::Derived { fields: [ResultNode::Entry(s), ResultNode::Entry(p), _], by: Derivation::Path, .. }] if *s == people[0] && *p == name),
        "{derived:?}"
    );
    assert!(graph.nodes.is_empty());
    // A relation of one column gives no link, but its tuples' support:
    // here, each person's name link, so the people aren't nodes alone.
    let mut q = Query::new();
    let named = q.relation("named", 1);
    let (x, n) = (q.var("x"), q.var("n"));
    let mut body = Group::new();
    body.pattern(x, name, n);
    q.rule(named, [x], body);
    let z = q.var("z");
    q.call(named, [z]).select([z]);
    let graph = q.graph(&g).unwrap();
    assert_eq!(graph.derived().count(), 0);
    assert_eq!(graph.stored().count(), 4);
    assert!(graph.nodes.is_empty());
    assert!(StandingQuery::new(&recording, &g).is_err());
}

#[test]
fn random_queries_asserting() {
    // A query's asserted triples are instantiated once for each distinct
    // binding of their variables and scope in its rows, as brute force
    // finds them: live triples as their stored links, the rest derived, a
    // new node for each instantiation's blank node.
    let mut rng = Rng(0x3c6e_f372_fe94_f82b);
    let (mut queries, mut plain, mut stored, mut blanks) = (0, 0, 0, 0);
    for _ in 0..6 {
        let (g, nodes, predicates) = random_graph(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..80 {
            let mut q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            if q.limit.is_some() || q.offset > 0 {
                continue;
            }
            let vars: Vec<Var> = q
                .root
                .maybe_vars()
                .into_iter()
                .filter(|v| !q.hidden.contains(v) && !q.params.contains(v))
                .collect();
            if vars.is_empty() {
                continue;
            }
            // A triple of variables, nodes and values (one not in the
            // graph), and perhaps two through a blank node.
            let field = |rng: &mut Rng, consts: &[u64]| -> TemplateNode {
                match rng.below(5) {
                    0 | 1 => TemplateNode::from(rng.pick(&vars)),
                    2 => TemplateNode::from(rng.pick(consts)),
                    3 => TemplateNode::Value(Cell::Text("p1".into())),
                    _ => TemplateNode::Value(Cell::Text("fresh".into())),
                }
            };
            let mut triples = vec![[
                field(&mut rng, &nodes),
                field(&mut rng, &predicates),
                field(&mut rng, &nodes),
            ]];
            // Often one of the query's own patterns, which is stored.
            if !q.root.patterns.is_empty() && rng.chance(40) {
                let p = rng.pick(&q.root.patterns);
                triples[0] = p.terms().map(TemplateNode::from);
            }
            if rng.chance(40) {
                triples.push([
                    field(&mut rng, &nodes),
                    field(&mut rng, &predicates),
                    TemplateNode::Blank(0),
                ]);
                triples.push([
                    TemplateNode::Blank(0),
                    field(&mut rng, &predicates),
                    field(&mut rng, &nodes),
                ]);
            }
            let mut a = Assertion {
                triples,
                scope: None,
            };
            if rng.chance(40) {
                a = a.scope(rng.pick(&vars));
            }
            q.assert(a.clone());
            let graph = q.graph(&g).unwrap();
            queries += 1;
            // The oracle: brute force's rows of the assertion's variables.
            let used = a.vars();
            let mut rows = q.clone();
            rows.select(used.clone());
            rows.distinct = false;
            let instances: BTreeSet<Vec<u64>> = BruteForce::rows(&rows, &g)
                .into_iter()
                .filter(|row| {
                    a.scope
                        .is_none_or(|v| row[used.iter().position(|&u| u == v).unwrap()] != 0)
                })
                .collect();
            let live: HashMap<[u64; 3], u64> = live_links(&g, q.as_of)
                .into_iter()
                .map(|(id, f)| (f, id))
                .collect();
            let (mut expected, mut expected_blanks) = (BTreeSet::new(), 0);
            for row in &instances {
                let value = |n: &TemplateNode| -> Option<String> {
                    Some(match n {
                        TemplateNode::Term(Term::Var(v)) => {
                            let id = row[used.iter().position(|u| u == v).unwrap()];
                            if id == 0 {
                                return None;
                            }
                            format!("#{id}")
                        }
                        TemplateNode::Term(Term::Id(id)) => format!("#{id}"),
                        TemplateNode::Value(Cell::Text(t)) => match g.find_value(t.as_bytes()) {
                            Some(id) => format!("#{id}"),
                            None => t.clone(),
                        },
                        TemplateNode::Blank(_) => "_".into(),
                        _ => unreachable!(),
                    })
                };
                let mut blank = false;
                for t in &a.triples {
                    let Some(f) = t.iter().map(value).collect::<Option<Vec<String>>>() else {
                        continue;
                    };
                    if f.contains(&"_".to_string()) {
                        blank = true;
                    } else {
                        expected.insert(f);
                    }
                }
                expected_blanks += usize::from(blank);
            }
            // What the graph gives: stored links for live triples, derived
            // ones for the rest, once each.
            let got_stored: BTreeSet<u64> = graph.stored().collect();
            let name = |n: &ResultNode| match n {
                ResultNode::Entry(id) => format!("#{id}"),
                ResultNode::Value(Cell::Text(t)) => t.clone(),
                ResultNode::Blank(_) => "_".into(),
                other => panic!("{other:?}"),
            };
            let mut got = Vec::new();
            let mut got_blanks = BTreeSet::new();
            for l in graph.derived() {
                let ResultLink::Derived {
                    fields,
                    by,
                    support,
                } = l
                else {
                    unreachable!()
                };
                if *by != Derivation::Asserted {
                    continue;
                }
                assert!(support.iter().all(|id| got_stored.contains(id)), "{q:?}");
                for f in fields {
                    if let ResultNode::Blank(n) = f {
                        got_blanks.insert(*n);
                    }
                }
                if !fields.iter().any(|f| matches!(f, ResultNode::Blank(_))) {
                    got.push(fields.iter().map(name).collect::<Vec<String>>());
                }
            }
            let got_set: BTreeSet<Vec<String>> = got.iter().cloned().collect();
            assert_eq!(got.len(), got_set.len(), "derived twice: {q:?}");
            let mut want_derived = BTreeSet::new();
            for f in &expected {
                let ids: Option<Vec<u64>> = f
                    .iter()
                    .map(|x| x.strip_prefix('#').map(|i| i.parse().unwrap()))
                    .collect();
                match ids.and_then(|ids| live.get(&[ids[0], ids[1], ids[2]])) {
                    Some(id) => {
                        assert!(got_stored.contains(id), "{f:?} {q:?}");
                        stored += 1;
                    }
                    None => {
                        want_derived.insert(f.clone());
                    }
                }
            }
            assert_eq!(got_set, want_derived, "{q:?}");
            assert_eq!(got_blanks.len(), expected_blanks, "{q:?}");
            plain += got_set.len();
            blanks += expected_blanks;
        }
    }
    assert!(queries > 250, "{queries}");
    assert!(plain > 150, "{plain}");
    assert!(stored > 150, "{stored}");
    assert!(blanks > 250, "{blanks}");
}

#[test]
fn asserting() {
    let (g, people, [name, nick, ..]) = people();
    let mut q = Query::new();
    let (p, n, k) = (q.var("p"), q.var("n"), q.var("k"));
    q.pattern(p, name, n);
    let mut o = Group::new();
    o.pattern(p, nick, k);
    q.optional(o);
    // Asserting a stored, live link gives it; a scope that is unbound
    // instantiates nothing.
    let mut stored = q.clone();
    stored.assert(Assertion::new().triple(p, name, n));
    let graph = stored.graph(&g).unwrap();
    assert_eq!(graph.derived().count(), 0);
    assert_eq!(graph.stored().count(), 4 + 2);
    let mut scoped = q.clone();
    scoped.assert(
        Assertion::new()
            .triple(
                p,
                TemplateNode::Value(Cell::Text("nicknamed".into())),
                TemplateNode::Value(Cell::Text("true".into())),
            )
            .scope(k),
    );
    let graph = scoped.graph(&g).unwrap();
    assert_eq!(graph.derived().count(), 2);
    // The rows are as they were.
    assert_eq!(sorted(scoped.run(&g).unwrap()), sorted(q.run(&g).unwrap()));
    // Variables must be bound by the patterns.
    let mut unbound = q.clone();
    let z = unbound.var("z");
    unbound.assert(Assertion::new().triple(p, name, z));
    assert!(unbound.graph(&g).is_err());
    // A rule asserts for each tuple it derives, when called: here, with a
    // key not in the graph, and a blank node for each tuple.
    let mut q = Query::new();
    let named = q.relation("named", 1);
    let (x, n) = (q.var("x"), q.var("n"));
    let mut body = Group::new();
    body.pattern(x, name, n);
    q.rule(named, [x], body);
    q.rule_asserts(
        Assertion::new()
            .triple(x, TemplateNode::Key("/has".into()), TemplateNode::Blank(7))
            .triple(TemplateNode::Blank(7), nick, x),
    );
    let z = q.var("z");
    q.call(named, [z]).select([z]);
    let graph = q.graph(&g).unwrap();
    let derived: Vec<&ResultLink> = graph.derived().collect();
    assert_eq!(derived.len(), 8, "{derived:?}");
    let blanks: BTreeSet<u32> = derived
        .iter()
        .flat_map(|l| match l {
            ResultLink::Derived { fields, by, .. } => {
                assert_eq!(*by, Derivation::Rule("named".into()));
                fields.to_vec()
            }
            _ => unreachable!(),
        })
        .filter_map(|f| match f {
            ResultNode::Blank(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(blanks.len(), 4);
    // A rule's assertions may use only its head's variables.
    let mut bad = q.clone();
    bad.rule_asserts(Assertion::new().triple(x, name, n));
    assert!(bad.graph(&g).is_err());
    // A tuple derived by several rules gets each one's assertions: here
    // the people with nicks, named first by the rule asserting nothing.
    let mut q = Query::new();
    let named = q.relation("named", 1);
    let (x, n) = (q.var("x"), q.var("n"));
    let mut body = Group::new();
    body.pattern(x, name, n);
    q.rule(named, [x], body);
    let (y, k) = (q.var("y"), q.var("k"));
    let mut body = Group::new();
    body.pattern(y, nick, k);
    q.rule(named, [y], body);
    q.rule_asserts(Assertion::new().triple(y, nick, nick));
    let z = q.var("z");
    q.call(named, [z]).select([z]);
    let graph = q.graph(&g).unwrap();
    let nicknamed: BTreeSet<u64> = graph
        .derived()
        .map(|l| match l {
            ResultLink::Derived {
                fields: [ResultNode::Entry(s), ..],
                ..
            } => *s,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(nicknamed.len(), 2);
    assert!(nicknamed.is_subset(&people.iter().copied().collect()));
}

/// A random overlay of `g`: over it, or over nothing; with live links of
/// its nodes and predicates (some of facts it has, some of new nodes), and
/// dead links deleting some of its live facts.
fn random_overlay(rng: &mut Rng, g: &Graph, nodes: &[u64], predicates: &[u64]) -> Overlay {
    let mut ov = if rng.chance(80) {
        Overlay::new()
    } else {
        Overlay::alone()
    };
    let fresh: Vec<u64> = (0..2).map(|_| g.ephemeral_node()).collect();
    let mut all_nodes = nodes.to_vec();
    all_nodes.extend(&fresh);
    let live = live_links(g, None);
    for _ in 0..rng.below(25) {
        let spo = [
            rng.pick(&all_nodes),
            rng.pick(predicates),
            rng.pick(&all_nodes),
        ];
        ov.insert(g, spo);
    }
    for _ in 0..rng.below(15) {
        let (_, spo) = rng.pick(&live);
        if rng.chance(70) {
            ov.delete(g, spo);
        } else {
            ov.insert(g, spo);
        }
    }
    ov
}

#[test]
fn random_value_limits() {
    // Nested limits keep, within each group of the earlier limited
    // variables' values, the rows whose value is among the group's first,
    // in value order: as an oracle filtering brute force's rows does.
    let mut rng = Rng(0x1f83_d9ab_fb41_bd6b);
    let (mut queries, mut cut) = (0, 0);
    for _ in 0..6 {
        let (g, nodes, predicates) = random_graph(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..80 {
            let mut q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            if q.limit.is_some() || q.offset > 0 || q.distinct {
                continue;
            }
            q.order.clear();
            let vars: Vec<Var> = q
                .root
                .maybe_vars()
                .into_iter()
                .filter(|v| !q.hidden.contains(v))
                .collect();
            if vars.is_empty() {
                continue;
            }
            let mut limits = Vec::new();
            for _ in 0..1 + rng.below(2) {
                let v = rng.pick(&vars);
                if !limits.iter().any(|&(w, _, _)| w == v) {
                    limits.push((v, 1 + rng.below(3), rng.below(2)));
                }
            }
            // The oracle: brute force's rows, of every variable, filtered.
            let mut all = q.clone();
            let every: Vec<Var> = (0..q.names.len()).map(Var).collect();
            all.select(every.clone());
            let mut rows = BruteForce::rows(&all, &g);
            for (k, &(v, n, off)) in limits.iter().enumerate() {
                let before: Vec<Var> = limits[..k].iter().map(|l| l.0).collect();
                let key = |r: &Vec<u64>| -> Vec<u64> { before.iter().map(|b| r[b.0]).collect() };
                let mut groups: BTreeMap<Vec<u64>, Vec<u64>> = BTreeMap::new();
                for r in rows.iter().filter(|r| r[v.0] != 0) {
                    let values = groups.entry(key(r)).or_default();
                    if !values.contains(&r[v.0]) {
                        values.push(r[v.0]);
                    }
                }
                for values in groups.values_mut() {
                    values.sort_by(|&a, &b| {
                        aggregate::compare_cells(&g, &Cell::Id(a), &Cell::Id(b)).then(a.cmp(&b))
                    });
                    *values = values.iter().copied().skip(off).take(n).collect();
                }
                rows.retain(|r| r[v.0] == 0 || groups[&key(r)].contains(&r[v.0]));
            }
            let selected = q.selected();
            let expected: Vec<Vec<u64>> = rows
                .iter()
                .map(|r| selected.iter().map(|v| r[v.0]).collect())
                .collect();
            for &(v, n, off) in &limits {
                q.limit_values(v, n, off);
            }
            let got = q.run(&g).unwrap();
            assert_eq!(sorted(got.clone()), sorted(expected), "{limits:?} {q:?}");
            queries += 1;
            cut += usize::from(got.len() < BruteForce::rows(&all, &g).len());
        }
    }
    assert!(queries > 200, "{queries}");
    assert!(cut > 60, "{cut}");
}

/// The live links of `g`, as of its end, from a scan of its log rather
/// than its indexes: the latest link of each fact, if live.
fn live_links_by_scan(g: &Graph) -> BTreeSet<(u64, [u64; 3])> {
    let mut latest: HashMap<[u64; 3], (u64, bool)> = HashMap::new();
    for r in g.records(g.start_id()) {
        let r = r.unwrap();
        if let Some(l) = r.as_link() {
            latest.insert([l.subject(), l.predicate(), l.object()], (r.id(), l.dead()));
        }
    }
    latest
        .into_iter()
        .filter(|&(_, (_, dead))| !dead)
        .map(|(spo, (id, _))| (id, spo))
        .collect()
}

#[test]
fn readers_beside_a_writer() {
    // Readers on other threads, taking the latest commit's reader while a
    // writer appends, deletes, commits, rolls back and grows its tables,
    // see each commit whole: every link through the indexes is a live link
    // of the reader's log, and random queries match brute force on it.
    let mut rng = Rng(0x5be0_cd19_137e_2179);
    let (mut g, nodes, predicates) = random_graph_of(&mut rng, 200);
    g.commit().unwrap();
    let readers = g.readers();
    let ids = (g.start_id(), g.next_id());
    let done = std::sync::atomic::AtomicBool::new(false);
    let checked = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..3u64 {
            let (readers, nodes, predicates, done, checked) =
                (&readers, &nodes, &predicates, &done, &checked);
            s.spawn(move || {
                let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (t + 1));
                let mut mine = 0;
                // Until the writer is done, and at least a few times.
                while !done.load(std::sync::atomic::Ordering::Relaxed) || mine < 15 {
                    mine += 1;
                    let r = readers.latest();
                    // Every link, through the indexes, as the log has it.
                    let mut q = Query::new();
                    let (l, x, p, y) = (q.var("l"), q.var("x"), q.var("p"), q.var("y"));
                    q.link_pattern(l, x, p, y);
                    let by_index: BTreeSet<(u64, [u64; 3])> = q
                        .run(&r)
                        .unwrap()
                        .into_iter()
                        .map(|row| (row[0], [row[1], row[2], row[3]]))
                        .collect();
                    assert_eq!(by_index, live_links_by_scan(&r));
                    // A random query, as brute force has it on the reader.
                    let q = Generator {
                        rng: &mut rng,
                        nodes,
                        predicates,
                        ids,
                    }
                    .query();
                    match q.run(&r) {
                        Ok(rows) => check_rows(&q, &r, rows, &BruteForce::rows(&q, &r), "reader"),
                        Err(crate::graph::Error::Compacted { .. }) => {}
                        Err(e) => panic!("{e}: {q:?}"),
                    }
                    checked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
        // The writer: many links, so that tables expand and large entries
        // are promoted, with commits and rollbacks.
        for round in 0..300 {
            for _ in 0..20 {
                let (s, p, o) = (rng.pick(&nodes), rng.pick(&predicates), rng.pick(&nodes));
                if rng.chance(15) {
                    g.delete_link(s, p, o).unwrap();
                } else {
                    g.link(s, p, o).unwrap();
                }
                if rng.chance(5) {
                    g.node().unwrap();
                }
            }
            if round % 7 == 3 {
                g.rollback().unwrap();
            } else {
                g.commit().unwrap();
            }
            // Compaction rebuilds large entries while readers read them.
            if round % 60 == 59 {
                let horizon = g.next_id() - 1;
                g.compact(horizon).unwrap();
            }
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let checked = checked.into_inner();
    assert!(checked > 30, "{checked}");
    // The last reader is the graph as committed.
    let r = readers.latest();
    assert_eq!(r.next_id(), g.next_id());
    assert_eq!(live_links_by_scan(&r), live_links_by_scan(&g));
}

#[test]
fn overlays() {
    let (mut g, people, [name, nick, ..]) = people();
    let [carol, alice, ..] = people[..] else {
        unreachable!()
    };
    let likes = g.ephemeral_value(b"likes");
    let fresh = g.ephemeral_node();
    // Facts set, replaced, and deleted beneath.
    let mut ov = Overlay::new();
    let l = ov.insert(&g, [carol, likes, alice]);
    assert!(l & crate::graph::EPHEMERAL != 0);
    assert_eq!(ov.insert(&g, [carol, likes, alice]), l);
    let carol_name = g
        .find_link(carol, name, g.find_value(b"carol").unwrap())
        .unwrap();
    let f = g.get(carol_name).unwrap().as_link().unwrap().fields();
    ov.delete(&g, [f.subject, f.predicate, f.object]);
    ov.insert(&g, [fresh, likes, carol]);
    let ov = Arc::new(ov);
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, name, y).select([x]);
    // Carol's name is deleted beneath the overlay, not in the graph.
    assert_eq!(q.run_over(&g, &ov).unwrap().len(), 3);
    assert_eq!(q.run(&g).unwrap().len(), 4);
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, likes, y);
    assert_eq!(
        sorted(q.run_over(&g, &ov).unwrap()),
        sorted(vec![vec![carol, alice], vec![fresh, carol]])
    );
    // Alone, only its own links.
    let mut alone = Overlay::alone();
    alone.insert(&g, [carol, nick, alice]);
    let mut q = Query::new();
    let (x, p, y) = (q.var("x"), q.var("p"), q.var("y"));
    q.pattern(x, p, y);
    assert_eq!(
        q.run_over(&g, &Arc::new(alone.clone())).unwrap(),
        [[carol, nick, alice]]
    );
    // Layered: the upper's links replace the lower's, over its base.
    let mut upper = Overlay::new();
    upper.delete(&g, [carol, nick, alice]);
    upper.insert(&g, [alice, nick, carol]);
    let both = upper.over(&alone);
    assert!(!both.over_database());
    assert_eq!(
        q.run_over(&g, &Arc::new(both)).unwrap(),
        [[alice, nick, carol]]
    );
    // Its graph's links of the overlay are its own.
    let graph = q.graph_over(&g, &ov).unwrap();
    assert!(graph.links.iter().any(|l| matches!(
        l,
        ResultLink::Derived { by: Derivation::Overlay, fields: [ResultNode::Entry(s), ..], .. }
            if *s == fresh
    )));
    // Inserted: a node for the new one, the deletion, and a value for the
    // predicate.
    let n = ov.insert_into(&mut g, 0, None).unwrap();
    assert_eq!(n, 3);
    assert!(
        g.find_link(carol, name, g.find_value(b"carol").unwrap())
            .is_none()
    );
    let likes = g.find_value(b"likes").unwrap();
    assert!(g.find_link(carol, likes, alice).is_some());
    assert_eq!(g.follow_backward(likes, [carol]).len(), 1);
}

#[test]
fn random_queries_over_overlays() {
    // Queries beneath an overlay give the rows brute force gives over the
    // overlay's links layered on the graph's (or alone), with its dead
    // links deleting the graph's facts; under each join.
    let mut rng = Rng(0x9b05_688c_2b3e_6c1f);
    let (mut queries, mut with_rows, mut overlay_rows) = (0, 0, 0);
    for _ in 0..6 {
        let (g, nodes, predicates) = random_graph(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..80 {
            let q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            let ov = Arc::new(random_overlay(&mut rng, &g, &nodes, &predicates));
            let expected = BruteForce::rows_over(&q, &g, Some(&ov));
            for force in [None, Some(Join::Bind), Some(Join::Hash)] {
                let plan = plan::plan_with(&q, &g, force).unwrap();
                let got: Vec<Vec<u64>> = plan
                    .execute_over(&g, &ov, &[])
                    .collect::<Result<_>>()
                    .unwrap_or_else(|e| panic!("{e}: {q:?}"));
                check_rows(&q, &g, got, &expected, &plan.explain());
            }
            // Without the overlay, as before.
            check(&q, &g);
            queries += 1;
            with_rows += usize::from(!expected.is_empty());
            overlay_rows += usize::from(
                expected
                    .iter()
                    .flatten()
                    .any(|&id| id & crate::graph::EPHEMERAL != 0),
            );
            // Its graph: the overlay's links it matches are the overlay's.
            if q.limit.is_none() && q.offset == 0 {
                let graph = q.graph_over(&g, &ov).unwrap();
                for l in graph.links.iter() {
                    match l {
                        ResultLink::Stored(id) => {
                            let f = g.get(*id).unwrap().as_link().unwrap().fields();
                            assert!(
                                ov.over_database()
                                    && ov.fact([f.subject, f.predicate, f.object]).is_none(),
                                "{l:?} {q:?}"
                            );
                        }
                        ResultLink::Derived {
                            fields,
                            by: Derivation::Overlay,
                            ..
                        } => {
                            let spo = fields.clone().map(|f| match f {
                                ResultNode::Entry(e) => e,
                                other => panic!("{other:?}"),
                            });
                            assert!(ov.fact(spo).is_some_and(|l| l.live), "{l:?}");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    assert!(queries > 400, "{queries}");
    assert!(with_rows > 150, "{with_rows}");
    assert!(overlay_rows > 25, "{overlay_rows}");
}

/// The tuples `(s, v, scope)` of a link aggregate over `rows` of its
/// subject, object and scope (0 if it has none), worked out independently
/// of `LinkAggregate::tuples`: a rank is how many subjects come before.
fn aggregate_oracle(g: &Graph, a: &LinkAggregate, rows: &[Vec<u64>]) -> BTreeSet<Vec<u64>> {
    let mut groups: BTreeMap<(u64, u64), Vec<u64>> = BTreeMap::new();
    for r in rows {
        if r[0] != 0 && (a.scope.is_none() || r[2] != 0) {
            groups.entry((r[0], r[2])).or_default().push(r[1]);
        }
    }
    let mut out = BTreeSet::new();
    let number = |n: usize| g.ephemeral_value(n.to_string().as_bytes());
    if let LinkFunction::Fold(f) = a.function {
        for ((s, scope), objects) in &groups {
            let bound: Vec<u64> = objects.iter().copied().filter(|&o| o != 0).collect();
            let v = match f {
                Function::Count if a.distinct => {
                    Some(number(bound.iter().collect::<BTreeSet<_>>().len()))
                }
                Function::Count => Some(number(bound.len())),
                Function::Min | Function::Max => bound.iter().copied().min_by(|&x, &y| {
                    let o = aggregate::compare_cells(g, &Cell::Id(x), &Cell::Id(y));
                    if f == Function::Min { o } else { o.reverse() }
                }),
                _ => unreachable!(),
            };
            if let Some(v) = v {
                out.insert(vec![*s, v, *scope]);
            }
        }
        return out;
    }
    // The best key of each subject, by scope; a subject before another.
    let cmp = |x: u64, y: u64| {
        let o = aggregate::compare_cells(g, &Cell::Id(x), &Cell::Id(y));
        if a.descending { o.reverse() } else { o }
    };
    let mut by_scope: BTreeMap<u64, Vec<(u64, Option<u64>)>> = BTreeMap::new();
    for ((s, scope), keys) in &groups {
        let best = keys
            .iter()
            .copied()
            .filter(|&k| k != 0)
            .min_by(|&x, &y| cmp(x, y));
        by_scope.entry(*scope).or_default().push((*s, best));
    }
    // Keys compared, those missing after the rest.
    let key_cmp = |x: Option<u64>, y: Option<u64>| match (x, y) {
        (Some(x), Some(y)) => cmp(x, y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    for (scope, subjects) in &by_scope {
        for &(s, k) in subjects {
            let rank = match a.function {
                LinkFunction::Rank => subjects
                    .iter()
                    .filter(|&&(_, j)| key_cmp(j, k).is_lt())
                    .count(),
                LinkFunction::DenseRank => {
                    // The keys before, counted once for each that compare
                    // equal.
                    let mut before: Vec<Option<u64>> = subjects
                        .iter()
                        .map(|&(_, j)| j)
                        .filter(|&j| key_cmp(j, k).is_lt())
                        .collect();
                    before.sort_by(|&x, &y| key_cmp(x, y));
                    before.dedup_by(|x, y| key_cmp(*x, *y).is_eq());
                    before.len()
                }
                _ => subjects
                    .iter()
                    .filter(|&&(t, j)| key_cmp(j, k).then(t.cmp(&s)).is_lt())
                    .count(),
            };
            out.insert(vec![s, number(rank), *scope]);
        }
    }
    out
}

#[test]
fn random_link_aggregates() {
    // A query with an aggregate deriving links, and a pattern matching them,
    // has the rows of the query without them joined with the aggregate's
    // tuples, as brute force and an oracle of the aggregate give them.
    let mut rng = Rng(0x510e_527f_ade6_82d1);
    let (mut queries, mut rows_seen, mut ranked) = (0, 0, 0);
    let functions = [
        LinkFunction::Fold(Function::Count),
        LinkFunction::Fold(Function::Min),
        LinkFunction::Fold(Function::Max),
        LinkFunction::Rank,
        LinkFunction::DenseRank,
        LinkFunction::RowNumber,
    ];
    for _ in 0..6 {
        let (g, nodes, predicates) = random_graph(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..160 {
            let mut q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            if q.limit.is_some() || q.offset > 0 || q.distinct {
                continue;
            }
            q.order.clear();
            let vars: Vec<Var> = q
                .root
                .maybe_vars()
                .into_iter()
                .filter(|v| !q.hidden.contains(v))
                .collect();
            if vars.is_empty() {
                continue;
            }
            let function = functions[rng.below(functions.len())];
            let mut a = LinkAggregate::new(
                function,
                rng.pick(&vars),
                g.ephemeral_value(b"derived"),
                rng.pick(&vars),
            );
            if matches!(function, LinkFunction::Fold(Function::Count)) && rng.chance(50) {
                a = a.distinct();
            }
            if function != LinkFunction::Fold(Function::Count) && rng.chance(50) {
                a = a.desc();
            }
            if rng.chance(50) {
                a = a.scope(rng.pick(&vars));
            }
            // The oracle's rows: the query's, with the aggregate's columns
            // and the variable its links are matched from.
            // Bound by every row, so that matching the links joins as the
            // oracle does.
            let always: Vec<Var> = q
                .root
                .bound_vars()
                .into_iter()
                .filter(|v| !q.hidden.contains(v))
                .collect();
            if always.is_empty() {
                continue;
            }
            let from = rng.pick(&always);
            let selected = q.selected();
            let mut base = q.clone();
            let mut columns = vec![a.subject, a.object, a.scope.unwrap_or(a.subject), from];
            columns.extend(&selected);
            base.select(columns);
            let base_rows = BruteForce::rows(&base, &g);
            let heads: Vec<Vec<u64>> = base_rows
                .iter()
                .map(|r| vec![r[0], r[1], if a.scope.is_some() { r[2] } else { 0 }])
                .collect();
            let tuples = aggregate_oracle(&g, &a, &heads);
            let mut expected = Vec::new();
            for r in &base_rows {
                let values: BTreeSet<u64> = tuples
                    .iter()
                    .filter(|t| t[0] == r[3])
                    .map(|t| t[1])
                    .collect();
                for v in values {
                    let mut row = r[4..].to_vec();
                    row.push(v);
                    expected.push(row);
                }
            }
            // The query: the aggregate, and its links matched.
            let v = q.var("value");
            q.link_aggregate(a.clone());
            q.pattern(from, g.ephemeral_value(b"derived"), v);
            q.select(selected.into_iter().chain([v]));
            let got = q.run(&g).unwrap_or_else(|e| panic!("{e}: {q:?}"));
            assert_eq!(sorted(got), sorted(expected), "{a:?} {q:?}");
            queries += 1;
            rows_seen += usize::from(!base_rows.is_empty());
            ranked += usize::from(!tuples.is_empty() && function.ranks());
            // Its graph has the aggregate's links for each row's subject.
            let graph = q.graph(&g).unwrap();
            for l in graph.derived() {
                let ResultLink::Derived {
                    fields: [s, _, _],
                    by: Derivation::Aggregate(_),
                    support,
                } = l
                else {
                    continue;
                };
                let ResultNode::Entry(s) = s else {
                    panic!("{l:?}")
                };
                assert!(tuples.iter().any(|t| t[0] == *s), "{l:?}");
                assert!(support.iter().all(|id| graph.stored().any(|x| x == *id)));
            }
        }
    }
    assert!(queries > 400, "{queries}");
    assert!(rows_seen > 150, "{rows_seen}");
    assert!(ranked > 60, "{ranked}");
}

#[test]
fn link_aggregates() {
    let (g, people, [name, _, banned, age, yes]) = people();
    let [carol, alice, dave, bob] = people[..] else {
        unreachable!()
    };
    let derived = |data: &str| g.ephemeral_value(data.as_bytes());
    let zero = derived("0");
    // The oldest: ranked by age, descending, and rank 0 matched.  The
    // comparison of the rank is of the patterns matching the ranking's
    // links, not ranked.
    let oldest = |not_banned: bool| {
        let mut q = Query::new();
        let (p, a, r) = (q.var("p"), q.var("a"), q.var("r"));
        q.pattern(p, age, a);
        if not_banned {
            let mut n = Group::new();
            n.pattern(p, banned, yes);
            q.not(n);
        }
        q.link_aggregate(LinkAggregate::new(LinkFunction::Rank, p, derived("rank"), a).desc());
        q.pattern(p, derived("rank"), r);
        q.filter(Filter::Eq(Term::Var(r), Term::Id(zero)));
        q.select([p]);
        q.run(&g).unwrap()
    };
    assert_eq!(oldest(false), [[dave]]);
    // A negation is part of what is ranked: of those not banned.
    assert_eq!(oldest(true), [[alice]]);
    // An aggregate can read another's links: each person's count of ages,
    // ranked.
    let mut q = Query::new();
    let (p, a, k, r) = (q.var("p"), q.var("a"), q.var("k"), q.var("r"));
    q.pattern(p, age, a);
    q.link_aggregate(LinkAggregate::new(
        LinkFunction::Fold(Function::Count),
        p,
        derived("ages"),
        a,
    ));
    q.pattern(p, derived("ages"), k);
    q.link_aggregate(LinkAggregate::new(
        LinkFunction::DenseRank,
        p,
        derived("by_ages"),
        k,
    ));
    q.pattern(p, derived("by_ages"), r);
    q.select([p, k, r]);
    assert_eq!(
        sorted(q.run(&g).unwrap()),
        sorted(
            [carol, alice, dave]
                .map(|p| vec![p, derived("1"), zero])
                .to_vec()
        )
    );
    // But not a later one's: here the first's object is bound only by the
    // second's links.
    let mut q = Query::new();
    let (p, a, k, j) = (q.var("p"), q.var("a"), q.var("k"), q.var("j"));
    q.pattern(p, age, a);
    q.pattern(p, derived("cb"), k);
    q.pattern(p, derived("ca"), j);
    q.link_aggregate(LinkAggregate::new(
        LinkFunction::Fold(Function::Count),
        p,
        derived("ca"),
        k,
    ));
    q.link_aggregate(LinkAggregate::new(
        LinkFunction::Fold(Function::Count),
        p,
        derived("cb"),
        j,
    ));
    let err = q.run(&g).unwrap_err().to_string();
    assert!(err.contains("must be bound"), "{err}");
    // Errors: two aggregates of one predicate; a link variable on an
    // aggregate's link; a subject not bound.
    let mut q = Query::new();
    let (p, a, l, x) = (q.var("p"), q.var("a"), q.var("l"), q.var("x"));
    q.pattern(p, age, a);
    let count = LinkAggregate::new(LinkFunction::Fold(Function::Count), p, derived("n"), a);
    let mut twice = q.clone();
    twice
        .link_aggregate(count.clone())
        .link_aggregate(count.clone());
    assert!(twice.run(&g).is_err());
    let mut linked = q.clone();
    linked
        .link_aggregate(count.clone())
        .link_pattern(l, p, derived("n"), a);
    assert!(linked.run(&g).is_err());
    let mut unbound = q.clone();
    unbound.link_aggregate(LinkAggregate::new(
        LinkFunction::Fold(Function::Count),
        x,
        derived("n"),
        a,
    ));
    assert!(unbound.run(&g).is_err());
    // A standing query can't have one.
    let mut standing = q.clone();
    standing.link_aggregate(count.clone());
    assert!(StandingQuery::new(&standing, &g).is_err());
    // The graph: each row's subject's link, supported by the links it
    // folds; a scope's links are each row's scope's.  The rows are the
    // query's without the aggregate.
    let mut q = Query::new();
    let (p, n, a) = (q.var("p"), q.var("n"), q.var("a"));
    q.pattern(p, name, n);
    let mut o = Group::new();
    o.pattern(p, age, a);
    q.optional(o);
    let rows = sorted(q.run(&g).unwrap());
    q.link_aggregate(LinkAggregate::new(
        LinkFunction::Fold(Function::Count),
        p,
        derived("aged"),
        a,
    ));
    assert_eq!(sorted(q.run(&g).unwrap()), rows);
    let graph = q.graph(&g).unwrap();
    let counts: BTreeMap<u64, (ResultNode, Vec<u64>)> = graph
        .derived()
        .map(|l| match l {
            ResultLink::Derived {
                fields: [ResultNode::Entry(s), ResultNode::Value(Cell::Text(pr)), v],
                by: Derivation::Aggregate(f),
                support,
            } => {
                assert_eq!((pr.as_str(), f.as_str()), ("aged", "COUNT"));
                (*s, (v.clone(), support.clone()))
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(counts.len(), 4);
    // Bob has no age: a count of 0, from his name.
    assert_eq!(counts[&bob].0, ResultNode::Value(Cell::Text("0".into())));
    assert_eq!(counts[&bob].1.len(), 1);
    assert_eq!(counts[&carol].1.len(), 2);
    // An aggregate in a rule's body: the oldest, called.
    let mut q = Query::new();
    let oldest = q.relation("oldest", 1);
    let (p, a, r) = (q.var("p"), q.var("a"), q.var("r"));
    let mut body = Group::new();
    body.pattern(p, age, a)
        .pattern(p, derived("rank"), r)
        .filter(Filter::Eq(Term::Var(r), Term::Id(zero)));
    q.rule(oldest, [p], body);
    q.rule_link_aggregate(LinkAggregate::new(LinkFunction::Rank, p, derived("rank"), a).desc());
    let y = q.var("y");
    q.call(oldest, [y]).select([y]);
    assert_eq!(q.run(&g).unwrap(), [[dave]]);
}

/// `q` with a link variable for each pattern of its top group, its unions'
/// branches and its optional groups (not its negations), as recording links
/// should have, and those variables.
fn with_own_link_vars(q: &Query) -> (Query, Vec<Var>) {
    fn group(q: &mut Query, g: &mut Group, vars: &mut Vec<Var>) {
        for p in &mut g.patterns {
            if p.link.is_none() {
                p.link = Some(q.var("own_link"));
            }
            vars.push(p.link.unwrap());
        }
        for b in g.unions.iter_mut().flatten() {
            group(q, b, vars);
        }
        for o in &mut g.optionals {
            group(q, o, vars);
        }
    }
    let mut q = q.clone();
    q.record = false;
    let mut root = std::mem::take(&mut q.root);
    let mut vars = Vec::new();
    group(&mut q, &mut root, &mut vars);
    q.root = root;
    (q, vars)
}

/// A random graph whose facts are in random subgraphs, by random authors.
fn random_graph_with_subgraphs(rng: &mut Rng) -> (Graph, Vec<u64>, Vec<u64>, Vec<u64>, Vec<u64>) {
    let (mut g, nodes, predicates) = random_graph_of(rng, 150);
    let subgraphs: Vec<u64> = (0..3)
        .map(|i| g.value(format!("sg{i}").as_bytes()).unwrap())
        .collect();
    let authors: Vec<u64> = (0..2).map(|_| g.node().unwrap()).collect();
    let live = live_links(&g, None);
    for _ in 0..120 {
        let (_, [s, p, o]) = rng.pick(&live);
        let sg = rng.pick(&subgraphs);
        match rng.below(6) {
            0 => {
                g.remove_from_subgraph(s, p, o, sg).unwrap();
            }
            1 => {
                g.delete_link(s, p, o).unwrap();
            }
            // Into a subgraph, by an author, or by no one.
            _ => {
                let author = if rng.chance(70) {
                    rng.pick(&authors)
                } else {
                    0
                };
                g.add_link(&crate::record::LinkFields {
                    subject: s,
                    predicate: p,
                    object: o,
                    author,
                    subgraphs: vec![sg],
                    ..Default::default()
                })
                .unwrap();
            }
        }
    }
    (g, nodes, predicates, subgraphs, authors)
}

/// Gives some of the patterns of `group`, and the groups in it, a subgraph
/// or an author: a constant, a new variable, or one of `vars`.
fn restrict(
    rng: &mut Rng,
    q: &mut Query,
    group: &mut Group,
    vars: &mut Vec<Var>,
    subgraphs: &[u64],
    authors: &[u64],
) {
    fn term(rng: &mut Rng, q: &mut Query, vars: &mut Vec<Var>, consts: &[u64]) -> Term {
        match rng.below(3) {
            0 => Term::Id(rng.pick(consts)),
            1 if !vars.is_empty() => Term::Var(rng.pick(vars)),
            _ => {
                let v = q.var("sg");
                vars.push(v);
                Term::Var(v)
            }
        }
    }
    for p in &mut group.patterns {
        if rng.chance(40) {
            p.graph = Some(term(rng, q, vars, subgraphs));
        }
        if rng.chance(20) {
            p.author = Some(term(rng, q, vars, authors));
        }
    }
    for g in group
        .unions
        .iter_mut()
        .flatten()
        .chain(&mut group.nots)
        .chain(&mut group.optionals)
    {
        restrict(rng, q, g, vars, subgraphs, authors);
    }
}

#[test]
fn random_queries_in_subgraphs() {
    // Patterns restricted to subgraphs and authors, constant or variable,
    // against brute force, under each join.
    let mut rng = Rng(0x3c6e_f372_fe94_f82b);
    let (mut rows, mut restricted, mut by_index) = (0, 0, 0);
    for _ in 0..6 {
        let (g, nodes, predicates, subgraphs, authors) = random_graph_with_subgraphs(&mut rng);
        let ids = (g.start_id(), g.next_id());
        for _ in 0..80 {
            let q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            let mut q = q;
            // The selected variables, as they were: the new ones aren't.
            let selected = q.selected();
            let mut root = std::mem::take(&mut q.root);
            // Their variables bind only in the top group's patterns: those
            // of negations and optional groups take constants.
            let mut vars = Vec::new();
            restrict(&mut rng, &mut q, &mut root, &mut vars, &subgraphs, &authors);
            q.root = root;
            q.select(selected);
            if q.validate().is_err() || q.plan(&g).is_err() {
                continue;
            }
            check(&q, &g);
            restricted += usize::from(q.restricts_links());
            rows += usize::from(!BruteForce::rows(&q, &g).is_empty());
            // And with each pattern of a constant subgraph or author read
            // through its index.
            let expected = BruteForce::rows(&q, &g);
            for force in [Some(Join::Bind), Some(Join::Hash)] {
                let mut plan = plan::plan_with(&q, &g, force).unwrap();
                by_index += through_index(&mut plan.root);
                let got: Vec<Vec<u64>> = plan.execute(&g).collect::<Result<_>>().unwrap();
                check_rows(&q, &g, got, &expected, &plan.explain());
            }
        }
    }
    assert!(restricted > 200, "{restricted}");
    assert!(rows > 100, "{rows}");
    assert!(by_index > 200, "{by_index}");
}

#[test]
fn subgraph_and_author_patterns() {
    let mut g = graph();
    let [p, g1, g2] = [&b"p"[..], b"g1", b"g2"].map(|d| g.value(d).unwrap());
    let (a, b, c, alice) = (
        g.node().unwrap(),
        g.node().unwrap(),
        g.node().unwrap(),
        g.node().unwrap(),
    );
    g.link(a, p, c).unwrap();
    g.add_to_subgraph(a, p, b, g1).unwrap();
    g.add_link(&crate::record::LinkFields {
        subject: a,
        predicate: p,
        object: b,
        author: alice,
        subgraphs: vec![g2],
        ..Default::default()
    })
    .unwrap();
    let run = |q: &Query| sorted(q.run(&g).unwrap());
    // A constant subgraph, through the Subgraph index.
    let mut q = Query::new();
    let o = q.var("o");
    q.pattern(a, p, o).in_subgraph(g1);
    assert_eq!(run(&q), [[b]]);
    let explain = q.plan(&g).unwrap().explain();
    assert!(explain.contains(" in "), "{explain}");
    // A variable: each subgraph.
    let mut q = Query::new();
    let (o, sg) = (q.var("o"), q.var("sg"));
    q.pattern(a, p, o).in_subgraph(sg);
    assert_eq!(run(&q), [[b, g1], [b, g2]]);
    // An author.
    let mut q = Query::new();
    let (o, who) = (q.var("o"), q.var("who"));
    q.pattern(a, p, o).by_author(who);
    assert_eq!(run(&q), [[b, alice]]);
    // A group in a subgraph: its patterns, and those added after.
    let mut q = Query::new();
    let o = q.var("o");
    q.in_graph(g2).pattern(a, p, o);
    assert_eq!(run(&q), [[b]]);
    // Not a standing query.
    assert!(StandingQuery::new(&q, &g).is_err());
}

/// Makes each step of `plan` (and the plans in it) that joins a pattern of
/// a constant subgraph or author read it through that index, returning how
/// many.
fn through_index(plan: &mut plan::GroupPlan) -> usize {
    let mut n = 0;
    for step in &mut plan.steps {
        match step {
            Step::Pattern {
                pattern,
                access,
                reduced: None,
                ..
            } => {
                if let Some(Term::Id(_)) = pattern.graph {
                    *access = Access::Graph;
                    n += 1;
                } else if let Some(Term::Id(_)) = pattern.author {
                    *access = Access::Author;
                    n += 1;
                }
            }
            Step::Union { branches, .. } => {
                n += branches.iter_mut().map(through_index).sum::<usize>()
            }
            Step::Not { group, .. } | Step::Optional { group, .. } => n += through_index(group),
            _ => {}
        }
    }
    n
}

/// The facts of subgraph `sg` of `g`.
fn subgraph_facts(g: &Graph, sg: u64) -> BTreeSet<[u64; 3]> {
    g.subgraph_links(sg, None)
        .into_iter()
        .map(|id| {
            let f = g.get(id).unwrap().as_link().unwrap().fields();
            [f.subject, f.predicate, f.object]
        })
        .collect()
}

#[test]
fn random_maintained_subgraphs() {
    // A subgraph maintained as `?a q ?b` for each row of a random standing
    // query, through random changes and commits, is always that of the
    // query's rows as brute force finds them.
    let mut rng = Rng(0xa54f_f53a_5f1d_36f1);
    let (mut maintained, mut changed) = (0, 0);
    for _ in 0..5 {
        let (mut g, nodes, predicates) = random_graph(&mut rng);
        let q_pred = g.value(b"q").unwrap();
        g.commit().unwrap();
        let ids = (g.start_id(), g.next_id());
        for round in 0..40 {
            let mut q = Generator {
                rng: &mut rng,
                nodes: &nodes,
                predicates: &predicates,
                ids,
            }
            .query();
            q.limit = None;
            q.offset = 0;
            q.order.clear();
            q.as_of = None;
            let selected = q.selected();
            if selected.is_empty() || q.root.has_paths_or_calls() || q.root.has_optionals() {
                continue;
            }
            let (a, b) = (rng.pick(&selected), rng.pick(&selected));
            let sg = g.value(format!("maintained{round}").as_bytes()).unwrap();
            let template = [[Term::Var(a), Term::Id(q_pred), Term::Var(b)]];
            let expected = |g: &Graph| -> BTreeSet<[u64; 3]> {
                let (ka, kb) = (
                    selected.iter().position(|&v| v == a).unwrap(),
                    selected.iter().position(|&v| v == b).unwrap(),
                );
                let mut plain = q.clone();
                plain.distinct = false;
                BruteForce::rows(&plain, g)
                    .into_iter()
                    .map(|r| [r[ka], q_pred, r[kb]])
                    .filter(|f| !f.contains(&0))
                    .collect()
            };
            let (mut m, _) = MaintainedSubgraph::new(&q, template, sg, &mut g).unwrap();
            g.commit().unwrap();
            assert_eq!(subgraph_facts(&g, sg), expected(&g), "{q:?}");
            maintained += 1;
            for _ in 0..3 {
                mutate(&mut rng, &mut g, &nodes, &predicates);
                g.commit().unwrap();
                let changes = m.update(&mut g).unwrap();
                g.commit().unwrap();
                changed += usize::from(!changes.is_empty());
                let want = expected(&g);
                assert_eq!(subgraph_facts(&g, sg), want, "{q:?}");
                assert_eq!(m.facts_given().collect::<BTreeSet<_>>(), want);
            }
        }
    }
    assert!(maintained > 60, "{maintained}");
    assert!(changed > 25, "{changed}");
}

#[test]
fn maintained_subgraphs_settle() {
    // A template giving the facts its query matches, through link
    // variables: the subgraph's own tag changes the links, but not the
    // facts, so updating again writes nothing.
    let mut g = graph();
    let knows = g.value(b"knows").unwrap();
    let [a, b, c] = [0, 1, 2].map(|_| g.node().unwrap());
    g.link(a, knows, b).unwrap();
    g.link(b, knows, c).unwrap();
    let sg = g.value(b"friends").unwrap();
    // A fact in the subgraph that the query won't give: removed.
    let likes = g.value(b"likes").unwrap();
    g.add_to_subgraph(c, likes, a, sg).unwrap();
    g.commit().unwrap();
    let mut q = Query::new();
    let (l, x, y) = (q.var("l"), q.var("x"), q.var("y"));
    q.link_pattern(l, x, knows, y);
    let template = [[Term::Var(x), Term::Id(knows), Term::Var(y)]];
    let (mut m, changes) = MaintainedSubgraph::new(&q, template, sg, &mut g).unwrap();
    assert_eq!(changes.added, [[a, knows, b], [b, knows, c]]);
    assert_eq!(changes.removed, [[c, likes, a]]);
    g.commit().unwrap();
    for _ in 0..3 {
        let changes = m.update(&mut g).unwrap();
        assert!(changes.is_empty(), "{changes:?}");
        g.commit().unwrap();
    }
    // A new fact joins; a deleted one leaves.
    g.link(a, knows, c).unwrap();
    g.delete_link(a, knows, b).unwrap();
    g.commit().unwrap();
    let changes = m.update(&mut g).unwrap();
    assert_eq!(changes.added, [[a, knows, c]]);
    assert!(changes.removed.is_empty(), "deleted already: {changes:?}");
    g.commit().unwrap();
    assert_eq!(
        subgraph_facts(&g, sg),
        BTreeSet::from([[a, knows, c], [b, knows, c]])
    );
    assert!(m.update(&mut g).unwrap().is_empty());
    // A template's variable must be one the query selects.
    q.select([x]);
    assert!(
        MaintainedSubgraph::new(
            &q,
            [[Term::Var(x), Term::Id(knows), Term::Var(y)]],
            sg,
            &mut g
        )
        .is_err()
    );
}

#[test]
fn nearest_matches_a_brute_force_search() {
    use crate::vector::{self, Metric};
    let mut g = graph();
    let mut rng = Rng(7);
    let p = g.value(b"embedding").unwrap();
    let mut random = |n: usize| -> Vec<f32> {
        (0..n)
            .map(|_| (rng.below(2001) as f32 - 1000.0) / 1000.0)
            .collect()
    };
    let mut vectors = Vec::new();
    for _ in 0..300 {
        let s = g.node().unwrap();
        let v = random(8);
        let id = g.value(&vector::encode(&v)).unwrap();
        g.link(s, p, id).unwrap();
        vectors.push((id, v));
    }
    // A value of another dimension, and one that isn't a vector, are skipped.
    let s = g.node().unwrap();
    let other = g.value(&vector::encode(&[1.0, 2.0])).unwrap();
    g.link(s, p, other).unwrap();
    let text = g.value(b"[1, 2, 3, 4, 5, 6, 7, 8]").unwrap();
    g.link(s, p, text).unwrap();
    g.commit().unwrap();
    for (metric, k) in [(Metric::Cosine, 10), (Metric::L2, 1), (Metric::Dot, 25)] {
        let query = random(8);
        let mut want: Vec<(f64, u64)> = vectors
            .iter()
            .map(|(id, v)| (vector::distance(&query, v, metric).unwrap(), *id))
            .collect();
        want.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        want.truncate(k);
        let mut q = Query::new();
        let (v, d, s) = (q.var("v"), q.var("d"), q.var("s"));
        let qv = q.param("q");
        q.group()
            .nearest(Nearest::new(p, qv, k, v, d).metric(metric), s);
        q.select([v, d]);
        let rows = q
            .plan(&g)
            .unwrap()
            .execute_values(&g, &[aggregate::Cell::Text(vector::format(&query))])
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let got: Vec<u64> = rows.iter().map(|r| r[0]).collect();
        assert_eq!(
            got,
            want.iter().map(|w| w.1).collect::<Vec<_>>(),
            "{metric:?}"
        );
        // Distances read back, to the precision a computed value has.
        for (row, w) in rows.iter().zip(&want) {
            let d: f64 = std::str::from_utf8(&g.data(row[1]).unwrap())
                .unwrap()
                .parse()
                .unwrap();
            assert!((d - w.0).abs() < 1e-6, "{d} {}", w.0);
        }
    }
}

#[test]
fn nearest_sees_overlays_and_history() {
    use crate::vector;
    let mut g = graph();
    let p = g.value(b"embedding").unwrap();
    let (a, b) = (g.node().unwrap(), g.node().unwrap());
    let va = g.value(&vector::encode(&[1.0, 0.0])).unwrap();
    g.link(a, p, va).unwrap();
    g.commit().unwrap();
    let before = g.next_id();
    let vb = g.value(&vector::encode(&[0.0, 1.0])).unwrap();
    g.link(b, p, vb).unwrap();
    g.commit().unwrap();
    let nearest = |as_of: Option<u64>| {
        let mut q = Query::new();
        let (v, d, s) = (q.var("v"), q.var("d"), q.var("s"));
        let query = g.ephemeral_value(&vector::encode(&[0.1, 1.0]));
        let mut n = Nearest::new(p, query, 1, v, d);
        n.as_of = as_of;
        q.group().nearest(n, s);
        q.select([v]);
        q
    };
    assert_eq!(nearest(None).run(&g).unwrap(), vec![vec![vb]]);
    // As of before /b's link.
    assert_eq!(nearest(Some(before)).run(&g).unwrap(), vec![vec![va]]);
    let mut q = nearest(None);
    q.as_of(before);
    assert_eq!(q.run(&g).unwrap(), vec![vec![va]]);
    // Over an overlay with a vector of its own, and one deleting /b's link.
    let mut ov = Overlay::new();
    let c = g.ephemeral_node();
    let vc = ov
        .ephemeral_hold(&g)
        .value(&g, &vector::encode(&[0.1, 1.0]));
    ov.insert(&g, [c, p, vc]);
    let ov = Arc::new(ov);
    assert_eq!(nearest(None).run_over(&g, &ov).unwrap(), vec![vec![vc]]);
    let mut del = Overlay::new();
    del.delete(&g, [b, p, vb]);
    assert_eq!(
        nearest(None).run_over(&g, &Arc::new(del)).unwrap(),
        vec![vec![va]]
    );
    // A standing query can't search.
    assert!(StandingQuery::new(&nearest(None), &g).is_err());
}

#[test]
fn nearest_uses_the_vector_index_as_the_scan_would() {
    use crate::graph::VectorIndex;
    use crate::vector::{self, Metric};
    let mut g = graph();
    let mut rng = Rng(11);
    let p = g.value(b"embedding").unwrap();
    let mut random = || -> Vec<f32> {
        (0..8)
            .map(|_| (rng.below(2001) as f32 - 1000.0) / 1000.0)
            .collect()
    };
    let mut vectors = Vec::new();
    let mut first_link = 0;
    for _ in 0..1000 {
        let s = g.node().unwrap();
        let v = random();
        let id = g.value(&vector::encode(&v)).unwrap();
        let l = g.link(s, p, id).unwrap();
        if first_link == 0 {
            first_link = l;
        }
        vectors.push((s, id, v));
    }
    g.create_vector_index(&VectorIndex::new(p, 8, Metric::L2))
        .unwrap();
    // Declared but not committed: searched by scanning.
    assert!(g.vector_search(p).is_none());
    g.commit().unwrap();
    assert_eq!(g.vector_search(p).unwrap().hnsw.live_len(), 1000);
    let query = |as_of: Option<u64>, k: usize| {
        let mut q = Query::new();
        let (v, d, s) = (q.var("v"), q.var("d"), q.var("s"));
        let qv = q.param("q");
        let mut n = Nearest::new(p, qv, k, v, d).metric(Metric::L2);
        n.as_of = as_of;
        q.group().nearest(n, s);
        q.select([v]);
        q
    };
    let run = |g: &Graph, q: &Query, at: &[f32]| -> Vec<u64> {
        q.plan(g)
            .unwrap()
            .execute_values(g, &[aggregate::Cell::Text(vector::format(at))])
            .map(|r| r.unwrap()[0])
            .collect()
    };
    let exact = |live: &[(u64, u64, Vec<f32>)], at: &[f32], k: usize| -> Vec<u64> {
        let mut all: Vec<(f64, u64)> = live
            .iter()
            .map(|(_, id, v)| (vector::distance(at, v, Metric::L2).unwrap(), *id))
            .collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        all.into_iter().take(k).map(|a| a.1).collect()
    };
    // The plan says the index is used; it finds what the scan would.
    let explained = query(None, 10).plan(&g).unwrap().explain();
    assert!(
        explained.contains("via the vector index of 1000 vectors"),
        "{explained}"
    );
    let mut found = 0;
    for _ in 0..20 {
        let at = random();
        let got = run(&g, &query(None, 10), &at);
        let want = exact(&vectors, &at, 10);
        found += want.iter().filter(|w| got.contains(w)).count();
    }
    assert!(found >= 190, "recall {found}/200");
    // A vector linked but not committed is found, by scanning; then by the
    // index, once committed.
    let at = random();
    let s = g.node().unwrap();
    let new = g.value(&vector::encode(&at)).unwrap();
    g.link(s, p, new).unwrap();
    assert!(g.vector_search(p).is_none());
    assert_eq!(run(&g, &query(None, 1), &at), vec![new]);
    g.commit().unwrap();
    assert!(g.vector_search(p).is_some());
    assert_eq!(run(&g, &query(None, 1), &at), vec![new]);
    // Deleted: a reader made before, and a search as of before, still find
    // it; others don't.
    let before = g.next_id() - 1;
    let r = g.reader();
    g.delete_link(s, p, new).unwrap();
    g.commit().unwrap();
    assert_ne!(run(&g, &query(None, 1), &at), vec![new]);
    assert_eq!(run(&r, &query(None, 1), &at), vec![new]);
    assert_eq!(run(&g, &query(Some(before), 1), &at), vec![new]);
    let mut q = query(None, 1);
    q.as_of(before);
    assert_eq!(run(&g, &q, &at), vec![new]);
    // As of before the index was built: by scanning, which finds the same.
    let first = vectors[0].1;
    assert_eq!(
        run(&g, &query(Some(first_link), 1), &vectors[0].2),
        vec![first]
    );
    // Over an overlay with the predicate's links, by scanning.
    let mut ov = Overlay::new();
    let vc = ov.ephemeral_hold(&g).value(&g, &vector::encode(&at));
    let c = g.ephemeral_node();
    ov.insert(&g, [c, p, vc]);
    let plan = query(None, 1).plan(&g).unwrap();
    let rows: Vec<u64> = plan
        .execute_cells_over(
            &g,
            &Arc::new(ov),
            &[aggregate::Cell::Text(vector::format(&at))],
        )
        .unwrap()
        .into_iter()
        .map(|r| match r[0] {
            aggregate::Cell::Id(id) => id,
            _ => 0,
        })
        .collect();
    assert_eq!(rows, vec![vc]);
    // Built again once those no longer live are a quarter of it.
    drop(r);
    for (s, id, _) in vectors.iter().take(400) {
        g.delete_link(*s, p, *id).unwrap();
    }
    g.commit().unwrap();
    assert_eq!(g.vector_search(p).unwrap().hnsw.len(), 1001);
    g.tick().unwrap();
    let search = g.vector_search(p).unwrap();
    assert_eq!((search.hnsw.len(), search.hnsw.live_len()), (600, 600));
    let at = random();
    let got = run(&g, &query(None, 5), &at);
    let want = exact(&vectors[400..], &at, 5);
    assert!(want.iter().filter(|w| got.contains(w)).count() >= 4);
    // Dropped: by scanning again.
    g.drop_vector_index(p).unwrap();
    g.commit().unwrap();
    assert!(g.vector_search(p).is_none());
}
