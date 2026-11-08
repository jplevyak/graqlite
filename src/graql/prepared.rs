//! Prepared statements: a `select`, `verify` or `lookup` lowered and planned once,
//! then run with its parameters' values (see [`Session::prepare`]).

use super::{
    AsOf, Error, Node, Output, Param, Params, Path, Pattern, Result, RuleDef, SelectItem, Session,
    Statement, print,
};
use std::sync::Arc;

use crate::graph::{Graph, Holds};
use crate::query;
use crate::query::aggregate::Cell;

/// A statement made by [`Session::prepare`], to run with
/// [`Session::run_prepared`].
pub struct PreparedStatement {
    statement: Statement,
    /// The query planned, for a `select`, `verify` or `lookup`; `None` runs the
    /// statement as [`Session::run_with`] does.
    fast: Option<Fast>,
}

struct Fast {
    query: query::Prepared,
    /// The names of the query's parameters, in its order.
    params: Vec<String>,
    /// The names of the rows' columns.
    names: Vec<String>,
    /// The rules when lowered: lowered again if they change.
    rules: Arc<[RuleDef]>,
    /// Whether lowering resolved something that can change as the graph
    /// does: a constant not in it, which matched nothing, or a time.
    fragile: bool,
    /// The graph's next id when lowered.
    next_id: u64,
}

impl PreparedStatement {
    pub fn statement(&self) -> &Statement {
        &self.statement
    }

    /// The plan, if the statement is planned once: a `select`, `verify` or
    /// `lookup`.
    pub fn plan(&self) -> Option<&query::Plan> {
        self.fast.as_ref().map(|f| f.query.plan())
    }
}

impl Session {
    /// Prepares `statement` to run many times on `g` with different values
    /// of its parameters (`$name`), with [`run_prepared`](Self::run_prepared).
    ///
    /// A `select`, `verify` or `lookup` is lowered and planned once, its parameters
    /// variables of the query given values when it runs (see
    /// [`Query::param`](crate::query::Query::param)): a key or id is that
    /// entry, and a value is, in a pattern, the entry with that data, and in
    /// a comparison or expression the data itself, which need not be in the
    /// graph, as when the value is written in (see
    /// [`Plan::execute_values`](crate::query::Plan::execute_values)).  It is
    /// lowered again if the rules change, or if it named something not in
    /// the graph, or a time, and the graph has changed.  Any other statement
    /// is run as [`run_with`](Self::run_with) runs it.
    pub fn prepare(&mut self, g: &Graph, statement: &Statement) -> Result<PreparedStatement> {
        if let Statement::Rule(_) = statement {
            // Checks that it has no parameters.
            statement.bind(&Params::new())?;
        }
        // Its symbols are their values now.
        let mut statement = statement.clone();
        self.resolve_symbols(&mut statement)?;
        let rules = self.saved(g)?.defs.clone();
        let fast = Fast::new(g, &statement, rules)?;
        Ok(PreparedStatement { statement, fast })
    }

    /// Runs a prepared statement on `g`, with its parameters' values given
    /// by `params`.
    pub fn run_prepared(
        &mut self,
        g: &mut Graph,
        prepared: &mut PreparedStatement,
        params: &Params,
    ) -> Result<Output> {
        // Planned for the database: over another graph, it runs as any.
        let Some(fast) = prepared.fast.as_mut().filter(|_| self.view().is_none()) else {
            return self.run_with(g, &prepared.statement, params);
        };
        let rules = &self.saved(g)?.defs;
        let changed = !Arc::ptr_eq(rules, &fast.rules) && **rules != *fast.rules;
        if changed || (fast.fragile && g.next_id() != fast.next_id) {
            let rules = rules.clone();
            match Fast::new(g, &prepared.statement, rules)? {
                Some(f) => *fast = f,
                None => unreachable!("prepared once"),
            }
        }
        let mut values = Vec::with_capacity(fast.params.len());
        for name in &fast.params {
            values.push(match params.get(name) {
                None => return Err(Error::Invalid(format!("${name} is not given"))),
                Some(Param::Id(id)) => Cell::of_id(*id),
                Some(Param::Key(k)) => Cell::of_id(g.find_by_key(k.as_bytes()).unwrap_or(0)),
                // The entry with the data in a pattern, the data elsewhere.
                Some(Param::Value(v)) => Cell::Text(v.clone()),
            });
        }
        let vars = fast.names.clone();
        let holds = Holds::of(fast.query.query(), None);
        Ok(match &prepared.statement {
            Statement::Verify { .. } => {
                Output::Verified(!fast.query.run_values(g, &values)?.is_empty())
            }
            Statement::Select { .. } if fast.query.query().computes() => Output::Table {
                vars,
                rows: fast.query.cells_values(g, &values)?,
                holds,
            },
            Statement::Select { .. } => Output::Rows {
                vars,
                rows: fast.query.run_values(g, &values)?,
                holds,
            },
            Statement::Lookup { .. } => Output::Graph(fast.query.graph_values(g, &values)?),
            _ => unreachable!("a select, verify or lookup"),
        })
    }
}

