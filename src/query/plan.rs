//! Cost-based planning of a [`Query`]; see the [module](super) docs.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use super::aggregate::Cell;
use super::exec::Rows;
use super::profile::{Counts, Profile, Profiler, ReductionProfile, StepProfile, step_key};
use super::reduce::{self, Link, Reduced, Reduction};
use super::result::ResultGraph;
use super::rules::{self, Relations, Tuples};
use super::{
    Call, Filter, Group, Namer, Nearest, PathExpr, PathPattern, Pattern, Query, Term, Var,
};
use crate::graph::{Error, Graph, Result};
use crate::local_table::IdSet;
use crate::record::Linkage;
use crate::tuple_index;

/// The most patterns and unions of a group planned exhaustively; more are
/// planned greedily.
pub const EXHAUSTIVE_PATTERNS: usize = 12;

/// The cost of an index lookup, apart from the entries it reads.
const LOOKUP_COST: f64 = 4.0;
/// The cost of reading an index entry and the link it names.
const ENTRY_COST: f64 = 1.0;
/// The cost of adding a row to a hash table.
const BUILD_COST: f64 = 2.0;
/// The cost of probing a hash table with a row.
const PROBE_COST: f64 = 1.0;
/// The cost of testing a row with a filter.
const FILTER_COST: f64 = 0.2;
/// The cost of starting to evaluate a group.
const GROUP_COST: f64 = 1.0;
/// Measuring the distance from a vector to one searched.
const DISTANCE_COST: f64 = 1.0;
/// The cost of a lookup in reduced links, apart from the links it finds.
const REDUCED_LOOKUP_COST: f64 = 0.5;
/// The cost of a reduced link found by a lookup or a scan.
const REDUCED_ENTRY_COST: f64 = 0.2;
/// How much more a reduction costs than a join, for the same lookups and
/// entries, as it builds sets of candidates and links: about twice, by
/// `benches/lookups.rs`.
const REDUCTION_FACTOR: f64 = 2.0;

/// The index a pattern is looked up in, given what is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// The link is bound: read it.
    Link,
    Spo,
    SubjectPredicate,
    PredicateObject,
    Subject,
    Predicate,
    Object,
    /// Nothing is bound: every predicate's links.
    All,
    /// The links naming a subgraph, or by an author, each checked to be its
    /// fact's latest and live: for a pattern restricted to a constant one.
    Graph,
    Author,
}

impl Access {
    /// The access for a pattern with these positions bound.
    pub(crate) fn of(link: bool, [s, p, o]: [bool; 3]) -> Access {
        if link {
            return Access::Link;
        }
        match (s, p, o) {
            (true, true, true) => Access::Spo,
            (true, true, false) => Access::SubjectPredicate,
            (false, true, true) => Access::PredicateObject,
            (true, false, _) => Access::Subject,
            (false, true, false) => Access::Predicate,
            (false, false, true) => Access::Object,
            (false, false, false) => Access::All,
        }
    }

    /// The positions of the index's key.
    fn key(self) -> [bool; 3] {
        match self {
            Access::Link => [false; 3],
            Access::Spo => [true; 3],
            Access::SubjectPredicate => [true, true, false],
            Access::PredicateObject => [false, true, true],
            Access::Subject => [true, false, false],
            Access::Predicate => [false, true, false],
            Access::Object => [false, false, true],
            Access::All | Access::Graph | Access::Author => [false; 3],
        }
    }

