//! Path expressions: regular expressions over links, matched by searching
//! the graph.
//!
//! A [`PathExpr`] describes the chains of links that join a subject to an
//! object: one link with a predicate, the inverse of a path, a sequence of
//! paths, alternatives, or a path repeated between a least and a greatest
//! number of times.  It is compiled to an automaton ([`Automaton`]) whose
//! transitions follow links forward or backward, and the pairs it matches
//! are found by a breadth-first search over pairs of an entry and a state,
//! from whichever end is known.  Each pair matches once.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use crate::id_hash::{IdHashSet, IdMap};

/// The most times a bounded repetition may give, so that compiling it
/// stays small.
pub const MAX_REPEAT: u32 = 64;

/// A path through the graph: see the [module](self) docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathExpr {
    /// One live link with the predicate.
    Link(u64),
    /// The path followed backward: `^p`.
    Inverse(Box<PathExpr>),
    /// Each path in turn: `p / q`.
    Seq(Vec<PathExpr>),
    /// Any of the paths: `p | q`.
    Alt(Vec<PathExpr>),
    /// The path repeated at least `min` and at most `max` (unbounded if
    /// `None`) times: `p*` is `(0, None)`, `p+` is `(1, None)`.
    Repeat(Box<PathExpr>, u32, Option<u32>),
}

impl PathExpr {
    /// Whether the path can be empty, joining an entry to itself.
    pub fn nullable(&self) -> bool {
        match self {
            PathExpr::Link(_) => false,
            PathExpr::Inverse(e) => e.nullable(),
            PathExpr::Seq(es) => es.iter().all(PathExpr::nullable),
            PathExpr::Alt(es) => es.iter().any(PathExpr::nullable),
            PathExpr::Repeat(e, min, _) => *min == 0 || e.nullable(),
        }
    }

    /// The predicates the path uses.
    pub fn predicates(&self, out: &mut Vec<u64>) {
        match self {
            PathExpr::Link(p) => out.push(*p),
            PathExpr::Inverse(e) | PathExpr::Repeat(e, ..) => e.predicates(out),
            PathExpr::Seq(es) | PathExpr::Alt(es) => es.iter().for_each(|e| e.predicates(out)),
        }
    }

    /// Checks that repetitions are within [`MAX_REPEAT`] and that `min`
    /// is at most `max`.
    pub fn check(&self) -> Result<(), &'static str> {
        match self {
            PathExpr::Link(_) => Ok(()),
            PathExpr::Inverse(e) => e.check(),
            PathExpr::Seq(es) | PathExpr::Alt(es) => {
                if es.is_empty() {
                    return Err("a path sequence or alternative needs a path");
                }
                es.iter().try_for_each(PathExpr::check)
            }
            PathExpr::Repeat(e, min, max) => {
                if max.is_some_and(|max| max < *min) {
                    return Err("a path repetition's least is more than its most");
                }
                if *min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT) {
                    return Err("a path repetition is bounded by 64");
                }
                e.check()
            }
        }
    }

    /// The path written with `name` naming each predicate, in GraQL's
    /// syntax.
    pub fn display(&self, name: &dyn Fn(u64) -> String) -> String {
        self.show(name, 0)
    }

    /// `self`, parenthesized if its precedence is below `outer` (0:
    /// alternatives, 1: sequences, 2: postfix and inverse).
    fn show(&self, name: &dyn Fn(u64) -> String, outer: u8) -> String {
        let (text, precedence) = match self {
            PathExpr::Link(p) => (name(*p), 3),
            PathExpr::Inverse(e) => (format!("^{}", e.show(name, 3)), 2),
            PathExpr::Seq(es) => {
                let parts: Vec<String> = es.iter().map(|e| e.show(name, 2)).collect();
                (parts.join(" / "), 1)
            }
            PathExpr::Alt(es) => {
                let parts: Vec<String> = es.iter().map(|e| e.show(name, 1)).collect();
                (parts.join(" | "), 0)
            }
            PathExpr::Repeat(e, min, max) => {
                let suffix = match (min, max) {
                    (0, None) => "*".to_string(),
                    (1, None) => "+".to_string(),
                    (m, None) => format!("{{{m},}}"),
                    (m, Some(n)) if m == n => format!("{{{m}}}"),
                    (m, Some(n)) => format!("{{{m},{n}}}"),
                };
                (format!("{}{suffix}", e.show(name, 3)), 2)
            }
        };
        if precedence < outer {
            format!("({text})")
        } else {
            text
        }
    }
}

