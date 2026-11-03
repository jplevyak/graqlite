//! Expressions: values computed from the variables of a row, for
//! [`compute`](super::Query::compute)d columns, [aggregate](super::Query::aggregate)
//! arguments, [`having`](super::Query::having) and
//! [`Filter::Holds`](super::Filter::Holds).
//!
//! Values in the graph are untyped bytes; computation is typed.  An
//! expression's value is a [`Cell`]: an entry, a number, text, a boolean,
//! or unbound.  Entries and text convert as an operation needs: an entry is
//! its data (or its key) as text, and text that is a number is a number.  An
//! operation that can't be computed (a word in arithmetic, a missing
//! argument, division by zero) is unbound, which a comparison never holds
//! for, as in SPARQL.
//!
//! ```
//! use graqlite::graph::{Graph, Options};
//! use graqlite::query::expr::Expr;
//! use graqlite::query::{Filter, Query};
//!
//! let mut g = Graph::open(None, Options::default()).unwrap();
//! let [price, qty] = [&b"price"[..], b"qty"].map(|d| g.value(d).unwrap());
//! let item = g.node().unwrap();
//! let (p, n) = (g.value(b"2.5").unwrap(), g.value(b"4").unwrap());
//! g.link(item, price, p).unwrap();
//! g.link(item, qty, n).unwrap();
//!
//! let mut q = Query::new();
//! let (i, p, n) = (q.var("i"), q.var("p"), q.var("n"));
//! q.pattern(i, price, p).pattern(i, qty, n);
//! let total = Expr::from(p) * Expr::from(n);
//! q.filter(Filter::Holds(total.clone().gt(5)));
//! let t = q.compute("total", total);
//! q.select([i, t]);
//! let rows = q.cells(&g).unwrap();
//! assert_eq!(rows[0][1].number(&g), Some(10.0));
//! ```

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ops;

use super::aggregate::{Cell, Number, compare_cells};
use super::{Cmp, Var, compare_data};
use crate::graph::Graph;
use crate::times::{format_time, parse_time};

/// An expression over the variables of a row.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A variable's value: its entry, or, for a variable
    /// [computed](super::Query::compute) or
    /// [aggregated](super::Query::aggregate), its value.
    Var(Var),
    /// A constant: an entry (`Cell::Id`), a number, text or a boolean.
    Const(Cell),
    Call(Func, Vec<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    /// Whether a comparison holds: a boolean.
    Compare(Box<Expr>, Comparison, Box<Expr>),
    /// Whether every one is true, any one is, or it is not.
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    /// The second's value if the first is true, else the third's.
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

/// A comparison between two values (see [`compare`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Eq,
    Ne,
    Data(Cmp),
}

impl Comparison {
    pub fn symbol(self) -> &'static str {
        match self {
            Comparison::Eq => "=",
            Comparison::Ne => "!=",
            Comparison::Data(c) => c.symbol(),
        }
    }
}

impl From<Var> for Expr {
    fn from(v: Var) -> Self {
        Expr::Var(v)
    }
}

impl From<Cell> for Expr {
    fn from(c: Cell) -> Self {
        Expr::Const(c)
    }
}

impl From<i64> for Expr {
    fn from(n: i64) -> Self {
        Expr::Const(Cell::Int(n))
    }
}

impl From<i32> for Expr {
    fn from(n: i32) -> Self {
        Expr::Const(Cell::Int(n.into()))
    }
}

impl From<f64> for Expr {
    fn from(x: f64) -> Self {
        Expr::Const(Cell::Float(x))
    }
}

impl From<bool> for Expr {
    fn from(b: bool) -> Self {
        Expr::Const(Cell::Bool(b))
    }
}

impl From<&str> for Expr {
    fn from(t: &str) -> Self {
        Expr::Const(Cell::Text(t.to_string()))
    }
}

impl From<String> for Expr {
    fn from(t: String) -> Self {
        Expr::Const(Cell::Text(t))
    }
}