    fn name(self) -> &'static str {
        match self {
            Access::Link => "link",
            Access::Spo => "spo index",
            Access::SubjectPredicate => "subject+predicate index",
            Access::PredicateObject => "predicate+object index",
            Access::Subject => "subject index",
            Access::Predicate => "predicate index",
            Access::Object => "object index",
            Access::All => "all links",
            Access::Graph => "subgraph index",
            Access::Author => "author index",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Join {
    Bind,
    Hash,
}

/// A step of a group's plan, applied to the rows of the steps before it.
#[derive(Debug, Clone)]
pub(crate) enum Step {
    /// Joins a pattern: for a bind join, `access` is the lookup for each
    /// row; for a hash join, the scan, joined on `keys`.  If the pattern was
    /// reduced, its links are `reduced` and `access` is unused; if its group
    /// was, the values it binds must be among the variables' `candidates`.
    Pattern {
        pattern: Pattern,
        join: Join,
        access: Access,
        keys: Vec<Var>,
        rows: f64,
        /// The index entries' ids expected to be read, over every row.
        entries: f64,
        reduced: Option<Arc<Vec<Link>>>,
        /// Whether `reduced` must be used: the links are given, not found.
        required: bool,
        candidates: Option<Arc<Vec<Option<IdSet>>>>,
    },
    /// Joins a union, whose branches bind `vars`.  The branches' plans are
    /// for the rows so far, for a bind join, or alone, for a hash join on
    /// `keys`.
    Union {
        branches: Vec<GroupPlan>,
        vars: Vec<Var>,
        keys: Vec<Var>,
        join: Join,
        rows: f64,
    },
    /// Joins a path pattern: for each row, a search from whichever of its
    /// ends is bound.
    Path {
        path: PathPattern,
        rows: f64,
    },
    /// Joins a call to a relation, whose tuples are `tuples`: given, if
    /// `given`, or else the relation's when planned.
    Call {
        call: Call,
        tuples: Tuples,
        given: bool,
        rows: f64,
    },
    /// Joins a search for the values nearest a vector: for each row, those
    /// of the links its `scan` (planned alone, run once) finds.
    /// The index's search is used instead when it finds the same (see
    /// `NearestLookup`): `indexed` is its vectors, if the predicate had one
    /// when planned.
    Nearest {
        nearest: Nearest,
        scan: GroupPlan,
        indexed: Option<usize>,
        rows: f64,
    },
    Filter(Vec<Filter>),
    /// Removes the rows for which a group has a match, given their values of
    /// `keys`.  The group's plan is for the rows so far, for a bind join, or
    /// alone, for a hash join.
    Not {
        group: GroupPlan,
        keys: Vec<Var>,
        join: Join,
        rows: f64,
    },
    /// Extends each row with each match of an optional group, or keeps it,
    /// with `vars` (the variables only the group binds) unbound, if there is
    /// none.  The group's plan is for the rows so far, for a bind join, or
    /// alone, for a hash join on `keys`.
    Optional {
        group: GroupPlan,
        vars: Vec<Var>,
        keys: Vec<Var>,
        join: Join,
        rows: f64,
    },
    /// Keeps the rows for which a negated group's match changes between the
    /// graph as `old` sees it and as `new` does: to no match if
    /// `to_unmatched`, else to a match.  Only rows whose values of
    /// `flip_vars` are in `flips` can change.
    NotChange {
        old: GroupPlan,
        new: GroupPlan,
        keys: Vec<Var>,
        flips: Arc<HashSet<Vec<u64>>>,
        flip_vars: Vec<Var>,
        to_unmatched: bool,
        rows: f64,
    },
}

/// Replacements in a query, for computing its changes: patterns whose
/// links are given, and negations whose changes are wanted, by the address
/// of their group and their index in it.
#[derive(Default)]
pub(crate) struct Overrides {
    pub patterns: HashMap<(usize, usize), Vec<Link>>,
    pub nots: HashMap<(usize, usize), NotChange>,
    /// Calls whose tuples are given.
    pub calls: HashMap<(usize, usize), Tuples>,
}

/// A negated group as the graph was and is; see [`Step::NotChange`].
pub(crate) struct NotChange {
    pub old: Group,
    pub new: Group,
    pub flips: Arc<HashSet<Vec<u64>>>,
    pub flip_vars: Vec<Var>,
    pub to_unmatched: bool,
}

/// The plan of a group.
#[derive(Debug, Clone)]
pub(crate) struct GroupPlan {
    /// Shared by the plan's clones, which a profile knows them by.
    pub steps: Arc<Vec<Step>>,
    /// The estimated cost and rows for each row given.
    pub cost: f64,
    pub rows: f64,
}

/// A plan for running a [`Query`], made by [`Query::plan`].
#[derive(Debug, Clone)]
pub struct Plan {
    pub(crate) query: Query,
    pub(crate) root: GroupPlan,
    /// The reduction of the query's top group, if any.
    pub(crate) reduction: Option<Arc<Reduction>>,
    /// The relations the query calls, as evaluated when planned.
    pub(crate) relations: Arc<Relations>,
    /// Whether they were given (as a rule's body is planned while its
    /// relations are evaluated), so never to be evaluated again.
    pub(crate) relations_given: bool,
    /// The graph's next id when planned: the reduction is of the graph then.
    pub(crate) next_id: u64,
    /// Whether each slot of a row is a parameter's.
    pub(crate) params: Arc<[bool]>,
    /// What the planner expected the reduction to cost, if it estimated it.
    pub(crate) reduction_cost: Option<f64>,
    /// The counters of a profiled run; see [`Plan::profile`].
    pub(crate) profiler: Option<Arc<Profiler>>,
}

impl Plan {
    /// The estimated cost of running the plan, in rough units of index
    /// entries read.
    pub fn estimated_cost(&self) -> f64 {
        self.root.cost
    }

    /// The estimated rows, before any limit or deduplication.
    pub fn estimated_rows(&self) -> f64 {
        self.root.rows
    }

    /// A description of the plan, a line per step, with ids as numbers.
    pub fn explain(&self) -> String {
        self.explain_with(&|id| id.to_string())
    }

    /// The same, naming each id of the query with `name`, such as by its
    /// key or data.
    pub fn explain_with(&self, name: &dyn Fn(u64) -> String) -> String {
        self.explain_of(name, None)
    }

    /// The plan, as [`explain_with`](Self::explain_with) prints it, with
    /// what `profile`, a [profile](Self::profile) of it, says each step did.
    pub fn explain_profile(&self, profile: &Profile, name: &dyn Fn(u64) -> String) -> String {
        self.explain_of(name, Some(profile))
    }

    /// `profile` of this plan, from its counters.
    pub(crate) fn profile_of(
        &self,
        counts: HashMap<usize, Counts>,
        rows: u64,
        time: Duration,
    ) -> Profile {
        let mut sink = Sink {
            counts: &counts,
            steps: Vec::new(),
        };
        let name = |id: u64| id.to_string();
        let mut text = String::new();
        explain_group(
            &self.query,
            &self.root,
            0,
            true,
            &name,
            &mut text,
            Some(&mut sink),
        );
        let steps = sink.steps;
        Profile {
            steps,
            rows,
            time,
            strata: self.relations.strata.clone(),
            reduction: self.reduction.as_ref().map(|r| ReductionProfile {
                entries: r.entries,
                estimated_cost: self.reduction_cost,
                time: r.time,
            }),
            counts,
        }
    }

    fn explain_of(&self, name: &dyn Fn(u64) -> String, profile: Option<&Profile>) -> String {
        let q = &self.query;
        let mut out = String::new();
        for (i, tuples) in self.relations.tuples.iter().enumerate() {
            if let Some(t) = tuples {
                let _ = writeln!(out, "relation {}: {} tuples", q.relations[i].0, t.len());
            }
        }
        for s in profile.iter().flat_map(|p| &p.strata) {
            let time: Duration = s.rounds.iter().map(|r| r.time).sum();
            let _ = writeln!(
                out,
                "stratum {}: {} rounds, {}",
                s.relations.join(", "),
                s.rounds.len(),
                millis(time)
            );
            for (i, r) in s.rounds.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "  round {}: {} new tuples, {}",
                    i + 1,
                    r.new_tuples,
                    millis(r.time)
                );
            }
        }
        if let Some(r) = &self.reduction {
            let vars: Vec<String> = r
                .vars
                .iter()
                .enumerate()
                .filter_map(|(v, s)| Some(format!("{} {}", q.var_name(Var(v)), s.as_ref()?.len())))
                .collect();
            let candidates = if vars.is_empty() {
                String::new()
            } else {
                format!("{} candidates, ", vars.join(", "))
            };
            let measured = profile
                .and_then(|p| p.reduction)
                .map_or(String::new(), |r| {
                    let estimate = r
                        .estimated_cost
                        .map_or(String::new(), |c| format!(", estimated cost ~{c:.0}"));
                    format!("{estimate}, {}", millis(r.time))
                });
            let _ = writeln!(
                out,
                "reduce: {candidates}{} index entries read{}{measured}",
                r.entries,
                if r.complete {
                    ""
                } else {
                    ", stopped at budget"
                }
            );
        }
        let mut sink = profile.map(|p| Sink {
            counts: &p.counts,
            steps: Vec::new(),
        });
        explain_group(q, &self.root, 0, true, name, &mut out, sink.as_mut());
        let var = |v: Var| q.var_name(v);
        // Nested limits, after matching: each within the values of those
        // before it.
        for (k, &(v, limit, offset)) in q.value_limits.iter().enumerate() {
            let _ = write!(out, "nested limit {} {limit}", var(v));
            if offset > 0 {
                let _ = write!(out, " offset {offset}");
            }
            if k > 0 {
                let before: Vec<String> = q.value_limits[..k].iter().map(|l| var(l.0)).collect();
                let _ = write!(out, " for each {}", before.join(", "));
            }
            out.push('\n');
        }
        for (v, c) in &q.computed {
            let _ = match c {
                super::Computed::Value(e) => {
                    writeln!(out, "compute {} = {}", var(*v), e.show(&var, name))
                }
                super::Computed::Aggregate {
                    function,
                    arg,
                    distinct,
                } => writeln!(
                    out,
                    "aggregate {} = {}({}{})",
                    var(*v),
                    function.name(),
                    if *distinct { "distinct " } else { "" },
                    arg.as_ref().map_or("*".into(), |e| e.show(&var, name))
                ),
            };
        }
        if q.aggregates() {
            let keys: Vec<String> = q.grouping().into_iter().map(var).collect();
            let _ = writeln!(out, "group by {}", keys.join(", "));
        }
        for e in &q.having {
            let _ = writeln!(out, "having {}", e.show(&var, name));
        }
        if q.distinct {
            out.push_str("distinct\n");
        }
        if !q.order.is_empty() {
            let keys: Vec<String> = q.order.iter().map(|k| q.sort_key_name(k)).collect();
            let _ = writeln!(out, "order by {}", keys.join(", "));
        }
        if q.offset > 0 {
            let _ = writeln!(out, "offset {}", q.offset);
        }
        if let Some(n) = q.limit {
            let _ = writeln!(out, "limit {n}");
        }
        if let Some(id) = q.as_of {
            let _ = writeln!(out, "as of {id}");
        }
        if let Some(id) = q.since {
            let _ = writeln!(out, "since {id}");
        }
        if q.history {
            let _ = writeln!(out, "history");
        }
        out
    }
}

fn join_name(q: &Query, first: bool, join: Join, keys: &[Var]) -> String {
    match (first, join) {
        (true, _) => "scan".to_string(),
        (_, Join::Bind) => "bind join".to_string(),
        (_, Join::Hash) if keys.is_empty() => "hash join (cross product)".to_string(),
        (_, Join::Hash) => {
            let keys: Vec<String> = keys.iter().map(|&v| q.var_name(v)).collect();
            format!("hash join on {}", keys.join(", "))
        }
    }
}

/// Describes `plan`, whose rows start as a single row binding nothing if
/// `alone`.
/// The time, as `\explain analyze` prints it.
fn millis(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}

/// What a walk of a plan's steps is told and collects, for a profile.
struct Sink<'a> {
    counts: &'a HashMap<usize, Counts>,
    steps: Vec<StepProfile>,
}