impl fmt::Display for PathExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display(&|p| p.to_string()))
    }
}

/// A step of an automaton: a link with `predicate`, followed from its
/// subject to its object if `forward`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Step {
    pub predicate: u64,
    pub forward: bool,
}

/// A nondeterministic automaton over links, compiled from a [`PathExpr`].
#[derive(Debug, Clone)]
pub struct Automaton {
    /// Each state's transitions: by a step, or by nothing (`None`).
    transitions: Vec<Vec<(Option<Step>, usize)>>,
    start: usize,
    accept: usize,
    /// Each state's transitions by a step.
    moves: Vec<Vec<(Step, usize)>>,
    /// The states that matter of those reachable from each state by
    /// transitions that take no link: the accepting state, and those with
    /// a transition by a step.  A search need visit no others.
    live: Vec<Vec<usize>>,
}

impl Automaton {
    /// The automaton of `expr`, followed forward, or backward (matching
    /// from object to subject) if not `forward`.
    pub fn new(expr: &PathExpr, forward: bool) -> Self {
        let mut a = Automaton {
            transitions: Vec::new(),
            start: 0,
            accept: 0,
            moves: Vec::new(),
            live: Vec::new(),
        };
        let (start, accept) = a.build(expr, forward);
        a.start = start;
        a.accept = accept;
        a.moves = a
            .transitions
            .iter()
            .map(|ts| ts.iter().filter_map(|&(s, to)| Some((s?, to))).collect())
            .collect();
        a.live = (0..a.transitions.len())
            .map(|s| {
                let mut live = a.closure([s]);
                live.retain(|&t| t == a.accept || !a.moves[t].is_empty());
                live.sort_unstable();
                live
            })
            .collect();
        a
    }

    fn state(&mut self) -> usize {
        self.transitions.push(Vec::new());
        self.transitions.len() - 1
    }

    fn edge(&mut self, from: usize, step: Option<Step>, to: usize) {
        self.transitions[from].push((step, to));
    }

    /// Adds the states of `expr`, returning its start and accepting states.
    fn build(&mut self, expr: &PathExpr, forward: bool) -> (usize, usize) {
        match expr {
            PathExpr::Link(p) => {
                let (s, t) = (self.state(), self.state());
                self.edge(
                    s,
                    Some(Step {
                        predicate: *p,
                        forward,
                    }),
                    t,
                );
                (s, t)
            }
            PathExpr::Inverse(e) => self.build(e, !forward),
            PathExpr::Seq(es) => {
                // Backward, a sequence is followed from its end.
                let parts: Vec<(usize, usize)> = if forward {
                    es.iter().map(|e| self.build(e, forward)).collect()
                } else {
                    es.iter().rev().map(|e| self.build(e, forward)).collect()
                };
                for w in parts.windows(2) {
                    self.edge(w[0].1, None, w[1].0);
                }
                (parts[0].0, parts[parts.len() - 1].1)
            }
            PathExpr::Alt(es) => {
                let (s, t) = (self.state(), self.state());
                for e in es {
                    let (es, et) = self.build(e, forward);
                    self.edge(s, None, es);
                    self.edge(et, None, t);
                }
                (s, t)
            }
            PathExpr::Repeat(e, min, max) => {
                let s = self.state();
                let mut end = s;
                // The required copies, then the optional ones, or a loop.
                for _ in 0..*min {
                    let (es, et) = self.build(e, forward);
                    self.edge(end, None, es);
                    end = et;
                }
                let t = self.state();
                self.edge(end, None, t);
                match max {
                    None => {
                        let (es, et) = self.build(e, forward);
                        self.edge(end, None, es);
                        self.edge(et, None, end);
                    }
                    Some(max) => {
                        for _ in *min..*max {
                            let (es, et) = self.build(e, forward);
                            self.edge(end, None, es);
                            end = et;
                            self.edge(end, None, t);
                        }
                    }
                }
                (s, t)
            }
        }
    }

