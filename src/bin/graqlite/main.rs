//! `graqlite`: an interactive shell for graqlite graphs, in the manner of
//! `psql`.  Run `graqlite --help` for its options, and `\?` in it for its
//! commands.

mod render;
mod shell;

use std::fs::{self, File};
use std::io::{self, BufReader, IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use graqlite::graph::{self, Graph, Options, Synchronous};
use graqlite::graql;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Editor, Helper};

use render::{Format, Style};
use shell::{COMMANDS, Origin, Shell, Step};

const USAGE: &str = "\
Usage: graqlite [OPTIONS] [DIR]

An interactive shell for a graqlite graph database.  DIR is the directory
holding the database, created if it does not exist; without it the
database is in memory and lost on exit.

Options:
  -c, --command TEXT    run TEXT (GraQL or a backslash command), then exit;
                        may be given more than once
  -f, --file FILE       run the statements in FILE, then exit; may be given
                        more than once, and '-' reads standard input
  -1, --single-transaction
                        run the -c and -f inputs in one transaction
  -v, --set NAME=VALUE  set the parameter $NAME (a /key, #id or value)
      --prefix PREFIX   open the database whose files start with PREFIX,
                        instead of one in DIR
      --restore FILE    create the database from the snapshot in FILE
                        (see \\dump); it must not exist already
      --synchronous LEVEL
                        what a commit waits for: full (the default), normal
                        (the log; after a crash, opening rebuilds the
                        indexes) or off (nothing; a power cut may lose the
                        database)
      --csv             print results as CSV
      --tsv             print results as tab separated values
  -x, --expanded        print each field of a row on a line of its own
  -q, --quiet           print no welcome message
  -h, --help            show this help
  -V, --version         show the version

Without -c or -f, the shell reads statements from the terminal, or runs
standard input as a script when it is not a terminal.  History is kept in
$GRAQLITE_HISTORY, or ~/.graqlite_history.  $GRAQLITE_TIME, an ISO 8601 time,
is recorded as the time of every commit instead of the clock's, for output
that is the same each run.
";

/// The file prefix of a database in a directory.
const DIR_PREFIX: &str = "graph.";

/// An input to run without prompting.
enum Input {
    Command(String),
    File(String),
}

struct Args {
    location: Option<PathBuf>,
    /// Whether `location` is a raw file prefix rather than a directory.
    raw_prefix: bool,
    inputs: Vec<Input>,
    single_transaction: bool,
    restore: Option<PathBuf>,
    synchronous: Synchronous,
    style: Style,
    quiet: bool,
    /// Parameters, as `NAME VALUE` for `\set`.
    params: Vec<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        location: None,
        raw_prefix: false,
        inputs: Vec::new(),
        single_transaction: false,
        restore: None,
        synchronous: Synchronous::Full,
        style: Style {
            format: Format::Aligned,
            expanded: false,
            ids: false,
        },
        quiet: false,
        params: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        // `--name=value` is the same as `--name value`.
        let (arg, mut inline) = match arg.split_once('=') {
            Some((a, v)) if a.starts_with("--") => (a.to_string(), Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = |name: &str| {
            inline
                .take()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("graqlite {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--command" => args.inputs.push(Input::Command(value(&arg)?)),
            "-f" | "--file" => args.inputs.push(Input::File(value(&arg)?)),
            "-1" | "--single-transaction" => args.single_transaction = true,
            "-v" | "--set" => {
                let v = value(&arg)?;
                let (name, value) = v
                    .split_once('=')
                    .ok_or_else(|| format!("{arg} needs NAME=VALUE"))?;
                args.params.push(format!("{name} {value}"));
            }
            "--prefix" => {
                if args.location.is_some() {
                    return Err("give a DIR or --prefix, not both".into());
                }
                args.location = Some(value(&arg)?.into());
                args.raw_prefix = true;
            }
            "--restore" => args.restore = Some(value(&arg)?.into()),
            "--synchronous" => {
                let level = value(&arg)?;
                args.synchronous = Synchronous::of_name(&level).ok_or_else(|| {
                    format!("--synchronous takes full, normal or off, not {level}")
                })?;
            }
            "--csv" => args.style.format = Format::Csv,
            "--tsv" => args.style.format = Format::Tsv,
            "-x" | "--expanded" => args.style.expanded = true,
            "-q" | "--quiet" => args.quiet = true,
            a if a.starts_with('-') && a != "-" => {
                return Err(format!("unknown option {a}; try --help"));
            }
            _ => {
                if args.location.is_some() {
                    return Err("give one database".into());
                }
                args.location = Some(arg.clone().into());
            }
        }
        if inline.is_some() {
            return Err(format!("{arg} takes no value"));
        }
    }
    Ok(Some(args))
}

/// Opens or creates the database, or restores it from a snapshot.
fn open(args: &Args) -> Result<Graph, String> {
    let prefix = match &args.location {
        None => None,
        Some(p) if args.raw_prefix => Some(p.clone()),
        Some(dir) => {
            fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
            Some(dir.join(DIR_PREFIX))
        }
    };
    let name = || match &args.location {
        Some(p) => p.display().to_string(),
        None => "the database".into(),
    };
    let options = || Options {
        synchronous: args.synchronous,
        ..Options::default()
    };
    if let Some(snapshot) = &args.restore {
        if let Some(p) = &prefix
            && graph::exists(p)
        {
            return Err(format!("{} already exists; not restoring over it", name()));
        }
        let file =
            File::open(snapshot).map_err(|e| format!("opening {}: {e}", snapshot.display()))?;
        return Graph::restore(prefix.as_deref(), options(), &mut BufReader::new(file))
            .map_err(|e| format!("restoring {}: {e}", snapshot.display()));
    }
    let created = prefix.as_ref().is_some_and(|p| !graph::exists(p));
    let g = Graph::open(prefix.as_deref(), options())
        .map_err(|e| format!("opening {}: {e} (is it open in another process?)", name()))?;
    if created && !args.quiet {
        eprintln!("created a new database in {}", name());
    }
    Ok(g)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("graqlite: {e}");
            return ExitCode::from(2);
        }
    };
    let g = match open(&args) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("graqlite: {e}");
            return ExitCode::from(2);
        }
    };
    let mut shell = Shell::new(g, args.location.clone(), args.style);
    if let Some(text) = std::env::var_os("GRAQLITE_TIME") {
        match text.to_str().and_then(graqlite::times::parse_time) {
            Some(t) => shell.fix_time(t),
            None => {
                eprintln!(
                    "graqlite: GRAQLITE_TIME is not an ISO 8601 time: {}",
                    text.display()
                );
                return ExitCode::from(2);
            }
        }
    }
    for p in &args.params {
        if let Err(e) = shell.set(p) {
            eprintln!("graqlite: {e}");
            return ExitCode::from(2);
        }
    }
    shell.warn_skipped();
    let step = if !args.inputs.is_empty() {
        run_inputs(&mut shell, &args)
    } else if io::stdin().is_terminal() {
        interactive(&mut shell, args.quiet)
    } else {
        let mut text = String::new();
        match io::stdin().read_to_string(&mut text) {
            Ok(_) => shell.script(&text, "<stdin>"),
            Err(e) => {
                eprintln!("graqlite: reading standard input: {e}");
                Step::Failed
            }
        }
    };
    shell.close();
    match step {
        Step::Failed => ExitCode::FAILURE,
        Step::Ok | Step::Quit => ExitCode::SUCCESS,
    }
}

