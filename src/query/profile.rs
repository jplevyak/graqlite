//! What running a plan did, step by step, beside what the planner expected:
//! see [`Plan::profile`](super::Plan::profile).
//!
//! The counters are in wrappers put around each step's operator and lookup
//! only when profiling, so a query run plainly doesn't pay for them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::plan::Step;

/// The counters of a step of a plan, over every time it was built.
#[derive(Debug, Default)]
pub(crate) struct StepStats {
    pub runs: AtomicU64,
    pub rows: AtomicU64,
    pub batches: AtomicU64,
    pub lookups: AtomicU64,
    pub entries: AtomicU64,
    pub nanos: AtomicU64,
}

impl StepStats {
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

/// The counters of a run of a plan, by the address of each step: the steps
/// are shared by the clones of a plan, and of the groups in it.
#[derive(Debug, Default)]
pub(crate) struct Profiler {
    steps: Mutex<HashMap<usize, Arc<StepStats>>>,
    /// The time spent so far in operators the current one called.
    pub nested: AtomicU64,
}

impl Profiler {
    /// The counters of `step`.
    pub fn step(&self, step: &Step) -> Arc<StepStats> {
        let mut steps = self.steps.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(steps.entry(step_key(step)).or_default())
    }

    /// The counters so far, by step.
    pub fn counts(&self) -> HashMap<usize, Counts> {
        let steps = self.steps.lock().unwrap_or_else(|e| e.into_inner());
        steps
            .iter()
            .map(|(&k, s)| {
                let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
                let counts = Counts {
                    runs: n(&s.runs),
                    rows: n(&s.rows),
                    batches: n(&s.batches),
                    lookups: n(&s.lookups),
                    entries: n(&s.entries),
                    time: Duration::from_nanos(n(&s.nanos)),
                };
                (k, counts)
            })
            .collect()
    }
}

pub(crate) fn step_key(step: &Step) -> usize {
    std::ptr::from_ref(step) as usize
}

/// What a step did, over every time it was run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// How many times it was started: more than once in a group run for
    /// each row, and never in a group no row reached.
    pub runs: u64,
    /// The rows it produced.
    pub rows: u64,
    /// The batches of rows it was asked for.
    pub batches: u64,
    /// The index lookups it made, or searches, or rule tuples looked up.
    pub lookups: u64,
    /// The index entries' ids it read, the planner's unit of cost.
    pub entries: u64,
    /// The time spent in it, not counting the steps it read from.
    pub time: Duration,
}

/// A step of a plan: what the planner expected, and what it did.
#[derive(Debug, Clone, Default)]
pub struct StepProfile {
    /// How deeply the step is nested in groups.
    pub depth: usize,
    /// What [`Plan::explain`](super::Plan::explain) says of it.
    pub step: String,
    /// The rows expected, if the step estimates them.
    pub estimated_rows: Option<f64>,
    /// The index entries expected to be read, if the step estimates them.
    pub estimated_entries: Option<f64>,
    pub actual: Counts,
}

/// A round of evaluating a recursive relation's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundProfile {
    /// The tuples the round found that were not known.
    pub new_tuples: u64,
    pub time: Duration,
}

/// The relations evaluated together, and the rounds that took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StratumProfile {
    pub relations: Vec<String>,
    pub rounds: Vec<RoundProfile>,
}

/// What reducing the query's top group, as it was planned, did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReductionProfile {
    /// The index entries read.
    pub entries: u64,
    /// Its estimated cost, if the planner decided by one.
    pub estimated_cost: Option<f64>,
    pub time: Duration,
}

/// What running a plan did; see [`Plan::profile`](super::Plan::profile).
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// The steps, in the order [`Plan::explain`](super::Plan::explain) prints
    /// them.
    pub steps: Vec<StepProfile>,
    /// The rows of the result, or links of a result graph.
    pub rows: u64,
    /// The time to run the plan, once planned.
    pub time: Duration,
    /// The rules' evaluation, when the query was planned.
    pub strata: Vec<StratumProfile>,
    pub reduction: Option<ReductionProfile>,
    pub(crate) counts: HashMap<usize, Counts>,
}
