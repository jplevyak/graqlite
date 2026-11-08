//! Running a [`Plan`]: pull-based operators passing batches of rows.
//!
//! A row has a slot for each variable of the query, of which those bound so
//! far are meaningful.  Each operator fills a batch with up to
//! [`BATCH_ROWS`] rows on request, and an empty batch means it is done.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};

use std::sync::Arc;

use super::aggregate::{self, Cell};
use super::compute;
use super::overlay::{self, Shared};
use super::plan::{Access, GroupPlan, Join, Plan, Step};
use super::provenance::{Recorder, RowProvenance, Support, witness};
use super::reduce::{Link, live_links};
use super::result::{Derivation, ResultGraph, ResultLink, ResultNode};
use super::rules::{self, Relations, Tuples};
use super::{
    Assertion, Filter, LinkAggregate, Nearest, Overlay, PathExpr, PathPattern, Pattern, Relation,
    SortKey, TemplateNode, Term, Var, paths,
};
use crate::graph::{EPHEMERAL, Error, Graph, Result};
use crate::local_table::{IdSet, Ids};
use crate::record::Linkage;
use crate::tuple_index::{self, Intersection, LinkKey};

/// The most rows in a batch.
const BATCH_ROWS: usize = 1024;

/// Rows of `width` ids each, at most `rows` of them when filled.
#[derive(Debug, Clone)]
struct Batch {
    width: usize,
    rows: usize,
    data: Vec<u64>,
}

impl Batch {
    fn new(width: usize) -> Self {
        Batch::with_rows(width, BATCH_ROWS)
    }

    /// A batch filled to at most `rows` rows, at least one.
    fn with_rows(width: usize, rows: usize) -> Self {
        Batch {
            width,
            rows: rows.clamp(1, BATCH_ROWS),
            data: Vec::new(),
        }
    }

    /// The rows that fit before it is full.
    fn room(&self) -> usize {
        self.rows.saturating_sub(self.len())
    }

    fn len(&self) -> usize {
        self.data.len() / self.width
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn is_full(&self) -> bool {
        self.len() >= self.rows
    }

    fn row(&self, i: usize) -> &[u64] {
        &self.data[i * self.width..(i + 1) * self.width]
    }

    fn push(&mut self, row: &[u64]) {
        self.data.extend_from_slice(row);
    }

    fn clear(&mut self) {
        self.data.clear();
    }
}

trait Operator {
    /// Fills `out`, which is empty, with the next rows, leaving it empty if
    /// there are no more.
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()>;
}

/// Runs `op` to the end, appending its rows to `out`.
fn drain(op: &mut dyn Operator, g: &Graph, out: &mut Batch) -> Result<()> {
    let mut batch = Batch::new(out.width);
    loop {
        batch.clear();
        op.next(g, &mut batch)?;
        if batch.is_empty() {
            return Ok(());
        }
        out.data.extend_from_slice(&batch.data);
    }
}

/// Given rows, once.
struct Seed {
    rows: Option<Batch>,
}

impl Operator for Seed {
    fn next(&mut self, _: &Graph, out: &mut Batch) -> Result<()> {
        if let Some(rows) = self.rows.take() {
            out.data = rows.data;
        }
        Ok(())
    }
}

/// A single row: `start`, binding the parameters only.
fn unit(width: usize, start: &[u64]) -> Box<dyn Operator> {
    let mut row = Batch::new(width);
    row.data.extend_from_slice(start);
    Box::new(Seed { rows: Some(row) })
}

/// The rows of each operator in turn.
struct Concat {
    ops: Vec<Box<dyn Operator>>,
}

impl Operator for Concat {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        while let Some(op) = self.ops.first_mut() {
            op.next(g, out)?;
            if !out.is_empty() {
                return Ok(());
            }
            self.ops.remove(0);
        }
        Ok(())
    }
}

