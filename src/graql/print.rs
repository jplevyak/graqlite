//! Writing parsed GraQL back as text that parses to the same statement.
//!
//! The text is canonical: printing what it parses to gives it again.  The
//! parser groups patterns in more `and`s than the text shows, so a statement
//! and the parse of its text are the same once nested `and`s and `or`s are
//! flattened (and blank nodes renamed, see below).

use std::collections::{HashMap, HashSet};
use std::fmt;

use super::{
    AsOf, Comparison, GraphExpr, KEYWORDS, Node, Path, Pattern, RuleDef, SelectItem, SetValue,
    Statement, TestOperand,
};
use crate::query::Cmp;
use crate::times::format_time;

/// `text` as a GraQL value: bare if it is an identifier or a number, and
/// otherwise a quoted string.
pub fn literal(text: &str) -> String {
    if is_bare(text) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `text` as a quoted GraQL string.
pub(crate) fn literal_text(text: &str) -> String {
    if is_bare(text) {
        format!("\"{text}\"")
    } else {
        literal(text)
    }
}

/// Whether GraQL reads `text`, unquoted, as the value with that data.
fn is_bare(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_ascii_digit() {
        // A number: alphanumerics, then perhaps `.` and digits.
        return match text.split_once('.') {
            None => text.chars().all(|c| c.is_ascii_alphanumeric()),
            Some((int, frac)) => {
                int.chars().all(|c| c.is_ascii_alphanumeric())
                    && !frac.is_empty()
                    && frac.chars().all(|c| c.is_ascii_digit())
            }
        };
    }
    let lower = text.to_ascii_lowercase();
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        // `a`, as a predicate, is the value `type`.
        && lower != "a"
        // `true` and `false` are keywords for those values.
        && (!KEYWORDS.contains(&lower.as_str()) || text == "true" || text == "false")
}

/// Prints a statement's nodes and patterns, naming its blank nodes.
///
/// A blank node written `_:x` keeps its label.  One written `?` or `[ ... ]`
/// has no label, and gets one (`_:b1`, `_:b2`, ... skipping labels the
/// statement uses), since `?` would be a new blank node each time it is
/// printed.
struct Printer {
    names: HashMap<String, String>,
    used: HashSet<String>,
    next: usize,
}

impl Printer {
    fn new<'a>(patterns: impl IntoIterator<Item = &'a Pattern>) -> Self {
        let mut used = HashSet::new();
        let mut add = |n: &Node| {
            if let Node::Blank(b) = n
                && b.starts_with("_:")
            {
                used.insert(b.clone());
            }
        };
        for p in patterns {
            each_node(p, &mut add);
        }
        Printer {
            names: HashMap::new(),
            used,
            next: 0,
        }
    }

    fn node(&mut self, n: &Node) -> String {
        match n {
            Node::Var(v) => format!("?{v}"),
            Node::Blank(b) if b.starts_with("_:") => b.clone(),
            Node::Blank(b) => {
                if let Some(name) = self.names.get(b) {
                    return name.clone();
                }
                let name = loop {
                    self.next += 1;
                    let name = format!("_:b{}", self.next);
                    if !self.used.contains(&name) {
                        break name;
                    }
                };
                self.names.insert(b.clone(), name.clone());
                name
            }
            Node::Key(k) => k.clone(),
            Node::Value(v) => literal(v),
            Node::Id(id) => format!("#{id}"),
            Node::Param(p) => format!("${p}"),
            Node::Symbol(s) => s.clone(),
        }
    }

    /// A pattern, as after `where`: its `or`s unparenthesized.
    fn disjunction(&mut self, p: &Pattern) -> String {
        let mut branches = Vec::new();
        flatten_or(p, &mut branches);
        let parts: Vec<String> = branches.into_iter().map(|b| self.conjunction(b)).collect();
        parts.join(" or ")
    }

    /// A pattern within an `or`.  Runs of triples, paths, calls and
    /// comparisons are written as subjects separated by `.`, and are
    /// separated from the parenthesized `or`s and `not`s by `and`.
    fn conjunction(&mut self, p: &Pattern) -> String {
        let mut parts = Vec::new();
        flatten_and(p, &mut parts);
        // A node followed by its limit is written once: `?v limit n`.
        let limited = |a: &Pattern, b: Option<&&Pattern>| {
            matches!((a, b), (Pattern::Node(Node::Var(v) | Node::Blank(v)),
                Some(Pattern::LimitValues { var, .. })) if v == var)
        };
        let parts: Vec<&Pattern> = parts
            .iter()
            .enumerate()
            .filter(|&(i, p)| !limited(p, parts.get(i + 1)))
            .map(|(_, p)| *p)
            .collect();
        let mut segments: Vec<String> = Vec::new();
        let mut run: Vec<String> = Vec::new();
        for part in parts {
            match part {
                Pattern::Or(_) => {
                    segments.extend(take_run(&mut run));
                    segments.push(format!("( {} )", self.disjunction(part)));
                }
                Pattern::Not(inner) => {
                    segments.extend(take_run(&mut run));
                    segments.push(format!("not ( {} )", self.disjunction(inner)));
                }
                Pattern::Optional(inner) => {
                    segments.extend(take_run(&mut run));
                    segments.push(format!("optional ( {} )", self.disjunction(inner)));
                }
                Pattern::AsOf(inner, id) => {
                    segments.extend(take_run(&mut run));
                    segments.push(format!("( {} ) as of {id}", self.disjunction(inner)));
                }
                Pattern::InGraph(inner, g) => {
                    segments.extend(take_run(&mut run));
                    let inner = self.disjunction(inner);
                    segments.push(format!("( {inner} ) in {}", self.node(g)));
                }
                _ => run.push(self.subject(part)),
            }
        }
        segments.extend(take_run(&mut run));
        segments.join(" and ")
    }

    /// A triple, path, call or comparison.
    fn subject(&mut self, p: &Pattern) -> String {
        match p {
            Pattern::Node(n) => self.node(n),
            Pattern::InGraph(inner, g) => {
                let inner = self.disjunction(inner);
                format!("( {inner} ) in {}", self.node(g))
            }
            Pattern::Triple(s, pr, o) => {
                format!("{} {} {}", self.node(s), self.node(pr), self.node(o))
            }
            Pattern::Tagged(s, pr, o, author, graph) => {
                let mut text = format!("{} {} {}", self.node(s), self.node(pr), self.node(o));
                match (author, graph) {
                    (Some(a), _) => text = format!("{text} {}", self.node(a)),
                    // A subgraph alone: any author.
                    (None, Some(_)) => text.push_str(" ?"),
                    (None, None) => {}
                }
                if let Some(g) = graph {
                    text = format!("{text} {}", self.node(g));
                }
                text
            }
            Pattern::Path(s, pr, o) => {
                format!("{} {}++ {}", self.node(s), self.node(pr), self.node(o))
            }
            Pattern::PathExpr(s, path, o) => {
                format!("{} ({}) {}", self.node(s), path, self.node(o))
            }
            Pattern::Link(l, s, pr, o) => format!(
                "<< :{} {} {} {} >>",
                self.node(l),
                self.node(s),
                self.node(pr),
                self.node(o)
            ),
            Pattern::Call(name, args) => {
                let args: Vec<String> = args.iter().map(|a| self.node(a)).collect();
                format!("{name}({})", args.join(" "))
            }
            Pattern::Compare(a, cmp, b) => {
                let a = a.show(&mut |n| self.node(n), 0);
                let b = b.show(&mut |n| self.node(n), 0);
                format!("{a} {} {b}", comparison(*cmp))
            }
            Pattern::Test(e) => e.show(&mut |n| self.node(n), 0),
            Pattern::Let(v, e) => format!("let ?{v} = {}", e.show(&mut |n| self.node(n), 0)),
            Pattern::LinkAggregate {
                name,
                distinct,
                triple,
                descending,
                scope,
            } => {
                let (s, pr, o) = &**triple;
                let distinct = if *distinct { "distinct " } else { "" };
                let desc = if *descending { " desc" } else { "" };
                let (s, pr, o) = (self.node(s), self.node(pr), self.node(o));
                let mut text = format!("{name}({distinct}{s} {pr} {o}{desc})");
                if let Some(v) = scope {
                    text.push_str(&format!(" @?{v}"));
                }
                text
            }
            Pattern::LimitValues { var, limit, offset } => {
                let mut text = if var.starts_with('[') || var.starts_with("_:") {
                    self.node(&Node::Blank(var.clone()))
                } else {
                    format!("?{var}")
                };
                if *limit != usize::MAX {
                    text.push_str(&format!(" limit {limit}"));
                }
                if *offset > 0 {
                    text.push_str(&format!(" offset {offset}"));
                }
                text
            }
            Pattern::Assert(triples, scope) => {
                let Some((Pattern::Triple(s, pr, o), inner)) = triples.split_first() else {
                    unreachable!("an asserted triple, then its object's")
                };
                let (s, pr) = (self.node(s), self.node(pr));
                let o = self.asserted_object(o, inner);
                match scope {
                    Some(v) => format!("{s} {pr} {o}! @?{v}"),
                    None => format!("{s} {pr} {o}!"),
                }
            }
            Pattern::And(_)
            | Pattern::Or(_)
            | Pattern::Not(_)
            | Pattern::AsOf(..)
            | Pattern::Optional(_) => {
                unreachable!("flattened by conjunction")
            }
        }
    }

    /// An asserted triple's object: a blank node with triples of its own in
    /// `inner` as `[ p o ; … ]`, as it was written.
    fn asserted_object(&mut self, o: &Node, inner: &[Pattern]) -> String {
        if let Node::Blank(b) = o
            && b.starts_with('[')
        {
            let own: Vec<(&Node, &Node)> = inner
                .iter()
                .filter_map(|t| match t {
                    Pattern::Triple(s, p, o2) if s == o => Some((p, o2)),
                    _ => None,
                })
                .collect();
            if !own.is_empty() {
                let parts: Vec<String> = own
                    .into_iter()
                    .map(|(p, o2)| format!("{} {}", self.node(p), self.asserted_object(o2, inner)))
                    .collect();
                return format!("[ {} ]", parts.join(" ; "));
            }
        }
        self.node(o)
    }

    fn template(&mut self, template: &[Pattern]) -> String {
        let triples: Vec<String> = template.iter().map(|t| self.subject(t)).collect();
        triples.join(" . ")
    }
}

