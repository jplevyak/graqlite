//! Runs the `graqlite` shell on scripts, comparing what it prints with the
//! expected output beside each script, and tests its options.
//!
//! Each `examples/*.graql` and `tests/cli/*.graql` runs as `graqlite -q -f FILE`
//! on a database in memory.  Its standard output and error, interleaved, and
//! `[exit N]` if it exits with status N other than 0, must match the file's
//! `.out`.  Run with `GRAQLITE_BLESS=1` to write the `.out` files from what
//! the scripts print, then review the change.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Runs `graqlite` with `args` in the crate's directory, giving it `stdin`,
/// and returns what it prints to standard output and error, interleaved,
/// and its exit status.
fn run(args: &[&str], stdin: &str) -> (String, i32) {
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_graqlite"));
    command
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("GRAQLITE_HISTORY")
        // Commit times appear in \show and \info.
        .env("GRAQLITE_TIME", "2026-10-31T12:00:00Z")
        .stdin(Stdio::piped())
        .stdout(writer.try_clone().unwrap())
        .stderr(writer);
    let mut child = command.spawn().unwrap();
    // Close this process's copies of the pipe's writer, so that reading
    // ends when the shell exits.
    drop(command);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    let status = child.wait().unwrap();
    (output, status.code().unwrap_or(-1))
}

/// Runs each script in `dir`, comparing its output with its `.out` file.
fn check_scripts(dir: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let bless = std::env::var_os("GRAQLITE_BLESS").is_some();
    let mut scripts: Vec<PathBuf> = fs::read_dir(root.join(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "graql"))
        .collect();
    scripts.sort();
    assert!(!scripts.is_empty(), "no scripts in {dir}");
    let mut failures = Vec::new();
    for script in &scripts {
        let relative = script.strip_prefix(root).unwrap().to_str().unwrap();
        let (mut output, status) = run(&["-q", "-f", relative], "");
        if status != 0 {
            output.push_str(&format!("[exit {status}]\n"));
        }
        let expected_path = script.with_extension("out");
        if bless {
            fs::write(&expected_path, &output).unwrap();
            continue;
        }
        match fs::read_to_string(&expected_path) {
            Ok(expected) if expected == output => {}
            Ok(expected) => failures.push(format!(
                "{relative}: output differs from {}\n{}",
                expected_path.display(),
                first_difference(&expected, &output)
            )),
            Err(_) => failures.push(format!(
                "{relative}: no {}; run with GRAQLITE_BLESS=1 to write it",
                expected_path.display()
            )),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

/// The first line where `expected` and `actual` differ, as a message.
fn first_difference(expected: &str, actual: &str) -> String {
    let (e, a): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), actual.lines().collect());
    let n = e.iter().zip(&a).take_while(|(x, y)| x == y).count();
    format!(
        "line {}:\n  expected: {}\n  actual:   {}",
        n + 1,
        e.get(n).copied().unwrap_or("<end>"),
        a.get(n).copied().unwrap_or("<end>")
    )
}

#[test]
fn examples() {
    check_scripts("examples");
}

#[test]
fn cli_scripts() {
    check_scripts("tests/cli");
}

/// A directory for a test's databases, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graqlite-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn data_and_rules_persist_between_runs() {
    let dir = TempDir::new("persist");
    let db = dir.path("db");
    let (out, status) = run(
        &[
            "-q",
            &db,
            "-c",
            "insert /a parent /b . /b parent /c",
            "-c",
            "rule ancestor(?x ?y) ?x parent ?y",
            "-c",
            "rule ancestor(?x ?y) ?x parent ?z . ancestor(?z ?y)",
        ],
        "",
    );
    assert_eq!(
        (out.as_str(), status),
        ("INSERT 2\nRULE ancestor\nRULE ancestor\n", 0)
    );
    let (out, status) = run(
        &[
            "-q",
            &db,
            "--csv",
            "-c",
            "select ?y by ?y where ancestor(/a ?y)",
        ],
        "",
    );
    assert_eq!((out.as_str(), status), ("y\n/b\n/c\n", 0));
}

#[test]
fn a_new_database_is_announced() {
    let dir = TempDir::new("new");
    let db = dir.path("db");
    let (out, _) = run(&[&db, "-c", "verify /a p 1"], "");
    assert_eq!(out, format!("created a new database in {db}\nfalse\n"));
    let (out, _) = run(&[&db, "-c", "verify /a p 1"], "");
    assert_eq!(out, "false\n");
}

#[test]
fn single_transaction_rolls_back_on_error() {
    let dir = TempDir::new("single");
    let db = dir.path("db");
    let (out, status) = run(
        &[
            "-q",
            &db,
            "-1",
            "-c",
            "insert /a p 1",
            "-c",
            "select ?x where",
        ],
        "",
    );
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("the open transaction was rolled back"),
        "{out}"
    );
    let (out, _) = run(&["-q", &db, "-c", "verify /a p 1"], "");
    assert_eq!(out, "false\n");
    // Without -1, each statement commits as it runs.
    run(
        &["-q", &db, "-c", "insert /a p 1", "-c", "select ?x where"],
        "",
    );
    let (out, _) = run(&["-q", &db, "-c", "verify /a p 1"], "");
    assert_eq!(out, "true\n");
}

#[test]
fn piped_scripts() {
    let (out, status) = run(&["-q"], "insert /a p 1\n\nselect ?x where ?x p 1\n");
    assert_eq!(
        (out.as_str(), status),
        ("INSERT 1\n x\n----\n /a\n(1 row)\n", 0)
    );
    let (out, status) = run(&["-q", "-f", "-"], "verify /a p 1\n");
    assert_eq!((out.as_str(), status), ("false\n", 0));
    let (out, status) = run(&["-q"], "select ?x where\n  ?x p )\n");
    assert_eq!(
        (out.as_str(), status),
        ("<stdin>:2:8: error: expected a statement, found ')'\n", 1)
    );
}