impl Sink<'_> {
    /// Records `step`, whose text starts at `start` of `out`, in `slot`, and
    /// adds what it did to its first line.
    fn annotate(&mut self, step: &Step, slot: usize, depth: usize, start: usize, out: &mut String) {
        let actual = self
            .counts
            .get(&step_key(step))
            .copied()
            .unwrap_or_default();
        let end = start + out[start..].find('\n').unwrap_or(out.len() - start);
        let line = &out[start..end];
        let (text, colon) = match line.strip_suffix(':') {
            Some(text) => (text, ":"),
            None => (line, ""),
        };
        let (estimated_rows, estimated_entries) = step.estimates();
        let mut note = String::new();
        if actual.runs == 0 {
            note.push_str(" never run");
        } else {
            let _ = write!(note, " actual {}", actual.rows);
            if actual.runs > 1 {
                let _ = write!(note, " over {} runs", actual.runs);
            }
            if let Some(e) = estimated_entries {
                let _ = write!(note, ", entries ~{e:.0} actual {}", actual.entries);
            }
            if actual.lookups > 0 {
                let _ = write!(note, ", {} lookups", actual.lookups);
            }
            let _ = write!(note, ", {}", millis(actual.time));
        }
        self.steps[slot] = StepProfile {
            depth,
            step: text.trim_start().to_string(),
            estimated_rows,
            estimated_entries,
            actual,
        };
        let at = start + text.len();
        debug_assert_eq!(&out[at..end], colon);
        out.insert_str(at, &note);
    }
}

impl Step {
    /// The rows and index entries the planner expected of the step, of
    /// those it estimates.
    fn estimates(&self) -> (Option<f64>, Option<f64>) {
        match self {
            Step::Pattern { rows, entries, .. } => (Some(*rows), Some(*entries)),
            Step::Union { rows, .. }
            | Step::Path { rows, .. }
            | Step::Call { rows, .. }
            | Step::Nearest { rows, .. }
            | Step::Not { rows, .. }
            | Step::Optional { rows, .. }
            | Step::NotChange { rows, .. } => (Some(*rows), None),
            Step::Filter(_) => (None, None),
        }
    }
}

fn explain_group(
    q: &Query,
    plan: &GroupPlan,
    depth: usize,
    alone: bool,
    name: Namer<'_>,
    out: &mut String,
    mut sink: Option<&mut Sink<'_>>,
) {
    let pad = "  ".repeat(depth);
    let mut first = alone;
    for step in plan.steps.iter() {
        let start = out.len();
        let slot = sink.as_deref_mut().map(|s| {
            s.steps.push(StepProfile::default());
            s.steps.len() - 1
        });
        match step {
            Step::Pattern {
                pattern,
                join,
                access,
                keys,
                rows,
                reduced,
                ..
            } => {
                let access = match reduced {
                    Some(links) => format!("{} reduced links", links.len()),
                    None => access.name().to_string(),
                };
                let _ = writeln!(
                    out,
                    "{pad}{} {} via {access}, rows ~{rows:.0}",
                    join_name(q, first, *join, keys),
                    q.pattern_name(pattern, name),
                );
                first = false;
            }
            Step::Union {
                branches,
                keys,
                join,
                rows,
                ..
            } => {
                let _ = writeln!(
                    out,
                    "{pad}union, {}, rows ~{rows:.0}",
                    join_name(q, first, *join, keys)
                );
                for b in branches {
                    let _ = writeln!(out, "{pad}  branch:");
                    explain_group(
                        q,
                        b,
                        depth + 2,
                        *join == Join::Hash,
                        name,
                        out,
                        sink.as_deref_mut(),
                    );
                }
                first = false;
            }
            Step::Path { path, rows } => {
                let how = if first { "scan" } else { "bind join" };
                let _ = writeln!(
                    out,
                    "{pad}{how} {}, rows ~{rows:.0}",
                    q.path_name(path, name)
                );
                first = false;
            }
            Step::Call {
                call, tuples, rows, ..
            } => {
                let how = if first { "scan" } else { "bind join" };
                let _ = writeln!(
                    out,
                    "{pad}{how} {} via {} tuples, rows ~{rows:.0}",
                    q.call_name(call, name),
                    tuples.len()
                );
                first = false;
            }
            Step::Nearest {
                nearest,
                scan,
                indexed,
                rows,
            } => {
                let how = if first { "scan" } else { "bind join" };
                let _ = writeln!(
                    out,
                    "{pad}{how} {}, rows ~{rows:.0}",
                    q.nearest_name(nearest, name)
                );
                match indexed {
                    Some(n) => {
                        let _ = writeln!(out, "{pad}  via the vector index of {n} vectors, or:");
                    }
                    None => {
                        let _ = writeln!(out, "{pad}  searching, once:");
                    }
                }
                explain_group(q, scan, depth + 2, true, name, out, sink.as_deref_mut());
                first = false;
            }
            Step::Filter(filters) => {
                for f in filters {
                    let _ = writeln!(out, "{pad}filter {}", q.filter_name(f, name));
                }
            }
            Step::Not {
                group,
                keys,
                join,
                rows,
            } => {
                let how = match join {
                    Join::Bind => "for the rows",
                    Join::Hash => "once",
                };
                let keys: Vec<String> = keys.iter().map(|&v| q.var_name(v)).collect();
                let _ = writeln!(
                    out,
                    "{pad}not ({how}, on {}), rows ~{rows:.0}:",
                    keys.join(", ")
                );
                explain_group(
                    q,
                    group,
                    depth + 1,
                    *join == Join::Hash,
                    name,
                    out,
                    sink.as_deref_mut(),
                );
            }
            Step::Optional {
                group,
                keys,
                join,
                rows,
                ..
            } => {
                let how = match join {
                    Join::Bind => "for the rows".to_string(),
                    Join::Hash => {
                        let keys: Vec<String> = keys.iter().map(|&v| q.var_name(v)).collect();
                        format!("once, on {}", keys.join(", "))
                    }
                };
                let _ = writeln!(out, "{pad}optional ({how}), rows ~{rows:.0}:");
                explain_group(
                    q,
                    group,
                    depth + 1,
                    *join == Join::Hash,
                    name,
                    out,
                    sink.as_deref_mut(),
                );
                first = false;
            }
            Step::NotChange {
                old,
                new,
                to_unmatched,
                flips,
                rows,
                ..
            } => {
                let to = if *to_unmatched { "no match" } else { "a match" };
                let _ = writeln!(
                    out,
                    "{pad}not changing to {to}, of {} keys, rows ~{rows:.0}; was:",
                    flips.len()
                );
                explain_group(q, old, depth + 1, false, name, out, sink.as_deref_mut());
                let _ = writeln!(out, "{pad}is:");
                explain_group(q, new, depth + 1, false, name, out, sink.as_deref_mut());
            }
        }
        if let (Some(s), Some(slot)) = (sink.as_deref_mut(), slot) {
            s.annotate(step, slot, depth, start, out);
        }
    }
}

fn dedup(mut vars: Vec<Var>) -> Vec<Var> {
    vars.sort();
    vars.dedup();
    vars
}

/// The distinct variables of `pattern` that are `bound`.
pub(crate) fn shared_vars(pattern: &Pattern, bound: &[bool]) -> Vec<Var> {
    dedup(pattern.vars().filter(|v| bound[v.0]).collect())
}

/// Estimates of the links matching patterns.
struct Estimator<'g> {
    g: &'g Graph,
    /// The fraction of all links that are live.
    live_ratio: f64,
    /// Estimates of the distinct subjects and objects of all links.
    subjects: f64,
    objects: f64,
    /// The links matching the constants of a pattern at a subset of its
    /// positions (by bit), live and dead.
    counts: HashMap<(Pattern, u8), f64>,
}

/// An estimate of a lookup: the live links matching, and the index entries
/// read.
#[derive(Debug, Clone, Copy)]
struct Estimate {
    matches: f64,
    entries: f64,
}