macro_rules! binary {
    ($trait:ident, $method:ident, $op:ident) => {
        impl<T: Into<Expr>> ops::$trait<T> for Expr {
            type Output = Expr;
            fn $method(self, other: T) -> Expr {
                Expr::Binary(BinOp::$op, Box::new(self), Box::new(other.into()))
            }
        }
    };
}
binary!(Add, add, Add);
binary!(Sub, sub, Sub);
binary!(Mul, mul, Mul);
binary!(Div, div, Div);
binary!(Rem, rem, Rem);

impl ops::Neg for Expr {
    type Output = Expr;
    fn neg(self) -> Expr {
        Expr::Neg(Box::new(self))
    }
}

impl Expr {
    /// The entry `id` (0 is unbound).
    pub fn id(id: u64) -> Expr {
        Expr::Const(Cell::of_id(id))
    }

    /// `f(args)`.
    pub fn call(f: Func, args: impl IntoIterator<Item = Expr>) -> Expr {
        Expr::Call(f, args.into_iter().collect())
    }

    /// Whether `self cmp other` holds.
    pub fn compare(self, cmp: Comparison, other: impl Into<Expr>) -> Expr {
        Expr::Compare(Box::new(self), cmp, Box::new(other.into()))
    }

    /// `self = other`: the same entry, or equal values.
    pub fn eq(self, other: impl Into<Expr>) -> Expr {
        self.compare(Comparison::Eq, other)
    }

    pub fn ne(self, other: impl Into<Expr>) -> Expr {
        self.compare(Comparison::Ne, other)
    }

    pub fn lt(self, other: impl Into<Expr>) -> Expr {
        self.compare(Comparison::Data(Cmp::Lt), other)
    }

    pub fn le(self, other: impl Into<Expr>) -> Expr {
        self.compare(Comparison::Data(Cmp::Le), other)
    }

    pub fn gt(self, other: impl Into<Expr>) -> Expr {
        self.compare(Comparison::Data(Cmp::Gt), other)
    }

    pub fn ge(self, other: impl Into<Expr>) -> Expr {
        self.compare(Comparison::Data(Cmp::Ge), other)
    }

    pub fn and(self, other: impl Into<Expr>) -> Expr {
        match self {
            Expr::And(mut es) => {
                es.push(other.into());
                Expr::And(es)
            }
            e => Expr::And(vec![e, other.into()]),
        }
    }

