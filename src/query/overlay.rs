//! Overlays: links held in memory, layered over the database (or over
//! nothing), that queries read as if they were stored, and that are never
//! written unless inserted (see [`Overlay`]).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use super::aggregate::Cell;
use super::reduce::Link;
use super::result::{ResultGraph, ResultLink, ResultNode};
use crate::graph::{EPHEMERAL, EphemeralHold, Graph, Holds, Result};

/// A link of an [`Overlay`]: its fact, whether it is live (a dead one
/// deletes the fact from what is beneath), its author and subgraphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLink {
    /// An ephemeral id (see [`Graph::ephemeral_node`]), which no stored
    /// link has.
    pub id: u64,
    pub spo: [u64; 3],
    pub live: bool,
    pub author: u64,
    pub subgraphs: Vec<u64>,
}

/// Which field of a link an overlay's index is by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Subject,
    Predicate,
    Object,
    Subgraph,
    Author,
}

/// Links held in memory, layered over the database or over nothing: a
/// graph that queries read (see
/// [`Plan::execute_over`](super::Plan::execute_over)) as the links beneath
/// it but where it has the same fact, and its own live links.
///
/// An overlay has one link per fact: setting it again (live, or dead)
/// replaces it.  Its links, nodes and values not in the graph have
/// [ephemeral](Graph::ephemeral_value) ids, so they work in rows and
/// compare by id as stored ones do; its [hold](Self::ephemeral_hold) keeps
/// them while it lives.
///
/// - **History:** an overlay has none.  A query `as of` an id sees the
///   database as of then, and the overlay as it is.
/// - **Over nothing** ([`alone`](Self::alone)), the database's links aren't
///   seen at all: a graph of the overlay's own links.
/// - **Layering:** [`over`](Self::over) puts one overlay over another,
///   whose base it takes: its links replace those of the same facts.
/// - **Materializing:** [`insert_into`](Self::insert_into) writes the
///   overlay to the database.
#[derive(Debug, Clone)]
pub struct Overlay {
    base: bool,
    links: Vec<OverlayLink>,
    by_fact: HashMap<[u64; 3], usize>,
    by_id: HashMap<u64, usize>,
    index: HashMap<(Field, u64), Vec<usize>>,
    /// The hold of the ephemeral ids its links name, and the holds of the
    /// overlays it was layered from (see [`holds`](Self::holds)).
    hold: Option<EphemeralHold>,
    holds: Holds,
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    /// An overlay with no links over the database.
    pub fn new() -> Self {
        Overlay {
            base: true,
            links: Vec::new(),
            by_fact: HashMap::new(),
            by_id: HashMap::new(),
            index: HashMap::new(),
            hold: None,
            holds: Holds::default(),
        }
    }

    /// An overlay with no links over nothing: an empty graph.
    pub fn alone() -> Self {
        Overlay {
            base: false,
            ..Self::new()
        }
    }

    /// The hold of the ephemeral ids its links name, which they are held by
    /// while it lives: its links' ids, their facts' nodes not in the graph,
    /// and its blank nodes.
    pub fn ephemeral_hold(&mut self, g: &Graph) -> &EphemeralHold {
        self.hold.get_or_insert_with(|| g.ephemeral_hold())
    }

    /// The holds of the ephemeral ids it names: for a result reading it to
    /// keep.
    pub fn holds(&self) -> Holds {
        let mut out = self.holds.clone();
        if let Some(h) = &self.hold {
            out.push(h);
        }
        out
    }

    /// Whether the database's links are beneath.
    pub fn over_database(&self) -> bool {
        self.base
    }

    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// The links, live and dead, in the order their facts were first set.
    pub fn links(&self) -> impl Iterator<Item = &OverlayLink> {
        self.links.iter()
    }

    /// The link with id `id`.
    pub fn link(&self, id: u64) -> Option<&OverlayLink> {
        self.by_id.get(&id).map(|&i| &self.links[i])
    }

    /// The link of a fact, live or dead.
    pub fn fact(&self, spo: [u64; 3]) -> Option<&OverlayLink> {
        self.by_fact.get(&spo).map(|&i| &self.links[i])
    }

    /// Whether a stored link of `spo` is hidden: the overlay has its fact,
    /// or nothing is beneath.
    pub(crate) fn hides(&self, spo: [u64; 3]) -> bool {
        !self.base || self.by_fact.contains_key(&spo)
    }