impl<'g> Estimator<'g> {
    fn new(g: &'g Graph) -> Self {
        let stats = g.stats();
        let max = |f: fn(&crate::stats::PredicateStats) -> u64| {
            stats.predicates().map(|(_, s)| f(s)).max().unwrap_or(0) as f64
        };
        Estimator {
            g,
            live_ratio: ratio(stats.live(), stats.links()),
            // Most subjects (objects) likely have the commonest predicate.
            subjects: max(|s| s.subjects.len()).max(1.0),
            objects: max(|s| s.objects.len()).max(1.0),
            counts: HashMap::new(),
        }
    }

    /// The links, live and dead, matching the constants of `pattern` at the
    /// positions in `mask` (bit 0 subject, 1 predicate, 2 object).
    fn count(&mut self, pattern: &Pattern, mask: u8) -> f64 {
        if let Some(&c) = self.counts.get(&(*pattern, mask)) {
            return c;
        }
        let id = |t: Term, bit: u8| match t {
            Term::Id(id) if mask & bit != 0 => Some(id),
            _ => None,
        };
        let (s, p, o) = (
            id(pattern.subject, 1),
            id(pattern.predicate, 2),
            id(pattern.object, 4),
        );
        let g = self.g;
        let local = g.local();
        let stats = g.stats();
        let c = match (s, p, o) {
            (Some(s), Some(p), Some(o)) => {
                tuple_index::find_links_by_spo(local, s, p, o).len() as u64
            }
            (Some(s), Some(p), None) => g.property_count(s, Linkage::Subject, p).unwrap_or(0),
            (None, Some(p), Some(o)) => g.property_count(o, Linkage::Object, p).unwrap_or(0),
            (Some(s), None, Some(o)) => g
                .link_count(Linkage::Subject, s)
                .min(g.link_count(Linkage::Object, o)),
            (Some(s), None, None) => g.link_count(Linkage::Subject, s),
            (None, None, Some(o)) => g.link_count(Linkage::Object, o),
            (None, Some(p), None) => stats.predicate(p).map_or(0, |s| s.links),
            (None, None, None) => stats.links(),
        } as f64;
        self.counts.insert((*pattern, mask), c);
        c
    }

    /// Estimates a lookup of `pattern` with the variables `bound`.
    fn estimate(&mut self, pattern: &Pattern, bound: &[bool]) -> Estimate {
        if pattern.link.is_some_and(|v| bound[v.0]) {
            return Estimate {
                matches: 1.0,
                entries: 1.0,
            };
        }
        let terms = pattern.terms();
        let bits =
            |f: &dyn Fn(Term) -> bool| (0..3).fold(0u8, |m, k| m | ((f(terms[k]) as u8) << k));
        let consts = bits(&|t| matches!(t, Term::Id(_)));
        let vars = bits(&|t| t.var().is_some_and(|v| bound[v.0]));
        let pstats = match pattern.predicate {
            Term::Id(p) => Some(self.g.stats().predicate(p).cloned().unwrap_or_default()),
            Term::Var(_) => None,
        };
        let (live_ratio, subjects, objects) = match &pstats {
            Some(s) => (
                ratio(s.live(), s.links),
                s.subjects.len().max(1) as f64,
                s.objects.len().max(1) as f64,
            ),
            None => (self.live_ratio, self.subjects, self.objects),
        };
        let predicates = self.g.stats().predicate_count().max(1) as f64;
        // The live links matching the positions in `mask`.
        let mut matching = |mask: u8| {
            let mut n = self.count(pattern, mask & consts) * live_ratio;
            for (bit, distinct) in [(1, subjects), (2, predicates), (4, objects)] {
                if mask & vars & bit != 0 {
                    n /= distinct;
                }
            }
            n
        };
        let all = consts | vars;
        let access = Access::of(false, [0, 1, 2].map(|k| all & (1 << k) != 0));
        let key = (0..3).fold(0u8, |m, k| m | ((access.key()[k] as u8) << k));
        Estimate {
            matches: matching(all),
            entries: matching(key) / live_ratio.max(f64::MIN_POSITIVE),
        }
    }
}

impl Estimator<'_> {
    /// The estimated entries one step of `expr` reaches from an entry, and
    /// the entries it can start from.
    fn expr_fanout(&self, expr: &PathExpr) -> (f64, f64) {
        let stats = self.g.stats();
        match expr {
            PathExpr::Link(p) => stats
                .predicate(*p)
                .map_or((0.0, 0.0), |s| (s.per_subject(), s.subjects.len() as f64)),
            PathExpr::Inverse(e) => match &**e {
                PathExpr::Link(p) => stats
                    .predicate(*p)
                    .map_or((0.0, 0.0), |s| (s.per_object(), s.objects.len() as f64)),
                e => self.expr_fanout(e),
            },
            PathExpr::Seq(es) => {
                let first = es.first().map_or(0.0, |e| self.expr_fanout(e).1);
                (es.iter().map(|e| self.expr_fanout(e).0).product(), first)
            }
            PathExpr::Alt(es) => es
                .iter()
                .map(|e| self.expr_fanout(e))
                .fold((0.0, 0.0), |(f, s), (ef, es)| (f + ef, s + es)),
            PathExpr::Repeat(e, min, max) => {
                let (f, starts) = self.expr_fanout(e);
                // A guess, as for p++: a few hops of the fan-out.
                let hops = max.map_or(4.0, |m| f64::from(m.min(4)));
                let reach = (f * hops).max(f.powf(f64::from(*min).min(4.0)));
                (reach.max(f64::from(u8::from(*min == 0))), starts)
            }
        }
    }

    /// Estimates a search of `path` with the variables `bound`.
    fn path_estimate(&self, path: &PathPattern, bound: &[bool]) -> Estimate {
        let known = |t: Term| match t {
            Term::Id(_) => true,
            Term::Var(v) => bound[v.0],
        };
        if let Some(expr) = &path.expr {
            let (fanout, starts) = self.expr_fanout(expr);
            let reach = fanout.clamp(1.0, self.objects.max(1.0));
            let matches = match (known(path.subject), known(path.object)) {
                (true, true) => 0.5,
                (true, false) | (false, true) => reach,
                // An empty path from every entry: not to be planned so.
                (false, false) if expr.nullable() => 1e12,
                (false, false) => starts.max(1.0) * reach,
            };
            return Estimate {
                matches,
                entries: matches * fanout.max(1.0),
            };
        }
        let stats = self.g.stats();
        let (links, subjects, objects) = match path.predicate {
            Term::Id(p) => stats.predicate(p).map_or((0.0, 1.0, 1.0), |s| {
                (
                    s.live() as f64,
                    s.subjects.len() as f64,
                    s.objects.len() as f64,
                )
            }),
            Term::Var(_) => (stats.live() as f64, self.subjects, self.objects),
        };
        let fanout = links / subjects.max(1.0);
        // A guess: a few hops of the fan-out, up to every object.
        let reach = (fanout * 4.0).clamp(1.0, objects.max(1.0));
        let matches = match (known(path.subject), known(path.object)) {
            (true, true) => 0.5,
            (true, false) | (false, true) => reach,
            (false, false) => subjects * reach,
        };
        Estimate {
            matches,
            entries: matches * fanout.max(1.0),
        }
    }
}