    pub fn or(self, other: impl Into<Expr>) -> Expr {
        match self {
            Expr::Or(mut es) => {
                es.push(other.into());
                Expr::Or(es)
            }
            e => Expr::Or(vec![e, other.into()]),
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn not(self) -> Expr {
        Expr::Not(Box::new(self))
    }

    /// The variables the expression reads.
    pub fn vars(&self) -> BTreeSet<Var> {
        let mut vars = BTreeSet::new();
        self.each_var(&mut |v| {
            vars.insert(v);
        });
        vars
    }

    fn each_var(&self, f: &mut impl FnMut(Var)) {
        match self {
            Expr::Var(v) => f(*v),
            Expr::Const(_) => {}
            Expr::Call(_, args) | Expr::And(args) | Expr::Or(args) => {
                args.iter().for_each(|a| a.each_var(f))
            }
            Expr::Binary(_, a, b) | Expr::Compare(a, _, b) => {
                a.each_var(f);
                b.each_var(f);
            }
            Expr::Neg(a) | Expr::Not(a) => a.each_var(f),
            Expr::If(c, a, b) => {
                c.each_var(f);
                a.each_var(f);
                b.each_var(f);
            }
        }
    }

    /// The expression's value for `row`, whose slot for each variable is
    /// its value.
    pub fn eval<R: Row + ?Sized>(&self, g: &Graph, row: &R) -> Cell {
        match self {
            Expr::Var(v) => row.cell(v.index()),
            Expr::Const(c) => c.clone(),
            Expr::Neg(a) => match a.eval(g, row).numeric(g) {
                Some(Number::Int(n)) => n.checked_neg().map_or(Cell::Float(-(n as f64)), Cell::Int),
                Some(Number::Float(x)) => Cell::Float(-x),
                None => Cell::Unbound,
            },
            Expr::Binary(op, a, b) => arithmetic(g, *op, &a.eval(g, row), &b.eval(g, row)),
            Expr::Call(f, args) => {
                let args: Vec<Cell> = args.iter().map(|a| a.eval(g, row)).collect();
                call(g, *f, &args)
            }
            Expr::Compare(a, cmp, b) => {
                Cell::Bool(compare(g, &a.eval(g, row), *cmp, &b.eval(g, row)))
            }
            Expr::And(es) => Cell::Bool(es.iter().all(|e| e.holds(g, row))),
            Expr::Or(es) => Cell::Bool(es.iter().any(|e| e.holds(g, row))),
            Expr::Not(e) => Cell::Bool(!e.holds(g, row)),
            Expr::If(c, a, b) => {
                if c.holds(g, row) {
                    a.eval(g, row)
                } else {
                    b.eval(g, row)
                }
            }
        }
    }

    /// Whether the expression is true for `row`.
    pub fn holds<R: Row + ?Sized>(&self, g: &Graph, row: &R) -> bool {
        is_true(&self.eval(g, row))
    }

    /// The expression written, with `name` naming its variables and ids.
    pub(crate) fn show(&self, var: &dyn Fn(Var) -> String, id: &dyn Fn(u64) -> String) -> String {
        self.show_in(var, id, 0)
    }

    fn show_in(
        &self,
        var: &dyn Fn(Var) -> String,
        id: &dyn Fn(u64) -> String,
        outer: u8,
    ) -> String {
        let (text, precedence) = match self {
            Expr::Var(v) => (var(*v), 5),
            Expr::Const(Cell::Id(i)) => (id(*i), 5),
            // Text that is a number, as GraQL writes it.
            Expr::Const(Cell::Text(t)) if super::aggregate::parse_number(t).is_some() => {
                (t.clone(), 5)
            }
            Expr::Const(Cell::Text(t)) => (format!("{t:?}"), 5),
            Expr::Const(Cell::Int(n)) => (n.to_string(), 5),
            Expr::Const(Cell::Float(x)) => (x.to_string(), 5),
            Expr::Const(Cell::Bool(b)) => (b.to_string(), 5),
            Expr::Const(Cell::Unbound) => ("unbound".to_string(), 5),
            Expr::Call(f, args) => {
                let args: Vec<String> = args.iter().map(|a| a.show_in(var, id, 0)).collect();
                (format!("{}({})", f.name(), args.join(", ")), 5)
            }
            Expr::Neg(a) => (format!("-{}", a.show_in(var, id, 4)), 4),
            Expr::Binary(op, a, b) => {
                let p = op.precedence() + 1;
                let text = format!(
                    "{} {} {}",
                    a.show_in(var, id, p),
                    op.symbol(),
                    b.show_in(var, id, p + 1)
                );
                (text, p)
            }
            Expr::Compare(a, cmp, b) => (
                format!(
                    "{} {} {}",
                    a.show_in(var, id, 2),
                    cmp.symbol(),
                    b.show_in(var, id, 2)
                ),
                1,
            ),
            Expr::And(es) | Expr::Or(es) => {
                let joint = if matches!(self, Expr::And(_)) {
                    " and "
                } else {
                    " or "
                };
                let parts: Vec<String> = es.iter().map(|e| e.show_in(var, id, 1)).collect();
                (parts.join(joint), 0)
            }
            Expr::Not(e) => (format!("not {}", e.show_in(var, id, 5)), 4),
            Expr::If(c, a, b) => (
                format!(
                    "if {} then {} else {}",
                    c.show_in(var, id, 0),
                    a.show_in(var, id, 0),
                    b.show_in(var, id, 0)
                ),
                0,
            ),
        };
        if precedence < outer {
            format!("({text})")
        } else {
            text
        }
    }
}

/// A row an expression reads its slots from.
pub trait Row {
    fn cell(&self, slot: usize) -> Cell;
}

impl Row for [u64] {
    fn cell(&self, slot: usize) -> Cell {
        Cell::of_id(self[slot])
    }
}

impl Row for [Cell] {
    fn cell(&self, slot: usize) -> Cell {
        self[slot].clone()
    }
}

/// A row of ids some of whose slots are values instead: the parameters
/// given values that are not entries, by slot (see
/// [`Plan::execute_values`](super::Plan::execute_values)).
pub(crate) struct WithValues<'a> {
    pub ids: &'a [u64],
    pub values: &'a [(usize, Cell)],
}

impl Row for WithValues<'_> {
    fn cell(&self, slot: usize) -> Cell {
        match self.values.iter().find(|(s, _)| *s == slot) {
            Some((_, c)) => c.clone(),
            None => Cell::of_id(self.ids[slot]),
        }
    }
}

