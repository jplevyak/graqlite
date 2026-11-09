//! Evaluation of the relations defined by a query's rules, to a fixpoint.
//!
//! The relations a query calls, and those they call, are evaluated in
//! strata: each set of relations that call each other is evaluated after
//! the relations it calls otherwise, and a relation may not call one of its
//! own set within a negation.  A set is evaluated semi-naively: first its
//! rules with its relations empty, then, while that adds tuples, each rule
//! once for each call in it to the set, with that call matching only the
//! tuples added last, the calls before it all the tuples, and those after
//! it the tuples from before.  So each derivation is found once a tuple it
//! uses is new, and no work is repeated for tuples found before.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use super::plan::{self, Overrides};
use super::profile::{RoundProfile, StratumProfile};
use super::provenance::{Asserting, Derivations, RowProvenance};
use super::{Group, Query, Relation, link_aggregate};
use crate::graph::{Error, Graph, Result};

/// The tuples of a relation.
pub(crate) type Tuples = Arc<Vec<Vec<u64>>>;

/// The tuples of each relation of a query, if it was evaluated.
#[derive(Debug, Clone, Default)]
pub(crate) struct Relations {
    pub tuples: Vec<Option<Tuples>>,
    /// If the query records its links, each tuple's first derivation.
    pub derivations: Option<Arc<Derivations>>,
    /// If the query records its links, the rules asserting triples that
    /// derive each tuple.
    pub asserting: Option<Arc<Asserting>>,
    /// The components evaluated, in order, and the rounds each took.
    pub strata: Vec<StratumProfile>,
}

/// A place in a group: the union and branch taken at each level.
type Path = Vec<(usize, usize)>;

fn group_at<'a>(group: &'a Group, path: &[(usize, usize)]) -> &'a Group {
    path.iter().fold(group, |g, &(u, b)| &g.unions[u][b])
}

/// The calls in `group` outside its negations: where, and to which
/// relation.
fn calls(group: &Group, path: &mut Path, out: &mut Vec<(Path, usize, Relation)>) {
    for (i, c) in group.calls.iter().enumerate() {
        out.push((path.clone(), i, c.relation));
    }
    for (u, branches) in group.unions.iter().enumerate() {
        for (b, branch) in branches.iter().enumerate() {
            path.push((u, b));
            calls(branch, path, out);
            path.pop();
        }
    }
}

/// The relations `group` calls, and whether each call is within a negation.
fn dependencies(group: &Group, negated: bool, out: &mut Vec<(Relation, bool)>) {
    out.extend(group.calls.iter().map(|c| (c.relation, negated)));
    for g in group.unions.iter().flatten() {
        dependencies(g, negated, out);
    }
    // An optional group, like a negation, is not monotonic: a new match can
    // replace a row it did not match.
    for g in group.nots.iter().chain(&group.optionals) {
        dependencies(g, true, out);
    }
}

/// The strongly connected components of the relations reachable from
/// `roots`, those depended on first (Tarjan's algorithm).
fn components(
    edges: &HashMap<Relation, Vec<(Relation, bool)>>,
    roots: &[Relation],
) -> Vec<Vec<Relation>> {
    struct Tarjan<'a> {
        edges: &'a HashMap<Relation, Vec<(Relation, bool)>>,
        index: HashMap<Relation, usize>,
        low: HashMap<Relation, usize>,
        stack: Vec<Relation>,
        on_stack: HashSet<Relation>,
        out: Vec<Vec<Relation>>,
    }
    impl Tarjan<'_> {
        fn visit(&mut self, r: Relation) {
            let i = self.index.len();
            self.index.insert(r, i);
            self.low.insert(r, i);
            self.stack.push(r);
            self.on_stack.insert(r);
            for &(s, _) in self.edges.get(&r).into_iter().flatten() {
                if !self.index.contains_key(&s) {
                    self.visit(s);
                    let low = self.low[&r].min(self.low[&s]);
                    self.low.insert(r, low);
                } else if self.on_stack.contains(&s) {
                    let low = self.low[&r].min(self.index[&s]);
                    self.low.insert(r, low);
                }
            }
            if self.low[&r] == self.index[&r] {
                let mut component = Vec::new();
                while let Some(s) = self.stack.pop() {
                    self.on_stack.remove(&s);
                    component.push(s);
                    if s == r {
                        break;
                    }
                }
                self.out.push(component);
            }
        }
    }
    let mut t = Tarjan {
        edges,
        index: HashMap::new(),
        low: HashMap::new(),
        stack: Vec::new(),
        on_stack: HashSet::new(),
        out: Vec::new(),
    };
    for &r in roots {
        if !t.index.contains_key(&r) {
            t.visit(r);
        }
    }
    t.out
}