fn take_run(run: &mut Vec<String>) -> Option<String> {
    if run.is_empty() {
        None
    } else {
        Some(std::mem::take(run).join(" . "))
    }
}

fn comparison(c: Comparison) -> &'static str {
    match c {
        Comparison::Eq => "=",
        Comparison::Ne => "!=",
        Comparison::Data(Cmp::Lt) => "<",
        Comparison::Data(Cmp::Le) => "<=",
        Comparison::Data(Cmp::Gt) => ">",
        Comparison::Data(Cmp::Ge) => ">=",
    }
}

/// The branches of nested `or`s.
fn flatten_or<'p>(p: &'p Pattern, out: &mut Vec<&'p Pattern>) {
    match p {
        Pattern::Or(ps) => ps.iter().for_each(|p| flatten_or(p, out)),
        // An `and` of one `or` is that `or`.
        Pattern::And(ps) if ps.len() == 1 => flatten_or(&ps[0], out),
        _ => out.push(p),
    }
}

/// The parts of nested `and`s.
pub(super) fn flatten_and<'p>(p: &'p Pattern, out: &mut Vec<&'p Pattern>) {
    match p {
        Pattern::And(ps) => ps.iter().for_each(|p| flatten_and(p, out)),
        Pattern::Or(ps) if ps.len() == 1 => flatten_and(&ps[0], out),
        _ => out.push(p),
    }
}