/// An arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl BinOp {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
        }
    }

    /// Additive operators are 1, multiplicative 2.
    pub(crate) fn precedence(self) -> u8 {
        match self {
            BinOp::Add | BinOp::Sub => 1,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 2,
        }
    }
}

/// A function of an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Func {
    /// Numbers: the absolute value, and rounding to an integer.
    Abs,
    Round,
    Ceil,
    Floor,
    /// The value as text, and as a number.
    Str,
    Number,
    /// Text: its length in characters, its case changed, and tests of what
    /// it contains (booleans).
    Length,
    Upper,
    Lower,
    Contains,
    Starts,
    Ends,
    /// Whether text matches a regular expression, anywhere in it.
    Matches,
    /// Texts joined.
    Concat,
    /// Whether a variable is bound.
    Bound,
    /// The id of an entry.
    Id,
    /// A link's creation time (milliseconds since 1970) and author.
    When,
    Author,
    /// The milliseconds since 1970 of an ISO 8601 time, and the time of a
    /// number of milliseconds.
    Time,
    Iso,
}

const FUNCS: &[(Func, &str, usize, usize)] = &[
    (Func::Abs, "abs", 1, 1),
    (Func::Round, "round", 1, 1),
    (Func::Ceil, "ceil", 1, 1),
    (Func::Floor, "floor", 1, 1),
    (Func::Str, "str", 1, 1),
    (Func::Number, "number", 1, 1),
    (Func::Length, "length", 1, 1),
    (Func::Upper, "upper", 1, 1),
    (Func::Lower, "lower", 1, 1),
    (Func::Contains, "contains", 2, 2),
    (Func::Starts, "starts", 2, 2),
    (Func::Ends, "ends", 2, 2),
    // GraQL's PREFIX, as in `(PREFIX("Bil"))`.
    (Func::Starts, "prefix", 2, 2),
    (Func::Matches, "matches", 2, 2),
    (Func::Concat, "concat", 1, usize::MAX),
    (Func::Bound, "bound", 1, 1),
    (Func::Id, "id", 1, 1),
    (Func::When, "when", 1, 1),
    (Func::Author, "author", 1, 1),
    (Func::Time, "time", 1, 1),
    (Func::Iso, "iso", 1, 1),
];

impl Func {
    pub fn of_name(name: &str) -> Option<Func> {
        let name = name.to_ascii_lowercase();
        FUNCS.iter().find(|f| f.1 == name).map(|f| f.0)
    }

    pub fn name(self) -> &'static str {
        FUNCS.iter().find(|f| f.0 == self).expect("a function").1
    }

    /// The least and most arguments it takes.
    pub fn arity(self) -> (usize, usize) {
        let f = FUNCS.iter().find(|f| f.0 == self).expect("a function");
        (f.2, f.3)
    }

    /// Whether its value is a boolean, so that it can be a pattern's test.
    pub fn is_test(self) -> bool {
        matches!(
            self,
            Func::Contains | Func::Starts | Func::Ends | Func::Matches | Func::Bound
        )
    }
}

/// The names of the functions, for completion and to keep rules from using
/// them.
pub fn function_names() -> impl Iterator<Item = &'static str> {
    FUNCS.iter().map(|f| f.1)
}