/// Whether reducing `group` could cost less than its plan without
/// reduction, of `cost` and `rows`: whether an optimistic estimate of the
/// cost of reducing it, and of joining what it finds, is below `cost`.
///
/// The reduction reads the pattern with the fewest index entries first,
/// then each pattern by the cheaper of its own constants or a lookup per
/// candidate of a variable it shares with one read, each at
/// [`REDUCTION_FACTOR`] times a join's cost.  A variable's
/// candidates are estimated as the fewest links of a pattern read with it,
/// and at most `rows`, as are each pattern's reduced links: it is optimistic
/// in taking the reduction to narrow everything to what the rows use.
fn reduction_may_pay(
    est: &mut Estimator,
    group: &Group,
    width: usize,
    cost: f64,
    rows: f64,
) -> Option<f64> {
    let rows = rows.max(1.0);
    let patterns = &group.patterns;
    let unbound = vec![false; width];
    // The candidates of each variable read so far.
    let mut candidates: Vec<Option<f64>> = vec![None; width];
    let mut read = vec![false; patterns.len()];
    let mut reducing = 0.0;
    for _ in 0..patterns.len() {
        // The cheapest pattern to read next: its cost and links.
        let mut next: Option<(usize, f64, f64)> = None;
        for i in (0..patterns.len()).filter(|&i| !read[i]) {
            let p = &patterns[i];
            let alone = est.estimate(p, &unbound);
            let mut way = (LOOKUP_COST + alone.entries * ENTRY_COST, alone.matches);
            let shared = p.vars().filter_map(|v| candidates[v.0]).reduce(f64::min);
            if let Some(n) = shared {
                let bound: Vec<bool> = (0..width).map(|v| candidates[v].is_some()).collect();
                let per = est.estimate(p, &bound);
                let by_candidates = (
                    n * (LOOKUP_COST + per.entries * ENTRY_COST),
                    n * per.matches,
                );
                if by_candidates.0 < way.0 {
                    way = by_candidates;
                }
            }
            if next.is_none_or(|(_, c, _)| way.0 < c) {
                next = Some((i, way.0, way.1));
            }
        }
        let (i, c, links) = next.expect("a pattern not read");
        read[i] = true;
        reducing += c * REDUCTION_FACTOR;
        if reducing >= cost {
            return None;
        }
        let links = links.clamp(1.0, rows);
        for v in patterns[i].vars() {
            let n = candidates[v.0].map_or(links, |n| n.min(links));
            candidates[v.0] = Some(n);
        }
    }
    // Joining the reduced links: scanning the first, and a reduced lookup
    // per row for each other.
    let n = patterns.len() as f64;
    let joining = n * rows * REDUCED_ENTRY_COST + (n - 1.0) * rows * REDUCED_LOOKUP_COST;
    (reducing + joining < cost).then_some(reducing)
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 { 1.0 } else { a as f64 / b as f64 }
}