    /// The states reachable from `states` by transitions that take no link.
    fn closure(&self, states: impl IntoIterator<Item = usize>) -> Vec<usize> {
        let mut seen = HashSet::new();
        let mut stack: Vec<usize> = states.into_iter().collect();
        let mut out = Vec::new();
        while let Some(s) = stack.pop() {
            if !seen.insert(s) {
                continue;
            }
            out.push(s);
            for &(step, to) in &self.transitions[s] {
                if step.is_none() {
                    stack.push(to);
                }
            }
        }
        out
    }

    /// The entries reachable from `start` by a path the automaton accepts,
    /// sorted, with `next(step, x, out)` adding to `out` the entries one
    /// link from `x`.
    pub fn reach(&self, start: u64, next: impl FnMut(Step, u64, &mut Vec<u64>)) -> Vec<u64> {
        self.reach_from(&[start], next)
    }

    /// The entries reachable from any of `starts`, sorted: the union of
    /// [`reach`](Self::reach) from each, found by one search, so that what
    /// several starts reach is visited once.
    pub fn reach_from(
        &self,
        starts: &[u64],
        mut next: impl FnMut(Step, u64, &mut Vec<u64>),
    ) -> Vec<u64> {
        // Each pair of an entry and a state is visited once, in any order.
        let mut seen: IdHashSet<(u64, usize)> = IdHashSet::default();
        let mut stack: Vec<(u64, usize)> = Vec::new();
        let mut out = Vec::new();
        let mut ys = Vec::new();
        for &start in starts {
            for &s in &self.live[self.start] {
                if seen.insert((start, s)) {
                    stack.push((start, s));
                }
            }
        }
        while let Some((x, s)) = stack.pop() {
            if s == self.accept {
                out.push(x);
            }
            for &(step, to) in &self.moves[s] {
                ys.clear();
                next(step, x, &mut ys);
                for &y in &ys {
                    for &t in &self.live[to] {
                        if seen.insert((y, t)) {
                            stack.push((y, t));
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// The entries reachable from `start`, sorted, each with the least
    /// number of links of a path the automaton accepts to it: 0 for `start`
    /// itself, if the empty path is accepted.  The search is breadth first,
    /// so a pair of an entry and a state is first reached by a shortest
    /// path.
    pub fn reach_lengths(
        &self,
        start: u64,
        mut next: impl FnMut(Step, u64, &mut Vec<u64>),
    ) -> Vec<(u64, u32)> {
        let mut seen: IdHashSet<(u64, usize)> = IdHashSet::default();
        let mut queue: VecDeque<(u64, usize, u32)> = VecDeque::new();
        let mut out = Vec::new();
        let mut ys = Vec::new();
        for &s in &self.live[self.start] {
            seen.insert((start, s));
            queue.push_back((start, s, 0));
        }
        while let Some((x, s, n)) = queue.pop_front() {
            if s == self.accept {
                out.push((x, n));
            }
            for &(step, to) in &self.moves[s] {
                ys.clear();
                next(step, x, &mut ys);
                for &y in &ys {
                    for &t in &self.live[to] {
                        if seen.insert((y, t)) {
                            queue.push_back((y, t, n + 1));
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// The links of a shortest path the automaton accepts from `start` to
    /// `goal`, in order, if there is one; `next(step, x)` gives the links one
    /// step from `x` and the entries they reach.
    pub fn witness(
        &self,
        start: u64,
        goal: u64,
        mut next: impl FnMut(Step, u64) -> Vec<(u64, u64)>,
    ) -> Option<Vec<u64>> {
        type Pair = (u64, usize);
        // Each pair reached: the pair it was reached from, and by which link.
        let mut parent: IdMap<Pair, Option<(Pair, u64)>> = IdMap::default();
        let mut queue: VecDeque<Pair> = VecDeque::new();
        for &s in &self.live[self.start] {
            if parent.insert((start, s), None).is_none() {
                queue.push_back((start, s));
            }
        }
        let mut next_cache: HashMap<(Step, u64), Vec<(u64, u64)>> = HashMap::new();
        while let Some((x, s)) = queue.pop_front() {
            if s == self.accept && x == goal {
                let mut links = Vec::new();
                let mut at = (x, s);
                while let Some(&Some((from, link))) = parent.get(&at) {
                    links.push(link);
                    at = from;
                }
                links.reverse();
                return Some(links);
            }
            for &(step, to) in &self.moves[s] {
                let ys = next_cache
                    .entry((step, x))
                    .or_insert_with(|| next(step, x))
                    .clone();
                for (link, y) in ys {
                    for &t in &self.live[to] {
                        if let std::collections::hash_map::Entry::Vacant(e) = parent.entry((y, t)) {
                            e.insert(Some(((x, s), link)));
                            queue.push_back((y, t));
                        }
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small graph of predicate 1 (a -> b -> c -> d) and 2 (a -> x, d -> a).
    fn next(step: Step, x: u64) -> Vec<u64> {
        let links = [
            (10, 1, 11),
            (11, 1, 12),
            (12, 1, 13),
            (10, 2, 20),
            (13, 2, 10),
        ];
        links
            .iter()
            .filter(|&&(s, p, o)| p == step.predicate && if step.forward { s == x } else { o == x })
            .map(|&(s, _, o)| if step.forward { o } else { s })
            .collect()
    }

    fn reach(e: &PathExpr, start: u64, forward: bool) -> Vec<u64> {
        Automaton::new(e, forward).reach(start, |step, x, out| out.extend(next(step, x)))
    }

    #[test]
    fn matching_paths() {
        use PathExpr::*;
        let p = || Link(1);
        let q = || Link(2);
        assert_eq!(reach(&p(), 10, true), [11]);
        assert_eq!(
            reach(&Repeat(Box::new(p()), 1, None), 10, true),
            [11, 12, 13]
        );
        assert_eq!(
            reach(&Repeat(Box::new(p()), 0, None), 10, true),
            [10, 11, 12, 13]
        );
        assert_eq!(
            reach(&Repeat(Box::new(p()), 2, Some(3)), 10, true),
            [12, 13]
        );
        assert_eq!(
            reach(&Repeat(Box::new(p()), 0, Some(1)), 10, true),
            [10, 11]
        );
        assert_eq!(reach(&Repeat(Box::new(p()), 2, Some(2)), 10, true), [12]);
        assert_eq!(reach(&Inverse(Box::new(p())), 12, true), [11]);
        assert_eq!(reach(&Seq(vec![p(), p()]), 10, true), [12]);
        assert_eq!(reach(&Alt(vec![p(), q()]), 10, true), [11, 20]);
        // p+ then q: from d back to a, through a cycle.
        let cycle = Seq(vec![Repeat(Box::new(p()), 1, None), q()]);
        assert_eq!(reach(&cycle, 10, true), [10]);
        assert_eq!(
            reach(&Repeat(Box::new(cycle.clone()), 1, None), 11, true),
            [10]
        );
        // Backward: the subjects joined to an object.
        assert_eq!(reach(&Seq(vec![p(), p()]), 12, false), [10]);
        assert_eq!(reach(&Seq(vec![p(), q()]), 10, false), [12]);
        // ^(p / q) from a: the y with y p m and m q a, which is c.
        assert_eq!(
            reach(&Inverse(Box::new(Seq(vec![p(), q()]))), 10, true),
            [12]
        );
        assert_eq!(
            reach(&Repeat(Box::new(p()), 1, None), 13, false),
            [10, 11, 12]
        );
    }

    #[test]
    fn lengths_and_many_starts() {
        use PathExpr::*;
        let p = || Link(1);
        let q = || Link(2);
        let lengths = |e: &PathExpr, start: u64, forward: bool| {
            Automaton::new(e, forward)
                .reach_lengths(start, |step, x, out| out.extend(next(step, x)))
        };
        let plus = Repeat(Box::new(p()), 1, None);
        assert_eq!(lengths(&plus, 10, true), [(11, 1), (12, 2), (13, 3)]);
        assert_eq!(lengths(&plus, 13, false), [(10, 3), (11, 2), (12, 1)]);
        let star = Repeat(Box::new(p()), 0, None);
        assert_eq!(lengths(&star, 12, true), [(12, 0), (13, 1)]);
        // Around the cycle a -> b -> c -> d -> a, by either predicate: the
        // start is reached again, by the 4 links of the cycle.
        let any = Repeat(Box::new(Alt(vec![p(), q()])), 1, None);
        assert_eq!(
            lengths(&any, 10, true),
            [(10, 4), (11, 1), (12, 2), (13, 3), (20, 1)]
        );
        // Both ways, the far side of the cycle is nearer backward.
        let both = Repeat(
            Box::new(Alt(vec![any.clone(), Inverse(Box::new(q()))])),
            1,
            None,
        );
        assert_eq!(lengths(&both, 10, true)[3], (13, 1));
        // Every link of a sequence counts, and the least of the chains.
        let seq = Seq(vec![plus.clone(), q()]);
        assert_eq!(lengths(&seq, 11, true), [(10, 3)]);
        // Each length is that of the shortest path `witness` finds.
        for (y, n) in lengths(&any, 10, true) {
            let links = Automaton::new(&any, true)
                .witness(10, y, |step, x| {
                    next(step, x)
                        .into_iter()
                        .map(|y| (x * 100 + y, y))
                        .collect()
                })
                .expect("reached");
            assert_eq!(links.len() as u32, n);
        }

        let from = |e: &PathExpr, starts: &[u64]| {
            Automaton::new(e, true).reach_from(starts, |step, x, out| out.extend(next(step, x)))
        };
        assert_eq!(from(&plus, &[11, 12]), [12, 13]);
        assert_eq!(from(&plus, &[]), [] as [u64; 0]);
        assert_eq!(from(&Seq(vec![p(), p()]), &[10, 11, 10]), [12, 13]);
        for starts in [&[10u64, 13][..], &[12, 20, 11]] {
            let mut each: Vec<u64> = starts.iter().flat_map(|&s| reach(&any, s, true)).collect();
            each.sort_unstable();
            each.dedup();
            assert_eq!(from(&any, starts), each);
        }
    }

    #[test]
    fn properties() {
        use PathExpr::*;
        let star = Repeat(Box::new(Link(1)), 0, None);
        assert!(star.nullable());
        assert!(!Seq(vec![star.clone(), Link(2)]).nullable());
        assert!(Alt(vec![Link(1), star.clone()]).nullable());
        assert!(Repeat(Box::new(Link(1)), 2, Some(1)).check().is_err());
        assert!(Repeat(Box::new(Link(1)), 0, Some(65)).check().is_err());
        let e = Seq(vec![
            Inverse(Box::new(Link(1))),
            Alt(vec![Link(2), Repeat(Box::new(Link(3)), 1, Some(3))]),
            Repeat(Box::new(Seq(vec![Link(4), Link(5)])), 0, None),
        ]);
        assert_eq!(e.to_string(), "^1 / (2 | 3{1,3}) / (4 / 5)*");
    }
}
