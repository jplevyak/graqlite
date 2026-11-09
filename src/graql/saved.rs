//! Rules saved in the graph.
//!
//! Each definition of a rule is a link from the node with key [`RULES_KEY`]
//! by [`RULE_PREDICATE`] to the value of its canonical text (see
//! [`RuleDef`]'s `Display`).  Its link has reference links:
//! [`USES_PREDICATE`] to each value it names and to a key reference (see
//! [`key_reference`]) for each key it names, and [`CALLS_PREDICATE`] to the
//! name of each rule it calls.  The text is what is defined; the reference
//! links are derived from it, to find the rules that use something, and
//! [`rebuild_references`] makes them match it again.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::print::each_node;
use super::{Error, Node, Path, Pattern, Result, RuleDef, Statement, lower, parse};
use crate::Linkage;
use crate::graph::Graph;

/// The key of the node that saved rules are linked from.
pub const RULES_KEY: &[u8] = b"/graqlite/rules";
/// The predicate linking [`RULES_KEY`] to the text of each saved rule.
pub const RULE_PREDICATE: &[u8] = b"graqlite:rule";
/// The predicate linking a rule's link to each value or key reference its
/// text names.
pub const USES_PREDICATE: &[u8] = b"graqlite:uses";
/// The predicate linking a rule's link to the name of each rule it calls.
pub const CALLS_PREDICATE: &[u8] = b"graqlite:calls";
/// The predicate linking a key reference to the key it refers to.
pub const REFERS_TO_KEY_PREDICATE: &[u8] = b"graqlite:refers_to_key";

/// A saved rule whose text does not define a rule, and is left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRule {
    /// The link saving it.
    pub link: u64,
    pub text: String,
    /// Why it is left out.
    pub error: String,
}

/// The rules saved in a graph, as of a set of rule links.
#[derive(Debug, Clone, Default)]
pub(super) struct Saved {
    /// The live rule links, in order, and the text each saves.
    links: Vec<(u64, u64)>,
    /// The rule each link that defines one defines.
    pub(super) rules: Vec<(u64, RuleDef)>,
    pub(super) defs: Arc<[RuleDef]>,
    pub(super) skipped: Vec<SkippedRule>,
}

impl Saved {
    /// The rules saved in `g`, reusing `old` if they have not changed.
    pub(super) fn load(g: &Graph, old: Option<Saved>) -> Result<Saved> {
        let links = rule_links(g)?;
        if let Some(old) = old
            && old.links == links
        {
            return Ok(old);
        }
        let mut saved = Saved {
            links,
            ..Saved::default()
        };
        for &(link, text_id) in &saved.links {
            let text = g
                .data(text_id)
                .map(|d| String::from_utf8_lossy(&d).into_owned())
                .unwrap_or_default();
            match parse_rule(&text) {
                Ok(rule) => saved.rules.push((link, rule)),
                Err(error) => saved.skipped.push(SkippedRule { link, text, error }),
            }
        }
        saved.defs = saved.rules.iter().map(|(_, r)| r.clone()).collect();
        Ok(saved)
    }
}