/// Finds the matches of something, given a row.
trait Lookup {
    /// Appends to `out` a row extending `row` for each match.
    fn matches(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<()>;

    /// Starts appending the matches of `row` to `out`, returning whether it
    /// appended them all.  If not, [`resume`](Self::resume) appends more, a
    /// batch at a time, so that a consumer that stops early stops the work.
    fn begin(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<bool> {
        self.matches(g, row, out)?;
        Ok(true)
    }

    /// Appends more matches of the row begun, as many as fit in `out`,
    /// returning whether they are all appended.
    fn resume(&mut self, _g: &Graph, _row: &[u64], _out: &mut Batch) -> Result<bool> {
        Ok(true)
    }
}

/// A lookup whose rows are marked: their slot `mark` set to 1, so that a
/// query recording its links knows which paths and calls each row matched.
struct Marked<L> {
    inner: L,
    mark: usize,
}

impl<L: Lookup> Marked<L> {
    fn set(&self, out: &mut Batch, from: usize) {
        let width = out.width;
        for i in from..out.len() {
            out.data[i * width + self.mark] = 1;
        }
    }
}

impl<L: Lookup> Lookup for Marked<L> {
    fn matches(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<()> {
        let from = out.len();
        self.inner.matches(g, row, out)?;
        self.set(out, from);
        Ok(())
    }

    fn begin(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<bool> {
        let from = out.len();
        let done = self.inner.begin(g, row, out)?;
        self.set(out, from);
        Ok(done)
    }

    fn resume(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<bool> {
        let from = out.len();
        let done = self.inner.resume(g, row, out)?;
        self.set(out, from);
        Ok(done)
    }
}

/// `lookup`, marking its rows with `mark` if given.
fn marked<L: Lookup + 'static>(lookup: L, mark: Option<Var>) -> Box<dyn Lookup> {
    match mark {
        Some(v) => Box::new(Marked {
            inner: lookup,
            mark: v.0,
        }),
        None => Box::new(lookup),
    }
}

/// Extends `row`, binding `bound`, with `values` for `terms`, into `new`:
/// constants and variables bound or repeated must match.
fn extend_row(
    bound: &[bool],
    row: &[u64],
    fields: impl IntoIterator<Item = (Term, u64)>,
    new: &mut Vec<u64>,
) -> bool {
    new.clear();
    new.extend_from_slice(row);
    let mut set: Vec<Var> = Vec::new();
    for (term, value) in fields {
        match term {
            Term::Id(c) => {
                if c != value {
                    return false;
                }
            }
            Term::Var(v) if bound[v.0] || set.contains(&v) => {
                if new[v.0] != value {
                    return false;
                }
            }
            Term::Var(v) => {
                new[v.0] = value;
                set.push(v);
            }
        }
    }
    true
}

/// Finds the pairs a path pattern matches, by searching from whichever end
/// is bound.
struct PathLookup {
    path: PathPattern,
    bound: Vec<bool>,
    as_of: Option<u64>,
    overlay: Option<Shared>,
    /// The other ends of the live links from (or, if not forward, to) a node
    /// with a predicate: by predicate, node and direction.
    neighbors: HashMap<(u64, u64, bool), Arc<Vec<u64>>>,
}

impl PathLookup {
    fn value(&self, t: Term, row: &[u64]) -> Option<u64> {
        match t {
            Term::Id(id) => Some(id),
            Term::Var(v) if self.bound[v.0] => Some(row[v.0]),
            Term::Var(_) => None,
        }
    }

    fn neighbors(&mut self, g: &Graph, p: u64, x: u64, forward: bool) -> Arc<Vec<u64>> {
        let as_of = self.as_of;
        let overlay = self.overlay.as_deref();
        self.neighbors
            .entry((p, x, forward))
            .or_insert_with(|| {
                let mut ends: Vec<u64> = overlay::step_links(g, overlay, p, x, forward, as_of)
                    .into_iter()
                    .map(|(_, [s, _, o])| if forward { o } else { s })
                    .collect();
                ends.sort_unstable();
                ends.dedup();
                Arc::new(ends)
            })
            .clone()
    }

    /// The nodes reachable from `start` by one or more links with `p`.
    fn reach(&mut self, g: &Graph, p: u64, start: u64, forward: bool) -> Vec<u64> {
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        let mut out = Vec::new();
        while let Some(x) = stack.pop() {
            for &y in self.neighbors(g, p, x, forward).iter() {
                if seen.insert(y) {
                    out.push(y);
                    stack.push(y);
                }
            }
        }
        out.sort_unstable();
        out
    }
}

impl PathLookup {
    /// Appends a row for each pair the path expression `expr` matches.
    fn expr_matches(
        &mut self,
        g: &Graph,
        expr: &PathExpr,
        row: &[u64],
        out: &mut Batch,
    ) -> Result<()> {
        let (subject, object) = (self.path.subject, self.path.object);
        let (s, o) = (self.value(subject, row), self.value(object, row));
        let bound = self.bound.clone();
        let mut new = Vec::new();
        let mut emit = |s: u64, o: u64, out: &mut Batch| {
            if extend_row(&bound, row, [(subject, s), (object, o)], &mut new) {
                out.push(&new);
            }
        };
        let reach = |this: &mut Self, from: u64, forward: bool| -> Vec<u64> {
            let automaton = paths::Automaton::new(expr, forward);
            automaton.reach(from, |step, x| {
                this.neighbors(g, step.predicate, x, step.forward).to_vec()
            })
        };
        match (s, o) {
            (Some(s), Some(o)) => {
                if reach(self, s, true).binary_search(&o).is_ok() {
                    emit(s, o, out);
                }
            }
            (Some(s), None) => {
                for o in reach(self, s, true) {
                    emit(s, o, out);
                }
            }
            (None, Some(o)) => {
                for s in reach(self, o, false) {
                    emit(s, o, out);
                }
            }
            (None, None) => {
                if expr.nullable() {
                    return Err(crate::graph::Error::InvalidArgument(
                        "a path expression that may be empty needs one of its ends known",
                    ));
                }
                // Every entry a path can start from: the subjects and
                // objects of links with its predicates.
                let mut predicates = Vec::new();
                expr.predicates(&mut predicates);
                predicates.sort_unstable();
                predicates.dedup();
                let mut starts = Vec::new();
                for p in predicates {
                    let links = overlay::predicate_links(g, self.overlay.as_deref(), p, self.as_of);
                    for (_, [ls, _, lo]) in links {
                        starts.extend([ls, lo]);
                    }
                }
                starts.sort_unstable();
                starts.dedup();
                for s in starts {
                    for o in reach(self, s, true) {
                        emit(s, o, out);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Lookup for PathLookup {
    fn matches(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<()> {
        if let Some(expr) = self.path.expr.clone() {
            return self.expr_matches(g, &expr, row, out);
        }
        let path = self.path.clone();
        let [s, pr, o] = path.terms().map(|t| self.value(t, row));
        let predicates: Vec<u64> = match pr {
            Some(p) => vec![p],
            None => overlay::predicates(g, self.overlay.as_deref())
                .into_iter()
                .collect(),
        };
        let mut new = Vec::new();
        let mut emit = |bound: &[bool], s: u64, p: u64, o: u64, out: &mut Batch| {
            let fields = [(path.subject, s), (path.predicate, p), (path.object, o)];
            if extend_row(bound, row, fields, &mut new) {
                out.push(&new);
            }
        };
        let bound = self.bound.clone();
        for p in predicates {
            match (s, o) {
                (Some(s), Some(o)) => {
                    if self.reach(g, p, s, true).binary_search(&o).is_ok() {
                        emit(&bound, s, p, o, out);
                    }
                }
                (Some(s), None) => {
                    for o in self.reach(g, p, s, true) {
                        emit(&bound, s, p, o, out);
                    }
                }
                (None, Some(o)) => {
                    for s in self.reach(g, p, o, false) {
                        emit(&bound, s, p, o, out);
                    }
                }
                (None, None) => {
                    let mut subjects: Vec<u64> =
                        overlay::predicate_links(g, self.overlay.as_deref(), p, self.as_of)
                            .into_iter()
                            .map(|(_, [s, _, _])| s)
                            .collect();
                    subjects.sort_unstable();
                    subjects.dedup();
                    for s in subjects {
                        for o in self.reach(g, p, s, true) {
                            emit(&bound, s, p, o, out);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Finds the tuples of a relation matching a call, given a row.
struct CallLookup {
    args: Vec<Term>,
    tuples: Tuples,
    bound: Vec<bool>,
    /// The tuples by their values of the arguments known, once built.
    index: Option<HashMap<Vec<u64>, Vec<usize>>>,
}

impl Lookup for CallLookup {
    fn matches(&mut self, _: &Graph, row: &[u64], out: &mut Batch) -> Result<()> {
        let known: Vec<(usize, Option<u64>)> = self
            .args
            .iter()
            .enumerate()
            .filter_map(|(k, t)| match *t {
                Term::Id(id) => Some((k, Some(id))),
                Term::Var(v) if self.bound[v.0] => Some((k, None)),
                Term::Var(_) => None,
            })
            .collect();
        let tuples = &self.tuples;
        let index = self.index.get_or_insert_with(|| {
            let mut index: HashMap<Vec<u64>, Vec<usize>> = HashMap::new();
            for (i, t) in tuples.iter().enumerate() {
                index
                    .entry(known.iter().map(|&(k, _)| t[k]).collect())
                    .or_default()
                    .push(i);
            }
            index
        });
        let key: Vec<u64> = known
            .iter()
            .map(|&(k, c)| {
                c.unwrap_or_else(|| match self.args[k] {
                    Term::Var(v) => row[v.0],
                    Term::Id(id) => id,
                })
            })
            .collect();
        let mut new = Vec::new();
        for &i in index.get(&key).into_iter().flatten() {
            let fields = self
                .args
                .iter()
                .copied()
                .zip(self.tuples[i].iter().copied());
            if extend_row(&self.bound, row, fields, &mut new) {
                out.push(&new);
            }
        }
        Ok(())
    }
}

/// Finds the values nearest a vector, given a row: the `k` nearest of the
/// vectors its scan finds, which it finds once.
struct NearestLookup {
    nearest: Nearest,
    scan: GroupPlan,
    ctx: Context,
    bound: Vec<bool>,
    /// The values the scan found that are vectors, by id, once found.
    vectors: Option<Vec<(u64, Vec<f32>)>>,
}

impl Lookup for NearestLookup {
    fn matches(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<()> {
        let n = &self.nearest;
        let query = match n.query {
            Term::Id(id) => id,
            Term::Var(v) => row[v.0],
        };
        let Some(query) = Cell::of_id(query).vector(g) else {
            return Ok(());
        };
        if self.vectors.is_none() {
            let unbound = self.ctx.unbound();
            let values = matched(&self.ctx, g, &self.scan, &[n.value], &unbound, None)?;
            let mut vectors: Vec<(u64, Vec<f32>)> = values
                .into_iter()
                .filter_map(|v| Some((v[0], crate::vector::decode(&g.data(v[0])?)?)))
                .collect();
            vectors.sort_unstable_by_key(|v| v.0);
            self.vectors = Some(vectors);
        }
        let vectors = self.vectors.as_ref().expect("found");
        let mut found: Vec<(f64, u64)> = vectors
            .iter()
            .filter_map(|(id, v)| Some((crate::vector::distance(&query, v, n.metric)?, *id)))
            .collect();
        // Nearest first; of those as near, the earlier.
        let order = |a: &(f64, u64), b: &(f64, u64)| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1));
        if found.len() > n.k {
            found.select_nth_unstable_by(n.k, order);
            found.truncate(n.k);
        }
        found.sort_unstable_by(order);
        let mut new = Vec::new();
        for (d, id) in found {
            let d = Cell::Float(d).data(g).expect("a number");
            let d = match &self.ctx.hold {
                Some(h) => h.value(g, &d),
                None => g.ephemeral_value(&d),
            };
            let fields = [(Term::Var(n.value), id), (Term::Var(n.distance), d)];
            if extend_row(&self.bound, row, fields, &mut new) {
                out.push(&new);
            }
        }
        Ok(())
    }
}

/// Finds the live links matching a pattern, given a row.
struct Matcher {
    pattern: Pattern,
    access: Access,
    /// Whether each variable is bound in the rows given.
    bound: Vec<bool>,
    as_of: Option<u64>,
    /// The overlay read over the database, if any.
    overlay: Option<Shared>,
    /// The pattern's reduced links, used in place of the indexes, and an
    /// index of them by the values of the fields bound, once built.
    reduced: Option<Arc<Vec<Link>>>,
    index: Option<HashMap<Vec<u64>, Vec<Link>>>,
    /// The candidates of the variables, which the values bound must be in.
    candidates: Option<Arc<Vec<Option<IdSet>>>>,
    /// A large index entry being streamed for a row (see [`Lookup::begin`]).
    stream: Option<LinkStream>,
    /// Buffers reused from row to row: the row being built, and a small
    /// entry's live links and the subjects, predicates and objects seen.
    new: Vec<u64>,
    live: Vec<Link>,
    seen: Vec<[u64; 3]>,
}

/// The links of a large index entry, newest first, and the subjects,
/// predicates and objects seen: the first link seen of each is the latest,
/// so liveness is known as the entry is read.
struct LinkStream {
    ids: Ids,
    seen: HashSet<[u64; 3]>,
}

impl Matcher {
    /// The index entry a lookup for `row` reads, if it is one entry with every
    /// link of each subject, predicate and object in it.
    fn link_key(&self, row: &[u64]) -> Option<LinkKey> {
        let [s, pr, o] = self.pattern.terms().map(|t| self.value(t, row));
        let key = match self.access {
            Access::Spo => LinkKey::Spo(s?, pr?, o?),
            Access::SubjectPredicate => LinkKey::Pair(Intersection::SubjectPredicate, s?, pr?),
            Access::PredicateObject => LinkKey::Pair(Intersection::PredicateObject, pr?, o?),
            Access::Subject => LinkKey::Linkage(Linkage::Subject, s?),
            Access::Predicate => LinkKey::Linkage(Linkage::Predicate, pr?),
            Access::Object => LinkKey::Linkage(Linkage::Object, o?),
            Access::Link | Access::All | Access::Graph | Access::Author => return None,
        };
        key.has_whole_spos().then_some(key)
    }

    /// Appends the row extending `row` by the link `id` with `spo`, if it
    /// matches the pattern and the candidates.
    fn push_match(
        &self,
        g: &Graph,
        row: &[u64],
        id: u64,
        [s, pr, o]: [u64; 3],
        new: &mut Vec<u64>,
        out: &mut Batch,
    ) {
        let p = &self.pattern;
        new.clear();
        new.extend_from_slice(row);
        // The variables set from this link, which a repeat must match.
        let mut set = [Var(usize::MAX); 4];
        let mut n = 0;
        let fields = [
            (Some(p.subject), s),
            (Some(p.predicate), pr),
            (Some(p.object), o),
            (p.link.map(Term::Var), id),
        ];
        for (term, value) in fields {
            match term {
                None => {}
                Some(Term::Id(c)) => {
                    if c != value {
                        return;
                    }
                }
                Some(Term::Var(v)) if self.bound[v.0] || set[..n].contains(&v) => {
                    if new[v.0] != value {
                        return;
                    }
                }
                Some(Term::Var(v)) => {
                    let allowed = self.candidates.as_ref().and_then(|c| c[v.0].as_ref());
                    if allowed.is_some_and(|c| !c.contains(&value)) {
                        return;
                    }
                    new[v.0] = value;
                    set[n] = v;
                    n += 1;
                }
            }
        }
        if !p.restricts() {
            out.push(new);
            return;
        }
        // The link's author and subgraphs, from its record, or the
        // overlay's.
        let found = if id & EPHEMERAL != 0 {
            self.overlay
                .as_ref()
                .and_then(|o| o.link(id))
                .map(|l| (l.author, l.subgraphs.clone()))
        } else {
            g.get(id)
                .ok()
                .and_then(|r| r.as_link().map(|l| (l.author(), l.subgraphs())))
        };
        let Some((author, subgraphs)) = found else {
            return;
        };
        let known = |v: Var, set: &[Var]| self.bound[v.0] || set.contains(&v);
        let allowed = |v: Var, value: u64| {
            self.candidates
                .as_ref()
                .and_then(|c| c[v.0].as_ref())
                .is_none_or(|c| c.contains(&value))
        };
        if let Some(t) = p.author {
            if author == 0 {
                return;
            }
            match t {
                Term::Id(c) if c != author => return,
                Term::Id(_) => {}
                Term::Var(v) if known(v, &set[..n]) => {
                    if new[v.0] != author {
                        return;
                    }
                }
                Term::Var(v) => {
                    if !allowed(v, author) {
                        return;
                    }
                    new[v.0] = author;
                    set[n] = v;
                    n += 1;
                }
            }
        }
        match p.graph {
            None => out.push(new),
            Some(Term::Id(c)) => {
                if subgraphs.contains(&c) {
                    out.push(new);
                }
            }
            Some(Term::Var(v)) if known(v, &set[..n]) => {
                if subgraphs.contains(&new[v.0]) {
                    out.push(new);
                }
            }
            // Each subgraph the link is in.
            Some(Term::Var(v)) => {
                for sg in subgraphs {
                    if allowed(v, sg) {
                        new[v.0] = sg;
                        out.push(new);
                    }
                }
            }
        }
    }
    fn value(&self, t: Term, row: &[u64]) -> Option<u64> {
        match t {
            Term::Id(id) => Some(id),
            Term::Var(v) if self.bound[v.0] => Some(row[v.0]),
            Term::Var(_) => None,
        }
    }

    /// The candidate links for `row`: a superset of the matches, possibly
    /// including dead links and ids that are not links.
    fn candidates(&self, g: &Graph, row: &[u64]) -> IdSet {
        let local = g.local();
        let p = &self.pattern;
        let [s, pr, o] = p.terms().map(|t| self.value(t, row));
        let v = |x: Option<u64>| x.expect("bound by the access");
        match self.access {
            Access::Link => {
                let link = row[p.link.expect("a link variable").0];
                match g
                    .get(link)
                    .ok()
                    .and_then(|r| r.as_link().map(|l| l.fields()))
                {
                    Some(f) => {
                        tuple_index::find_links_by_spo(local, f.subject, f.predicate, f.object)
                    }
                    None => IdSet::new(),
                }
            }
            Access::Spo => tuple_index::find_links_by_spo(local, v(s), v(pr), v(o)),
            Access::SubjectPredicate => tuple_index::find_links_by_intersection(
                local,
                Intersection::SubjectPredicate,
                v(s),
                v(pr),
            ),
            Access::PredicateObject => tuple_index::find_links_by_intersection(
                local,
                Intersection::PredicateObject,
                v(pr),
                v(o),
            ),
            Access::Subject => tuple_index::find_links(local, Linkage::Subject, v(s)),
            Access::Predicate => tuple_index::find_links(local, Linkage::Predicate, v(pr)),
            Access::Object => tuple_index::find_links(local, Linkage::Object, v(o)),
            Access::All => {
                let predicates: BTreeSet<u64> = g.stats().predicates().map(|(p, _)| p).collect();
                let mut links = IdSet::new();
                for p in predicates {
                    links.extend(tuple_index::find_links(local, Linkage::Predicate, p));
                }
                links
            }
            Access::Graph => match p.graph.and_then(|t| self.value(t, row)) {
                Some(sg) => tuple_index::find_links(local, Linkage::Subgraph, sg),
                None => IdSet::new(),
            },
            Access::Author => match p.author.and_then(|t| self.value(t, row)) {
                Some(a) => tuple_index::find_links(local, Linkage::Author, a),
                None => IdSet::new(),
            },
        }
    }

    /// The pattern's fields bound in the rows given: subject, predicate,
    /// object, link.
    fn bound_fields(&self) -> Vec<usize> {
        let p = &self.pattern;
        [
            Some(p.subject),
            Some(p.predicate),
            Some(p.object),
            p.link.map(Term::Var),
        ]
        .iter()
        .enumerate()
        .filter(|(_, f)| matches!(f, Some(Term::Var(v)) if self.bound[v.0]))
        .map(|(k, _)| k)
        .collect()
    }

    /// The reduced links matching the fields of `row` bound.
    fn reduced_links(&mut self, row: &[u64]) -> Vec<Link> {
        let fields = self.bound_fields();
        let p = self.pattern;
        let value = |k: usize, &(id, spo): &Link| if k < 3 { spo[k] } else { id };
        let key_of_row: Vec<u64> = fields
            .iter()
            .map(|&k| {
                match [
                    Some(p.subject),
                    Some(p.predicate),
                    Some(p.object),
                    p.link.map(Term::Var),
                ][k]
                {
                    Some(Term::Var(v)) => row[v.0],
                    _ => unreachable!("a bound variable"),
                }
            })
            .collect();
        let reduced = self.reduced.as_ref().expect("reduced links");
        let index = self.index.get_or_insert_with(|| {
            let mut index: HashMap<Vec<u64>, Vec<Link>> = HashMap::new();
            for link in reduced.iter() {
                let key = fields.iter().map(|&k| value(k, link)).collect();
                index.entry(key).or_default().push(*link);
            }
            index
        });
        index.get(&key_of_row).cloned().unwrap_or_default()
    }
}

impl Lookup for Matcher {
    /// Appends to `out` a row extending `row` for each live link matching.
    fn matches(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<()> {
        let live = if self.overlay.as_ref().is_some_and(|o| !o.over_database()) {
            Vec::new()
        } else if self.reduced.is_some() {
            self.reduced_links(row)
        } else if matches!(self.access, Access::Graph | Access::Author) {
            // The entry isn't every link of its facts: each link must be
            // its fact's latest.
            let as_of = self.as_of.unwrap_or(u64::MAX);
            self.candidates(g, row)
                .range(..=as_of)
                .filter_map(|&id| {
                    let f = g.get(id).ok()?.as_link()?.fields();
                    let links = g.find_links(f.subject, f.predicate, f.object);
                    let latest = *links.range(..=as_of).next_back()?;
                    (latest == id && f.state == crate::record::LinkState::Live)
                        .then_some((id, [f.subject, f.predicate, f.object]))
                })
                .collect()
        } else {
            live_links(g, &self.candidates(g, row), self.as_of)
        };
        let live = match &self.overlay {
            None => live,
            Some(ov) => {
                let p = &self.pattern;
                let mut known = p.terms().map(|t| self.value(t, row));
                // A link bound: its fact.
                if let Some(l) = p.link.and_then(|v| self.value(Term::Var(v), row))
                    && let Some(link) = ov.link(l)
                {
                    known = link.spo.map(Some);
                }
                let sg = p.graph.and_then(|t| self.value(t, row));
                let author = p.author.and_then(|t| self.value(t, row));
                overlay::layered(Some(ov), live, known, sg, author)
            }
        };
        let mut new = std::mem::take(&mut self.new);
        for (id, spo) in live {
            self.push_match(g, row, id, spo, &mut new, out);
        }
        self.new = new;
        Ok(())
    }

    /// Streams a large index entry, newest first from the `as of` cutoff;
    /// matches others at once, as [`matches`](Lookup::matches) does.
    fn begin(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<bool> {
        let key = if self.reduced.is_none() && self.overlay.is_none() {
            self.link_key(row)
        } else {
            None
        };
        let Some(key) = key else {
            self.matches(g, row, out)?;
            return Ok(true);
        };
        let ids = tuple_index::links_newest_first(g.local(), key, self.as_of);
        if let Ids::Tree(_) = ids {
            self.stream = Some(LinkStream {
                ids,
                seen: HashSet::new(),
            });
            return self.resume(g, row, out);
        }
        // A small entry: its live links at once, oldest first.  Read newest
        // first, the first link seen of a subject, predicate and object is its
        // latest.
        let (mut new, mut live, mut seen) = (
            std::mem::take(&mut self.new),
            std::mem::take(&mut self.live),
            std::mem::take(&mut self.seen),
        );
        live.clear();
        seen.clear();
        for id in ids {
            let Ok(record) = g.get(id) else { continue };
            let Some(link) = record.as_link() else {
                continue;
            };
            let spo = [link.subject(), link.predicate(), link.object()];
            if seen.contains(&spo) {
                continue;
            }
            seen.push(spo);
            if !link.dead() {
                live.push((id, spo));
            }
        }
        for &(id, spo) in live.iter().rev() {
            self.push_match(g, row, id, spo, &mut new, out);
        }
        (self.new, self.live, self.seen) = (new, live, seen);
        Ok(true)
    }

    fn resume(&mut self, g: &Graph, row: &[u64], out: &mut Batch) -> Result<bool> {
        let Some(mut stream) = self.stream.take() else {
            return Ok(true);
        };
        let mut new = Vec::with_capacity(row.len());
        while !out.is_full() {
            let Some(id) = stream.ids.next() else {
                return Ok(true);
            };
            let Ok(record) = g.get(id) else { continue };
            let Some(link) = record.as_link() else {
                continue;
            };
            let spo = [link.subject(), link.predicate(), link.object()];
            // The first seen of a subject, predicate and object is its latest.
            if !stream.seen.insert(spo) || link.dead() {
                continue;
            }
            self.push_match(g, row, id, spo, &mut new, out);
        }
        self.stream = Some(stream);
        Ok(false)
    }
}

/// The rows of the input whose values are within nested limits (see
/// [`Query::limit_values`](super::Query::limit_values)).
struct ValueLimits {
    input: Option<Box<dyn Operator>>,
    limits: Vec<(Var, usize, usize)>,
    rows: Batch,
    pos: usize,
}

impl ValueLimits {
    /// The rows kept: for each limit in turn, those whose value is among
    /// the first of its group's values.
    fn keep(&self, g: &Graph) -> Vec<bool> {
        let n = self.rows.len();
        let mut keep = vec![true; n];
        for (k, &(v, limit, offset)) in self.limits.iter().enumerate() {
            let before: Vec<Var> = self.limits[..k].iter().map(|l| l.0).collect();
            let key = |row: &[u64]| -> Vec<u64> { before.iter().map(|b| row[b.0]).collect() };
            let mut groups: HashMap<Vec<u64>, BTreeSet<u64>> = HashMap::new();
            for i in (0..n).filter(|&i| keep[i]) {
                let row = self.rows.row(i);
                if row[v.0] != 0 {
                    groups.entry(key(row)).or_default().insert(row[v.0]);
                }
            }
            let allowed: HashMap<Vec<u64>, HashSet<u64>> = groups
                .into_iter()
                .map(|(key, values)| {
                    let mut values: Vec<u64> = values.into_iter().collect();
                    values.sort_by(|&a, &b| {
                        aggregate::compare_cells(g, &Cell::Id(a), &Cell::Id(b)).then(a.cmp(&b))
                    });
                    let kept = values.into_iter().skip(offset).take(limit).collect();
                    (key, kept)
                })
                .collect();
            for (i, kept) in keep.iter_mut().enumerate() {
                let row = self.rows.row(i);
                if *kept && row[v.0] != 0 {
                    *kept = allowed
                        .get(&key(row))
                        .is_some_and(|a| a.contains(&row[v.0]));
                }
            }
        }
        keep
    }
}

impl Operator for ValueLimits {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        if let Some(mut input) = self.input.take() {
            drain(input.as_mut(), g, &mut self.rows)?;
            let keep = self.keep(g);
            let mut kept = Batch::new(self.rows.width);
            for (i, k) in keep.into_iter().enumerate() {
                if k {
                    kept.push(self.rows.row(i));
                }
            }
            self.rows = kept;
        }
        while self.pos < self.rows.len() && !out.is_full() {
            out.push(self.rows.row(self.pos));
            self.pos += 1;
        }
        Ok(())
    }
}

/// Rows extended by each matching link of a pattern, looked up per row.
struct BindJoin {
    input: Box<dyn Operator>,
    matcher: Box<dyn Lookup>,
    in_batch: Batch,
    in_pos: usize,
    pending: Batch,
    pending_pos: usize,
    /// Whether the matcher has more matches of the row before `in_pos`.
    streaming: bool,
}

impl Operator for BindJoin {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        loop {
            while self.pending_pos < self.pending.len() {
                if out.is_full() {
                    return Ok(());
                }
                out.push(self.pending.row(self.pending_pos));
                self.pending_pos += 1;
            }
            if out.is_full() {
                return Ok(());
            }
            self.pending.clear();
            self.pending_pos = 0;
            // A streaming lookup reads no more than the output has room for.
            self.pending.rows = out.room().max(1);
            if self.streaming {
                let row = self.in_batch.row(self.in_pos - 1);
                self.streaming = !self.matcher.resume(g, row, &mut self.pending)?;
                continue;
            }
            if self.in_pos >= self.in_batch.len() {
                self.in_batch.clear();
                self.in_pos = 0;
                self.input.next(g, &mut self.in_batch)?;
                if self.in_batch.is_empty() {
                    return Ok(());
                }
            }
            let row = self.in_batch.row(self.in_pos);
            self.streaming = !self.matcher.begin(g, row, &mut self.pending)?;
            self.in_pos += 1;
        }
    }
}

/// Rows joined on `keys` to the rows of another operator, read once into a
/// hash table.
struct HashJoin {
    probe: Box<dyn Operator>,
    /// The other rows, until the table is built.
    build: Option<Box<dyn Operator>>,
    keys: Vec<Var>,
    /// The variables the other rows bind that the probe rows do not.
    new_vars: Vec<Var>,
    /// The values of `new_vars` of each other row, by its key values; with
    /// no new variables, a 0 for each row.
    table: HashMap<Vec<u64>, Vec<u64>>,
    in_batch: Batch,
    in_pos: usize,
    /// The matches of the current probe row: its index and the next value.
    current: Option<(usize, usize)>,
}

impl HashJoin {
    fn key(&self, row: &[u64]) -> Vec<u64> {
        self.keys.iter().map(|v| row[v.0]).collect()
    }

    fn build_table(&mut self, g: &Graph) -> Result<()> {
        let Some(mut build) = self.build.take() else {
            return Ok(());
        };
        let mut rows = Batch::new(self.in_batch.width);
        drain(build.as_mut(), g, &mut rows)?;
        for i in 0..rows.len() {
            let row = rows.row(i);
            let values = self.table.entry(self.key(row)).or_default();
            if self.new_vars.is_empty() {
                values.push(0);
            }
            values.extend(self.new_vars.iter().map(|v| row[v.0]));
        }
        Ok(())
    }
}

impl Operator for HashJoin {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        self.build_table(g)?;
        let stride = self.new_vars.len().max(1);
        loop {
            if let Some((i, mut pos)) = self.current {
                let mut new = self.in_batch.row(i).to_vec();
                let key = self.key(&new);
                let values = &self.table[&key];
                while pos < values.len() {
                    if out.is_full() {
                        self.current = Some((i, pos));
                        return Ok(());
                    }
                    for (k, v) in self.new_vars.iter().enumerate() {
                        new[v.0] = values[pos + k];
                    }
                    out.push(&new);
                    pos += stride;
                }
                self.current = None;
            }
            if out.is_full() {
                return Ok(());
            }
            if self.in_pos >= self.in_batch.len() {
                self.in_batch.clear();
                self.in_pos = 0;
                self.probe.next(g, &mut self.in_batch)?;
                if self.in_batch.is_empty() {
                    return Ok(());
                }
            }
            let i = self.in_pos;
            self.in_pos += 1;
            if self.table.contains_key(&self.key(self.in_batch.row(i))) {
                self.current = Some((i, 0));
            }
        }
    }
}

/// The rows of a union's branches, evaluated for each batch of input rows.
struct UnionBind {
    input: Box<dyn Operator>,
    branches: Vec<GroupPlan>,
    bound: Vec<bool>,
    ctx: Context,
    pending: Batch,
    pending_pos: usize,
}

impl Operator for UnionBind {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        loop {
            while self.pending_pos < self.pending.len() {
                if out.is_full() {
                    return Ok(());
                }
                out.push(self.pending.row(self.pending_pos));
                self.pending_pos += 1;
            }
            let mut rows = Batch::new(out.width);
            self.input.next(g, &mut rows)?;
            if rows.is_empty() {
                return Ok(());
            }
            self.pending.clear();
            self.pending_pos = 0;
            for branch in &self.branches {
                let seed = Box::new(Seed {
                    rows: Some(rows.clone()),
                });
                let mut op = self.ctx.build(branch, seed, &mut self.bound.clone());
                drain(op.as_mut(), g, &mut self.pending)?;
            }
        }
    }
}

/// Rows extended by each match of an optional group, or kept as they are
/// if it has none, leaving `vars` unbound (0).  For each batch, the group is
/// evaluated for its rows; or, if `table` is set, it is evaluated once alone
/// and joined on `keys`.
struct OptionalJoin {
    input: Box<dyn Operator>,
    group: GroupPlan,
    /// The variables the input rows bind.
    bound: Vec<bool>,
    vars: Vec<Var>,
    keys: Vec<Var>,
    ctx: Context,
    /// For evaluating once: the group's rows, once known.
    table: Option<Option<OptionalTable>>,
    pending: Batch,
    pending_pos: usize,
}

/// The values of an optional group's new variables in each of its rows, by
/// their values of the keys it is joined on.
type OptionalTable = HashMap<Vec<u64>, Vec<Vec<u64>>>;

impl OptionalJoin {
    /// Appends to `pending` the rows for `input`.
    fn join(&mut self, g: &Graph, input: &Batch) -> Result<()> {
        if let Some(table) = &mut self.table {
            if table.is_none() {
                let mut bound = self.ctx.unbound();
                let mut op = self.ctx.build(&self.group, self.ctx.unit(), &mut bound);
                let mut rows = Batch::new(self.ctx.width);
                drain(op.as_mut(), g, &mut rows)?;
                let mut built = OptionalTable::new();
                for i in 0..rows.len() {
                    let row = rows.row(i);
                    let key = self.keys.iter().map(|v| row[v.0]).collect();
                    built
                        .entry(key)
                        .or_default()
                        .push(self.vars.iter().map(|v| row[v.0]).collect());
                }
                *table = Some(built);
            }
            let table = table.as_ref().expect("built");
            for i in 0..input.len() {
                let row = input.row(i);
                let key: Vec<u64> = self.keys.iter().map(|v| row[v.0]).collect();
                match table.get(&key) {
                    None => self.pending.push(row),
                    Some(matches) => {
                        let mut new = row.to_vec();
                        for values in matches {
                            for (v, &value) in self.vars.iter().zip(values) {
                                new[v.0] = value;
                            }
                            self.pending.push(&new);
                        }
                    }
                }
            }
            return Ok(());
        }
        // Evaluated for the batch: each match extends an input row, which
        // is known by its values of the variables bound.
        let bound: Vec<Var> = (0..self.bound.len())
            .filter(|&v| self.bound[v])
            .map(Var)
            .collect();
        let seed = Box::new(Seed {
            rows: Some(input.clone()),
        });
        let mut op = self.ctx.build(&self.group, seed, &mut self.bound.clone());
        let mut matches = Batch::new(self.ctx.width);
        drain(op.as_mut(), g, &mut matches)?;
        let mut matched = HashSet::new();
        for i in 0..matches.len() {
            let row = matches.row(i);
            matched.insert(bound.iter().map(|v| row[v.0]).collect::<Vec<u64>>());
            self.pending.push(row);
        }
        for i in 0..input.len() {
            let row = input.row(i);
            if !matched.contains(&bound.iter().map(|v| row[v.0]).collect::<Vec<u64>>()) {
                self.pending.push(row);
            }
        }
        Ok(())
    }
}

impl Operator for OptionalJoin {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        loop {
            while self.pending_pos < self.pending.len() {
                if out.is_full() {
                    return Ok(());
                }
                out.push(self.pending.row(self.pending_pos));
                self.pending_pos += 1;
            }
            let mut input = Batch::new(out.width);
            self.input.next(g, &mut input)?;
            if input.is_empty() {
                return Ok(());
            }
            self.pending.clear();
            self.pending_pos = 0;
            self.join(g, &input)?;
        }
    }
}

/// The rows for which a group has no match, given their values of `keys`.
/// For each batch, the group is evaluated for its rows; or, if `once` is
/// set, it is evaluated once alone.
struct AntiJoin {
    input: Box<dyn Operator>,
    group: GroupPlan,
    keys: Vec<Var>,
    bound: Vec<bool>,
    ctx: Context,
    /// For evaluating once: the keys with a match, once known.
    once: Option<Option<HashSet<Vec<u64>>>>,
}

/// The values of `keys` of the rows `group` has a match for, evaluated for
/// `rows`, which bind `bound` (or alone, if `rows` is `None`).
fn matched(
    ctx: &Context,
    g: &Graph,
    group: &GroupPlan,
    keys: &[Var],
    bound: &[bool],
    rows: Option<&Batch>,
) -> Result<HashSet<Vec<u64>>> {
    let (seed, mut bound): (Box<dyn Operator>, Vec<bool>) = match rows {
        Some(rows) => (
            Box::new(Seed {
                rows: Some(rows.clone()),
            }),
            bound.to_vec(),
        ),
        None => (ctx.unit(), ctx.unbound()),
    };
    let mut op = ctx.build(group, seed, &mut bound);
    let mut matches = Batch::new(ctx.width);
    drain(op.as_mut(), g, &mut matches)?;
    Ok((0..matches.len())
        .map(|i| keys.iter().map(|v| matches.row(i)[v.0]).collect())
        .collect())
}

impl AntiJoin {
    fn matched(&self, g: &Graph, rows: Option<&Batch>) -> Result<HashSet<Vec<u64>>> {
        matched(&self.ctx, g, &self.group, &self.keys, &self.bound, rows)
    }
}

/// The rows for which a group's match changes between two plans of it; see
/// [`Step::NotChange`].
struct NotChange {
    input: Box<dyn Operator>,
    old: GroupPlan,
    new: GroupPlan,
    keys: Vec<Var>,
    flips: Arc<HashSet<Vec<u64>>>,
    flip_vars: Vec<Var>,
    to_unmatched: bool,
    bound: Vec<bool>,
    ctx: Context,
}

impl Operator for NotChange {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        loop {
            let mut input = Batch::new(out.width);
            self.input.next(g, &mut input)?;
            if input.is_empty() {
                return Ok(());
            }
            let mut rows = Batch::new(out.width);
            for i in 0..input.len() {
                let row = input.row(i);
                let flip: Vec<u64> = self.flip_vars.iter().map(|v| row[v.0]).collect();
                if self.flips.contains(&flip) {
                    rows.push(row);
                }
            }
            if rows.is_empty() {
                continue;
            }
            let old = matched(
                &self.ctx,
                g,
                &self.old,
                &self.keys,
                &self.bound,
                Some(&rows),
            )?;
            let new = matched(
                &self.ctx,
                g,
                &self.new,
                &self.keys,
                &self.bound,
                Some(&rows),
            )?;
            for i in 0..rows.len() {
                let row = rows.row(i);
                let key: Vec<u64> = self.keys.iter().map(|v| row[v.0]).collect();
                let (was, is) = (old.contains(&key), new.contains(&key));
                if was != is && was == self.to_unmatched {
                    out.push(row);
                }
            }
            if !out.is_empty() {
                return Ok(());
            }
        }
    }
}

impl Operator for AntiJoin {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        if let Some(None) = self.once {
            self.once = Some(Some(self.matched(g, None)?));
        }
        loop {
            let mut rows = Batch::new(out.width);
            self.input.next(g, &mut rows)?;
            if rows.is_empty() {
                return Ok(());
            }
            let fresh;
            let matched = match &self.once {
                Some(Some(m)) => m,
                _ => {
                    fresh = self.matched(g, Some(&rows))?;
                    &fresh
                }
            };
            for i in 0..rows.len() {
                let row = rows.row(i);
                let key: Vec<u64> = self.keys.iter().map(|v| row[v.0]).collect();
                if !matched.contains(&key) {
                    out.push(row);
                }
            }
            if !out.is_empty() {
                return Ok(());
            }
        }
    }
}

/// The rows passing every filter.
struct FilterOp {
    input: Box<dyn Operator>,
    filters: Vec<Filter>,
    batch: Batch,
    /// The parameters' values that are not entries, by slot.
    values: Values,
}

impl Operator for FilterOp {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        loop {
            self.batch.clear();
            self.input.next(g, &mut self.batch)?;
            if self.batch.is_empty() {
                return Ok(());
            }
            for i in 0..self.batch.len() {
                let row = self.batch.row(i);
                let values = self.values.as_deref().unwrap_or_default();
                if self.filters.iter().all(|f| f.eval(g, row, values)) {
                    out.push(row);
                }
            }
            if !out.is_empty() {
                return Ok(());
            }
        }
    }
}

/// The first row with each value of `keys`.
struct Distinct {
    input: Box<dyn Operator>,
    keys: Vec<Var>,
    seen: HashSet<Vec<u64>>,
    batch: Batch,
}

impl Operator for Distinct {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        loop {
            self.batch.clear();
            self.input.next(g, &mut self.batch)?;
            if self.batch.is_empty() {
                return Ok(());
            }
            for i in 0..self.batch.len() {
                let row = self.batch.row(i);
                if self
                    .seen
                    .insert(self.keys.iter().map(|v| row[v.0]).collect())
                {
                    out.push(row);
                }
            }
            if !out.is_empty() {
                return Ok(());
            }
        }
    }
}

/// All the rows, in order.
struct Sort {
    input: Option<Box<dyn Operator>>,
    keys: Vec<SortKey>,
    rows: Batch,
    pos: usize,
}

impl Sort {
    fn compare(g: &Graph, keys: &[SortKey], a: &[u64], b: &[u64]) -> Ordering {
        for k in keys {
            let (x, y) = (a[k.var.0], b[k.var.0]);
            let mut ord = if k.by_data {
                aggregate::compare_cells(g, &Cell::of_id(x), &Cell::of_id(y)).then(x.cmp(&y))
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
}

impl Operator for Sort {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        if let Some(mut input) = self.input.take() {
            drain(input.as_mut(), g, &mut self.rows)?;
            let width = self.rows.width;
            let mut order: Vec<usize> = (0..self.rows.len()).collect();
            order
                .sort_by(|&i, &j| Self::compare(g, &self.keys, self.rows.row(i), self.rows.row(j)));
            let mut sorted = Vec::with_capacity(self.rows.data.len());
            for i in order {
                sorted.extend_from_slice(&self.rows.data[i * width..(i + 1) * width]);
            }
            self.rows.data = sorted;
        }
        while self.pos < self.rows.len() && !out.is_full() {
            out.push(self.rows.row(self.pos));
            self.pos += 1;
        }
        Ok(())
    }
}

/// The rows after the first `skip`, up to `remaining` of them.
struct Limit {
    input: Box<dyn Operator>,
    skip: usize,
    remaining: usize,
}

impl Operator for Limit {
    fn next(&mut self, g: &Graph, out: &mut Batch) -> Result<()> {
        while self.remaining > 0 {
            // No more rows than are wanted, so that the input reads no more.
            let wanted = self.skip.saturating_add(self.remaining).min(out.room());
            let mut batch = Batch::with_rows(out.width, wanted);
            self.input.next(g, &mut batch)?;
            if batch.is_empty() {
                return Ok(());
            }
            let skipped = batch.len().min(self.skip);
            self.skip -= skipped;
            let keep = (batch.len() - skipped).min(self.remaining);
            for i in skipped..skipped + keep {
                out.push(batch.row(i));
            }
            self.remaining -= keep;
            if !out.is_empty() {
                return Ok(());
            }
        }
        Ok(())
    }
}

/// What building operators needs to know.
#[derive(Debug, Clone)]
struct Context {
    /// The slots of a row.
    width: usize,
    /// The query's `as_of`.
    as_of: Option<u64>,
    /// Whether to use the reduction: whether the graph is as it was planned.
    reduced: bool,
    /// The relations evaluated again, if the graph has changed since
    /// planned.
    relations: Option<Arc<Relations>>,
    /// The row a group run alone starts from, with the parameters' values,
    /// and which variables those are.
    start: Arc<[u64]>,
    params: Arc<[bool]>,
    /// The values of the parameters given values that are not entries, by
    /// slot, which expressions read.
    values: Values,
    /// The overlay read over the database, if any.
    overlay: Option<Shared>,
    /// The hold of the ephemeral ids the query gives, if any.
    hold: Option<crate::graph::EphemeralHold>,
}

/// The values of the parameters given values that are not entries, by slot;
/// `None` if there are none, as when every value is an entry.
type Values = Option<Arc<[(usize, Cell)]>>;

/// The values given a query's parameters: entries, or any values.
#[derive(Clone, Copy)]
enum Given<'a> {
    Ids(&'a [u64]),
    Cells(&'a [Cell]),
}

impl Given<'_> {
    fn len(self) -> usize {
        match self {
            Given::Ids(ids) => ids.len(),
            Given::Cells(cells) => cells.len(),
        }
    }
}

impl Context {
    /// The single row a group run alone starts from.
    fn unit(&self) -> Box<dyn Operator> {
        unit(self.width, &self.start)
    }

    /// What a group run alone has bound: the parameters.
    fn unbound(&self) -> Vec<bool> {
        self.params.to_vec()
    }

    /// The operator evaluating `plan` for the rows of `input`, which bind
    /// `bound`; on return, `bound` is what its rows bind.
    fn build(
        &self,
        plan: &GroupPlan,
        input: Box<dyn Operator>,
        bound: &mut [bool],
    ) -> Box<dyn Operator> {
        let width = self.width;
        let mut op = input;
        for step in &plan.steps {
            op = match step {
                Step::Pattern {
                    pattern,
                    join,
                    access,
                    keys,
                    reduced,
                    required,
                    candidates,
                    ..
                } => {
                    let reduced = reduced.clone().filter(|_| *required || self.reduced);
                    let candidates = candidates.clone().filter(|_| self.reduced);
                    let access = match (&reduced, access) {
                        // The reduction can not be used: look the pattern
                        // up by what is bound.
                        (None, Access::All) => Access::of(
                            pattern.link.is_some_and(|v| bound[v.0]),
                            pattern.terms().map(|t| match t {
                                Term::Id(_) => true,
                                Term::Var(v) => *join == Join::Bind && bound[v.0],
                            }),
                        ),
                        (_, access) => *access,
                    };
                    let scan = |input, bound: &[bool]| BindJoin {
                        input,
                        matcher: Box::new(Matcher {
                            pattern: *pattern,
                            access,
                            bound: bound.to_vec(),
                            as_of: pattern.as_of.or(self.as_of),
                            overlay: self.overlay.clone(),
                            reduced: reduced.clone(),
                            index: None,
                            candidates: candidates.clone(),
                            stream: None,
                            new: Vec::new(),
                            live: Vec::new(),
                            seen: Vec::new(),
                        }),
                        in_batch: Batch::new(width),
                        in_pos: 0,
                        pending: Batch::new(width),
                        pending_pos: 0,
                        streaming: false,
                    };
                    let joined: Box<dyn Operator> = match join {
                        Join::Bind => Box::new(scan(op, bound)),
                        Join::Hash => Box::new(HashJoin {
                            probe: op,
                            build: Some(Box::new(scan(self.unit(), &self.unbound()))),
                            keys: keys.clone(),
                            new_vars: new_vars(pattern.vars(), bound),
                            table: HashMap::new(),
                            in_batch: Batch::new(width),
                            in_pos: 0,
                            current: None,
                        }),
                    };
                    pattern.vars().for_each(|v| bound[v.0] = true);
                    joined
                }
                Step::Union {
                    branches,
                    vars,
                    keys,
                    join,
                    ..
                } => {
                    let joined: Box<dyn Operator> = match join {
                        Join::Bind => Box::new(UnionBind {
                            input: op,
                            branches: branches.clone(),
                            bound: bound.to_vec(),
                            ctx: self.clone(),
                            pending: Batch::new(width),
                            pending_pos: 0,
                        }),
                        Join::Hash => {
                            let ops = branches
                                .iter()
                                .map(|b| self.build(b, self.unit(), &mut self.unbound()))
                                .collect();
                            Box::new(HashJoin {
                                probe: op,
                                build: Some(Box::new(Concat { ops })),
                                keys: keys.clone(),
                                new_vars: new_vars(vars.iter().copied(), bound),
                                table: HashMap::new(),
                                in_batch: Batch::new(width),
                                in_pos: 0,
                                current: None,
                            })
                        }
                    };
                    vars.iter().for_each(|v| bound[v.0] = true);
                    joined
                }
                Step::Path { path, .. } => {
                    let joined = Box::new(BindJoin {
                        input: op,
                        matcher: marked(
                            PathLookup {
                                path: path.clone(),
                                bound: bound.to_vec(),
                                as_of: path.as_of.or(self.as_of),
                                overlay: self.overlay.clone(),
                                neighbors: HashMap::new(),
                            },
                            path.mark,
                        ),
                        in_batch: Batch::new(width),
                        in_pos: 0,
                        pending: Batch::new(width),
                        pending_pos: 0,
                        streaming: false,
                    });
                    path.vars().for_each(|v| bound[v.0] = true);
                    joined
                }
                Step::Call {
                    call,
                    tuples,
                    given,
                    ..
                } => {
                    let tuples = match &self.relations {
                        Some(r) if !given => r.tuples[call.relation.0].clone().unwrap_or_default(),
                        _ => tuples.clone(),
                    };
                    let joined = Box::new(BindJoin {
                        input: op,
                        matcher: marked(
                            CallLookup {
                                args: call.args.clone(),
                                tuples,
                                bound: bound.to_vec(),
                                index: None,
                            },
                            call.mark,
                        ),
                        in_batch: Batch::new(width),
                        in_pos: 0,
                        pending: Batch::new(width),
                        pending_pos: 0,
                        streaming: false,
                    });
                    call.vars().for_each(|v| bound[v.0] = true);
                    joined
                }
                Step::Nearest { nearest, scan, .. } => {
                    let joined = Box::new(BindJoin {
                        input: op,
                        matcher: Box::new(NearestLookup {
                            nearest: nearest.clone(),
                            scan: scan.clone(),
                            ctx: self.clone(),
                            bound: bound.to_vec(),
                            vectors: None,
                        }),
                        in_batch: Batch::new(width),
                        in_pos: 0,
                        pending: Batch::new(width),
                        pending_pos: 0,
                        streaming: false,
                    });
                    nearest.vars().for_each(|v| bound[v.0] = true);
                    joined
                }
                Step::Filter(filters) => Box::new(FilterOp {
                    input: op,
                    filters: filters.clone(),
                    batch: Batch::new(width),
                    values: self.values.clone(),
                }),
                Step::Not {
                    group, keys, join, ..
                } => Box::new(AntiJoin {
                    input: op,
                    group: group.clone(),
                    keys: keys.clone(),
                    bound: bound.to_vec(),
                    ctx: self.clone(),
                    once: (*join == Join::Hash).then_some(None),
                }),
                Step::Optional {
                    group,
                    vars,
                    keys,
                    join,
                    ..
                } => {
                    let joined = Box::new(OptionalJoin {
                        input: op,
                        group: group.clone(),
                        bound: bound.to_vec(),
                        vars: vars.clone(),
                        keys: keys.clone(),
                        ctx: self.clone(),
                        table: (*join == Join::Hash).then_some(None),
                        pending: Batch::new(width),
                        pending_pos: 0,
                    });
                    vars.iter().for_each(|v| bound[v.0] = true);
                    joined
                }
                Step::NotChange {
                    old,
                    new,
                    keys,
                    flips,
                    flip_vars,
                    to_unmatched,
                    ..
                } => Box::new(NotChange {
                    input: op,
                    old: old.clone(),
                    new: new.clone(),
                    keys: keys.clone(),
                    flips: flips.clone(),
                    flip_vars: flip_vars.clone(),
                    to_unmatched: *to_unmatched,
                    bound: bound.to_vec(),
                    ctx: self.clone(),
                }),
            };
        }
        op
    }
}

/// The distinct variables of `vars` that are not `bound`.
fn new_vars(vars: impl Iterator<Item = Var>, bound: &[bool]) -> Vec<Var> {
    let mut vars: Vec<Var> = vars.filter(|v| !bound[v.0]).collect();
    vars.sort();
    vars.dedup();
    vars
}

/// The operator matching a query's top group, the row it starts from, the
/// parameters' values that are not entries, by slot, and the rows' width.
struct Matched {
    op: Box<dyn Operator>,
    start: Arc<[u64]>,
    values: Values,
    width: usize,
}

/// The rows of a query, made by [`Plan::execute`].
pub struct Rows<'g> {
    g: &'g Graph,
    /// An error to return first: from evaluating the relations again.
    error: Option<crate::graph::Error>,
    op: Box<dyn Operator>,
    select: Vec<Var>,
    batch: Batch,
    pos: usize,
    done: bool,
}

impl Iterator for Rows<'_> {
    type Item = Result<Vec<u64>>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.error.take() {
            self.done = true;
            return Some(Err(e));
        }
        while self.pos >= self.batch.len() {
            if self.done {
                return None;
            }
            self.batch.clear();
            self.pos = 0;
            if let Err(e) = self.op.next(self.g, &mut self.batch) {
                self.done = true;
                return Some(Err(e));
            }
            if self.batch.is_empty() {
                self.done = true;
                return None;
            }
        }
        let row = self.batch.row(self.pos);
        self.pos += 1;
        Some(Ok(self.select.iter().map(|v| row[v.0]).collect()))
    }
}

impl Plan {
    /// Runs the plan on `g`, which should be the graph it was planned for
    /// (otherwise the results are right but the plan may be slow).  Fails if
    /// the query has [parameters](super::Query::param): see
    /// [`execute_with`](Self::execute_with).
    pub fn execute<'g>(&self, g: &'g Graph) -> Rows<'g> {
        self.execute_with(g, &[])
    }

    /// Runs the plan on `g` with `values` as the values of the query's
    /// [parameters](super::Query::param), in the order they were declared.
    /// Fails if there are more or fewer values than parameters, or if the
    /// query [computes](super::Query::computes) values: see
    /// [`execute_cells`](Self::execute_cells).
    pub fn execute_with<'g>(&self, g: &'g Graph, values: &[u64]) -> Rows<'g> {
        self.rows(g, Given::Ids(values), None)
    }

    /// The same, with values that need not be entries.  A parameter given an
    /// entry (`Cell::Id`) is that entry wherever it is used.  One given
    /// another value, such as text or a number, is in a pattern the entry
    /// with that data, if there is one (else it matches nothing), and in an
    /// expression ([`Filter::Holds`], a computed value,
    /// `having`) the value itself, which need not be in the graph: `?age >=
    /// $min` holds for `min` = 21 by comparing numbers.
    pub fn execute_values<'g>(&self, g: &'g Graph, values: &[Cell]) -> Rows<'g> {
        self.rows(g, Given::Cells(values), None)
    }

    fn rows<'g>(&self, g: &'g Graph, values: Given, overlay: Option<&Shared>) -> Rows<'g> {
        self.rows_of(
            g,
            values,
            self.query.selected(),
            self.query.distinct,
            overlay,
        )
    }

    /// Runs the plan on `g` beneath `overlay` (see [`Overlay`]): reading
    /// the overlay's links over the database's (or over nothing), with
    /// `values` as the values of the query's parameters, as
    /// [`execute_values`](Self::execute_values) takes.  Its rules are
    /// evaluated beneath it too.
    pub fn execute_over<'g>(
        &self,
        g: &'g Graph,
        overlay: &Arc<Overlay>,
        values: &[Cell],
    ) -> Rows<'g> {
        self.rows(g, Given::Cells(values), Some(overlay))
    }

    /// The same, as cells: see [`execute_cells`](Self::execute_cells).
    pub fn execute_cells_over(
        &self,
        g: &Graph,
        overlay: &Arc<Overlay>,
        values: &[Cell],
    ) -> Result<Vec<Vec<Cell>>> {
        self.cells(g, Given::Cells(values), Some(overlay))
    }

    /// The same, as a graph: see [`execute_graph`](Self::execute_graph).
    /// The overlay's links it matches are [`Derivation::Overlay`] links.
    pub fn execute_graph_over(
        &self,
        g: &Graph,
        overlay: &Arc<Overlay>,
        values: &[Cell],
    ) -> Result<ResultGraph> {
        self.graph(g, Given::Cells(values), Some(overlay))
    }

    /// The rows, of the variables `select`, deduplicated if `distinct`.
    fn rows_of<'g>(
        &self,
        g: &'g Graph,
        values: Given,
        select: Vec<Var>,
        distinct: bool,
        overlay: Option<&Shared>,
    ) -> Rows<'g> {
        let q = &self.query;
        let (mut op, mut error, width) = match self.matched(g, values, overlay) {
            Ok(m) => (m.op, None, m.width),
            Err(e) => (unit(1, &[0]), Some(e), 1),
        };
        if error.is_none() && q.computes() {
            error = Some(Error::InvalidArgument(
                "a query that computes values returns cells: see Plan::execute_cells",
            ));
        }
        if !q.value_limits.is_empty() {
            op = Box::new(ValueLimits {
                input: Some(op),
                limits: q.value_limits.clone(),
                rows: Batch::new(width),
                pos: 0,
            });
        }
        if distinct {
            op = Box::new(Distinct {
                input: op,
                keys: select.clone(),
                seen: HashSet::new(),
                batch: Batch::new(width),
            });
        }
        if !q.order.is_empty() {
            op = Box::new(Sort {
                input: Some(op),
                keys: q.order.clone(),
                rows: Batch::new(width),
                pos: 0,
            });
        }
        if q.limit.is_some() || q.offset > 0 {
            op = Box::new(Limit {
                input: op,
                skip: q.offset,
                remaining: q.limit.unwrap_or(usize::MAX),
            });
        }
        Rows {
            g,
            error,
            op,
            select,
            batch: Batch::new(width),
            pos: 0,
            done: false,
        }
    }

    /// The operator matching the query's top group, before it is
    /// deduplicated, ordered or limited, with `values` as the values of its
    /// parameters.
    fn matched(&self, g: &Graph, values: Given, overlay: Option<&Shared>) -> Result<Matched> {
        let q = &self.query;
        if values.len() != q.params.len() {
            return Err(Error::InvalidArgument(
                "a query needs a value for each of its parameters",
            ));
        }
        // A row has a slot even with no variables, so that it is a row.
        let width = q.names.len().max(1);
        let mut start = vec![0; width];
        let mut cells = Vec::new();
        match values {
            Given::Ids(ids) => {
                for (v, &id) in q.params.iter().zip(ids) {
                    start[v.0] = id;
                }
            }
            Given::Cells(values) => {
                for (v, value) in q.params.iter().zip(values) {
                    // An entry is itself; another value is, in a pattern, the
                    // value with its data (ephemeral, if the graph doesn't
                    // have it, so it matches no link but compares by id),
                    // and in an expression itself.
                    start[v.0] = match value {
                        Cell::Id(id) => *id,
                        Cell::Unbound => 0,
                        other => {
                            cells.push((v.0, other.clone()));
                            other.data(g).map_or(0, |d| q.ephemeral_value(g, &d))
                        }
                    };
                }
            }
        }
        let start: Arc<[u64]> = start.into();
        let values: Values = (!cells.is_empty()).then(|| cells.into());
        // Beneath an overlay, the graph isn't as planned.
        let unchanged = g.next_id() == self.next_id && overlay.is_none();
        // Relations evaluated for the graph as it was are evaluated again.
        let relations = if unchanged || self.relations_given || !q.root.has_paths_or_calls() {
            None
        } else {
            Some(Arc::new(rules::evaluate_over(q, g, overlay)?))
        };
        let ctx = Context {
            overlay: overlay.cloned(),
            width,
            as_of: q.as_of,
            reduced: unchanged,
            relations,
            start: start.clone(),
            params: self.params.clone(),
            values: values.clone(),
            hold: q.ephemeral_hold().cloned(),
        };
        let op = ctx.build(&self.root, ctx.unit(), &mut ctx.unbound());
        Ok(Matched {
            op,
            start,
            values,
            width,
        })
    }

    /// Runs the plan on `g` with `values` as the values of the query's
    /// parameters, collecting its rows as cells: the values of the variables
    /// selected, computed ones included (see
    /// [`Query::compute`](super::Query::compute)).
    pub fn execute_cells(&self, g: &Graph, values: &[u64]) -> Result<Vec<Vec<Cell>>> {
        self.cells(g, Given::Ids(values), None)
    }

    /// The same, with values that need not be entries, as
    /// [`execute_values`](Self::execute_values) takes.
    pub fn execute_cells_values(&self, g: &Graph, values: &[Cell]) -> Result<Vec<Vec<Cell>>> {
        self.cells(g, Given::Cells(values), None)
    }

    /// Runs the plan on `g` with `values` as the values of the query's
    /// parameters, returning the links its rows match and the entries its
    /// selected variables bind that aren't in them.  The query must
    /// [record its links](super::Query::record_links), and not compute
    /// values.
    pub fn execute_graph(&self, g: &Graph, values: &[u64]) -> Result<ResultGraph> {
        self.graph(g, Given::Ids(values), None)
    }

    /// The same, with values that need not be entries, as
    /// [`execute_values`](Self::execute_values) takes.
    pub fn execute_graph_values(&self, g: &Graph, values: &[Cell]) -> Result<ResultGraph> {
        self.graph(g, Given::Cells(values), None)
    }

    /// The rows of the query, which records its links, with what each
    /// matched.
    pub(crate) fn rows_with_provenance(
        &self,
        g: &Graph,
        overlay: Option<&Shared>,
    ) -> Result<Vec<(Vec<u64>, RowProvenance)>> {
        let q = &self.query;
        let recorder = Recorder::new(q);
        let selected = q.selected();
        let all: Vec<Var> = (0..q.names.len()).map(Var).collect();
        self.rows_of(g, Given::Ids(&[]), all, false, overlay)
            .map(|row| {
                let row = row?;
                let values = selected.iter().map(|v| row[v.0]).collect();
                Ok((values, recorder.row(&row)))
            })
            .collect()
    }

    fn graph(&self, g: &Graph, values: Given, overlay: Option<&Shared>) -> Result<ResultGraph> {
        let q = &self.query;
        if !q.record {
            return Err(Error::InvalidArgument(
                "the query doesn't record its links: see Query::record_links",
            ));
        }
        if q.computes() {
            return Err(Error::InvalidArgument(
                "a query that computes values can't return a graph",
            ));
        }
        // The relations' derivations, as the rows see them: evaluated again
        // if the graph has changed since planned, as the rows' relations are.
        let again;
        let relations = if (g.next_id() == self.next_id && overlay.is_none())
            || self.relations_given
            || !q.root.has_paths_or_calls()
        {
            &self.relations
        } else {
            again = Arc::new(rules::evaluate_over(q, g, overlay)?);
            &again
        };
        let no_derivations = Default::default();
        let derivations = relations.derivations.as_deref().unwrap_or(&no_derivations);
        let mut support = Support::new(derivations, q.as_of, overlay.cloned());
        let recorder = Recorder::new(q);
        // The entries of the selected variables, other than link variables'.
        let nodes: Vec<Var> = q
            .selected()
            .into_iter()
            .filter(|v| !q.recorded.contains(v))
            .collect();
        let all: Vec<Var> = (0..q.names.len()).map(Var).collect();
        let (mut links, mut entries) = (BTreeSet::new(), BTreeSet::new());
        let mut derived: Vec<ResultLink> = Vec::new();
        let mut seen = HashSet::new();
        let mut derive = |fields: [ResultNode; 3], by: Derivation, s: Vec<u64>| {
            if seen.insert(format!("{fields:?} {by:?}")) {
                derived.push(ResultLink::Derived {
                    fields,
                    by,
                    support: s,
                });
            }
        };
        let asserting = relations.asserting.as_deref();
        // The aggregates' relations, and the tuples of the top group's, by
        // subject and scope.
        let aggregates: HashMap<Relation, &LinkAggregate> = q
            .rules
            .iter()
            .filter_map(|r| r.aggregate.as_ref().map(|a| (r.relation, a)))
            .collect();
        let by_subject: Vec<BySubject> = q
            .aggregated
            .iter()
            .map(|(r, a)| {
                let mut index: HashMap<(u64, u64), Vec<&Vec<u64>>> = HashMap::new();
                for t in relations.tuples[r.0].iter().flat_map(|t| t.iter()) {
                    let scope = if a.scope.is_some() { t[2] } else { 0 };
                    index.entry((t[0], scope)).or_default().push(t);
                }
                index
            })
            .collect();
        let aggregate_link = |a: &LinkAggregate, t: &[u64]| {
            [
                result_node(g, t[0]),
                result_node(g, a.predicate),
                result_node(g, t[1]),
            ]
        };
        let mut instances = Instances::default();
        let query_vars: Vec<Vec<Var>> = q.assertions.iter().map(Assertion::vars).collect();
        for row in self.rows_of(g, values, all, false, overlay) {
            let row = row?;
            entries.extend(nodes.iter().map(|v| row[v.0]).filter(|&id| id != 0));
            recorder.links_into(&row, &mut links);
            let mut prov = None;
            for (i, a) in q.assertions.iter().enumerate() {
                let Some(triples) =
                    instances.instantiate(g, (0, i), a, &query_vars[i], &|v| row[v.0])
                else {
                    continue;
                };
                let p = prov.get_or_insert_with(|| recorder.row(&row));
                let s = support.of_row(g, p);
                links.extend(s.iter().copied());
                for fields in triples {
                    match stored(g, &fields, q.as_of, overlay.map(|o| &**o)) {
                        Some(id) => {
                            links.insert(id);
                        }
                        None => derive(fields, Derivation::Asserted, s.clone()),
                    }
                }
            }
            // The links of the top group's aggregates for the row's subject
            // and scope.
            for ((r, a), index) in q.aggregated.iter().zip(&by_subject) {
                let scope = a.scope.map_or(0, |v| row[v.0]);
                for t in index.get(&(row[a.subject.0], scope)).into_iter().flatten() {
                    let s = support.of_tuple(g, *r, t);
                    links.extend(s.iter().copied());
                    derive(
                        aggregate_link(a, t),
                        Derivation::Aggregate(a.function.name()),
                        s,
                    );
                }
            }
            if !recorder.derives() {
                continue;
            }
            let prov = prov.unwrap_or_else(|| recorder.row(&row));
            for m in &prov.paths {
                let Some(path) = witness(g, m, q.as_of, overlay.map(|o| &**o)) else {
                    continue;
                };
                links.extend(path.iter().copied());
                let predicate = match &m.path.expr {
                    Some(e) => ResultNode::Path(e.clone()),
                    None => ResultNode::Entry(m.predicate),
                };
                derive(
                    [
                        ResultNode::Entry(m.subject),
                        predicate,
                        ResultNode::Entry(m.object),
                    ],
                    Derivation::Path,
                    path,
                );
            }
            for (r, t) in &prov.calls {
                let s = support.of_tuple(g, *r, t);
                links.extend(s.iter().copied());
                if let Some(a) = aggregates.get(r) {
                    derive(
                        aggregate_link(a, t),
                        Derivation::Aggregate(a.function.name()),
                        s,
                    );
                    continue;
                }
                // A relation of two columns: its tuples are links.
                if let [a, b] = t[..] {
                    let name = q.relations[r.0].0.clone();
                    derive(
                        [
                            ResultNode::Entry(a),
                            ResultNode::Value(Cell::Text(name.clone())),
                            ResultNode::Entry(b),
                        ],
                        Derivation::Rule(name),
                        s.clone(),
                    );
                }
                // The triples asserted by the rules deriving the tuple.
                let rules = asserting.and_then(|a| a.get(&(*r, t.clone())));
                for &k in rules.into_iter().flatten() {
                    let rule = &q.rules[k];
                    let name = &q.relations[r.0].0;
                    let head = |v: Var| rule.head.iter().position(|&h| h == v).map_or(0, |i| t[i]);
                    for (j, a) in rule.assertions.iter().enumerate() {
                        let Some(triples) =
                            instances.instantiate(g, (k + 1, j), a, &a.vars(), &head)
                        else {
                            continue;
                        };
                        for fields in triples {
                            match stored(g, &fields, q.as_of, overlay.map(|o| &**o)) {
                                Some(id) => {
                                    links.insert(id);
                                }
                                None => derive(fields, Derivation::Rule(name.clone()), s.clone()),
                            }
                        }
                    }
                }
            }
        }
        // The overlay's links are its, not stored.
        let (stored_links, overlay_links): (Vec<u64>, Vec<u64>) =
            links.into_iter().partition(|&id| id & EPHEMERAL == 0);
        for id in overlay_links {
            if let Some(l) = overlay.and_then(|o| o.link(id)) {
                derive(
                    l.spo.map(|e| result_node(g, e)),
                    Derivation::Overlay,
                    Vec::new(),
                );
            }
        }
        // The nodes are the entries not in a link.
        for &id in &stored_links {
            if let Some(f) = g.get(id).ok().and_then(|r| r.as_link().map(|l| l.fields())) {
                for e in [f.subject, f.predicate, f.object] {
                    entries.remove(&e);
                }
            }
        }
        for l in &derived {
            if let ResultLink::Derived { fields, .. } = l {
                for f in fields {
                    if let ResultNode::Entry(e) = f {
                        entries.remove(e);
                    }
                }
            }
        }
        Ok(ResultGraph {
            links: stored_links
                .into_iter()
                .map(ResultLink::Stored)
                .chain(derived)
                .collect(),
            nodes: entries.into_iter().collect(),
            holds: crate::graph::Holds::of(q, overlay.map(|o| &**o)),
        })
    }

    fn cells(&self, g: &Graph, values: Given, overlay: Option<&Shared>) -> Result<Vec<Vec<Cell>>> {
        let q = &self.query;
        if !q.computes() {
            return self
                .rows(g, values, overlay)
                .map(|row| row.map(|r| r.into_iter().map(Cell::of_id).collect()))
                .collect();
        }
        let mut m = self.matched(g, values, overlay)?;
        // Nested limits apply to the matches, before computing, as they do
        // to rows.
        if !q.value_limits.is_empty() {
            m.op = Box::new(ValueLimits {
                input: Some(m.op),
                limits: q.value_limits.clone(),
                rows: Batch::new(m.width),
                pos: 0,
            });
        }
        let mut batch = Batch::new(m.width);
        drain(m.op.as_mut(), g, &mut batch)?;
        let rows = (0..batch.len()).map(|i| batch.row(i).to_vec()).collect();
        let values = m.values.as_deref().unwrap_or_default();
        Ok(compute::results(q, g, rows, &m.start, values))
    }
}

