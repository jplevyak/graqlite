//! The shell's state, and what it does with each input.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use graqlite::Linkage;
use graqlite::graph::{EntryCheck, Graph, KEY_PREDICATE};
use graqlite::graql::{self, Output, Session, Statement};
use graqlite::query::aggregate::Cell;
use graqlite::query::{Derivation, ResultLink, ResultNode};
use graqlite::times::{format_time, parse_time};

use crate::render::{Format, Namer, Style, Table, name_of};

/// The backslash commands, for help and completion.
pub const COMMANDS: &[(&str, &str)] = &[
    ("\\?", "show this help"),
    ("\\q", "quit"),
    ("\\d", "list predicates and their link counts"),
    ("\\dS", "the same, with graqlite's own predicates"),
    ("\\dr", "list the rules saved in the database"),
    ("\\subgraphs", "list the subgraphs, with their links"),
    ("\\drop NAME", "delete every definition of the rule NAME"),
    ("\\info", "summarize the database"),
    (
        "\\show ENTRY",
        "show an entry (#id, id, /key or value) and its live links",
    ),
    (
        "\\explain STMT",
        "show the query plan of a select, verify or lookup",
    ),
    ("\\begin", "start a transaction"),
    ("\\commit", "commit the transaction"),
    ("\\rollback", "discard the transaction"),
    ("\\x [on|off]", "toggle expanded output"),
    (
        "\\format [aligned|csv|tsv|tree]",
        "set or show the output format",
    ),
    ("\\ids [on|off]", "toggle showing ids beside names"),
    ("\\timing [on|off]", "toggle timing of statements"),
    (
        "\\set [NAME VALUE]",
        "set the parameter $NAME (a /key, #id or value), or list them",
    ),
    ("\\unset NAME", "remove the parameter $NAME"),
    ("\\i FILE", "run the statements in FILE"),
    (
        "\\o [FILE]",
        "send results to FILE, or back to standard output",
    ),
    ("\\dump FILE", "write a snapshot of the database to FILE"),
    (
        "\\compact ID|TIME",
        "drop the history before ID or TIME from large index entries",
    ),
];

/// The most links `\show` lists in each direction.
const SHOW_LIMIT: usize = 50;

/// How deeply `\i` files may include others.
const MAX_INCLUDE_DEPTH: usize = 16;

/// What became of an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Ok,
    /// It failed, and the error has been reported.
    Failed,
    Quit,
}

/// Where input came from, for error messages.
#[derive(Debug, Clone)]
pub struct Origin {
    /// The file's name, or `None` for interactive input.
    pub file: Option<String>,
    /// The line of the file the input starts on (from 1).
    pub line: usize,
}

impl Origin {
    pub fn interactive() -> Self {
        Origin {
            file: None,
            line: 1,
        }
    }

    /// The position `line` and `column` of the input (from 1, or 0 if not
    /// known), as a prefix for a message.
    fn at(&self, line: usize, column: usize) -> String {
        let pos = match (line, column) {
            (0, _) => String::new(),
            (l, 0) => format!("{l}:"),
            (l, c) => format!("{l}:{c}:"),
        };
        match &self.file {
            // At the prompt, a line alone is noise.
            None if pos.is_empty() || column == 0 => String::new(),
            None => format!("{pos} "),
            Some(f) if line == 0 => format!("{f}:{}: ", self.line),
            Some(f) => {
                let line = self.line + line - 1;
                if column == 0 {
                    format!("{f}:{line}: ")
                } else {
                    format!("{f}:{line}:{column}: ")
                }
            }
        }
    }
}

pub struct Shell {
    pub g: Graph,
    session: Session,
    /// The database's directory or prefix, or `None` in memory.
    location: Option<PathBuf>,
    pub style: Style,
    timing: bool,
    /// Whether a `\begin` transaction is open.
    in_transaction: bool,
    /// The time to record for every commit, from `GRAQLITE_TIME`, instead of
    /// the clock's.
    fixed_time: Option<graqlite::record::LinkTime>,
    /// The parameters statements run with: `$name`.
    params: graql::Params,
    out: Box<dyn Write>,
    depth: usize,
}