/// A pattern or union of a group, to be joined.
#[derive(Clone, Copy)]
enum Item<'q> {
    /// A pattern, its reduced links, if it has them, and whether they are
    /// required.
    Pattern(&'q Pattern, Option<&'q Reduced>, bool),
    Path(&'q PathPattern),
    /// A call, its tuples, and whether they were given.
    Call(&'q Call, &'q CallTuples, bool),
    Nearest(&'q Nearest),
    Union(&'q [Group]),
}

/// The tuples of a call, and the distinct values of each column.
struct CallTuples {
    tuples: Tuples,
    distinct: Vec<usize>,
}

impl CallTuples {
    fn new(tuples: Tuples, arity: usize) -> Self {
        let distinct = (0..arity)
            .map(|k| {
                tuples
                    .iter()
                    .map(|t| t[k])
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
            })
            .collect();
        CallTuples { tuples, distinct }
    }
}

/// A partial plan of a group.
#[derive(Clone)]
struct Partial {
    cost: f64,
    rows: f64,
    steps: Vec<Step>,
    bound: Vec<bool>,
    /// Whether the rows are still the single row given to the group's plan
    /// with nothing bound, so that a hash join gains nothing.
    unit: bool,
    filters_done: Vec<bool>,
    nots_done: Vec<bool>,
}

struct Planner<'g, 'o> {
    est: Estimator<'g>,
    overrides: &'o Overrides,
    relations: Arc<Relations>,
    /// The variables bound before the top group starts: the parameters.
    params: Vec<bool>,
    /// For testing: the join to use wherever it can be.
    force: Option<Join>,
    /// The top group (by address), and its reduction.
    root: usize,
    reduction: Option<Arc<Reduction>>,
    candidates: Option<Arc<Vec<Option<IdSet>>>>,
    /// The plans of groups (by address) for the variables bound.
    memo: HashMap<(usize, Vec<bool>), Option<GroupPlan>>,
}

impl Planner<'_, '_> {
    /// What a group planned alone has bound: only the parameters.
    fn unbound(&self) -> Vec<bool> {
        self.params.clone()
    }

    /// The plan of `group` for rows binding `bound`, or `None` if it has a
    /// filter or negation that can not be applied.
    fn plan_group(&mut self, group: &Group, bound: &[bool]) -> Option<GroupPlan> {
        let key = (group as *const Group as usize, bound.to_vec());
        if let Some(plan) = self.memo.get(&key) {
            return plan.clone();
        }
        let plan = self.plan_group_uncached(group, bound);
        self.memo.insert(key, plan.clone());
        plan
    }

    fn plan_group_uncached(&mut self, group: &Group, bound: &[bool]) -> Option<GroupPlan> {
        let address = group as *const Group as usize;
        let reduction = self.reduction.clone().filter(|_| address == self.root);
        // Each pattern's reduced links: given, or found by the reduction.
        let reduced: Vec<(Option<Reduced>, bool)> = (0..group.patterns.len())
            .map(|i| match self.overrides.patterns.get(&(address, i)) {
                Some(links) => (Some(Reduced::new(&group.patterns[i], links.clone())), true),
                None => (reduction.as_ref().and_then(|r| r.links[i].clone()), false),
            })
            .collect();
        // Each call's tuples: given, or the relation's.
        let calls: Vec<(CallTuples, bool)> = group
            .calls
            .iter()
            .enumerate()
            .map(|(i, c)| match self.overrides.calls.get(&(address, i)) {
                Some(t) => (CallTuples::new(t.clone(), c.args.len()), true),
                None => {
                    let t = self.relations.tuples[c.relation.0]
                        .clone()
                        .unwrap_or_default();
                    (CallTuples::new(t, c.args.len()), false)
                }
            })
            .collect();
        let items: Vec<Item> = group
            .patterns
            .iter()
            .zip(&reduced)
            .map(|(p, (r, required))| Item::Pattern(p, r.as_ref(), *required))
            .chain(group.paths.iter().map(Item::Path))
            .chain(
                group
                    .calls
                    .iter()
                    .zip(&calls)
                    .map(|(c, (t, given))| Item::Call(c, t, *given)),
            )
            .chain(group.nearest.iter().map(Item::Nearest))
            .chain(group.unions.iter().map(|u| Item::Union(u)))
            .collect();
        let candidates = reduction.as_ref().and(self.candidates.clone());
        // The variables bound once every item is joined.
        let mut bindable = bound.to_vec();
        group.maybe_vars().iter().for_each(|v| bindable[v.0] = true);
        let start = Partial {
            cost: 0.0,
            rows: 1.0,
            steps: Vec::new(),
            bound: bound.to_vec(),
            unit: bound == self.params,
            filters_done: vec![false; group.filters.len()],
            nots_done: vec![false; group.nots.len()],
        };
        let start = self.apply_ready(group, &bindable, start)?;
        let n = items.len();
        let best = if n <= EXHAUSTIVE_PATTERNS {
            let mut best: Vec<Option<Partial>> = vec![None; 1 << n];
            best[0] = Some(start);
            for mask in 0..(1usize << n) {
                let Some(partial) = best[mask].take() else {
                    continue;
                };
                for i in (0..n).filter(|&i| mask & (1 << i) == 0) {
                    for way in self.extend(group, &bindable, &partial, items[i], &candidates) {
                        let next = &mut best[mask | (1 << i)];
                        if next.as_ref().is_none_or(|b| way.cost < b.cost) {
                            *next = Some(way);
                        }
                    }
                }
                best[mask] = Some(partial);
            }
            best.pop().flatten()?
        } else {
            let mut partial = start;
            let mut left: Vec<Item> = items;
            while !left.is_empty() {
                let mut ways = Vec::new();
                for (i, &item) in left.iter().enumerate() {
                    for way in self.extend(group, &bindable, &partial, item, &candidates) {
                        ways.push((i, way));
                    }
                }
                let (i, way) = ways
                    .into_iter()
                    .min_by(|a, b| a.1.cost.total_cmp(&b.1.cost))?;
                left.remove(i);
                partial = way;
            }
            partial
        };
        let best = self.join_optionals(group, &bindable, best)?;
        if best.filters_done.contains(&false) || best.nots_done.contains(&false) {
            return None;
        }
        Some(GroupPlan {
            steps: Arc::new(best.steps),
            cost: best.cost,
            rows: best.rows,
        })
    }

    /// Joins the optional groups of `group` to `p`, its rows once the rest
    /// of it is joined, then applies the filters and negations that are
    /// ready; `None` if one can not be planned.
    fn join_optionals(
        &mut self,
        group: &Group,
        bindable: &[bool],
        mut p: Partial,
    ) -> Option<Partial> {
        for optional in &group.optionals {
            let keys: Vec<Var> = optional
                .all_vars()
                .into_iter()
                .filter(|v| p.bound[v.0])
                .collect();
            let vars: Vec<Var> = optional
                .maybe_vars()
                .into_iter()
                .filter(|v| !p.bound[v.0])
                .collect();
            let bind = self.plan_group(optional, &p.bound).map(|sub| {
                let cost = p.rows * sub.cost + GROUP_COST;
                (Join::Bind, sub, cost)
            });
            let unbound = self.unbound();
            let hash = self.plan_group(optional, &unbound).map(|sub| {
                let cost = sub.cost + sub.rows * BUILD_COST + p.rows * PROBE_COST;
                (Join::Hash, sub, cost)
            });
            let ways: Vec<_> = [bind, hash].into_iter().flatten().collect();
            let forced = ways.iter().any(|w| Some(w.0) == self.force);
            let (join, sub, cost) = ways
                .into_iter()
                .filter(|w| !forced || Some(w.0) == self.force)
                .min_by(|a, b| a.2.total_cmp(&b.2))?;
            // Each row is kept, or extended by each match.
            let matches = match join {
                Join::Bind => sub.rows,
                Join::Hash => sub.rows / (1.0 + p.rows),
            };
            p.cost += cost;
            p.rows *= matches.max(1.0);
            vars.iter().for_each(|v| p.bound[v.0] = true);
            p.unit = false;
            p.steps.push(Step::Optional {
                group: sub,
                vars,
                keys,
                join,
                rows: p.rows,
            });
            p = self.apply_ready(group, bindable, p)?;
        }
        Some(p)
    }

    /// Applies the filters and negations of `group` that are ready, or
    /// returns `None` if a negation ready can not be evaluated.
    fn apply_ready(&mut self, group: &Group, bindable: &[bool], mut p: Partial) -> Option<Partial> {
        let ready: Vec<usize> = (0..group.filters.len())
            .filter(|&f| !p.filters_done[f] && group.filters[f].vars().iter().all(|v| p.bound[v.0]))
            .collect();
        if !ready.is_empty() {
            p.cost += p.rows * FILTER_COST;
            for &f in &ready {
                p.filters_done[f] = true;
                p.rows *= group.filters[f].selectivity();
            }
            p.steps.push(Step::Filter(
                ready.iter().map(|&f| group.filters[f].clone()).collect(),
            ));
        }
        let address = group as *const Group as usize;
        for (i, not) in group.nots.iter().enumerate() {
            // The negation applies once the variables it shares are bound.
            let keys: Vec<Var> = not
                .all_vars()
                .into_iter()
                .filter(|v| bindable[v.0])
                .collect();
            if p.nots_done[i] || !keys.iter().all(|v| p.bound[v.0]) {
                continue;
            }
            if let Some(change) = self.overrides.nots.get(&(address, i)) {
                let old = self.plan_group(&change.old, &p.bound)?;
                let new = self.plan_group(&change.new, &p.bound)?;
                p.nots_done[i] = true;
                p.cost += p.rows * (old.cost + new.cost) + 2.0 * GROUP_COST;
                p.rows = p.rows.min(change.flips.len() as f64);
                p.steps.push(Step::NotChange {
                    old,
                    new,
                    keys,
                    flips: change.flips.clone(),
                    flip_vars: change.flip_vars.clone(),
                    to_unmatched: change.to_unmatched,
                    rows: p.rows,
                });
                continue;
            }
            let bind = self.plan_group(not, &p.bound).map(|sub| {
                let cost = p.rows * sub.cost + GROUP_COST;
                (Join::Bind, sub, cost)
            });
            let unbound = self.unbound();
            let hash = self.plan_group(not, &unbound).map(|sub| {
                let cost = sub.cost + sub.rows * BUILD_COST + p.rows * PROBE_COST;
                (Join::Hash, sub, cost)
            });
            let matches = match (&bind, &hash) {
                (Some(b), _) => b.1.rows,
                (None, Some(h)) => h.1.rows,
                (None, None) => return None,
            };
            let ways: Vec<_> = [bind, hash].into_iter().flatten().collect();
            let forced = ways.iter().any(|w| Some(w.0) == self.force);
            let (join, sub, cost) = ways
                .into_iter()
                .filter(|w| !forced || Some(w.0) == self.force)
                .min_by(|a, b| a.2.total_cmp(&b.2))?;
            p.nots_done[i] = true;
            p.cost += cost;
            // The more matches for each row, the fewer rows are left.
            p.rows /= 1.0 + matches;
            p.steps.push(Step::Not {
                group: sub,
                keys,
                join,
                rows: p.rows,
            });
        }
        Some(p)
    }

    /// The ways of joining `item` to `partial`.
    fn extend(
        &mut self,
        group: &Group,
        bindable: &[bool],
        partial: &Partial,
        item: Item,
        candidates: &Option<Arc<Vec<Option<IdSet>>>>,
    ) -> Vec<Partial> {
        let mut after = partial.bound.clone();
        let unbound = self.unbound();
        // The ways: a step, its cost, and the rows after it.
        let mut ways: Vec<(Step, f64, f64)> = Vec::new();
        match item {
            Item::Pattern(pattern, Some(r), required) => {
                pattern.vars().for_each(|v| after[v.0] = true);
                let n = r.links.len() as f64;
                // The links matching each row: the links over the distinct
                // values of the fields bound.
                let fields = [
                    Some(pattern.subject),
                    Some(pattern.predicate),
                    Some(pattern.object),
                    pattern.link.map(Term::Var),
                ];
                let mut matches = n;
                for (k, f) in fields.iter().enumerate() {
                    if let Some(Term::Var(v)) = f
                        && partial.bound[v.0]
                    {
                        matches /= r.distinct[k].max(1) as f64;
                    }
                }
                let rows = partial.rows * matches;
                let step = |join| Step::Pattern {
                    pattern: *pattern,
                    join,
                    access: Access::All,
                    keys: shared_vars(pattern, &partial.bound),
                    rows,
                    entries: match join {
                        Join::Bind => partial.rows * matches,
                        Join::Hash => n,
                    },
                    reduced: Some(r.links.clone()),
                    required,
                    candidates: candidates.clone(),
                };
                ways.push((
                    step(Join::Bind),
                    n * REDUCED_ENTRY_COST
                        + partial.rows * (REDUCED_LOOKUP_COST + matches * REDUCED_ENTRY_COST),
                    rows,
                ));
                if !partial.unit {
                    let cost = n * (REDUCED_ENTRY_COST + BUILD_COST) + partial.rows * PROBE_COST;
                    ways.push((step(Join::Hash), cost, rows));
                }
            }
            Item::Pattern(pattern, None, _) => {
                pattern.vars().for_each(|v| after[v.0] = true);
                let bind = self.est.estimate(pattern, &partial.bound);
                let rows = partial.rows * bind.matches;
                let positions = |bound: &[bool]| {
                    pattern.terms().map(|t| match t {
                        Term::Id(_) => true,
                        Term::Var(v) => bound[v.0],
                    })
                };
                let link = pattern.link.is_some_and(|v| partial.bound[v.0]);
                let keys = shared_vars(pattern, &partial.bound);
                ways.push((
                    Step::Pattern {
                        pattern: *pattern,
                        join: Join::Bind,
                        access: Access::of(link, positions(&partial.bound)),
                        keys: keys.clone(),
                        rows,
                        entries: partial.rows * bind.entries,
                        reduced: None,
                        required: false,
                        candidates: candidates.clone(),
                    },
                    partial.rows * (LOOKUP_COST + bind.entries * ENTRY_COST),
                    rows,
                ));
                // Through the Subgraph or Author index, for a constant one:
                // each link of its entry, and its fact, looked up.
                let restricted = [
                    (pattern.graph, Access::Graph, Linkage::Subgraph),
                    (pattern.author, Access::Author, Linkage::Author),
                ];
                for (term, access, linkage) in restricted {
                    let Some(Term::Id(id)) = term else { continue };
                    let entries = self.est.g.link_count(linkage, id) as f64;
                    let scan = LOOKUP_COST + entries * (ENTRY_COST + LOOKUP_COST);
                    let matches = entries.min(self.est.estimate(pattern, &unbound).matches);
                    let step = |join| Step::Pattern {
                        pattern: *pattern,
                        join,
                        access,
                        keys: keys.clone(),
                        rows,
                        entries: match join {
                            Join::Bind => partial.rows * entries,
                            Join::Hash => entries,
                        },
                        reduced: None,
                        required: false,
                        candidates: candidates.clone(),
                    };
                    ways.push((step(Join::Bind), partial.rows * scan, rows));
                    if !partial.unit {
                        let cost = scan + matches * BUILD_COST + partial.rows * PROBE_COST;
                        ways.push((step(Join::Hash), cost, rows));
                    }
                }
                if !partial.unit {
                    let scan = self.est.estimate(pattern, &unbound);
                    let cost = LOOKUP_COST
                        + scan.entries * ENTRY_COST
                        + scan.matches * BUILD_COST
                        + partial.rows * PROBE_COST;
                    ways.push((
                        Step::Pattern {
                            pattern: *pattern,
                            join: Join::Hash,
                            access: Access::of(false, positions(&unbound)),
                            keys,
                            rows,
                            entries: scan.entries,
                            reduced: None,
                            required: false,
                            candidates: candidates.clone(),
                        },
                        cost,
                        rows,
                    ));
                }
            }
            Item::Path(path) => {
                path.vars().for_each(|v| after[v.0] = true);
                let est = self.est.path_estimate(path, &partial.bound);
                let rows = partial.rows * est.matches;
                ways.push((
                    Step::Path {
                        path: path.clone(),
                        rows,
                    },
                    partial.rows * (LOOKUP_COST + est.entries * ENTRY_COST),
                    rows,
                ));
            }
            Item::Call(call, t, given) => {
                call.vars().for_each(|v| after[v.0] = true);
                let n = t.tuples.len() as f64;
                let mut matches = n;
                for (k, arg) in call.args.iter().enumerate() {
                    let known = match arg {
                        Term::Id(_) => true,
                        Term::Var(v) => partial.bound[v.0],
                    };
                    if known {
                        matches /= t.distinct[k].max(1) as f64;
                    }
                }
                let rows = partial.rows * matches;
                ways.push((
                    Step::Call {
                        call: call.clone(),
                        tuples: t.tuples.clone(),
                        given,
                        rows,
                    },
                    n * REDUCED_ENTRY_COST
                        + partial.rows * (REDUCED_LOOKUP_COST + matches * REDUCED_ENTRY_COST),
                    rows,
                ));
            }
            Item::Nearest(n) => {
                let ready = match n.query {
                    Term::Id(_) => true,
                    Term::Var(v) => partial.bound[v.0],
                };
                if ready && let Some(scan) = self.plan_group(&n.scan, &unbound) {
                    n.vars().for_each(|v| after[v.0] = true);
                    let found = scan.rows.max(1.0);
                    // A value bound already is kept if it is among them.
                    let matches = if partial.bound[n.value.0] {
                        (n.k as f64 / found).min(1.0)
                    } else {
                        (n.k as f64).min(found)
                    };
                    let rows = partial.rows * matches;
                    let indexed = self
                        .est
                        .g
                        .vector_search(n.predicate)
                        .filter(|s| s.index.metric == n.metric)
                        .map(|s| s.hnsw.live_len());
                    // An index's search measures about ef per layer.
                    let cost = match indexed {
                        Some(len) => {
                            let ef = self.est.g.vector_index(n.predicate).map_or(1, |i| i.ef);
                            partial.rows
                                * (ef.max(n.k) as f64)
                                * (len.max(2) as f64).log2()
                                * DISTANCE_COST
                        }
                        None => scan.cost + partial.rows * found * DISTANCE_COST,
                    };
                    ways.push((
                        Step::Nearest {
                            nearest: n.clone(),
                            scan: scan.clone(),
                            indexed,
                            rows,
                        },
                        cost,
                        rows,
                    ));
                }
            }
            Item::Union(branches) => {
                // The branches' variables: the same in each, but for hidden
                // link variables.
                let vars: Vec<Var> = branches
                    .iter()
                    .flat_map(Group::bound_vars)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                vars.iter().for_each(|v| after[v.0] = true);
                let keys: Vec<Var> = vars
                    .iter()
                    .copied()
                    .filter(|v| partial.bound[v.0])
                    .collect();
                let bind: Option<Vec<GroupPlan>> = branches
                    .iter()
                    .map(|b| self.plan_group(b, &partial.bound))
                    .collect();
                let bind_rows = bind
                    .as_ref()
                    .map(|plans| partial.rows * plans.iter().map(|p| p.rows).sum::<f64>());
                if let (Some(plans), Some(rows)) = (bind, bind_rows) {
                    let cost = partial.rows * plans.iter().map(|p| p.cost).sum::<f64>()
                        + plans.len() as f64 * GROUP_COST;
                    ways.push((
                        Step::Union {
                            branches: plans,
                            vars: vars.clone(),
                            keys: keys.clone(),
                            join: Join::Bind,
                            rows,
                        },
                        cost,
                        rows,
                    ));
                }
                let alone: Option<Vec<GroupPlan>> = branches
                    .iter()
                    .map(|b| self.plan_group(b, &unbound))
                    .collect();
                if let Some(plans) = alone.filter(|_| !partial.unit) {
                    let built: f64 = plans.iter().map(|p| p.rows).sum();
                    let rows = bind_rows.unwrap_or(partial.rows * built);
                    let cost = plans.iter().map(|p| p.cost).sum::<f64>()
                        + built * BUILD_COST
                        + partial.rows * PROBE_COST;
                    ways.push((
                        Step::Union {
                            branches: plans,
                            vars,
                            keys,
                            join: Join::Hash,
                            rows,
                        },
                        cost,
                        rows,
                    ));
                }
            }
        }
        let join = |step: &Step| match step {
            Step::Pattern { join, .. } | Step::Union { join, .. } => Some(*join),
            _ => None,
        };
        let force = self.force;
        let forced = ways.iter().any(|w| join(&w.0) == force);
        ways.into_iter()
            .filter(|w| !forced || join(&w.0) == force)
            .filter_map(|(step, cost, rows)| {
                let mut p = partial.clone();
                p.steps.push(step);
                p.cost += cost;
                p.rows = rows;
                p.bound.clone_from(&after);
                p.unit = false;
                self.apply_ready(group, bindable, p)
            })
            .collect()
    }
}

pub(crate) fn plan(q: &Query, g: &Graph) -> Result<Plan> {
    plan_with(q, g, None)
}

/// Plans `q`, using the join `force` wherever it can be.
pub(crate) fn plan_with(q: &Query, g: &Graph, force: Option<Join>) -> Result<Plan> {
    plan_full(q, g, force, &Overrides::default(), None)
}

/// Plans `q` with `overrides`, which are by the addresses of `q`'s groups,
/// and the tuples of the relations it calls, if given (else they are
/// evaluated).
pub(crate) fn plan_full(
    q: &Query,
    g: &Graph,
    force: Option<Join>,
    overrides: &Overrides,
    relations: Option<&Relations>,
) -> Result<Plan> {
    let aggregated = q.with_link_aggregates()?;
    let q = aggregated.as_ref().unwrap_or(q);
    let recording = q.with_link_vars();
    let q = recording.as_ref().unwrap_or(q);
    q.check()?;
    q.check_horizon(g)?;
    let relations_given = relations.is_some();
    let relations = Arc::new(match relations {
        Some(r) => r.clone(),
        None => rules::evaluate(q, g)?,
    });
    let width = q.names.len();
    let root = &q.root as *const Group as usize;
    let budget = q
        .reduction_budget
        .unwrap_or(reduce::DEFAULT_REDUCTION_BUDGET);
    let preset: HashMap<usize, Vec<Link>> = overrides
        .patterns
        .iter()
        .filter(|((group, _), _)| *group == root)
        .map(|(&(_, i), links)| (i, links.clone()))
        .collect();
    // Reducing narrows patterns by each other, so one pattern alone gains
    // nothing from it; and a query with a limit and no order can stop at
    // its first rows, which reducing it first, by reading its index entries,
    // would cost more than.
    let stops_early = q.limit.is_some() && q.order.is_empty() && !q.computes();
    // Nor can a query with parameters be reduced: their entries are unknown.
    // Nor one restricting its links' subgraphs or authors, which reduction
    // doesn't know.
    // Reducing keeps each fact's latest live link, and `history` needs all.
    let may_reduce = !q.uses_history()
        && budget > 0
        && q.root.patterns.len() >= 2
        && !stops_early
        && q.params.is_empty()
        && !q.restricts_links();
    let params = q.param_mask();
    let new_planner = |reduction: Option<Arc<Reduction>>| Planner {
        est: Estimator::new(g),
        overrides,
        relations: relations.clone(),
        params: params.clone(),
        force,
        root,
        candidates: reduction.as_ref().map(|r| Arc::new(r.vars.clone())),
        reduction,
        memo: HashMap::new(),
    };
    let unplannable =
        || Error::InvalidArgument("a filter or negation uses a variable not bound where it is");
    // Unless a budget is given (or links are, as a standing query gives
    // them), plan without reducing first, and reduce only if that could
    // cost less.
    let mut planned = None;
    let mut reduction_cost = None;
    if may_reduce && q.reduction_budget.is_none() && preset.is_empty() {
        let mut planner = new_planner(None);
        let plan = planner
            .plan_group(&q.root, &params)
            .ok_or_else(unplannable)?;
        match reduction_may_pay(&mut planner.est, &q.root, width, plan.cost, plan.rows) {
            Some(cost) => reduction_cost = Some(cost),
            None => planned = Some(plan),
        }
    }
    let (root, reduction) = match planned {
        Some(plan) => (plan, None),
        None => {
            let reduction = may_reduce.then(|| {
                Arc::new(reduce::reduce(
                    g, &q.root, width, q.as_of, q.since, budget, &preset,
                ))
            });
            let plan = new_planner(reduction.clone())
                .plan_group(&q.root, &params)
                .ok_or_else(unplannable)?;
            (plan, reduction)
        }
    };
    Ok(Plan {
        reduction_cost,
        profiler: None,
        query: q.clone(),
        root,
        reduction,
        relations,
        relations_given,
        next_id: g.next_id(),
        params: {
            let mut mask = q.param_mask();
            mask.resize(width.max(1), false);
            mask.into()
        },
    })
}

/// A query planned once to run many times, with different values of its
/// [parameters](Query::param): made by [`Query::prepare`].
///
/// Its plan is for any values, estimating each lookup by a parameter from
/// the averages of the graph's statistics, as a bind join's lookups are.  It
/// is planned again before running if the graph's links have grown or
/// shrunk by more than a factor of [`REPLAN_FACTOR`] since, so that a plan
/// made for a small graph doesn't stay.
#[derive(Debug, Clone)]
pub struct Prepared {
    plan: Plan,
    /// The graph's links when planned.
    links: u64,
}

/// How much a graph's links change before a [`Prepared`] query is planned
/// again.
pub const REPLAN_FACTOR: f64 = 2.0;

impl Prepared {
    pub(crate) fn new(q: &Query, g: &Graph) -> Result<Self> {
        Ok(Prepared {
            plan: plan(q, g)?,
            links: g.stats().links(),
        })
    }

    /// The plan, as made for the graph last run on.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn query(&self) -> &Query {
        &self.plan.query
    }

    /// Plans the query again if `g` has changed enough since it was planned,
    /// and checks that `g` can answer it.
    fn refresh(&mut self, g: &Graph) -> Result<()> {
        self.plan.query.check_horizon(g)?;
        let links = g.stats().links();
        let change = links.max(1) as f64 / self.links.max(1) as f64;
        if !(1.0 / REPLAN_FACTOR..=REPLAN_FACTOR).contains(&change) {
            self.plan = plan(&self.plan.query, g)?;
            self.links = links;
        }
        Ok(())
    }

    /// Runs the query on `g` with `values` as the values of its parameters,
    /// in the order they were declared, planning it again first if `g` has
    /// changed enough (see [`Prepared`]).
    pub fn execute<'g>(&mut self, g: &'g Graph, values: &[u64]) -> Result<Rows<'g>> {
        self.refresh(g)?;
        Ok(self.plan.execute_with(g, values))
    }

    /// The same, collecting the rows.
    pub fn run(&mut self, g: &Graph, values: &[u64]) -> Result<Vec<Vec<u64>>> {
        self.execute(g, values)?.collect()
    }

    /// The same, collecting the rows as cells, as a query that
    /// [computes](Query::computes) values must be run.
    pub fn cells(&mut self, g: &Graph, values: &[u64]) -> Result<Vec<Vec<Cell>>> {
        self.refresh(g)?;
        self.plan.execute_cells(g, values)
    }

    /// Runs the query, which must [record its links](Query::record_links),
    /// returning the graph of its rows' links (see [`Plan::execute_graph`]).
    pub fn graph(&mut self, g: &Graph, values: &[u64]) -> Result<ResultGraph> {
        self.refresh(g)?;
        self.plan.execute_graph(g, values)
    }

    /// The same, with values that need not be entries.
    pub fn graph_values(&mut self, g: &Graph, values: &[Cell]) -> Result<ResultGraph> {
        self.refresh(g)?;
        self.plan.execute_graph_values(g, values)
    }

    /// Runs the query with values of its parameters that need not be
    /// entries, as [`Plan::execute_values`] takes.
    pub fn execute_values<'g>(&mut self, g: &'g Graph, values: &[Cell]) -> Result<Rows<'g>> {
        self.refresh(g)?;
        Ok(self.plan.execute_values(g, values))
    }

    /// The same, collecting the rows.
    pub fn run_values(&mut self, g: &Graph, values: &[Cell]) -> Result<Vec<Vec<u64>>> {
        self.execute_values(g, values)?.collect()
    }

    /// The same, collecting the rows as cells.
    pub fn cells_values(&mut self, g: &Graph, values: &[Cell]) -> Result<Vec<Vec<Cell>>> {
        self.refresh(g)?;
        self.plan.execute_cells_values(g, values)
    }
}