impl Fast {
    /// `statement` lowered and planned, if it can be.
    fn new(g: &Graph, statement: &Statement, rules: Arc<[RuleDef]>) -> Result<Option<Fast>> {
        let (pattern, as_of, missing) = match statement {
            Statement::Select {
                vars,
                having,
                pattern,
                as_of,
                ..
            } => {
                // A key in the columns or `having` not in the graph.
                let mut missing = false;
                let mut each = |n: &Node| {
                    missing |= matches!(n, Node::Key(k) if g.find_by_key(k.as_bytes()).is_none())
                };
                for item in vars.iter().flatten() {
                    match item {
                        SelectItem::Var(_) => {}
                        SelectItem::Expr { expr, .. } => expr.each_node(&mut each),
                        SelectItem::Aggregate { arg, .. } => {
                            arg.iter().for_each(|a| a.each_node(&mut each))
                        }
                    }
                }
                for (a, _, b) in having {
                    a.each_node(&mut each);
                    b.each_node(&mut each);
                }
                (pattern, as_of, missing)
            }
            Statement::Verify { pattern, as_of } => (pattern, as_of, false),
            Statement::Lookup {
                pattern,
                where_,
                as_of,
            } => (
                pattern,
                as_of,
                where_.as_ref().is_some_and(|w| fragile(g, w)),
            ),
            _ => return Ok(None),
        };
        let (q, names) = statement.query_with(g, &rules)?;
        let params = q.params().iter().map(|&v| q.name(v).to_string()).collect();
        let fragile = missing || matches!(as_of, Some(AsOf::Time(_))) || fragile(g, pattern);
        Ok(Some(Fast {
            query: q.prepare(g)?,
            params,
            names,
            rules,
            fragile,
            next_id: g.next_id(),
        }))
    }
}

/// Whether lowering `p` resolves something that can change as `g` does: a
/// key or value not in `g`, or an `as of` a time.
fn fragile(g: &Graph, p: &Pattern) -> bool {
    let mut missing = false;
    print::each_node(p, &mut |n| {
        missing |= match n {
            Node::Key(k) => g.find_by_key(k.as_bytes()).is_none(),
            Node::Value(v) | Node::Symbol(v) => g.find_value(v.as_bytes()).is_none(),
            Node::Var(_) | Node::Blank(_) | Node::Id(_) | Node::Param(_) => false,
        }
    });
    missing || fragile_paths(g, p)
}

/// Whether `p` has an `as of` a time, or a path expression naming a
/// predicate not in `g`.
fn fragile_paths(g: &Graph, p: &Pattern) -> bool {
    match p {
        Pattern::AsOf(_, AsOf::Time(_)) => true,
        Pattern::PathExpr(_, path, _) => missing_predicate(g, path),
        Pattern::AsOf(p, _) | Pattern::Not(p) | Pattern::Optional(p) => fragile_paths(g, p),
        Pattern::And(ps) | Pattern::Or(ps) => ps.iter().any(|p| fragile_paths(g, p)),
        _ => false,
    }
}

fn missing_predicate(g: &Graph, path: &Path) -> bool {
    let mut steps = Vec::new();
    path.steps(&mut steps);
    steps.iter().any(|step| match step {
        Path::Pred(p) => g.find_value(p.as_bytes()).is_none(),
        Path::Key(k) => g.find_by_key(k.as_bytes()).is_none(),
        _ => false,
    })
}