impl Shell {
    pub fn new(g: Graph, location: Option<PathBuf>, style: Style) -> Self {
        Shell {
            g,
            session: Session::new(),
            location,
            style,
            timing: false,
            in_transaction: false,
            fixed_time: None,
            params: graql::Params::new(),
            out: Box::new(io::stdout()),
            depth: 0,
        }
    }

    /// Records `time` for every commit, instead of the clock's.
    pub fn fix_time(&mut self, time: graqlite::record::LinkTime) {
        self.fixed_time = Some(time);
    }

    /// Commits, then brings the subgraphs the session maintains up to date
    /// and commits what that writes.
    fn commit_graph(&mut self) -> Result<(), String> {
        let commit = |shell: &mut Self| match shell.fixed_time {
            Some(t) => shell.g.commit_at(t),
            None => shell.g.commit(),
        };
        commit(self).map_err(|e| e.to_string())?;
        if self.session.maintained().is_empty() {
            return Ok(());
        }
        let before = self.g.next_id();
        self.session
            .update(&mut self.g)
            .map_err(|e| format!("updating maintained subgraphs: {e}"))?;
        if self.g.next_id() != before {
            commit(self).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    /// The database's name for the prompt.
    pub fn name(&self) -> String {
        match &self.location {
            None => "memory".into(),
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().trim_end_matches('.').to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| p.display().to_string()),
        }
    }

    /// Runs one input: a backslash command, or GraQL statements.
    pub fn input(&mut self, text: &str, origin: &Origin) -> Step {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Step::Ok;
        }
        let step = if let Some(command) = trimmed.strip_prefix('\\') {
            self.command(command, origin)
        } else {
            self.graql(text, origin)
        };
        if let Err(e) = self.out.flush() {
            eprintln!("error: writing output: {e}");
        }
        step
    }

    /// Runs a script: backslash commands on lines of their own, and GraQL
    /// statements, run when a blank line, a command or the end of the
    /// script ends them.  Stops at the first input that fails.
    pub fn script(&mut self, text: &str, file: &str) -> Step {
        let mut chunk = String::new();
        let mut start = 1;
        let lines: Vec<&str> = text.lines().collect();
        for (i, &line) in lines.iter().enumerate() {
            let number = i + 1;
            let in_statement = !chunk.is_empty() && graql::is_incomplete(&chunk);
            let ends = line.trim().is_empty() || line.trim_start().starts_with('\\');
            if ends && !in_statement {
                let step = self.flush(&mut chunk, file, start);
                if step != Step::Ok {
                    return step;
                }
                if line.trim_start().starts_with('\\') {
                    let origin = Origin {
                        file: Some(file.into()),
                        line: number,
                    };
                    let step = self.input(line, &origin);
                    if step != Step::Ok {
                        return step;
                    }
                }
                continue;
            }
            if chunk.is_empty() {
                start = number;
            }
            chunk.push_str(line);
            chunk.push('\n');
        }
        self.flush(&mut chunk, file, start)
    }

    fn flush(&mut self, chunk: &mut String, file: &str, line: usize) -> Step {
        let text = std::mem::take(chunk);
        let origin = Origin {
            file: Some(file.into()),
            line,
        };
        self.input(&text, &origin)
    }

    fn error(&self, origin: &Origin, line: usize, column: usize, message: impl std::fmt::Display) {
        eprintln!("{}error: {message}", origin.at(line, column));
    }

    /// Reports a GraQL error at its position in the input, or if it has
    /// none, on `line`: the line of the statement it is from, or 0.
    fn graql_error(&self, origin: &Origin, line: usize, e: &graql::Error) {
        let at = |l: usize, c: usize| if l == 0 { (line, 0) } else { (l, c) };
        match e {
            graql::Error::Syntax {
                line,
                column,
                message,
            } => {
                let (l, c) = at(*line, *column);
                self.error(origin, l, c, message)
            }
            graql::Error::Unsupported { line, column, what } => {
                let (l, c) = at(*line, *column);
                self.error(origin, l, c, format_args!("{what} is not supported"))
            }
            graql::Error::Invalid(message) => self.error(origin, line, 0, message),
            graql::Error::Graph(e) => self.error(origin, line, 0, e),
        }
    }