    /// Sets the fact `spo`: live, by `author`, in `subgraphs` (added to
    /// those it is in, if it is live already), or dead.  Returns its link's
    /// id.
    pub fn set(
        &mut self,
        g: &Graph,
        spo: [u64; 3],
        live: bool,
        author: u64,
        subgraphs: &[u64],
    ) -> u64 {
        if let Some(&i) = self.by_fact.get(&spo) {
            if live {
                let h = self.hold.get_or_insert_with(|| g.ephemeral_hold());
                h.retain(author);
                subgraphs.iter().for_each(|&sg| h.retain(sg));
            }
            let was_live = self.links[i].live;
            let l = &mut self.links[i];
            l.live = live;
            if live {
                l.author = author;
                if !was_live {
                    l.subgraphs.clear();
                }
                for &sg in subgraphs {
                    if !l.subgraphs.contains(&sg) {
                        l.subgraphs.push(sg);
                    }
                }
            } else {
                l.subgraphs.clear();
            }
            let id = l.id;
            self.reindex(i);
            return id;
        }
        let h = self.hold.get_or_insert_with(|| g.ephemeral_hold());
        let id = h.node(g);
        for &e in spo.iter().chain(&[author]).chain(subgraphs) {
            h.retain(e);
        }
        let i = self.links.len();
        self.links.push(OverlayLink {
            id,
            spo,
            live,
            author: if live { author } else { 0 },
            subgraphs: if live { subgraphs.to_vec() } else { Vec::new() },
        });
        self.by_fact.insert(spo, i);
        self.by_id.insert(id, i);
        self.reindex(i);
        id
    }

    /// The live link of the fact `spo` beneath the overlay: its own, or
    /// the stored one, if it is over the database and hasn't the fact.
    pub fn live(&self, g: &Graph, spo: [u64; 3]) -> Option<u64> {
        if self.hides(spo) {
            return self.fact(spo).filter(|l| l.live).map(|l| l.id);
        }
        let [s, p, o] = spo;
        g.find_link(s, p, o)
    }

    /// Removes the live fact `spo` from subgraph `sg` beneath the overlay,
    /// leaving it live: returns whether it was in it.
    pub fn remove_from_subgraph(&mut self, g: &Graph, spo: [u64; 3], sg: u64) -> bool {
        let (author, mut subgraphs) = match self.fact(spo) {
            Some(l) if l.live => (l.author, l.subgraphs.clone()),
            Some(_) => return false,
            None if !self.base => return false,
            None => {
                let [s, p, o] = spo;
                let Some(id) = g.find_link(s, p, o) else {
                    return false;
                };
                let Some(f) = g.get(id).ok().and_then(|r| r.as_link().map(|l| l.fields())) else {
                    return false;
                };
                (f.author, f.subgraphs)
            }
        };
        let before = subgraphs.len();
        subgraphs.retain(|&x| x != sg);
        if subgraphs.len() == before {
            return false;
        }
        // Set again, with only the subgraphs left.
        self.set(g, spo, false, 0, &[]);
        self.set(g, spo, true, author, &subgraphs);
        true
    }

    /// Adds the live fact `spo`.
    pub fn insert(&mut self, g: &Graph, spo: [u64; 3]) -> u64 {
        self.set(g, spo, true, 0, &[])
    }

    /// Deletes the fact `spo`, from what is beneath too.
    pub fn delete(&mut self, g: &Graph, spo: [u64; 3]) -> u64 {
        self.set(g, spo, false, 0, &[])
    }

    /// Indexes link `i` by its fields: its subgraphs and author may have
    /// changed (an index entry may hold links no longer there, which
    /// lookups check).
    fn reindex(&mut self, i: usize) {
        let l = &self.links[i];
        let mut keys = vec![
            (Field::Subject, l.spo[0]),
            (Field::Predicate, l.spo[1]),
            (Field::Object, l.spo[2]),
        ];
        if l.author != 0 {
            keys.push((Field::Author, l.author));
        }
        keys.extend(l.subgraphs.iter().map(|&sg| (Field::Subgraph, sg)));
        for key in keys {
            let entry = self.index.entry(key).or_default();
            if !entry.contains(&i) {
                entry.push(i);
            }
        }
    }

    /// This overlay over `lower`: `lower`'s links, then this one's,
    /// replacing those of the same facts, over `lower`'s base.
    pub fn over(&self, lower: &Overlay) -> Overlay {
        let mut out = lower.clone();
        out.holds.extend(&self.holds());
        for l in &self.links {
            match out.by_fact.get(&l.spo) {
                Some(&i) => {
                    out.links[i] = l.clone();
                    out.by_id.insert(l.id, i);
                    out.reindex(i);
                }
                None => {
                    let i = out.links.len();
                    out.links.push(l.clone());
                    out.by_fact.insert(l.spo, i);
                    out.by_id.insert(l.id, i);
                    out.reindex(i);
                }
            }
        }
        out
    }