/// The instantiations of a query's assertions, and its rules', made so far.
#[derive(Default)]
struct Instances {
    /// Each instantiation: its assertion (0 and its index for the query's,
    /// or a rule's index + 1 and the assertion's), and its variables' values.
    seen: HashSet<((usize, usize), Vec<u64>)>,
    /// The blank nodes numbered so far.
    blanks: u32,
}

impl Instances {
    /// The triples of `a`, whose variables are `vars`, with `value` giving
    /// each variable's value, if they are a new instantiation with its scope
    /// bound: those with no variable unbound.
    fn instantiate(
        &mut self,
        g: &Graph,
        source: (usize, usize),
        a: &Assertion,
        vars: &[Var],
        value: &dyn Fn(Var) -> u64,
    ) -> Option<Vec<[ResultNode; 3]>> {
        if a.scope.is_some_and(|v| value(v) == 0) {
            return None;
        }
        let values: Vec<u64> = vars.iter().map(|&v| value(v)).collect();
        if !self.seen.insert((source, values)) {
            return None;
        }
        let mut blanks: HashMap<u32, u32> = HashMap::new();
        let mut out = Vec::new();
        'triples: for triple in &a.triples {
            let mut fields = [const { ResultNode::Entry(0) }; 3];
            for (f, n) in fields.iter_mut().zip(triple) {
                *f = match n {
                    TemplateNode::Term(t) => {
                        let id = match *t {
                            Term::Id(id) => id,
                            Term::Var(v) => value(v),
                        };
                        if id == 0 {
                            continue 'triples;
                        }
                        result_node(g, id)
                    }
                    TemplateNode::Value(Cell::Id(id)) => ResultNode::Entry(*id),
                    TemplateNode::Value(Cell::Unbound) => continue 'triples,
                    TemplateNode::Value(c) => {
                        let data = match c {
                            Cell::Text(t) => t.clone(),
                            c => c.to_string(),
                        };
                        match g.find_value(data.as_bytes()) {
                            Some(id) => ResultNode::Entry(id),
                            None => ResultNode::Value(Cell::Text(data)),
                        }
                    }
                    TemplateNode::Key(k) => match g.find_by_key(k.as_bytes()) {
                        Some(id) => ResultNode::Entry(id),
                        None => ResultNode::Key(k.clone()),
                    },
                    TemplateNode::Blank(b) => {
                        let n = *blanks.entry(*b).or_insert_with(|| {
                            self.blanks += 1;
                            self.blanks
                        });
                        ResultNode::Blank(n)
                    }
                };
            }
            out.push(fields);
        }
        Some(out)
    }
}