    fn graql(&mut self, text: &str, origin: &Origin) -> Step {
        let symbols: Vec<String> = self.session.symbols().map(String::from).collect();
        let statements = match graql::parse_with_symbols(text, symbols) {
            Ok(s) => s,
            Err(e) => {
                self.graql_error(origin, 0, &e);
                return Step::Failed;
            }
        };
        for (line, statement) in &statements {
            let line = *line;
            let start = Instant::now();
            let before = self.g.next_id();
            let result = self.session.run_with(&mut self.g, statement, &self.params);
            let changed = self.g.next_id() != before;
            match result {
                Ok(output) => {
                    if changed
                        && !self.in_transaction
                        && let Err(e) = self.commit_graph()
                    {
                        self.error(origin, line, 0, format_args!("commit failed: {e}"));
                        return Step::Failed;
                    }
                    let elapsed = start.elapsed();
                    if let Err(e) = self.output(&output) {
                        self.error(origin, line, 0, format_args!("writing output: {e}"));
                        return Step::Failed;
                    }
                    if self.timing {
                        let _ = writeln!(self.out, "Time: {:.3} ms", elapsed.as_secs_f64() * 1e3);
                    }
                }
                Err(e) => {
                    self.graql_error(origin, line, &e);
                    // A failed statement may have appended part of its changes.
                    if self.in_transaction {
                        self.in_transaction = false;
                        self.rollback(origin);
                        eprintln!("the transaction was rolled back");
                    } else if changed {
                        self.rollback(origin);
                    }
                    return Step::Failed;
                }
            }
        }
        Step::Ok
    }

    fn rollback(&mut self, origin: &Origin) -> bool {
        match self.g.rollback() {
            Ok(()) => true,
            Err(e) => {
                self.error(origin, 0, 0, format_args!("rollback failed: {e}"));
                false
            }
        }
    }

    fn output(&mut self, output: &Output) -> io::Result<()> {
        let mut namer = Namer::new(&self.g, self.style);
        match output {
            Output::Rows { vars, rows } => {
                let table = Table {
                    columns: vars.clone(),
                    rows: rows
                        .iter()
                        .map(|row| row.iter().map(|&id| namer.name(id)).collect())
                        .collect(),
                };
                table.write(&mut *self.out, self.style)
            }
            Output::Graph(graph) => {
                let mut rows = Vec::new();
                for id in graph.stored() {
                    let Some((s, p, o)) = self.g.get(id).ok().and_then(|r| {
                        r.as_link()
                            .map(|l| (l.subject(), l.predicate(), l.object()))
                    }) else {
                        continue;
                    };
                    rows.push(vec![
                        format!("#{id}"),
                        namer.name(s),
                        namer.name(p),
                        namer.name(o),
                    ]);
                }
                // Derived links have no id; they say how they were derived,
                // and from which stored links.
                let mut derived = Vec::new();
                for l in graph.derived() {
                    let ResultLink::Derived {
                        fields,
                        by,
                        support,
                    } = l
                    else {
                        continue;
                    };
                    let mut field = |f: &ResultNode| match f {
                        ResultNode::Entry(id) => namer.name(*id),
                        // A rule's name, as a predicate, or a value not
                        // in the graph.
                        ResultNode::Value(Cell::Text(t)) => graql::literal(t),
                        ResultNode::Key(k) => k.clone(),
                        ResultNode::Blank(n) => format!("_:{n}"),
                        ResultNode::Path(e) => {
                            let mut ids = Vec::new();
                            e.predicates(&mut ids);
                            let names: std::collections::HashMap<u64, String> =
                                ids.into_iter().map(|id| (id, namer.name(id))).collect();
                            format!("({})", e.display(&|id| names[&id].clone()))
                        }
                        other => format!("{other:?}"),
                    };
                    let how = match by {
                        Derivation::Path => "path".to_string(),
                        Derivation::Rule(name) => format!("rule {name}"),
                        Derivation::Asserted => "asserted".to_string(),
                        Derivation::Overlay => "overlay".to_string(),
                        Derivation::Aggregate(name) => name.to_ascii_lowercase(),
                        other => format!("{other:?}"),
                    };
                    let from: Vec<String> = support.iter().map(|id| format!("#{id}")).collect();
                    let mut row: Vec<String> = std::iter::once(String::new())
                        .chain(fields.iter().map(&mut field))
                        .collect();
                    // A transitive path's predicate, as written: `p++`.
                    if *by == Derivation::Path && matches!(fields[1], ResultNode::Entry(_)) {
                        row[2].push_str("++");
                    }
                    row.push(if from.is_empty() {
                        how
                    } else {
                        format!("{how} from {}", from.join(" "))
                    });
                    derived.push(row);
                }
                // The entries bound that aren't in a link: a row each.
                for &id in &graph.nodes {
                    rows.push(vec![
                        String::new(),
                        namer.name(id),
                        String::new(),
                        String::new(),
                    ]);
                }
                let mut columns: Vec<String> = ["link", "subject", "predicate", "object"]
                    .map(String::from)
                    .to_vec();
                if !derived.is_empty() {
                    columns.push("derived".into());
                    for row in &mut rows {
                        row.push(String::new());
                    }
                    rows.extend(derived);
                }
                let table = Table { columns, rows };
                table.write(&mut *self.out, self.style)
            }
            Output::Table { vars, rows } => {
                let table = Table {
                    columns: vars.clone(),
                    rows: rows
                        .iter()
                        .map(|row| {
                            row.iter()
                                .map(|cell| match cell {
                                    Cell::Id(id) => namer.name(*id),
                                    // Text as a literal, unless the format is raw.
                                    Cell::Text(t) if self.style.format == Format::Aligned => {
                                        graql::literal(t)
                                    }
                                    other => other.to_string(),
                                })
                                .collect()
                        })
                        .collect(),
                };
                table.write(&mut *self.out, self.style)
            }
            Output::Verified(found) => writeln!(self.out, "{found}"),
            Output::Done(what) => writeln!(self.out, "{what}"),
            Output::Inserted(n) => writeln!(self.out, "INSERT {n}"),
            Output::Deleted(n) => writeln!(self.out, "DELETE {n}"),
            Output::Defined(name) => writeln!(self.out, "RULE {name}"),
            Output::Default { what, id } => writeln!(self.out, "{what} {}", namer.name(*id)),
            Output::Maintained {
                subgraph,
                added,
                removed,
            } => writeln!(
                self.out,
                "MAINTAIN {} (+{added} -{removed})",
                namer.name(*subgraph)
            ),
            Output::DeletedRule { name, definitions } => writeln!(
                self.out,
                "DELETE RULE {name} ({definitions} definition{})",
                if *definitions == 1 { "" } else { "s" }
            ),
        }
    }