/// Runs the `-c` and `-f` inputs in order, stopping at the first that fails.
fn run_inputs(shell: &mut Shell, args: &Args) -> Step {
    if args.single_transaction && shell.input("\\begin", &Origin::interactive()) != Step::Ok {
        return Step::Failed;
    }
    for input in &args.inputs {
        let step = match input {
            Input::Command(text) => shell.input(text, &Origin::interactive()),
            Input::File(name) => {
                let text = if name == "-" {
                    let mut text = String::new();
                    io::stdin().read_to_string(&mut text).map(|_| text)
                } else {
                    fs::read_to_string(name)
                };
                match text {
                    Ok(text) => shell.script(&text, if name == "-" { "<stdin>" } else { name }),
                    Err(e) => {
                        eprintln!("graqlite: reading {name}: {e}");
                        Step::Failed
                    }
                }
            }
        };
        match step {
            Step::Ok => {}
            // A failure rolls back the transaction, and close() rolls back a
            // quit.
            Step::Failed | Step::Quit => return step,
        }
    }
    if args.single_transaction
        && shell.in_transaction()
        && shell.input("\\commit", &Origin::interactive()) != Step::Ok
    {
        return Step::Failed;
    }
    Step::Ok
}

fn history_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GRAQLITE_HISTORY") {
        return Some(p.into());
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".graqlite_history"))
}

