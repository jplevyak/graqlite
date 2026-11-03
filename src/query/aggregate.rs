//! Computed values, and aggregates: rows grouped and folded.
//!
//! A query's row is a list of ids, and a [`Cell`] can also hold a computed
//! value: a number, text or a boolean.  [`Aggregation::apply`] groups rows
//! of cells by some columns and folds the others with [`Function`]s.  A row's
//! id 0 is unbound (from an optional group), which aggregates skip, except
//! `count(*)`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::graph::Graph;

/// A value in a row of results: an entry, a computed value, or nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Id(u64),
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    /// Unbound, as from an optional group, or a value that could not be
    /// computed (the average of no numbers, text that is not a number).
    Unbound,
}

/// A cell, for comparing and hashing as a group's key: equal cells have
/// equal keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CellKey {
    Id(u64),
    Int(i64),
    Float(u64),
    Text(String),
    Bool(bool),
    Unbound,
}

impl Cell {
    /// The cell of an id from a row: 0 is unbound.
    pub fn of_id(id: u64) -> Cell {
        if id == 0 { Cell::Unbound } else { Cell::Id(id) }
    }

    pub fn key(&self) -> CellKey {
        match self {
            Cell::Id(id) => CellKey::Id(*id),
            Cell::Int(n) => CellKey::Int(*n),
            Cell::Float(x) => CellKey::Float(x.to_bits()),
            Cell::Text(t) => CellKey::Text(t.clone()),
            Cell::Bool(b) => CellKey::Bool(*b),
            Cell::Unbound => CellKey::Unbound,
        }
    }

    /// The number the cell is, or that its text or entry's data is.
    pub fn number(&self, g: &Graph) -> Option<f64> {
        self.numeric(g).map(Number::as_f64)
    }

    /// The same, kept as an integer while it is one.
    pub fn numeric(&self, g: &Graph) -> Option<Number> {
        match self {
            Cell::Int(n) => Some(Number::Int(*n)),
            Cell::Float(x) => Some(Number::Float(*x)),
            Cell::Id(id) => parse_number(std::str::from_utf8(g.data(*id)?).ok()?),
            Cell::Text(t) => parse_number(t),
            Cell::Bool(_) | Cell::Unbound => None,
        }
    }

    /// The cell as text: an entry's data, or its key if it has no data;
    /// a number or boolean as written.  `None` for an entry with neither,
    /// and for unbound.
    pub fn text(&self, g: &Graph) -> Option<String> {
        match self {
            Cell::Id(id) => g
                .data(*id)
                .or_else(|| g.key(*id))
                .map(|d| String::from_utf8_lossy(d).into_owned()),
            Cell::Int(n) => Some(n.to_string()),
            Cell::Float(x) => Some(format_float(*x)),
            Cell::Text(t) => Some(t.clone()),
            Cell::Bool(b) => Some(b.to_string()),
            Cell::Unbound => None,
        }
    }
}

impl fmt::Display for Cell {
    /// Values as they are, an entry by its id, and unbound as nothing.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cell::Id(id) => write!(f, "#{id}"),
            Cell::Int(n) => write!(f, "{n}"),
            Cell::Float(x) => f.write_str(&format_float(*x)),
            Cell::Text(t) => f.write_str(t),
            Cell::Bool(b) => write!(f, "{b}"),
            Cell::Unbound => Ok(()),
        }
    }
}

/// `x` to six decimal places, without trailing zeros: `3.666667`, `6.5`,
/// `2`.
pub fn format_float(x: f64) -> String {
    let text = format!("{x:.6}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".into()
    } else {
        text.into()
    }
}

/// Orders cells, totally: unbound first; then numbers, and text and entries
/// whose data is a number, by value; then booleans; then other text and
/// entries with data, by bytes; then entries without data (nodes and
/// links), by id.
pub fn compare_cells(g: &Graph, a: &Cell, b: &Cell) -> Ordering {
    /// A cell's rank, its number, its bytes, and its id.
    fn key<'a>(g: &'a Graph, c: &'a Cell) -> (u8, f64, &'a [u8], u64) {
        if let Some(n) = c.number(g) {
            return (1, n, &[], 0);
        }
        match c {
            Cell::Unbound => (0, 0.0, &[], 0),
            Cell::Bool(b) => (2, f64::from(u8::from(*b)), &[], 0),
            Cell::Text(t) => (3, 0.0, t.as_bytes(), 0),
            Cell::Id(id) => match g.data(*id) {
                Some(d) => (3, 0.0, d, 0),
                None => (4, 0.0, &[], *id),
            },
            Cell::Int(_) | Cell::Float(_) => unreachable!("numbers"),
        }
    }
    let (ka, kb) = (key(g, a), key(g, b));
    ka.0.cmp(&kb.0)
        .then(ka.1.total_cmp(&kb.1))
        .then(ka.2.cmp(kb.2))
        .then(ka.3.cmp(&kb.3))
}