/// Calls `f` with each node of `p`, to change it.
pub(super) fn each_node_mut(p: &mut Pattern, f: &mut impl FnMut(&mut Node)) {
    match p {
        Pattern::Node(n) => f(n),
        Pattern::Tagged(s, pr, o, author, graph) => {
            f(s);
            f(pr);
            f(o);
            author.iter_mut().for_each(&mut *f);
            graph.iter_mut().for_each(f);
        }
        Pattern::InGraph(p, g) => {
            f(g);
            each_node_mut(p, f);
        }
        Pattern::Triple(s, pr, o) | Pattern::Path(s, pr, o) => {
            f(s);
            f(pr);
            f(o);
        }
        Pattern::PathExpr(s, _, o) => {
            f(s);
            f(o);
        }
        Pattern::Link(l, s, pr, o) => {
            f(l);
            f(s);
            f(pr);
            f(o);
        }
        Pattern::Call(_, args) => args.iter_mut().for_each(f),
        Pattern::Compare(a, _, b) => {
            a.each_node_mut(f);
            b.each_node_mut(f);
        }
        Pattern::Test(e) | Pattern::Let(_, e) => e.each_node_mut(f),
        Pattern::LimitValues { .. } => {}
        Pattern::LinkAggregate { triple, .. } => {
            let (s, pr, o) = &mut **triple;
            f(s);
            f(pr);
            f(o);
        }
        Pattern::And(ps) | Pattern::Or(ps) | Pattern::Assert(ps, _) => {
            ps.iter_mut().for_each(|p| each_node_mut(p, f))
        }
        Pattern::Not(p) | Pattern::AsOf(p, _) | Pattern::Optional(p) => each_node_mut(p, f),
    }
}