fn interactive(shell: &mut Shell, quiet: bool) -> Step {
    let mut editor: Editor<ShellHelper, DefaultHistory> = match Editor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("graqlite: starting the line editor: {e}");
            return Step::Failed;
        }
    };
    editor.set_helper(Some(ShellHelper));
    let history = history_path();
    if let Some(h) = &history {
        let _ = editor.load_history(h);
    }
    if !quiet {
        println!(
            "graqlite {} ({}).  Type \\? for help, \\q to quit.",
            env!("CARGO_PKG_VERSION"),
            shell.name()
        );
    }
    let mut step = Step::Ok;
    loop {
        let prompt = format!(
            "{}{}> ",
            shell.name(),
            if shell.in_transaction() { "=*" } else { "=" }
        );
        match editor.readline(&prompt) {
            Ok(line) => {
                let text = line.trim_end();
                if text.trim().is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(text);
                // Errors at the prompt are reported, and do not end the
                // session or fail it.
                if shell.input(text, &Origin::interactive()) == Step::Quit {
                    break;
                }
            }
            // Ctrl-C abandons the input being edited.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("graqlite: reading input: {e}");
                step = Step::Failed;
                break;
            }
        }
    }
    if let Some(h) = &history {
        let _ = editor.save_history(h);
    }
    step
}

/// Line editing for the shell: multi-line statements and completion.
struct ShellHelper;

impl Helper for ShellHelper {}
impl Highlighter for ShellHelper {}

impl Hinter for ShellHelper {
    type Hint = String;
}

/// Whether interactive `text` is a statement still being entered.
fn continues(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() || trimmed.trim_start().starts_with('\\') {
        return false;
    }
    // A blank line runs what has been entered.
    if text.ends_with('\n') {
        return false;
    }
    // Patterns may end in '.' or ';', but more usually follows them.
    // A subject alone, or a predicate without an object, is complete but
    // more likely continued.
    trimmed.ends_with('.')
        || trimmed.ends_with(';')
        || graql::is_incomplete(trimmed)
        || graql::ends_short(trimmed)
}

impl Validator for ShellHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(if continues(ctx.input()) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '\\'))
            .map_or(0, |i| i + 1);
        let word = &before[start..];
        if word.is_empty() {
            return Ok((pos, Vec::new()));
        }
        let pair = |s: &str| Pair {
            display: s.to_string(),
            replacement: s.to_string(),
        };
        let candidates = if word.starts_with('\\') {
            if before[..start].trim().is_empty() {
                COMMANDS
                    .iter()
                    .map(|(c, _)| c.split(' ').next().unwrap_or(c))
                    .filter(|c| c.starts_with(word))
                    .map(pair)
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            let lower = word.to_ascii_lowercase();
            graql::keywords()
                .iter()
                .copied()
                .chain(graql::function_names())
                .filter(|k| k.starts_with(&lower))
                .map(pair)
                .collect()
        };
        Ok((start, candidates))
    }
}

#[cfg(test)]
mod tests {
    use super::continues;

    #[test]
    fn continuation() {
        assert!(!continues(""));
        assert!(!continues("\\d"));
        assert!(!continues("insert /a knows /b"));
        assert!(!continues("select ?x where /a knows ?x"));
        assert!(continues("insert /a knows /b ."));
        assert!(continues("insert /a knows /b ;"));
        assert!(continues("insert /a knows /b ,"));
        assert!(continues("select ?x where"));
        assert!(continues("insert /a name \"two\nlines"));
        // A blank line runs it anyway.
        assert!(!continues("insert /a knows /b .\n"));
        assert!(!continues("select ?x where\n"));
        assert!(!continues("insert /a knows ) /b"));
        // A subject alone, or a predicate without an object, waits for a
        // blank line.
        assert!(continues("lookup ?subjects"));
        assert!(continues("select ?x where ?x knows"));
        assert!(!continues("lookup ?subjects\n"));
        assert!(!continues("lookup ? knows ?o"));
    }
}