/// A number, kept as an integer while it can be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    pub fn as_f64(self) -> f64 {
        match self {
            Number::Int(n) => n as f64,
            Number::Float(x) => x,
        }
    }
}

/// The number `text` is, if it is one.
pub fn parse_number(text: &str) -> Option<Number> {
    let text = text.trim();
    if let Ok(n) = text.parse::<i64>() {
        return Some(Number::Int(n));
    }
    text.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .map(Number::Float)
}

/// A function folding the values of a column within a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Function {
    /// The number of bound values, or of rows for `count(*)`.
    Count,
    /// The sum of the values that are numbers: an integer while it can be.
    Sum,
    /// The mean of the values that are numbers, or unbound if none are.
    Avg,
    /// The least and greatest values (see [`compare_cells`]).
    Min,
    Max,
    /// Some value of the group.
    Sample,
}

impl Function {
    pub fn name(self) -> &'static str {
        match self {
            Function::Count => "count",
            Function::Sum => "sum",
            Function::Avg => "avg",
            Function::Min => "min",
            Function::Max => "max",
            Function::Sample => "sample",
        }
    }

    pub fn of_name(name: &str) -> Option<Function> {
        Some(match name.to_ascii_lowercase().as_str() {
            "count" => Function::Count,
            "sum" => Function::Sum,
            "avg" => Function::Avg,
            "min" => Function::Min,
            "max" => Function::Max,
            "sample" => Function::Sample,
            _ => return None,
        })
    }
}

/// An aggregate of a column: `function(column)`, of the distinct values if
/// `distinct`.  Without a column, it is `count(*)`: the rows of the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aggregate {
    pub function: Function,
    pub column: Option<usize>,
    pub distinct: bool,
}

/// Groups rows by the values of `group_by` and folds each group with
/// `aggregates`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aggregation {
    /// The columns of the rows to group by.
    pub group_by: Vec<usize>,
    pub aggregates: Vec<Aggregate>,
}

impl Aggregation {
    /// The groups of `rows`, in the order each group first appears: for each,
    /// its values of `group_by`, then its aggregates.  With no columns to
    /// group by there is one group, even with no rows (counting 0).
    pub fn apply(&self, g: &Graph, rows: impl IntoIterator<Item = Vec<Cell>>) -> Vec<Vec<Cell>> {
        let mut order: Vec<Vec<CellKey>> = Vec::new();
        let mut groups: HashMap<Vec<CellKey>, Vec<Vec<Cell>>> = HashMap::new();
        for row in rows {
            let key: Vec<CellKey> = self.group_by.iter().map(|&c| row[c].key()).collect();
            let members = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                Vec::new()
            });
            members.push(row);
        }
        if self.group_by.is_empty() && order.is_empty() {
            order.push(Vec::new());
            groups.insert(Vec::new(), Vec::new());
        }
        order
            .into_iter()
            .map(|key| {
                let members = &groups[&key];
                let first = members.first();
                self.group_by
                    .iter()
                    .map(|&c| first.map_or(Cell::Unbound, |r| r[c].clone()))
                    .chain(self.aggregates.iter().map(|a| fold(g, a, members)))
                    .collect()
            })
            .collect()
    }
}

