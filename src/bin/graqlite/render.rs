//! Showing ids as text, and results as tables.

use std::collections::HashMap;
use std::io::{self, Write};

use graqlite::graph::Graph;
use graqlite::graql;

/// How results are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Columns padded to line up, between `|`s.
    Aligned,
    Csv,
    Tsv,
    /// A forest: each row's first value a root, with its later values
    /// under it, rows with the same values before sharing them (GraQL's
    /// "forest of result trees dangled by the result order").
    Tree,
}

impl Format {
    pub fn parse(name: &str) -> Option<Format> {
        match name {
            "aligned" => Some(Format::Aligned),
            "csv" => Some(Format::Csv),
            "tsv" => Some(Format::Tsv),
            "tree" => Some(Format::Tree),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Aligned => "aligned",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Tree => "tree",
        }
    }
}

/// Settings for printing results.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub format: Format,
    /// One line per field, a block per row.
    pub expanded: bool,
    /// Show each entry's id beside its name.
    pub ids: bool,
}

/// Turns ids into text, remembering the names it has looked up.
pub struct Namer<'g> {
    g: &'g Graph,
    /// Write values as GraQL literals (quoting strings), rather than as
    /// their raw data.
    literal: bool,
    ids: bool,
    names: HashMap<u64, String>,
}

impl<'g> Namer<'g> {
    pub fn new(g: &'g Graph, style: Style) -> Self {
        Namer {
            g,
            literal: style.format == Format::Aligned,
            ids: style.ids,
            names: HashMap::new(),
        }
    }

    /// The name of `id`: its key, the data of a value, or `#id` for other
    /// entries.
    pub fn name(&mut self, id: u64) -> String {
        if let Some(name) = self.names.get(&id) {
            return name.clone();
        }
        let base = name_of(self.g, id, self.literal);
        let name = if self.ids && !base.starts_with('#') {
            format!("{base} #{id}")
        } else {
            base
        };
        self.names.insert(id, name.clone());
        name
    }
}

/// The name of `id` in `g`; see [`Namer::name`].
pub fn name_of(g: &Graph, id: u64, literal: bool) -> String {
    // Unbound, from an optional pattern.
    if id == 0 {
        return String::new();
    }
    if let Some(key) = g.key(id)
        && let Ok(key) = std::str::from_utf8(&key)
    {
        return key.to_string();
    }
    let data = g.data(id);
    if let Some(v) = data.as_deref().and_then(graqlite::vector::decode) {
        return graqlite::vector::format(&v);
    }
    match data.as_deref().map(std::str::from_utf8) {
        Some(Ok(text)) if literal => graql::literal(text),
        Some(Ok(text)) => text.to_string(),
        // A node or link held in memory (see `Graph::ephemeral_node`).
        _ if id & graqlite::graph::EPHEMERAL != 0 => {
            format!("#~{}", id & !graqlite::graph::EPHEMERAL)
        }
        _ => format!("#{id}"),
    }
}

/// A table of text cells.
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// Writes the table, and in the aligned format, a count of its rows.
    pub fn write(&self, out: &mut dyn Write, style: Style) -> io::Result<()> {
        match (style.format, style.expanded) {
            (Format::Aligned, false) => self.write_aligned(out)?,
            (Format::Aligned, true) => self.write_expanded(out)?,
            (Format::Csv, _) => self.write_separated(out, ',', csv_field)?,
            (Format::Tsv, _) => self.write_separated(out, '\t', tsv_field)?,
            (Format::Tree, _) => self.write_tree(out)?,
        }
        if style.format == Format::Aligned {
            let n = self.rows.len();
            writeln!(out, "({n} row{})", if n == 1 { "" } else { "s" })?;
        }
        Ok(())
    }

    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.columns.iter().map(|c| width(c)).collect();
        for row in &self.rows {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(width(cell));
            }
        }
        widths
    }

    fn write_aligned(&self, out: &mut dyn Write) -> io::Result<()> {
        let widths = self.widths();
        let line = |cells: &[String]| {
            let padded: Vec<String> = cells
                .iter()
                .zip(&widths)
                .map(|(c, &w)| format!(" {c}{} ", " ".repeat(w - width(c))))
                .collect();
            padded.join("|").trim_end().to_string()
        };
        writeln!(out, "{}", line(&self.columns))?;
        let rule: Vec<String> = widths.iter().map(|&w| "-".repeat(w + 2)).collect();
        writeln!(out, "{}", rule.join("+"))?;
        for row in &self.rows {
            writeln!(out, "{}", line(row))?;
        }
        Ok(())
    }

    /// The rows as a forest, in their order: a value is written, indented
    /// by its column, where it or a value before it differs from the row
    /// before's.
    fn write_tree(&self, out: &mut dyn Write) -> io::Result<()> {
        let mut last: Option<&Vec<String>> = None;
        for row in &self.rows {
            let same = last.map_or(0, |l| row.iter().zip(l).take_while(|(a, b)| a == b).count());
            for (depth, value) in row.iter().enumerate().skip(same) {
                writeln!(out, "{}{value}", "  ".repeat(depth))?;
            }
            last = Some(row);
        }
        Ok(())
    }

    fn write_expanded(&self, out: &mut dyn Write) -> io::Result<()> {
        let w = self.columns.iter().map(|c| width(c)).max().unwrap_or(0);
        for (i, row) in self.rows.iter().enumerate() {
            writeln!(
                out,
                "-[ RECORD {} ]{}",
                i + 1,
                "-".repeat(w.saturating_sub(4))
            )?;
            for (column, cell) in self.columns.iter().zip(row) {
                writeln!(out, "{column}{} | {cell}", " ".repeat(w - width(column)))?;
            }
        }
        Ok(())
    }

    fn write_separated(
        &self,
        out: &mut dyn Write,
        separator: char,
        field: fn(&str) -> String,
    ) -> io::Result<()> {
        let line = |cells: &[String]| {
            let fields: Vec<String> = cells.iter().map(|c| field(c)).collect();
            fields.join(&separator.to_string())
        };
        writeln!(out, "{}", line(&self.columns))?;
        for row in &self.rows {
            writeln!(out, "{}", line(row))?;
        }
        Ok(())
    }
}

/// The display width of `s`, counting each character as one column.
fn width(s: &str) -> usize {
    s.chars().count()
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn tsv_field(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        Table {
            columns: vec!["who".into(), "age".into()],
            rows: vec![
                vec!["/bob".into(), "25".into()],
                vec!["\"a, b\"".into(), "7".into()],
            ],
        }
    }

    fn written(style: Style) -> String {
        let mut out = Vec::new();
        table().write(&mut out, style).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn style(format: Format, expanded: bool) -> Style {
        Style {
            format,
            expanded,
            ids: false,
        }
    }

    #[test]
    fn aligned() {
        assert_eq!(
            written(style(Format::Aligned, false)),
            " who    | age\n\
             --------+-----\n \
             /bob   | 25\n \
             \"a, b\" | 7\n\
             (2 rows)\n"
        );
    }

    #[test]
    fn expanded() {
        assert_eq!(
            written(style(Format::Aligned, true)),
            "-[ RECORD 1 ]\nwho | /bob\nage | 25\n-[ RECORD 2 ]\nwho | \"a, b\"\nage | 7\n(2 rows)\n"
        );
    }

    #[test]
    fn separated() {
        assert_eq!(
            written(style(Format::Csv, false)),
            "who,age\n/bob,25\n\"\"\"a, b\"\"\",7\n"
        );
        assert_eq!(
            written(style(Format::Tsv, false)),
            "who\tage\n/bob\t25\n\"a, b\"\t7\n"
        );
    }
}