fn arithmetic(g: &Graph, op: BinOp, a: &Cell, b: &Cell) -> Cell {
    let (Some(x), Some(y)) = (a.numeric(g), b.numeric(g)) else {
        return Cell::Unbound;
    };
    let float = |f: fn(f64, f64) -> f64| {
        let v = f(x.as_f64(), y.as_f64());
        if v.is_finite() {
            Cell::Float(v)
        } else {
            Cell::Unbound
        }
    };
    match (op, x, y) {
        (BinOp::Add, Number::Int(m), Number::Int(n)) => m
            .checked_add(n)
            .map_or_else(|| float(|a, b| a + b), Cell::Int),
        (BinOp::Sub, Number::Int(m), Number::Int(n)) => m
            .checked_sub(n)
            .map_or_else(|| float(|a, b| a - b), Cell::Int),
        (BinOp::Mul, Number::Int(m), Number::Int(n)) => m
            .checked_mul(n)
            .map_or_else(|| float(|a, b| a * b), Cell::Int),
        (BinOp::Rem, Number::Int(m), Number::Int(n)) => {
            m.checked_rem(n).map_or(Cell::Unbound, Cell::Int)
        }
        (BinOp::Rem, ..) => Cell::Unbound,
        (BinOp::Add, ..) => float(|a, b| a + b),
        (BinOp::Sub, ..) => float(|a, b| a - b),
        (BinOp::Mul, ..) => float(|a, b| a * b),
        (BinOp::Div, ..) if y.as_f64() == 0.0 => Cell::Unbound,
        (BinOp::Div, ..) => float(|a, b| a / b),
    }
}

/// A float rounded to an integer, if it is one.
fn to_int(x: f64) -> Cell {
    if x.is_finite() && x.abs() < 9.0e15 {
        Cell::Int(x as i64)
    } else {
        Cell::Unbound
    }
}

fn call(g: &Graph, f: Func, args: &[Cell]) -> Cell {
    let text = |i: usize| args.get(i).and_then(|c| c.text(g));
    let number = |i: usize| args.get(i).and_then(|c| c.numeric(g));
    let test = |p: fn(&str, &str) -> bool| match (text(0), text(1)) {
        (Some(a), Some(b)) => Cell::Bool(p(&a, &b)),
        _ => Cell::Unbound,
    };
    match f {
        Func::Abs => match number(0) {
            Some(Number::Int(n)) => n.checked_abs().map_or(Cell::Unbound, Cell::Int),
            Some(Number::Float(x)) => Cell::Float(x.abs()),
            None => Cell::Unbound,
        },
        Func::Round | Func::Ceil | Func::Floor => match number(0) {
            Some(Number::Int(n)) => Cell::Int(n),
            Some(Number::Float(x)) => to_int(match f {
                Func::Round => x.round(),
                Func::Ceil => x.ceil(),
                _ => x.floor(),
            }),
            None => Cell::Unbound,
        },
        Func::Str => text(0).map_or(Cell::Unbound, Cell::Text),
        Func::Number => match number(0) {
            Some(Number::Int(n)) => Cell::Int(n),
            Some(Number::Float(x)) => Cell::Float(x),
            None => Cell::Unbound,
        },
        Func::Length => text(0).map_or(Cell::Unbound, |t| Cell::Int(t.chars().count() as i64)),
        Func::Upper => text(0).map_or(Cell::Unbound, |t| Cell::Text(t.to_uppercase())),
        Func::Lower => text(0).map_or(Cell::Unbound, |t| Cell::Text(t.to_lowercase())),
        Func::Contains => test(|a, b| a.contains(b)),
        Func::Starts => test(|a, b| a.starts_with(b)),
        Func::Ends => test(|a, b| a.ends_with(b)),
        Func::Matches => test(matches),
        Func::Concat => {
            let mut out = String::new();
            for i in 0..args.len() {
                match text(i) {
                    Some(t) => out.push_str(&t),
                    None => return Cell::Unbound,
                }
            }
            Cell::Text(out)
        }
        Func::Bound => Cell::Bool(args.first().is_some_and(|c| *c != Cell::Unbound)),
        Func::Id => match args.first() {
            Some(Cell::Id(id)) => Cell::Int(*id as i64),
            _ => Cell::Unbound,
        },
        Func::When | Func::Author => {
            let Some(Cell::Id(id)) = args.first() else {
                return Cell::Unbound;
            };
            let Some(link) = g
                .get(*id)
                .ok()
                .and_then(|r| r.as_link().map(|l| (l.when(), l.author())))
            else {
                return Cell::Unbound;
            };
            match f {
                Func::When => Cell::Int(link.0 as i64),
                _ => Cell::of_id(link.1),
            }
        }
        Func::Time => text(0)
            .and_then(|t| parse_time(&t))
            .map_or(Cell::Unbound, |ms| Cell::Int(ms as i64)),
        Func::Iso => match number(0) {
            Some(Number::Int(n)) if n >= 0 => Cell::Text(format_time(n as u64)),
            _ => Cell::Unbound,
        },
    }
}

