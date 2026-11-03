//! Expressions as written in GraQL, before they are resolved against a
//! graph into [`query::expr::Expr`](crate::query::expr::Expr)s, which
//! evaluate them.

use std::fmt;

use super::{Comparison, Node, print::literal};
pub use crate::query::expr::{BinOp, Func, function_names};

/// An expression, before it is resolved against a graph.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Node(Node),
    Call(Func, Vec<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    /// A call of a function `FUNCTION` defined: replaced by its body before
    /// the statement runs.
    Apply(String, Vec<Expr>),
    /// `a < b` and the other comparisons, in a condition.
    Compare(Box<Expr>, Comparison, Box<Expr>),
    /// `a && b` and `a || b`.
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    /// `if c then a else b`.
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Calls `f` with each node of the expression.
    pub(super) fn each_node(&self, f: &mut impl FnMut(&Node)) {
        match self {
            Expr::Node(n) => f(n),
            Expr::Call(_, args) | Expr::Apply(_, args) => args.iter().for_each(|a| a.each_node(f)),
            Expr::Binary(_, a, b) | Expr::Compare(a, _, b) | Expr::And(a, b) | Expr::Or(a, b) => {
                a.each_node(f);
                b.each_node(f);
            }
            Expr::Neg(a) => a.each_node(f),
            Expr::If(c, a, b) => {
                c.each_node(f);
                a.each_node(f);
                b.each_node(f);
            }
        }
    }

    pub(super) fn each_node_mut(&mut self, f: &mut impl FnMut(&mut Node)) {
        match self {
            Expr::Node(n) => f(n),
            Expr::Call(_, args) | Expr::Apply(_, args) => {
                args.iter_mut().for_each(|a| a.each_node_mut(f))
            }
            Expr::Binary(_, a, b) | Expr::Compare(a, _, b) | Expr::And(a, b) | Expr::Or(a, b) => {
                a.each_node_mut(f);
                b.each_node_mut(f);
            }
            Expr::Neg(a) => a.each_node_mut(f),
            Expr::If(c, a, b) => {
                c.each_node_mut(f);
                a.each_node_mut(f);
                b.each_node_mut(f);
            }
        }
    }

    /// Calls `f` with the expression and each within it, innermost last,
    /// to change them.
    pub(super) fn each_expr_mut(&mut self, f: &mut impl FnMut(&mut Expr)) {
        match self {
            Expr::Node(_) => {}
            Expr::Call(_, args) | Expr::Apply(_, args) => {
                args.iter_mut().for_each(|a| a.each_expr_mut(f))
            }
            Expr::Binary(_, a, b) | Expr::Compare(a, _, b) | Expr::And(a, b) | Expr::Or(a, b) => {
                a.each_expr_mut(f);
                b.each_expr_mut(f);
            }
            Expr::Neg(a) => a.each_expr_mut(f),
            Expr::If(c, a, b) => {
                c.each_expr_mut(f);
                a.each_expr_mut(f);
                b.each_expr_mut(f);
            }
        }
        f(self);
    }

    /// The expression written, with `node` writing its nodes.
    pub(super) fn show(&self, node: &mut dyn FnMut(&Node) -> String, outer: u8) -> String {
        let (text, precedence) = match self {
            Expr::Node(n) => (node(n), 4),
            Expr::Call(f, args) => {
                let args: Vec<String> = args.iter().map(|a| a.show(node, 0)).collect();
                (format!("{}({})", f.name(), args.join(", ")), 4)
            }
            Expr::Apply(name, args) => {
                let args: Vec<String> = args.iter().map(|a| a.show(node, 0)).collect();
                (format!("{name}({})", args.join(", ")), 4)
            }
            // Conditions bind looser than arithmetic: `||` 0 (as `if`), `&&`
            // 0, comparisons 0, each parenthesized within arithmetic.
            Expr::Compare(a, cmp, b) => (
                format!("{} {} {}", a.show(node, 1), cmp.symbol(), b.show(node, 1)),
                0,
            ),
            Expr::And(a, b) => (format!("{} && {}", a.show(node, 1), b.show(node, 1)), 0),
            Expr::Or(a, b) => (format!("{} || {}", a.show(node, 1), b.show(node, 1)), 0),
            Expr::If(c, a, b) => (
                format!(
                    "if {} then {} else {}",
                    c.show(node, 0),
                    a.show(node, 0),
                    b.show(node, 0)
                ),
                0,
            ),
            Expr::Neg(a) => (format!("-{}", a.show(node, 3)), 3),
            Expr::Binary(op, a, b) => {
                let p = op.precedence();
                // Left associative: the right side binds tighter.
                let text = format!(
                    "{} {} {}",
                    a.show(node, p),
                    op.symbol(),
                    b.show(node, p + 1)
                );
                (text, p)
            }
        };
        if precedence < outer {
            format!("({text})")
        } else {
            text
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut node = |n: &Node| match n {
            Node::Var(v) => format!("?{v}"),
            Node::Blank(b) => b.clone(),
            Node::Key(k) => k.clone(),
            Node::Value(v) => literal(v),
            Node::Id(id) => format!("#{id}"),
            Node::Param(p) => format!("${p}"),
            Node::Symbol(s) => s.clone(),
        };
        f.write_str(&self.show(&mut node, 0))
    }
}