/// Calls `f` with each node of `p`.
pub(super) fn each_node(p: &Pattern, f: &mut impl FnMut(&Node)) {
    match p {
        Pattern::Node(n) => f(n),
        Pattern::Tagged(s, pr, o, author, graph) => {
            f(s);
            f(pr);
            f(o);
            author.iter().for_each(&mut *f);
            graph.iter().for_each(f);
        }
        Pattern::InGraph(p, g) => {
            f(g);
            each_node(p, f);
        }
        Pattern::Triple(s, pr, o) | Pattern::Path(s, pr, o) => {
            f(s);
            f(pr);
            f(o);
        }
        Pattern::Link(l, s, pr, o) => {
            f(l);
            f(s);
            f(pr);
            f(o);
        }
        Pattern::PathExpr(s, _, o) => {
            f(s);
            f(o);
        }
        Pattern::Call(_, args) => args.iter().for_each(f),
        Pattern::Compare(a, _, b) => {
            a.each_node(f);
            b.each_node(f);
        }
        Pattern::Test(e) | Pattern::Let(_, e) => e.each_node(f),
        Pattern::LimitValues { .. } => {}
        Pattern::LinkAggregate { triple, .. } => {
            let (s, pr, o) = &**triple;
            f(s);
            f(pr);
            f(o);
        }
        Pattern::And(ps) | Pattern::Or(ps) | Pattern::Assert(ps, _) => {
            ps.iter().for_each(|p| each_node(p, f))
        }
        Pattern::Not(p) | Pattern::AsOf(p, _) | Pattern::Optional(p) => each_node(p, f),
    }
}

/// A column of a `select`, as written.
fn select_item(item: &SelectItem) -> String {
    match item {
        SelectItem::Var(v) => format!("?{v}"),
        SelectItem::Expr { expr, name: None } => expr.to_string(),
        SelectItem::Expr {
            expr,
            name: Some(n),
        } => format!("{expr} as ?{n}"),
        SelectItem::Aggregate { name: None, .. } => item.name(),
        SelectItem::Aggregate {
            function,
            distinct,
            arg,
            name: Some(n),
        } => {
            let unnamed = SelectItem::Aggregate {
                function: *function,
                distinct: *distinct,
                arg: arg.clone(),
                name: None,
            };
            format!("{} as ?{n}", unnamed.name())
        }
    }
}