    /// The live links that may match a pattern with these fields known: a
    /// superset, which the caller checks.
    pub(crate) fn live_links(
        &self,
        [s, p, o]: [Option<u64>; 3],
        subgraph: Option<u64>,
        author: Option<u64>,
    ) -> Vec<Link> {
        let entry = |f: Field, x: u64| self.index.get(&(f, x)).map_or(&[][..], |v| &v[..]);
        let candidates: Vec<usize> = match (s, p, o, subgraph, author) {
            (Some(s), ..) => entry(Field::Subject, s).to_vec(),
            (_, _, Some(o), ..) => entry(Field::Object, o).to_vec(),
            (_, _, _, Some(sg), _) => entry(Field::Subgraph, sg).to_vec(),
            (_, _, _, _, Some(a)) => entry(Field::Author, a).to_vec(),
            (_, Some(p), ..) => entry(Field::Predicate, p).to_vec(),
            _ => (0..self.links.len()).collect(),
        };
        let mut out: Vec<Link> = candidates
            .into_iter()
            .map(|i| &self.links[i])
            .filter(|l| l.live)
            .filter(|l| {
                s.is_none_or(|s| l.spo[0] == s)
                    && p.is_none_or(|p| l.spo[1] == p)
                    && o.is_none_or(|o| l.spo[2] == o)
                    && subgraph.is_none_or(|sg| l.subgraphs.contains(&sg))
                    && author.is_none_or(|a| l.author == a)
            })
            .map(|l| (l.id, l.spo))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The predicates of the live links.
    pub(crate) fn predicates(&self) -> BTreeSet<u64> {
        self.links
            .iter()
            .filter(|l| l.live)
            .map(|l| l.spo[1])
            .collect()
    }

    /// An overlay of a result graph's links, over the database if `base`:
    /// its stored links' facts, and its derived links (but a path's, whose
    /// predicate is a path) with values and keys not in the graph as
    /// ephemeral ones, and a new node for each blank node.
    pub fn of_graph(g: &Graph, graph: &ResultGraph, base: bool) -> Overlay {
        let mut out = if base {
            Overlay::new()
        } else {
            Overlay::alone()
        };
        let mut blanks: HashMap<u32, u64> = HashMap::new();
        for l in &graph.links {
            match l {
                ResultLink::Stored(id) => {
                    if let Some(f) = g
                        .get(*id)
                        .ok()
                        .and_then(|r| r.as_link().map(|l| l.fields()))
                    {
                        let sgs = f.subgraphs.clone();
                        out.set(g, [f.subject, f.predicate, f.object], true, f.author, &sgs);
                    }
                }
                // A path's derived link is its links, which are here.
                ResultLink::Derived {
                    by: super::result::Derivation::Path,
                    ..
                } => {}
                ResultLink::Derived { fields, .. } => {
                    let mut ids = [0; 3];
                    let mut ok = true;
                    for (id, f) in ids.iter_mut().zip(fields) {
                        *id = match f {
                            ResultNode::Entry(e) => *e,
                            ResultNode::Value(Cell::Text(t)) => {
                                out.ephemeral_hold(g).value(g, t.as_bytes())
                            }
                            ResultNode::Value(c) => {
                                let d = c.data(g).unwrap_or_default();
                                out.ephemeral_hold(g).value(g, &d)
                            }
                            ResultNode::Key(k) => out.ephemeral_hold(g).key(g, k.as_bytes()),
                            ResultNode::Blank(n) => match blanks.get(n) {
                                Some(&id) => id,
                                None => {
                                    let id = out.ephemeral_hold(g).node(g);
                                    blanks.insert(*n, id);
                                    id
                                }
                            },
                            _ => {
                                ok = false;
                                0
                            }
                        };
                    }
                    if ok {
                        out.insert(g, ids);
                    }
                }
            }
        }
        out
    }

    /// Writes the overlay to the database: each live link, with its author
    /// (or `author`, if it has none) and subgraphs and `subgraph` too, if
    /// given, and each dead one's fact
    /// deleted, if it is over the database.  Ephemeral values and keys are
    /// made, and each ephemeral node becomes a new node.  Returns the number
    /// of links written.
    pub fn insert_into(&self, g: &mut Graph, author: u64, subgraph: Option<u64>) -> Result<usize> {
        let mut nodes: HashMap<u64, u64> = HashMap::new();
        let mut count = 0;
        for l in &self.links {
            if !l.live && !self.base {
                continue;
            }
            let mut ids = [0; 3];
            for (id, &e) in ids.iter_mut().zip(&l.spo) {
                *id = real(g, e, &mut nodes)?;
            }
            let [s, p, o] = ids;
            if !l.live {
                if g.find_link(s, p, o).is_some() {
                    g.delete_link(s, p, o)?;
                    count += 1;
                }
                continue;
            }
            let author = match l.author {
                0 => author,
                a => real(g, a, &mut nodes)?,
            };
            let mut subgraphs = Vec::new();
            for &sg in l.subgraphs.iter().chain(&subgraph) {
                subgraphs.push(real(g, sg, &mut nodes)?);
            }
            let before = g.find_link(s, p, o);
            let id = g.add_link(&crate::record::LinkFields {
                subject: s,
                predicate: p,
                object: o,
                author,
                when: crate::record::now(),
                subgraphs,
                ..Default::default()
            })?;
            count += usize::from(before != Some(id));
        }
        Ok(count)
    }
}

/// The stored entry for `id`: itself, or an ephemeral one's value, key or
/// new node.
fn real(g: &mut Graph, id: u64, nodes: &mut HashMap<u64, u64>) -> Result<u64> {
    if id & EPHEMERAL == 0 {
        return Ok(id);
    }
    if let Some(d) = g.data(id) {
        let d = d.into_owned();
        return g.value(&d);
    }
    if let Some(k) = g.key(id) {
        let k = k.into_owned();
        return g.node_by_key(&k);
    }
    if let Some(&n) = nodes.get(&id) {
        return Ok(n);
    }
    let n = g.node()?;
    nodes.insert(id, n);
    Ok(n)
}

/// An overlay shared by the operators reading it.
pub(crate) type Shared = Arc<Overlay>;

/// `base`, the stored live links that may match, layered under `overlay`:
/// those it doesn't hide, and its live links that may match, with these
/// fields known.
pub(crate) fn layered(
    overlay: Option<&Overlay>,
    mut base: Vec<Link>,
    known: [Option<u64>; 3],
    subgraph: Option<u64>,
    author: Option<u64>,
) -> Vec<Link> {
    let Some(ov) = overlay else {
        return base;
    };
    base.retain(|&(_, spo)| !ov.hides(spo));
    base.extend(ov.live_links(known, subgraph, author));
    base
}

/// The live links with predicate `p` from `x` (or to it, if not
/// `forward`), as of `as_of` beneath `overlay`.
pub(crate) fn step_links(
    g: &Graph,
    overlay: Option<&Overlay>,
    p: u64,
    x: u64,
    forward: bool,
    as_of: Option<u64>,
) -> Vec<Link> {
    use crate::tuple_index::{self, Intersection};
    let base = if overlay.is_none_or(|o| o.over_database()) {
        let links = if forward {
            tuple_index::find_links_by_intersection(g.local(), Intersection::SubjectPredicate, x, p)
        } else {
            tuple_index::find_links_by_intersection(g.local(), Intersection::PredicateObject, p, x)
        };
        super::reduce::live_links(g, &links, as_of)
    } else {
        Vec::new()
    };
    let known = if forward {
        [Some(x), Some(p), None]
    } else {
        [None, Some(p), Some(x)]
    };
    layered(overlay, base, known, None, None)
        .into_iter()
        .filter(|&(_, [s, lp, o])| lp == p && if forward { s == x } else { o == x })
        .collect()
}

/// The live links with predicate `p`, as of `as_of` beneath `overlay`.
pub(crate) fn predicate_links(
    g: &Graph,
    overlay: Option<&Overlay>,
    p: u64,
    as_of: Option<u64>,
) -> Vec<Link> {
    use crate::record::Linkage;
    let base = if overlay.is_none_or(|o| o.over_database()) {
        let links = crate::tuple_index::find_links(g.local(), Linkage::Predicate, p);
        super::reduce::live_links(g, &links, as_of)
    } else {
        Vec::new()
    };
    layered(overlay, base, [None, Some(p), None], None, None)
        .into_iter()
        .filter(|&(_, [_, lp, _])| lp == p)
        .collect()
}

/// The predicates of the live links, beneath `overlay` too.
pub(crate) fn predicates(g: &Graph, overlay: Option<&Overlay>) -> BTreeSet<u64> {
    let mut ps: BTreeSet<u64> = if overlay.is_none_or(|o| o.over_database()) {
        g.stats().predicates().map(|(p, _)| p).collect()
    } else {
        BTreeSet::new()
    };
    if let Some(o) = overlay {
        ps.extend(o.predicates());
    }
    ps
}