/// Whether `text` matches the regular expression `pattern` anywhere: false
/// if `pattern` isn't one.  Compiled expressions are kept, a few at a time.
fn matches(text: &str, pattern: &str) -> bool {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static COMPILED: RefCell<HashMap<String, Option<regex_lite::Regex>>> =
            RefCell::new(HashMap::new());
    }
    COMPILED.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= 64 && !c.contains_key(pattern) {
            c.clear();
        }
        c.entry(pattern.to_string())
            .or_insert_with(|| regex_lite::Regex::new(pattern).ok())
            .as_ref()
            .is_some_and(|re| re.is_match(text))
    })
}

/// Whether `a cmp b` holds.  `=` and `!=` of two entries compare the
/// entries; otherwise numbers compare by value, booleans as booleans, and
/// other values by their text.  Nothing holds of an unbound value.
pub fn compare(g: &Graph, a: &Cell, cmp: Comparison, b: &Cell) -> bool {
    if *a == Cell::Unbound || *b == Cell::Unbound {
        return false;
    }
    let ordering = match (a, b) {
        (Cell::Id(x), Cell::Id(y)) if matches!(cmp, Comparison::Eq | Comparison::Ne) => {
            Some(x.cmp(y))
        }
        _ => order(g, a, b),
    };
    match (cmp, ordering) {
        (_, None) => false,
        (Comparison::Eq, Some(o)) => o.is_eq(),
        (Comparison::Ne, Some(o)) => o.is_ne(),
        (Comparison::Data(c), Some(o)) => holds(c, o),
    }
}

fn holds(c: Cmp, o: Ordering) -> bool {
    c.holds(o)
}

/// The order of two values: as numbers if both are, as booleans if both
/// are, else as text, or of two entries without data by [`compare_cells`].
fn order(g: &Graph, a: &Cell, b: &Cell) -> Option<Ordering> {
    let boolean = |c: &Cell| match c {
        Cell::Bool(b) => Some(*b),
        Cell::Text(t) if t == "true" => Some(true),
        Cell::Text(t) if t == "false" => Some(false),
        _ => None,
    };
    if let (Some(x), Some(y)) = (a.number(g), b.number(g)) {
        return x.partial_cmp(&y);
    }
    if matches!(a, Cell::Bool(_)) || matches!(b, Cell::Bool(_)) {
        return Some(boolean(a)?.cmp(&boolean(b)?));
    }
    let data = |c: &Cell| match c {
        Cell::Id(id) => g.data(*id).map(<[u8]>::to_vec),
        other => other.text(g).map(String::into_bytes),
    };
    match (data(a), data(b)) {
        (Some(x), Some(y)) => Some(compare_data(&x, &y)),
        _ if matches!((a, b), (Cell::Id(_), Cell::Id(_))) => Some(compare_cells(g, a, b)),
        _ => None,
    }
}

/// Whether a cell is the boolean true, as a pattern's test needs.
pub fn is_true(c: &Cell) -> bool {
    matches!(c, Cell::Bool(true))
}