impl Path {
    /// `self`, parenthesized if its precedence is below `outer` (0:
    /// alternatives, 1: sequences, 2: postfix and inverse).
    fn show(&self, outer: u8) -> String {
        let (text, precedence) = match self {
            Path::Pred(p) => (literal(p), 3),
            Path::Key(k) => (k.clone(), 3),
            Path::Id(id) => (format!("#{id}"), 3),
            Path::Inverse(p) => (format!("^{}", p.show(3)), 2),
            Path::Seq(ps) => {
                let parts: Vec<String> = ps.iter().map(|p| p.show(2)).collect();
                (parts.join(" / "), 1)
            }
            Path::Alt(ps) => {
                let parts: Vec<String> = ps.iter().map(|p| p.show(1)).collect();
                (parts.join(" | "), 0)
            }
            Path::Repeat(p, min, max) => {
                let suffix = match (min, max) {
                    (0, None) => "*".to_string(),
                    (1, None) => "+".to_string(),
                    (m, None) => format!("{{{m},}}"),
                    (m, Some(n)) if m == n => format!("{{{m}}}"),
                    (m, Some(n)) => format!("{{{m},{n}}}"),
                };
                (format!("{}{suffix}", p.show(3)), 2)
            }
        };
        if precedence < outer {
            format!("({text})")
        } else {
            text
        }
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.show(0))
    }
}

impl fmt::Display for AsOf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AsOf::Id(id) => write!(f, "{id}"),
            AsOf::Time(t) => write!(f, "\"{}\"", format_time(*t)),
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&Printer::new([self]).disjunction(self))
    }
}

impl fmt::Display for RuleDef {
    /// The rule as a `rule` statement.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut p = Printer::new([&self.body]);
        let head: Vec<String> = self.head.iter().map(|v| format!("?{v}")).collect();
        write!(
            f,
            "rule {}({}) {}",
            self.name,
            head.join(" "),
            p.disjunction(&self.body)
        )
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_statement(f, self, None)
    }
}

impl fmt::Display for GraphExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphExpr::Name(n) => f.write_str(n),
            GraphExpr::Literal(ps) if ps.is_empty() => f.write_str("{}"),
            GraphExpr::Literal(ps) => {
                let mut printer = Printer::new(ps.iter());
                let parts: Vec<String> = ps
                    .iter()
                    .map(|p| match p {
                        Pattern::Not(t) => format!("not {}", printer.subject(t)),
                        p => printer.subject(p),
                    })
                    .collect();
                write!(f, "{{ {} }}", parts.join(" . "))
            }
            GraphExpr::Over(base, e) => write!(f, "{base}:{e}"),
            GraphExpr::Lookup(s) => write!(f, "( {s} )"),
        }
    }
}