/// The strata of the relations `q` calls, and those they call: groups of
/// relations that depend on each other, each after those it depends on.
fn strata(q: &Query) -> Result<Vec<BTreeSet<Relation>>> {
    let mut roots = Vec::new();
    dependencies(&q.root, false, &mut roots);
    // The top group's aggregates, whose links its graph has.
    roots.extend(q.aggregated.iter().map(|&(r, _)| (r, false)));
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut edges: HashMap<Relation, Vec<(Relation, bool)>> = HashMap::new();
    for rule in &q.rules {
        let deps = edges.entry(rule.relation).or_default();
        // An aggregate, like a negation, needs all of what it reads.
        dependencies(&rule.body, rule.aggregate.is_some(), deps);
    }
    let roots: Vec<Relation> = roots.into_iter().map(|(r, _)| r).collect();
    let mut strata = Vec::new();
    for component in components(&edges, &roots) {
        let members: BTreeSet<Relation> = component.iter().copied().collect();
        let negated_within = members.iter().any(|r| {
            edges
                .get(r)
                .into_iter()
                .flatten()
                .any(|&(s, negated)| negated && members.contains(&s))
        });
        if negated_within {
            return Err(Error::InvalidArgument(
                "rules are not stratified: a relation depends on its own negation",
            ));
        }
        strata.push(members);
    }
    Ok(strata)
}

/// Checks that the relations `q` calls can be evaluated, without
/// evaluating them.
pub(crate) fn check(q: &Query) -> Result<()> {
    strata(q).map(|_| ())
}

/// Evaluates the relations `q` calls, and those they call.
pub(crate) fn evaluate(q: &Query, g: &Graph) -> Result<Relations> {
    evaluate_over(q, g, None)
}

/// The same, beneath an overlay.
pub(crate) fn evaluate_over(
    q: &Query,
    g: &Graph,
    overlay: Option<&Arc<super::Overlay>>,
) -> Result<Relations> {
    let mut relations = Relations {
        tuples: vec![None; q.relations.len()],
        derivations: None,
        asserting: None,
        strata: Vec::new(),
    };
    // If the query records its links, so do its rules, and each tuple keeps
    // the provenance of the row that first found it.
    let mut derivations = q.record.then(Derivations::new);
    let mut asserting = Asserting::new();
    for members in strata(q)? {
        evaluate_component(
            q,
            g,
            overlay,
            &members,
            &mut relations,
            &mut derivations,
            &mut asserting,
        )?;
    }
    relations.derivations = derivations.map(Arc::new);
    relations.asserting = q.record.then(|| Arc::new(asserting));
    Ok(relations)
}

/// The rows of `rule`'s head for its body, with the given calls' tuples,
/// and, if `record`, what each matched.
#[allow(clippy::too_many_arguments)]
fn rule_rows(
    q: &Query,
    g: &Graph,
    overlay: Option<&Arc<super::Overlay>>,
    head: &[super::Var],
    body: &Group,
    relations: &Relations,
    calls: &[(Path, usize, Tuples)],
    record: bool,
) -> Result<Vec<(Vec<u64>, RowProvenance)>> {
    let mut sub = Query {
        names: q.names.clone(),
        root: body.clone(),
        select: Some(head.to_vec()),
        as_of: q.as_of,
        reduction_budget: q.reduction_budget,
        relations: q.relations.clone(),
        rules: q.rules.clone(),
        ..Query::default()
    };
    // Recording rewrites the body, so the calls' addresses are its own.
    if record {
        sub.record_links();
        sub = sub.with_link_vars().expect("records its links");
    }
    let mut overrides = Overrides::default();
    for (path, i, tuples) in calls {
        let address = group_at(&sub.root, path) as *const Group as usize;
        overrides.calls.insert((address, *i), tuples.clone());
    }
    let plan = plan::plan_full(&sub, g, None, &overrides, Some(relations))?;
    if record {
        return plan.rows_with_provenance(g, overlay);
    }
    let rows = match overlay {
        Some(o) => plan.execute_over(g, o, &[]),
        None => plan.execute(g),
    };
    rows.map(|row| row.map(|r| (r, RowProvenance::default())))
        .collect()
}

fn snapshot(set: &HashSet<Vec<u64>>) -> Tuples {
    let mut tuples: Vec<Vec<u64>> = set.iter().cloned().collect();
    tuples.sort();
    Arc::new(tuples)
}