    fn command(&mut self, text: &str, origin: &Origin) -> Step {
        let (name, arg) = match text.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (text, ""),
        };
        let result = match name {
            "?" | "help" => self.help(),
            "q" | "quit" => return Step::Quit,
            "d" => self.predicates(false),
            "dS" => self.predicates(true),
            "dr" => self.rules(),
            "subgraphs" => self.subgraphs(),
            "drop" => return self.drop_rule(arg, origin),
            "info" => self.info(),
            "show" => self.show(arg),
            "explain" => self.explain(arg, origin),
            "begin" => self.begin(),
            "commit" => self.commit(),
            "rollback" => self.abort(origin),
            "x" => toggle(&mut self.style.expanded, arg, "expanded output"),
            "ids" => toggle(&mut self.style.ids, arg, "showing ids"),
            "timing" => toggle(&mut self.timing, arg, "timing"),
            "set" => self.set(arg),
            "unset" => match self.params.remove(arg.trim_start_matches('$')) {
                Some(_) => Ok(String::new()),
                None => Err(format!("${} is not set", arg.trim_start_matches('$'))),
            },
            "format" => self.format(arg),
            "i" => return self.include(arg, origin),
            "o" => self.redirect(arg),
            "dump" => self.dump(arg),
            "compact" => self.compact(arg),
            _ => Err(format!("unknown command \\{name}; try \\?")),
        };
        match result {
            Ok(message) => {
                if !message.is_empty() {
                    eprintln!("{message}");
                }
                Step::Ok
            }
            Err(message) => {
                self.error(origin, 0, 0, message);
                Step::Failed
            }
        }
    }

    fn help(&mut self) -> CommandResult {
        let width = COMMANDS.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
        let mut text = String::from(
            "Enter GraQL statements, or a backslash command.  A statement runs when\n\
             it is complete; a line ending in '.', ';', ',', 'and' or 'or' continues,\n\
             and a blank line runs what has been entered.  Changes are committed\n\
             after each statement, unless in a \\begin transaction.\n\n",
        );
        for (command, what) in COMMANDS {
            text.push_str(&format!("  {command:width$}  {what}\n"));
        }
        write!(self.out, "{text}").map_err(|e| e.to_string())?;
        Ok(String::new())
    }

    /// Lists the predicates, leaving out graqlite's own (those whose data
    /// starts `graqlite:`) unless `all`.
    fn predicates(&mut self, all: bool) -> CommandResult {
        let stats = self.g.stats();
        let g = &self.g;
        let mut predicates: Vec<_> = stats
            .predicates()
            .filter(|(_, s)| s.live() > 0)
            .filter(|&(p, _)| all || !g.data(p).is_some_and(|d| d.starts_with(b"graqlite:")))
            .map(|(p, s)| (p, s.live(), s.subjects.len(), s.objects.len()))
            .collect();
        predicates.sort_by_key(|&(p, live, ..)| (std::cmp::Reverse(live), p));
        let mut namer = Namer::new(&self.g, self.style);
        let table = Table {
            columns: ["predicate", "links", "subjects", "objects"]
                .map(String::from)
                .to_vec(),
            rows: predicates
                .into_iter()
                .map(|(p, live, s, o)| {
                    vec![
                        namer.name(p),
                        live.to_string(),
                        format!("~{s}"),
                        format!("~{o}"),
                    ]
                })
                .collect(),
        };
        table
            .write(&mut *self.out, self.style)
            .map_err(|e| e.to_string())?;
        Ok(String::new())
    }

    fn rules(&mut self) -> CommandResult {
        let rows = self
            .session
            .rules(&self.g)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|r| {
                let head: Vec<String> = r.head.iter().map(|v| format!("?{v}")).collect();
                vec![
                    format!("{}({})", r.name, head.join(" ")),
                    r.body.to_string(),
                ]
            })
            .collect();
        let table = Table {
            columns: vec!["rule".into(), "body".into()],
            rows,
        };
        table
            .write(&mut *self.out, self.style)
            .map_err(|e| e.to_string())?;
        self.warn_skipped();
        Ok(String::new())
    }

    /// Reports the saved text that does not define a rule.
    pub fn warn_skipped(&mut self) {
        let Ok(skipped) = self.session.skipped_rules(&self.g) else {
            return;
        };
        for s in skipped {
            eprintln!(
                "warning: saved rule #{} is left out: {}\n  {}",
                s.link, s.error, s.text
            );
        }
    }

    fn drop_rule(&mut self, arg: &str, origin: &Origin) -> Step {
        let valid = arg
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && arg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            self.error(origin, 0, 0, "\\drop needs a rule's name");
            return Step::Failed;
        }
        self.graql(&format!("delete rule {arg}"), origin)
    }

    fn info(&mut self) -> CommandResult {
        let stats = self.g.stats();
        let location = match &self.location {
            Some(p) => p.display().to_string(),
            None => "in memory".into(),
        };
        let times = self.g.times();
        let history = match times.span() {
            Some((first, last)) => format!(
                "{} commit{}, {} to {}",
                times.len(),
                if times.len() == 1 { "" } else { "s" },
                format_time(first),
                format_time(last)
            ),
            None => "no commits".into(),
        };
        let text = format!(
            "database:     {location}\n\
             ids:          {} to {}\n\
             history:      {history}\n\
             links:        {} live, {} deleted\n\
             predicates:   {}\n\
             rules:        {}\n\
             transaction:  {}\n{}",
            self.g.start_id(),
            self.g.next_id(),
            stats.live(),
            stats.dead(),
            stats.predicate_count(),
            self.session.rules(&self.g).map_or(0, |r| r.len()),
            if self.in_transaction { "open" } else { "none" },
            match self.g.horizon() {
                0 => String::new(),
                h => format!("compacted:    to {h}, so as of earlier fails\n"),
            },
        );
        write!(self.out, "{text}").map_err(|e| e.to_string())?;
        // Each table's postings have a header page; more means trees.
        let (pages, free) = self.g.local().postings_pages();
        if pages > self.g.local().table_count() as u64 {
            writeln!(
                self.out,
                "postings:     {pages} pages of {} KiB, {free} free",
                graqlite::postings::PAGE / 1024
            )
            .map_err(|e| e.to_string())?;
        }
        let mut namer = Namer::new(&self.g, self.style);
        if let Some(a) = self.session.author() {
            writeln!(self.out, "author:       {}", namer.name(a)).map_err(|e| e.to_string())?;
        }
        if let Some(sg) = self.session.subgraph() {
            writeln!(self.out, "subgraph:     {}", namer.name(sg)).map_err(|e| e.to_string())?;
        }
        Ok(String::new())
    }

    /// Lists the subgraphs, with the facts in each: from every live link,
    /// since the Subgraph index can't be listed.
    fn subgraphs(&mut self) -> CommandResult {
        let g = &self.g;
        let mut counts: std::collections::HashMap<u64, u64> = Default::default();
        for (p, _) in g.stats().predicates() {
            for id in g.live(&g.links_at(Linkage::Predicate, [p]), None) {
                if let Some(subgraphs) = g
                    .get(id)
                    .ok()
                    .and_then(|r| r.as_link().map(|l| l.subgraphs()))
                {
                    for sg in subgraphs {
                        *counts.entry(sg).or_default() += 1;
                    }
                }
            }
        }
        let mut counts: Vec<(u64, u64)> = counts.into_iter().collect();
        counts.sort_by_key(|&(sg, n)| (std::cmp::Reverse(n), sg));
        let mut namer = Namer::new(&self.g, self.style);
        let table = Table {
            columns: ["subgraph", "links"].map(String::from).to_vec(),
            rows: counts
                .into_iter()
                .map(|(sg, n)| vec![namer.name(sg), n.to_string()])
                .collect(),
        };
        table
            .write(&mut *self.out, self.style)
            .map_err(|e| e.to_string())?;
        Ok(String::new())
    }

    /// The id that `\show`'s argument names, and whether the indexes
    /// could confirm an entry starts there.
    fn entry(&self, arg: &str) -> Result<(u64, bool), String> {
        let not_found = || format!("no entry {arg}");
        if arg.is_empty() {
            return Err("\\show needs an entry: #id, id, /key or value".into());
        }
        let digits = arg.strip_prefix('#').unwrap_or(arg);
        if let Ok(id) = digits.parse::<u64>()
            && (arg.starts_with('#') || self.g.find_value(arg.as_bytes()).is_none())
        {
            return match self.g.check_entry(id) {
                EntryCheck::Entry => Ok((id, true)),
                EntryCheck::Unconfirmed => Ok((id, false)),
                EntryCheck::NotEntry => Err(format!("no entry starts at {id}")),
            };
        }
        if arg.starts_with('/') {
            return self
                .g
                .find_by_key(arg.as_bytes())
                .map(|id| (id, true))
                .ok_or_else(not_found);
        }
        let data = arg
            .strip_prefix('"')
            .and_then(|a| a.strip_suffix('"'))
            .unwrap_or(arg);
        self.g
            .find_value(data.as_bytes())
            .map(|id| (id, true))
            .ok_or_else(not_found)
    }

    fn show(&mut self, arg: &str) -> CommandResult {
        let (id, confirmed) = self.entry(arg)?;
        let record = self.g.get(id).map_err(|e| e.to_string())?;
        let mut namer = Namer::new(&self.g, self.style);
        let mut text = String::new();
        if !confirmed {
            text.push_str(&format!(
                "warning: no key or link names #{id}, so it may be a zero byte within another entry\n"
            ));
        }
        if let Some(link) = record.as_link() {
            let (s, p, o) = (link.subject(), link.predicate(), link.object());
            text.push_str(&format!(
                "#{id} link{}: {} {} {}\n",
                if link.dead() { " (deleted)" } else { "" },
                namer.name(s),
                namer.name(p),
                namer.name(o)
            ));
        } else if record.is_value() {
            text.push_str(&format!("#{id} value {}\n", name_of(&self.g, id, true)));
        } else {
            let key = self.g.key(id).map(String::from_utf8_lossy);
            match key {
                Some(k) => text.push_str(&format!("#{id} node {k}\n")),
                None => text.push_str(&format!("#{id} node\n")),
            }
        }
        match self.g.time_of(id) {
            Some(t) => text.push_str(&format!("committed {}\n", format_time(t))),
            None => text.push_str("not committed\n"),
        }
        if let Some(stats) = self.g.stats().predicate(id)
            && stats.live() > 0
        {
            text.push_str(&format!("predicate of {} live links\n", stats.live()));
        }
        let key_predicate = self.g.find_value(KEY_PREDICATE);
        for (linkage, heading) in [(Linkage::Subject, "out"), (Linkage::Object, "in")] {
            let links = self.g.live(&self.g.links_at(linkage, [id]), None);
            let rows: Vec<(u64, u64, u64)> = links
                .iter()
                .filter_map(|&l| {
                    let r = self.g.get(l).ok()?;
                    let l = r.as_link()?;
                    Some((l.subject(), l.predicate(), l.object()))
                })
                .filter(|&(_, p, _)| Some(p) != key_predicate)
                .collect();
            if rows.is_empty() {
                continue;
            }
            text.push_str(&format!("links {heading} ({}):\n", rows.len()));
            for &(s, p, o) in rows.iter().take(SHOW_LIMIT) {
                let line = match linkage {
                    Linkage::Subject => format!("  {} {}\n", namer.name(p), namer.name(o)),
                    _ => format!("  {} {}\n", namer.name(s), namer.name(p)),
                };
                text.push_str(&line);
            }
            if rows.len() > SHOW_LIMIT {
                text.push_str(&format!("  ... and {} more\n", rows.len() - SHOW_LIMIT));
            }
        }
        write!(self.out, "{text}").map_err(|e| e.to_string())?;
        Ok(String::new())
    }

    fn explain(&mut self, arg: &str, origin: &Origin) -> CommandResult {
        if arg.is_empty() {
            return Err("\\explain needs a select, verify or lookup statement".into());
        }
        let statements = graql::parse(arg).map_err(|e| e.to_string())?;
        for statement in &statements {
            if !matches!(
                statement,
                Statement::Select { .. } | Statement::Verify { .. } | Statement::Lookup { .. }
            ) {
                return Err("only select, verify and lookup statements have plans".into());
            }
            let params = &self.params;
            let plan = statement
                .bind(params)
                .and_then(|statement| {
                    let rules = self.session.rules(&self.g)?;
                    statement.query_with(&self.g, rules)
                })
                .and_then(|(q, _)| Ok(q.plan(&self.g)?));
            match plan {
                Ok(plan) => {
                    let ids = self.style.ids;
                    let g = &self.g;
                    let text = plan.explain_with(&|id| {
                        let name = name_of(g, id, true);
                        if ids && !name.starts_with('#') {
                            format!("{name} #{id}")
                        } else {
                            name
                        }
                    });
                    write!(self.out, "{text}").map_err(|e| e.to_string())?
                }
                Err(e) => {
                    self.graql_error(origin, 0, &e);
                    return Err("no plan".into());
                }
            }
        }
        Ok(String::new())
    }

    fn begin(&mut self) -> CommandResult {
        if self.in_transaction {
            return Err("a transaction is already open".into());
        }
        // A rollback discards everything since the last commit, so begin
        // from a commit.
        self.commit_graph().map_err(|e| e.to_string())?;
        self.in_transaction = true;
        Ok("BEGIN".into())
    }

    fn commit(&mut self) -> CommandResult {
        if !self.in_transaction {
            return Err("no transaction is open".into());
        }
        self.commit_graph().map_err(|e| e.to_string())?;
        self.in_transaction = false;
        self.session.end_transaction();
        Ok("COMMIT".into())
    }

    fn abort(&mut self, origin: &Origin) -> CommandResult {
        if !self.in_transaction {
            return Err("no transaction is open".into());
        }
        self.in_transaction = false;
        self.session.end_transaction();
        if !self.rollback(origin) {
            return Err("the transaction may be partly kept".into());
        }
        Ok("ROLLBACK".into())
    }

    /// Ends the session, discarding an open transaction.
    pub fn close(&mut self) {
        if self.in_transaction {
            self.in_transaction = false;
            if self.g.rollback().is_ok() {
                eprintln!("the open transaction was rolled back");
            }
        }
        let _ = self.out.flush();
    }

    /// Sets a parameter from `NAME VALUE`, or lists them if `arg` is empty.
    pub fn set(&mut self, arg: &str) -> CommandResult {
        if arg.is_empty() {
            let mut names: Vec<&String> = self.params.keys().collect();
            names.sort();
            let rows = names
                .into_iter()
                .map(|n| {
                    let value = match &self.params[n] {
                        graql::Param::Key(k) => k.clone(),
                        graql::Param::Value(v) => graql::literal(v),
                        graql::Param::Id(id) => format!("#{id}"),
                    };
                    vec![format!("${n}"), value]
                })
                .collect();
            let table = Table {
                columns: vec!["parameter".into(), "value".into()],
                rows,
            };
            table
                .write(&mut *self.out, self.style)
                .map_err(|e| e.to_string())?;
            return Ok(String::new());
        }
        let (name, value) = arg
            .split_once(char::is_whitespace)
            .ok_or("\\set needs a name and a value")?;
        let name = name.trim_start_matches('$');
        if !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!("{name} is not a parameter's name"));
        }
        let param = graql::Param::parse(value.trim()).map_err(|e| e.to_string())?;
        self.params.insert(name.to_string(), param);
        Ok(String::new())
    }

    fn format(&mut self, arg: &str) -> CommandResult {
        if !arg.is_empty() {
            self.style.format = Format::parse(arg)
                .ok_or_else(|| format!("unknown format {arg}; use aligned, csv, tsv or tree"))?;
        }
        Ok(format!("output format is {}", self.style.format.name()))
    }

    fn include(&mut self, arg: &str, origin: &Origin) -> Step {
        if arg.is_empty() {
            self.error(origin, 0, 0, "\\i needs a file");
            return Step::Failed;
        }
        if self.depth >= MAX_INCLUDE_DEPTH {
            self.error(origin, 0, 0, "\\i files are nested too deeply");
            return Step::Failed;
        }
        let text = match std::fs::read_to_string(arg) {
            Ok(t) => t,
            Err(e) => {
                self.error(origin, 0, 0, format_args!("reading {arg}: {e}"));
                return Step::Failed;
            }
        };
        self.depth += 1;
        let step = self.script(&text, arg);
        self.depth -= 1;
        step
    }

    fn redirect(&mut self, arg: &str) -> CommandResult {
        self.out.flush().map_err(|e| e.to_string())?;
        self.out = if arg.is_empty() {
            Box::new(io::stdout())
        } else {
            let file = File::create(arg).map_err(|e| format!("creating {arg}: {e}"))?;
            Box::new(BufWriter::new(file))
        };
        Ok(String::new())
    }

    fn compact(&mut self, arg: &str) -> CommandResult {
        let arg = arg.trim_matches('"');
        let horizon = match arg.parse::<u64>() {
            Ok(id) => id,
            Err(_) => match parse_time(arg) {
                Some(t) => self.g.id_at_time(t),
                None => return Err("\\compact needs an id or an ISO 8601 time".into()),
            },
        };
        let done = self.g.compact(horizon).map_err(|e| e.to_string())?;
        Ok(format!(
            "compacted to {2}: dropped {0} ids from {1} large index entries",
            done.dropped,
            done.trees,
            self.g.horizon()
        ))
    }

    fn dump(&mut self, arg: &str) -> CommandResult {
        if arg.is_empty() {
            return Err("\\dump needs a file".into());
        }
        let path = Path::new(arg);
        let mut out =
            BufWriter::new(File::create(path).map_err(|e| format!("creating {arg}: {e}"))?);
        self.g.snapshot(&mut out).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        Ok(format!("wrote a snapshot to {arg}"))
    }
}

/// A command's message for standard error, or its error.
type CommandResult = Result<String, String>;

fn toggle(flag: &mut bool, arg: &str, what: &str) -> CommandResult {
    *flag = match arg {
        "" => !*flag,
        "on" => true,
        "off" => false,
        _ => return Err(format!("expected on or off, not {arg}")),
    };
    Ok(format!("{what} is {}", if *flag { "on" } else { "off" }))
}