/// The tuples of an aggregate's relation, by subject and scope.
type BySubject<'t> = HashMap<(u64, u64), Vec<&'t Vec<u64>>>;

/// A field of a derived link: an entry, or an ephemeral value's data or
/// node's key (see [`Graph::ephemeral_value`]).
fn result_node(g: &Graph, id: u64) -> ResultNode {
    if id & EPHEMERAL != 0 {
        if let Some(d) = g.data(id) {
            return ResultNode::Value(Cell::of_data(&d));
        }
        if let Some(k) = g.key(id) {
            return ResultNode::Key(String::from_utf8_lossy(&k).into_owned());
        }
    }
    ResultNode::Entry(id)
}

/// The stored link, live as of `as_of`, with the subject, predicate and
/// object `fields`, if they are entries; or beneath `overlay`, its link.
fn stored(
    g: &Graph,
    fields: &[ResultNode; 3],
    as_of: Option<u64>,
    overlay: Option<&Overlay>,
) -> Option<u64> {
    let [
        ResultNode::Entry(s),
        ResultNode::Entry(p),
        ResultNode::Entry(o),
    ] = *fields
    else {
        return None;
    };
    if let Some(ov) = overlay
        && ov.hides([s, p, o])
    {
        return ov.fact([s, p, o]).filter(|l| l.live).map(|l| l.id);
    }
    live_links(g, &g.find_links(s, p, o), as_of)
        .into_iter()
        .find(|&(_, f)| f == [s, p, o])
        .map(|(id, _)| id)
}
