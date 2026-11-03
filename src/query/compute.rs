//! The values a query computes from its rows once they are matched: its
//! [computed](super::Query::compute) variables, its
//! [aggregates](super::Query::aggregate) and `having`, then ordering,
//! `distinct`, `offset` and `limit` over cells.

use std::cmp::Ordering;
use std::collections::HashSet;

use super::aggregate::{self, Aggregate, Aggregation, Cell};
use super::{Computed, Query};
use crate::graph::Graph;

/// The results of `q` from its matched rows, `rows`, which have a slot for
/// each variable; `start` is the row the query started from, with the
/// parameters' entries, and `values` the parameters' values that are not
/// entries, by slot.
pub(crate) fn results(
    q: &Query,
    g: &Graph,
    rows: Vec<Vec<u64>>,
    start: &[u64],
    values: &[(usize, Cell)],
) -> Vec<Vec<Cell>> {
    // A row of ids as cells, with the parameters' values.
    let cells_of = |row: &[u64]| -> Vec<Cell> {
        let mut cells: Vec<Cell> = row.iter().map(|&id| Cell::of_id(id)).collect();
        for (slot, value) in values {
            cells[*slot] = value.clone();
        }
        cells
    };
    let width = start.len();
    // Each row's cells, its computed values set, and then the arguments of
    // its aggregates, after the variables' slots.
    let args: Vec<&Computed> = q
        .computed
        .iter()
        .map(|(_, c)| c)
        .filter(|c| matches!(c, Computed::Aggregate { .. }))
        .collect();
    let cells = rows.into_iter().map(|row| {
        let mut cells = cells_of(&row);
        for (v, c) in &q.computed {
            if let Computed::Value(e) = c {
                cells[v.0] = e.eval(g, &cells[..]);
            }
        }
        for c in &args {
            if let Computed::Aggregate { arg, .. } = c {
                let value = arg
                    .as_ref()
                    .map_or(Cell::Unbound, |e| e.eval(g, &cells[..]));
                cells.push(value);
            }
        }
        cells
    });
    let mut rows: Vec<Vec<Cell>> = if args.is_empty() {
        cells.collect()
    } else {
        let grouping = q.grouping();
        let aggregates: Vec<_> = q
            .computed
            .iter()
            .filter(|(_, c)| matches!(c, Computed::Aggregate { .. }))
            .collect();
        let aggregation = Aggregation {
            group_by: grouping.iter().map(|v| v.0).collect(),
            aggregates: aggregates
                .iter()
                .enumerate()
                .map(|(k, (_, c))| match c {
                    Computed::Aggregate {
                        function,
                        arg,
                        distinct,
                    } => Aggregate {
                        function: *function,
                        column: arg.as_ref().map(|_| width + k),
                        distinct: *distinct,
                    },
                    Computed::Value(_) => unreachable!("an aggregate"),
                })
                .collect(),
        };
        // Each group a row: its values of the variables grouped by, its
        // aggregates and the parameters, the others unbound.
        aggregation
            .apply(g, cells)
            .into_iter()
            .map(|group| {
                let mut row = cells_of(start);
                for (cell, v) in group
                    .iter()
                    .zip(grouping.iter().chain(aggregates.iter().map(|(v, _)| v)))
                {
                    row[v.0] = cell.clone();
                }
                row
            })
            .collect()
    };
    rows.retain(|row| q.having.iter().all(|e| e.holds(g, &row[..])));
    if !q.order.is_empty() {
        rows.sort_by(|a, b| {
            q.order
                .iter()
                .map(|k| {
                    let (x, y) = (&a[k.var.0], &b[k.var.0]);
                    let o = match (x, y) {
                        (Cell::Id(x), Cell::Id(y)) if !k.by_data => x.cmp(y),
                        _ => aggregate::compare_cells(g, x, y),
                    };
                    if k.descending { o.reverse() } else { o }
                })
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        });
    }
    let selected = q.selected();
    let mut rows: Vec<Vec<Cell>> = rows
        .into_iter()
        .map(|row| selected.iter().map(|v| row[v.0].clone()).collect())
        .collect();
    if q.distinct {
        let mut seen = HashSet::new();
        rows.retain(|r| seen.insert(r.iter().map(Cell::key).collect::<Vec<_>>()));
    }
    rows.drain(..q.offset.min(rows.len()));
    if let Some(n) = q.limit {
        rows.truncate(n);
    }
    rows
}