/// `s`, reading `source` if given (`from graph G`, `in graph G`).
fn write_statement(
    f: &mut fmt::Formatter<'_>,
    s: &Statement,
    source: Option<&GraphExpr>,
) -> fmt::Result {
    let from = |kw: &str| match source {
        Some(e) => format!(" {kw} graph {e}"),
        None => String::new(),
    };
    {
        match s {
            Statement::From { source, statement } => write_statement(f, statement, Some(source)),
            Statement::Set { name, value } => match value {
                SetValue::Node(n) => write!(f, "set {name} = {}", Printer::new([]).node(n)),
                SetValue::Graph(e) => write!(f, "set {name} = graph {e}"),
            },
            Statement::Test {
                description,
                operands,
            } => {
                f.write_str("test")?;
                if let Some(d) = description {
                    write!(f, " {}", literal_text(d))?;
                }
                for o in operands.iter() {
                    match o {
                        TestOperand::Statement(s) => write!(f, " {s}")?,
                        TestOperand::Graph(e) => write!(f, " graph {e}")?,
                    }
                }
                Ok(())
            }
            Statement::Function { name, params, body } => {
                write!(f, "function {name}({}) {body}", params.join(", "))
            }
            Statement::Data(e) => write!(f, "data {e}"),
            Statement::Default(e) => write!(f, "default {e}"),
            Statement::Assume { template, graph } => {
                let mut printer = Printer::new(template.iter());
                write!(f, "assume {}", printer.template(template))?;
                match graph {
                    Some(g) => write!(f, " in {}", printer.node(g)),
                    None => Ok(()),
                }
            }
            Statement::Rule(rule) => write!(f, "{rule}"),
            Statement::Select {
                distinct,
                vars: selected,
                having,
                by,
                pattern,
                limit,
                offset,
                as_of,
            } => {
                f.write_str("select ")?;
                if *distinct {
                    f.write_str("distinct ")?;
                }
                match selected {
                    Some(items) => {
                        let items: Vec<String> = items.iter().map(select_item).collect();
                        f.write_str(&items.join(", "))?
                    }
                    None => f.write_str("*")?,
                }
                if !having.is_empty() {
                    let mut p = Printer::new([]);
                    let conditions: Vec<String> = having
                        .iter()
                        .map(|(a, c, b)| {
                            let a = a.show(&mut |n| p.node(n), 0);
                            let b = b.show(&mut |n| p.node(n), 0);
                            format!("{a} {} {b}", comparison(*c))
                        })
                        .collect();
                    write!(f, " having {}", conditions.join(" and "))?;
                }
                if !by.is_empty() {
                    let keys: Vec<String> = by
                        .iter()
                        .map(|k| {
                            if k.descending {
                                format!("?{} desc", k.var)
                            } else {
                                format!("?{}", k.var)
                            }
                        })
                        .collect();
                    write!(f, " by {}", keys.join(", "))?;
                }
                if let Some(n) = limit {
                    write!(f, " limit {n}")?;
                }
                if let Some(n) = offset {
                    write!(f, " offset {n}")?;
                }
                if let Some(id) = as_of {
                    write!(f, " as of {id}")?;
                }
                // A nested limit ending the pattern would be read back as
                // the statement's: in parentheses, it isn't.
                if pattern.ends_with_limit() {
                    write!(f, "{} where ( {pattern} )", from("from"))
                } else {
                    write!(f, "{} where {pattern}", from("from"))
                }
            }
            Statement::Verify { pattern, as_of } => match as_of {
                Some(id) => write!(f, "verify as of {id} {pattern}{}", from("in")),
                None => write!(f, "verify {pattern}{}", from("in")),
            },
            Statement::Lookup {
                pattern,
                where_,
                as_of,
            } => {
                match as_of {
                    Some(id) => write!(f, "lookup as of {id} {pattern}")?,
                    None => write!(f, "lookup {pattern}")?,
                }
                f.write_str(&from("in"))?;
                match where_ {
                    Some(w) => write!(f, " where {w}"),
                    None => Ok(()),
                }
            }
            Statement::Insert {
                template,
                pattern,
                graph,
            }
            | Statement::Delete {
                template,
                pattern,
                graph,
            } => {
                let mut printer = Printer::new(template.iter().chain(pattern));
                let (verb, to) = if matches!(s, Statement::Insert { .. }) {
                    ("insert", "into")
                } else {
                    ("delete", "from")
                };
                write!(f, "{verb} {}", printer.template(template))?;
                if let Some(g) = graph {
                    write!(f, " {to} {}", printer.node(g))?;
                }
                if let Some(p) = pattern {
                    write!(f, " where {}", printer.disjunction(p))?;
                }
                Ok(())
            }
            Statement::InsertGraph { source, graph } => {
                write!(f, "insert graph {source}")?;
                match graph {
                    Some(g) => write!(f, " into {}", Printer::new([]).node(g)),
                    None => Ok(()),
                }
            }
            Statement::Maintain {
                template,
                pattern,
                graph,
            } => {
                let mut printer = Printer::new(template.iter().chain([pattern]));
                write!(
                    f,
                    "maintain {} into {} where {}",
                    printer.template(template),
                    printer.node(graph),
                    printer.disjunction(pattern)
                )
            }
            Statement::Author(a) => write!(f, "author {}", Printer::new([]).node(a)),
            Statement::Subgraph(g) => write!(f, "subgraph {}", Printer::new([]).node(g)),
            Statement::DeleteRule(name) => write!(f, "delete rule {name}"),
            Statement::IndexVector {
                predicate,
                dims,
                metric,
                m,
                ef,
            } => write!(
                f,
                "index vector on {} dims {dims} metric {} m {m} ef {ef}",
                Printer::new([]).node(predicate),
                metric.name()
            ),
            Statement::DeleteIndex(predicate) => {
                write!(
                    f,
                    "delete index vector on {}",
                    Printer::new([]).node(predicate)
                )
            }
        }
    }
}