/// A statement over many lines is parsed once, not once a line: 5 000
/// lines take about as long as one line holding them (minutes in a debug
/// build, when each line parsed the statement so far).
#[test]
fn a_long_statement_is_parsed_once() {
    let mut script = String::from("insert\n");
    for i in 0..5_000 {
        script.push_str(&format!("  /p{i} name \"person {i}\" ; age {} .\n", i % 90));
    }
    script.push_str("  /last name \"last\"\n\nselect ?n where /last name ?n\n");
    let start = std::time::Instant::now();
    let (out, status) = run(&["-q"], &script);
    let took = start.elapsed();
    assert_eq!(status, 0, "{out}");
    assert!(out.starts_with("INSERT 10001\n"), "{out}");
    assert!(out.contains("\n last\n"), "{out}");
    // Generous, for a debug build on a busy machine.
    assert!(took < std::time::Duration::from_secs(15), "{took:?}");
}

#[test]
fn dump_and_restore() {
    let dir = TempDir::new("dump");
    let snapshot = dir.path("snapshot");
    let dump = format!("\\dump {snapshot}");
    let (out, status) = run(&["-q", "-c", "insert /a p 1", "-c", &dump], "");
    assert_eq!(status, 0, "{out}");
    let (out, status) = run(&["-q", "--restore", &snapshot, "-c", "verify /a p 1"], "");
    assert_eq!((out.as_str(), status), ("true\n", 0));
    // Not over an existing database.
    let db = dir.path("db");
    run(&["-q", &db, "-c", "verify /a p 1"], "");
    let (out, status) = run(&["-q", "--restore", &snapshot, &db], "");
    assert_eq!(status, 2);
    assert!(out.contains("already exists"), "{out}");
}

#[test]
fn output_to_a_file() {
    let dir = TempDir::new("output");
    let file = dir.path("out.csv");
    let to_file = format!("\\o {file}");
    let (out, status) = run(
        &[
            "-q",
            "--csv",
            "-c",
            "insert /a p 1",
            "-c",
            &to_file,
            "-c",
            "select ?x where ?x p 1",
            "-c",
            "\\o",
        ],
        "",
    );
    assert_eq!((out.as_str(), status), ("INSERT 1\n", 0));
    assert_eq!(fs::read_to_string(&file).unwrap(), "x\n/a\n");
}

#[test]
fn parameters_from_the_command_line() {
    let (out, status) = run(
        &[
            "-q",
            "-v",
            "who=/a",
            "--set=v=\"two words\"",
            "-c",
            "insert $who p $v",
            "-c",
            "select ?v where /a p ?v",
        ],
        "",
    );
    assert_eq!(
        (out.as_str(), status),
        ("INSERT 1\n v\n-------------\n \"two words\"\n(1 row)\n", 0)
    );
    let (out, status) = run(&["-q", "-v", "nope"], "");
    assert_eq!(
        (out.as_str(), status),
        ("graqlite: -v needs NAME=VALUE\n", 2)
    );
    let (out, status) = run(&["-q", "-c", "verify $x p 1"], "");
    assert_eq!((out.as_str(), status), ("error: $x is not given\n", 1));
}

#[test]
fn synchronous() {
    let dir = std::env::temp_dir().join(format!("graqlite-cli-sync-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let db = dir.to_str().unwrap();
    for level in ["full", "normal", "off"] {
        let (out, status) = run(
            &[
                "-q",
                "--synchronous",
                level,
                "-c",
                &format!("insert /{level} p 1"),
                db,
            ],
            "",
        );
        assert_eq!((out.as_str(), status), ("INSERT 1\n", 0));
    }
    let (out, status) = run(&["-q", "--csv", "-c", "select ?s where ?s p 1", db], "");
    assert_eq!(status, 0, "{out}");
    for level in ["full", "normal", "off"] {
        assert!(out.contains(level), "{level} is missing from {out}");
    }
    fs::remove_dir_all(&dir).unwrap();
    let (out, status) = run(&["--synchronous", "sometimes"], "");
    assert_eq!(
        (out.as_str(), status),
        (
            "graqlite: --synchronous takes full, normal or off, not sometimes\n",
            2
        )
    );
}

#[test]
fn options() {
    let (out, status) = run(&["--help"], "");
    assert_eq!(status, 0);
    assert!(out.starts_with("Usage: graqlite"), "{out}");
    let (out, status) = run(&["--version"], "");
    assert_eq!(
        (out.trim(), status),
        (concat!("graqlite ", env!("CARGO_PKG_VERSION")), 0)
    );
    let (out, status) = run(&["--bogus"], "");
    assert_eq!(
        (out.as_str(), status),
        ("graqlite: unknown option --bogus; try --help\n", 2)
    );
    let (out, status) = run(&["-c"], "");
    assert_eq!((out.as_str(), status), ("graqlite: -c needs a value\n", 2));
    let (out, status) = run(
        &[
            "-q",
            "-x",
            "-c",
            "insert /a p 1 ; q 2",
            "-c",
            "select * where /a p ?p ; q ?q",
        ],
        "",
    );
    assert_eq!(
        (out.as_str(), status),
        ("INSERT 2\n-[ RECORD 1 ]\np | 1\nq | 2\n(1 row)\n", 0)
    );
}