/// The aggregate `a` of a group's rows.
pub(crate) fn fold(g: &Graph, a: &Aggregate, rows: &[Vec<Cell>]) -> Cell {
    let Some(column) = a.column else {
        return Cell::Int(rows.len() as i64);
    };
    let mut values: Vec<&Cell> = rows
        .iter()
        .map(|r| &r[column])
        .filter(|c| **c != Cell::Unbound)
        .collect();
    if a.distinct {
        let mut seen = HashSet::new();
        values.retain(|v| seen.insert(v.key()));
    }
    match a.function {
        Function::Count => Cell::Int(values.len() as i64),
        Function::Sum | Function::Avg => {
            let numbers: Vec<Number> = values.iter().filter_map(|c| c.numeric(g)).collect();
            let sum = numbers.iter().try_fold(0i64, |sum, n| match n {
                Number::Int(n) => sum.checked_add(*n),
                Number::Float(_) => None,
            });
            match (a.function, sum) {
                (Function::Sum, Some(sum)) => Cell::Int(sum),
                (Function::Sum, None) => Cell::Float(numbers.iter().map(|n| n.as_f64()).sum()),
                _ if numbers.is_empty() => Cell::Unbound,
                _ => Cell::Float(
                    numbers.iter().map(|n| n.as_f64()).sum::<f64>() / numbers.len() as f64,
                ),
            }
        }
        Function::Min | Function::Max => {
            let best = values.iter().copied().reduce(|best, v| {
                let order = compare_cells(g, v, best);
                let better = match a.function {
                    Function::Min => order.is_lt(),
                    _ => order.is_gt(),
                };
                if better { v } else { best }
            });
            best.cloned().unwrap_or(Cell::Unbound)
        }
        Function::Sample => values.first().map_or(Cell::Unbound, |&c| c.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Options;

    fn ids(rows: Vec<Vec<u64>>) -> Vec<Vec<Cell>> {
        rows.into_iter()
            .map(|r| r.into_iter().map(Cell::of_id).collect())
            .collect()
    }

    #[test]
    fn grouping_and_folding() {
        let mut g = Graph::open(None, Options::default()).unwrap();
        let [paris, rome, n3, n4, n5, half, word] =
            ["paris", "rome", "3", "4", "5", "2.5", "word"].map(|d| g.value(d.as_bytes()).unwrap());
        // Rows of (city, value), one with the value unbound.
        let rows = ids(vec![
            vec![paris, n3],
            vec![rome, n4],
            vec![paris, n5],
            vec![paris, n3],
            vec![rome, half],
            vec![rome, word],
            vec![rome, 0],
        ]);
        let agg = |function, distinct| Aggregate {
            function,
            column: Some(1),
            distinct,
        };
        let a = Aggregation {
            group_by: vec![0],
            aggregates: vec![
                Aggregate {
                    function: Function::Count,
                    column: None,
                    distinct: false,
                },
                agg(Function::Count, false),
                agg(Function::Count, true),
                agg(Function::Sum, false),
                agg(Function::Avg, false),
                agg(Function::Min, false),
                agg(Function::Max, false),
                agg(Function::Sum, true),
            ],
        };
        let out = a.apply(&g, rows.clone());
        assert_eq!(
            out,
            [
                vec![
                    Cell::Id(paris),
                    Cell::Int(3),
                    Cell::Int(3),
                    Cell::Int(2),
                    Cell::Int(11),
                    Cell::Float(11.0 / 3.0),
                    Cell::Id(n3),
                    Cell::Id(n5),
                    Cell::Int(8),
                ],
                vec![
                    Cell::Id(rome),
                    Cell::Int(4),
                    Cell::Int(3),
                    Cell::Int(3),
                    Cell::Float(6.5),
                    Cell::Float(3.25),
                    Cell::Id(half),
                    // Numbers before words.
                    Cell::Id(word),
                    Cell::Float(6.5),
                ],
            ]
        );
        // No groups: one row, even of nothing.
        let total = Aggregation {
            group_by: vec![],
            aggregates: vec![agg(Function::Count, false), agg(Function::Avg, false)],
        };
        assert_eq!(
            total.apply(&g, rows),
            [vec![Cell::Int(6), Cell::Float(3.5)]]
        );
        assert_eq!(total.apply(&g, vec![]), [vec![Cell::Int(0), Cell::Unbound]]);
        // With groups, nothing has no groups.
        assert!(a.apply(&g, vec![]).is_empty());
        // Computed values group and fold too.
        let computed = vec![
            vec![Cell::Text("a".into()), Cell::Int(2)],
            vec![Cell::Text("a".into()), Cell::Float(0.5)],
            vec![Cell::Text("b".into()), Cell::Text("7".into())],
        ];
        let sums = Aggregation {
            group_by: vec![0],
            aggregates: vec![agg(Function::Sum, false)],
        };
        assert_eq!(
            sums.apply(&g, computed),
            [
                vec![Cell::Text("a".into()), Cell::Float(2.5)],
                vec![Cell::Text("b".into()), Cell::Int(7)],
            ]
        );
    }

    #[test]
    fn ordering_cells() {
        let mut g = Graph::open(None, Options::default()).unwrap();
        let [ten, nine, apple] = ["10", "9", "apple"].map(|d| g.value(d.as_bytes()).unwrap());
        let node = g.node().unwrap();
        let mut cells = vec![
            Cell::Id(apple),
            Cell::Int(3),
            Cell::Unbound,
            Cell::Text("banana".into()),
            Cell::Id(ten),
            Cell::Bool(true),
            Cell::Float(9.5),
            Cell::Text("4".into()),
            Cell::Id(nine),
            Cell::Bool(false),
            Cell::Id(node),
        ];
        cells.sort_by(|a, b| compare_cells(&g, a, b));
        assert_eq!(
            cells,
            [
                Cell::Unbound,
                Cell::Int(3),
                Cell::Text("4".into()),
                Cell::Id(nine),
                Cell::Float(9.5),
                Cell::Id(ten),
                Cell::Bool(false),
                Cell::Bool(true),
                Cell::Id(apple),
                Cell::Text("banana".into()),
                Cell::Id(node),
            ]
        );
    }
}