/// The rule saved as `text`, or why it is not one.
fn parse_rule(text: &str) -> std::result::Result<RuleDef, String> {
    match parse(text) {
        Ok(mut statements) if statements.len() == 1 => match statements.pop() {
            Some(Statement::Rule(rule)) => Ok(rule),
            _ => Err("it is not a rule".into()),
        },
        Ok(_) => Err("it is not one statement".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// The live links saving rules, in the order they were saved, and the
/// value of the text each saves.
fn rule_links(g: &Graph) -> Result<Vec<(u64, u64)>> {
    let (Some(node), Some(predicate)) = (g.find_by_key(RULES_KEY), g.find_value(RULE_PREDICATE))
    else {
        return Ok(Vec::new());
    };
    let (links, _) = g.query_property([node], Linkage::Subject, predicate, None)?;
    Ok(links
        .into_iter()
        .filter_map(|l| Some((l, g.get(l).ok()?.as_link()?.object())))
        .collect())
}

/// Saves `rule`, unless it is saved already, given the rules saved before.
pub(super) fn save(g: &mut Graph, saved: &[RuleDef], rule: &RuleDef) -> Result<()> {
    let text = rule.to_string();
    if let (Some(node), Some(predicate), Some(value)) = (
        g.find_by_key(RULES_KEY),
        g.find_value(RULE_PREDICATE),
        g.find_value(text.as_bytes()),
    ) && g.find_link(node, predicate, value).is_some()
    {
        return Ok(());
    }
    check(g, saved, rule)?;
    let node = g.node_by_key(RULES_KEY)?;
    let predicate = g.value(RULE_PREDICATE)?;
    let value = g.value(text.as_bytes())?;
    let link = g.link(node, predicate, value)?;
    for (predicate, object) in references(g, rule, true)? {
        g.link(link, predicate, object)?;
    }
    Ok(())
}

/// Checks that a query calling `rule` would be valid (see
/// [`Query::validate`](crate::query::Query::validate)), if the rules it
/// calls are all defined.  A rule may call rules defined after it, which can
/// only be checked once they are.
fn check(g: &Graph, saved: &[RuleDef], rule: &RuleDef) -> Result<()> {
    // A template rule's triples may use only its head's variables (and
    // blank nodes, new for each call).
    if rule.is_template() {
        let mut unbound = None;
        each_node(&rule.body, &mut |n| {
            if let Node::Var(v) = n
                && !rule.head.contains(v)
            {
                unbound.get_or_insert(v.clone());
            }
        });
        return match unbound {
            Some(v) => Err(Error::Invalid(format!(
                "?{v} in {}'s triples isn't in its head",
                rule.name
            ))),
            None => Ok(()),
        };
    }
    let mut rules = saved.to_vec();
    rules.push(rule.clone());
    let call = Pattern::Call(
        rule.name.clone(),
        rule.head.iter().map(|v| Node::Var(v.clone())).collect(),
    );
    let defined: BTreeSet<&str> = rules.iter().map(|r| r.name.as_str()).collect();
    if super::reachable(&rules, &call)
        .iter()
        .all(|name| defined.contains(name.as_str()))
    {
        let (query, _) = lower(g, &rules, &call)?;
        query.validate()?;
    }
    Ok(())
}

/// Deletes the saved definitions of the rule `name`, with their reference
/// links, returning how many there were.  Saved text that does not parse
/// counts as a definition of `name` if it starts `rule name(`.
pub(super) fn delete(g: &mut Graph, saved: &Saved, name: &str) -> Result<usize> {
    let prefix = format!("rule {name}(");
    let links: Vec<u64> = saved
        .rules
        .iter()
        .filter(|(_, r)| r.name == name)
        .map(|&(l, _)| l)
        .chain(
            saved
                .skipped
                .iter()
                .filter(|s| s.text.starts_with(&prefix))
                .map(|s| s.link),
        )
        .collect();
    if links.is_empty() {
        return Err(Error::Invalid(format!("there is no rule '{name}'")));
    }
    for &link in &links {
        for (predicate, object) in reference_links(g, link)? {
            g.delete_link(link, predicate, object)?;
        }
        let (s, p, o) = {
            let record = g.get(link)?;
            let l = record.as_link().expect("a rule link");
            (l.subject(), l.predicate(), l.object())
        };
        g.delete_link(s, p, o)?;
    }
    Ok(links.len())
}

/// The live reference links of the rule link `link`, as their predicates
/// and objects.
fn reference_links(g: &Graph, link: u64) -> Result<BTreeSet<(u64, u64)>> {
    let mut out = BTreeSet::new();
    for name in [USES_PREDICATE, CALLS_PREDICATE] {
        let Some(predicate) = g.find_value(name) else {
            continue;
        };
        let (_, objects) = g.query_property([link], Linkage::Subject, predicate, None)?;
        out.extend(objects.into_iter().map(|o| (predicate, o)));
    }
    Ok(out)
}

/// The reference links `rule`'s link should have, as their predicates and
/// objects, appending the values and key references they need if `create`
/// (or leaving out those not in the graph, if not).
fn references(g: &mut Graph, rule: &RuleDef, create: bool) -> Result<BTreeSet<(u64, u64)>> {
    let mut values = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut ids = BTreeSet::new();
    each_node(&rule.body, &mut |n| match n {
        Node::Value(v) | Node::Symbol(v) => {
            values.insert(v.clone());
        }
        Node::Key(k) => {
            keys.insert(k.clone());
        }
        Node::Id(id) => {
            ids.insert(*id);
        }
        Node::Var(_) | Node::Blank(_) | Node::Param(_) => {}
    });
    path_steps(&rule.body, &mut values, &mut keys, &mut ids);
    let mut called = Vec::new();
    super::calls(&rule.body, &mut called);
    let mut out = BTreeSet::new();
    let id = |g: &mut Graph, data: &[u8]| -> Result<Option<u64>> {
        Ok(if create {
            Some(g.value(data)?)
        } else {
            g.find_value(data)
        })
    };
    let (Some(uses), Some(calls)) = (id(g, USES_PREDICATE)?, id(g, CALLS_PREDICATE)?) else {
        return Ok(out);
    };
    for v in &values {
        if let Some(v) = id(g, v.as_bytes())? {
            out.insert((uses, v));
        }
    }
    for k in &keys {
        let reference = if create {
            Some(key_reference_or_new(g, k.as_bytes())?)
        } else {
            key_reference(g, k.as_bytes())
        };
        if let Some(r) = reference {
            out.insert((uses, r));
        }
    }
    // An entry named by id is used itself.
    for id in ids {
        out.insert((uses, id));
    }
    for name in called {
        if let Some(n) = id(g, name.as_bytes())? {
            out.insert((calls, n));
        }
    }
    Ok(out)
}

/// Adds to `values`, `keys` and `ids` the steps of the path expressions of
/// `p`.
fn path_steps(
    p: &Pattern,
    values: &mut BTreeSet<String>,
    keys: &mut BTreeSet<String>,
    ids: &mut BTreeSet<u64>,
) {
    match p {
        Pattern::PathExpr(_, path, _) => {
            let mut steps = Vec::new();
            path.steps(&mut steps);
            for step in steps {
                match step {
                    Path::Pred(v) => {
                        values.insert(v.clone());
                    }
                    Path::Key(k) => {
                        keys.insert(k.clone());
                    }
                    Path::Id(id) => {
                        ids.insert(*id);
                    }
                    _ => {}
                }
            }
        }
        Pattern::And(ps) | Pattern::Or(ps) => {
            ps.iter().for_each(|p| path_steps(p, values, keys, ids))
        }
        Pattern::Not(p) | Pattern::Window(p, _) | Pattern::Optional(p) => {
            path_steps(p, values, keys, ids)
        }
        _ => {}
    }
}

/// The key reference for `key`: the node linked to the value `key` by
/// [`REFERS_TO_KEY_PREDICATE`], standing for whatever entity has that key
/// when it is used.  It is not the entity itself, which is linked to `key`
/// by [`KEY_PREDICATE`](crate::graph::KEY_PREDICATE), and it exists whether
/// or not such an entity does.
pub fn key_reference(g: &Graph, key: &[u8]) -> Option<u64> {
    let predicate = g.find_value(REFERS_TO_KEY_PREDICATE)?;
    let value = g.find_value(key)?;
    let (_, subjects) = g
        .query_property([value], Linkage::Object, predicate, None)
        .ok()?;
    subjects.first().copied()
}

/// The key reference for `key`, appending one if there is none.
pub fn key_reference_or_new(g: &mut Graph, key: &[u8]) -> Result<u64> {
    if let Some(r) = key_reference(g, key) {
        return Ok(r);
    }
    let node = g.node()?;
    let predicate = g.value(REFERS_TO_KEY_PREDICATE)?;
    let value = g.value(key)?;
    g.link(node, predicate, value)?;
    Ok(node)
}

/// Makes the reference links of each saved rule match its text, returning
/// how many links were added or deleted.
pub fn rebuild_references(g: &mut Graph) -> Result<usize> {
    let saved = Saved::load(g, None)?;
    let mut changed = 0;
    for (link, rule) in &saved.rules {
        let want = references(g, rule, true)?;
        let have = reference_links(g, *link)?;
        for &(p, o) in have.difference(&want) {
            g.delete_link(*link, p, o)?;
            changed += 1;
        }
        for &(p, o) in want.difference(&have) {
            g.link(*link, p, o)?;
            changed += 1;
        }
    }
    // Text that is not a rule has no references.
    for skipped in &saved.skipped {
        for (p, o) in reference_links(g, skipped.link)? {
            g.delete_link(skipped.link, p, o)?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// The saved rules whose text names the value or key `name` (a key starts
/// with `/`), or calls the rule `name`, as their links.
pub fn rules_using(g: &Graph, name: &[u8]) -> Result<Vec<u64>> {
    let mut out = BTreeSet::new();
    let targets = [
        (USES_PREDICATE, g.find_value(name)),
        (USES_PREDICATE, key_reference(g, name)),
        (CALLS_PREDICATE, g.find_value(name)),
    ];
    for (predicate, target) in targets {
        let (Some(predicate), Some(target)) = (g.find_value(predicate), target) else {
            continue;
        };
        let (_, subjects) = g.query_property([target], Linkage::Object, predicate, None)?;
        out.extend(subjects);
    }
    Ok(out.into_iter().collect())
}