/// Evaluates the relations of a component, whose dependencies outside it
/// are in `relations`, semi-naively.
fn evaluate_component(
    q: &Query,
    g: &Graph,
    overlay: Option<&Arc<super::Overlay>>,
    members: &BTreeSet<Relation>,
    relations: &mut Relations,
    derivations: &mut Option<Derivations>,
    asserting: &mut Asserting,
) -> Result<()> {
    let record = derivations.is_some();
    let names = members.iter().map(|r| q.relations[r.0].0.clone()).collect();
    let mut stratum = StratumProfile {
        relations: names,
        rounds: Vec::new(),
    };
    let mut round = Instant::now();
    // The rules asserting triples that derive a tuple, each once.
    let mut asserts = |i: usize, r: Relation, row: &[u64]| {
        if record && !q.rules[i].assertions.is_empty() {
            let rules = asserting.entry((r, row.to_vec())).or_default();
            if !rules.contains(&i) {
                rules.push(i);
            }
        }
    };
    // Keeps the first derivation of a tuple.
    let mut derived = |r: Relation, row: &[u64], prov: RowProvenance| {
        if let Some(d) = derivations.as_mut() {
            d.entry((r, row.to_vec())).or_insert(prov);
        }
    };
    let (indexes, rules): (Vec<usize>, Vec<_>) = q
        .rules
        .iter()
        .enumerate()
        .filter(|(_, r)| members.contains(&r.relation))
        .unzip();
    // An aggregate's relation, alone in its component (an aggregate within a
    // cycle isn't stratified): its tuples fold its body's rows.
    if let [rule] = rules[..]
        && let Some(a) = &rule.aggregate
    {
        let rows = rule_rows(
            q,
            g,
            overlay,
            &rule.head,
            &rule.body,
            relations,
            &[],
            record,
        )?;
        let heads: Vec<Vec<u64>> = rows.iter().map(|(r, _)| r.clone()).collect();
        let mut set = HashSet::new();
        for (t, members) in a.tuples(q, g, &heads) {
            if record {
                derived(
                    rule.relation,
                    &t,
                    link_aggregate::merge(members.iter().map(|&i| rows[i].1.clone())),
                );
            }
            set.insert(t);
        }
        stratum.rounds.push(RoundProfile {
            new_tuples: set.len() as u64,
            time: round.elapsed(),
        });
        relations.tuples[rule.relation.0] = Some(snapshot(&set));
        relations.strata.push(stratum);
        return Ok(());
    }
    // Each rule's calls to the component.
    let recursive: Vec<Vec<(Path, usize, Relation)>> = rules
        .iter()
        .map(|r| {
            let mut all = Vec::new();
            calls(&r.body, &mut Vec::new(), &mut all);
            all.retain(|(_, _, rel)| members.contains(rel));
            all
        })
        .collect();
    let empty: Tuples = Arc::new(Vec::new());
    let mut known: HashMap<Relation, HashSet<Vec<u64>>> =
        members.iter().map(|&r| (r, HashSet::new())).collect();
    let mut delta: HashMap<Relation, HashSet<Vec<u64>>> = known.clone();
    // The rules with the component's relations empty.
    for ((rule, calls), &index) in rules.iter().zip(&recursive).zip(&indexes) {
        let given: Vec<_> = calls
            .iter()
            .map(|(p, i, _)| (p.clone(), *i, empty.clone()))
            .collect();
        for (row, prov) in rule_rows(
            q, g, overlay, &rule.head, &rule.body, relations, &given, record,
        )? {
            asserts(index, rule.relation, &row);
            derived(rule.relation, &row, prov);
            delta.get_mut(&rule.relation).unwrap().insert(row);
        }
    }
    let new_tuples = |delta: &HashMap<Relation, HashSet<Vec<u64>>>| {
        delta.values().map(|d| d.len() as u64).sum::<u64>()
    };
    stratum.rounds.push(RoundProfile {
        new_tuples: new_tuples(&delta),
        time: round.elapsed(),
    });
    while delta.values().any(|d| !d.is_empty()) {
        round = Instant::now();
        let old: HashMap<Relation, Tuples> = known.iter().map(|(&r, s)| (r, snapshot(s))).collect();
        let new: HashMap<Relation, Tuples> = delta.iter().map(|(&r, s)| (r, snapshot(s))).collect();
        for (r, d) in &delta {
            known.get_mut(r).unwrap().extend(d.iter().cloned());
        }
        let all: HashMap<Relation, Tuples> = known.iter().map(|(&r, s)| (r, snapshot(s))).collect();
        let mut next: HashMap<Relation, HashSet<Vec<u64>>> =
            members.iter().map(|&r| (r, HashSet::new())).collect();
        for ((rule, calls), &index) in rules.iter().zip(&recursive).zip(&indexes) {
            for i in 0..calls.len() {
                if new[&calls[i].2].is_empty() {
                    continue;
                }
                let given: Vec<_> = calls
                    .iter()
                    .enumerate()
                    .map(|(j, (p, k, rel))| {
                        let tuples = match j.cmp(&i) {
                            std::cmp::Ordering::Less => &all[rel],
                            std::cmp::Ordering::Equal => &new[rel],
                            std::cmp::Ordering::Greater => &old[rel],
                        };
                        (p.clone(), *k, tuples.clone())
                    })
                    .collect();
                for (row, prov) in rule_rows(
                    q, g, overlay, &rule.head, &rule.body, relations, &given, record,
                )? {
                    asserts(index, rule.relation, &row);
                    if !known[&rule.relation].contains(&row) {
                        derived(rule.relation, &row, prov);
                        next.get_mut(&rule.relation).unwrap().insert(row);
                    }
                }
            }
        }
        delta = next;
        stratum.rounds.push(RoundProfile {
            new_tuples: new_tuples(&delta),
            time: round.elapsed(),
        });
    }
    for (r, set) in known {
        relations.tuples[r.0] = Some(snapshot(&set));
    }
    relations.strata.push(stratum);
    Ok(())
}
