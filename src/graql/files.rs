//! Reading rows from, and writing them to, CSV, TSV and JSON lines files,
//! for `insert … from csv "FILE"` and `select … into csv "FILE"`.
//!
//! Reading streams a record at a time, so nothing holds the file.  A field
//! left empty is unbound, as a variable of an optional pattern is: the
//! template's triples that use it are left out.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};

use super::{Error, Result};

/// The format of a file of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    /// RFC 4180: comma separated, with `"` quoting.
    Csv,
    /// Tab separated, with `\t`, `\n`, `\r` and `\\` escapes, as the shell
    /// prints it.
    Tsv,
    /// One flat JSON object a line, its keys the columns.
    Jsonl,
}

impl FileFormat {
    pub fn of_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "csv" => Some(FileFormat::Csv),
            "tsv" => Some(FileFormat::Tsv),
            "jsonl" => Some(FileFormat::Jsonl),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FileFormat::Csv => "csv",
            FileFormat::Tsv => "tsv",
            FileFormat::Jsonl => "jsonl",
        }
    }
}

fn invalid(path: &str, line: u64, what: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{path}:{line}: {what}"))
}

fn io_error(path: &str, e: io::Error) -> Error {
    Error::Invalid(format!("{path}: {e}"))
}

/// A record read from a file.
pub(super) enum Record {
    Fields(Vec<Option<String>>),
    Object(HashMap<String, String>),
}

/// The records of a file, read one at a time.
pub(super) struct Records {
    path: String,
    format: FileFormat,
    reader: BufReader<File>,
    /// The line last read, from 1.
    line: u64,
    buffer: Vec<u8>,
    /// The columns of a CSV or TSV file, by name and by position from 1.
    columns: HashMap<String, usize>,
    width: Option<usize>,
}

impl Records {
    /// Opens `path`, reading its header, if it has one.
    pub fn open(path: &str, format: FileFormat, header: bool) -> Result<Self> {
        let file = File::open(path).map_err(|e| io_error(path, e))?;
        let mut records = Records {
            path: path.to_string(),
            format,
            reader: BufReader::with_capacity(1 << 16, file),
            line: 0,
            buffer: Vec::new(),
            columns: HashMap::new(),
            width: None,
        };
        if header && format != FileFormat::Jsonl {
            let Some(Record::Fields(names)) = records.next_record()? else {
                return Err(invalid(path, 1, "the file has no header"));
            };
            for (i, name) in names.iter().enumerate() {
                let name = name.clone().unwrap_or_default();
                records.columns.insert(name, i);
            }
            records.width = Some(names.len());
        }
        Ok(records)
    }

    /// Whether `column`, a name or position, can be read.  JSON lines
    /// objects have no fixed columns, so any name can.
    pub fn has_column(&self, column: &str) -> bool {
        match self.format {
            FileFormat::Jsonl => column.parse::<usize>().is_err(),
            _ => self.position(column).is_some() || self.width.is_none(),
        }
    }

    /// The names of the columns, for an error.
    pub fn column_names(&self) -> String {
        let mut names: Vec<(&usize, &String)> = self.columns.iter().map(|(n, i)| (i, n)).collect();
        names.sort();
        let names: Vec<&str> = names.into_iter().map(|(_, n)| n.as_str()).collect();
        names.join(", ")
    }

    fn position(&self, column: &str) -> Option<usize> {
        self.columns.get(column).copied().or_else(|| {
            let n = column.parse::<usize>().ok()?;
            (n >= 1 && self.width.is_none_or(|w| n <= w)).then(|| n - 1)
        })
    }

    /// The field of `record` at `column`, if it has a value.
    pub fn field<'a>(&self, record: &'a Record, column: &str) -> Option<&'a str> {
        match record {
            Record::Fields(fields) => fields.get(self.position(column)?)?.as_deref(),
            Record::Object(object) => object.get(column).map(String::as_str),
        }
    }

    /// The line last read, for an error.
    pub fn line(&self) -> u64 {
        self.line
    }

    /// The next line's bytes, into the buffer, without its line ending,
    /// appending to what is there.  Returns false at the end of the file.
    fn read_line(&mut self) -> Result<bool> {
        let start = self.buffer.len();
        let n = self
            .reader
            .read_until(b'\n', &mut self.buffer)
            .map_err(|e| io_error(&self.path, e))?;
        if n == 0 {
            return Ok(false);
        }
        self.line += 1;
        if self.line == 1 && self.buffer.starts_with(b"\xef\xbb\xbf") {
            self.buffer.drain(..3);
        }
        let _ = start;
        Ok(true)
    }

    fn text(&self, bytes: &[u8]) -> Result<String> {
        String::from_utf8(bytes.to_vec())
            .map_err(|_| invalid(&self.path, self.line, "the line is not UTF-8"))
    }

    /// The next record; blank lines are skipped.
    pub fn next_record(&mut self) -> Result<Option<Record>> {
        loop {
            self.buffer.clear();
            if !self.read_line()? {
                return Ok(None);
            }
            let record = match self.format {
                FileFormat::Csv => {
                    if is_blank(&self.buffer) {
                        continue;
                    }
                    Record::Fields(self.csv_fields()?)
                }
                FileFormat::Tsv => {
                    if is_blank(&self.buffer) {
                        continue;
                    }
                    Record::Fields(self.tsv_fields()?)
                }
                FileFormat::Jsonl => {
                    if self.buffer.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let text = self.text(&self.buffer)?;
                    let object =
                        parse_object(&text).map_err(|what| invalid(&self.path, self.line, what))?;
                    Record::Object(object)
                }
            };
            if let Record::Fields(fields) = &record
                && let Some(width) = self.width
                && fields.len() != width
            {
                let what = format!("expected {width} fields, found {}", fields.len());
                return Err(invalid(&self.path, self.line, what));
            }
            return Ok(Some(record));
        }
    }

    /// The fields of the record in the buffer, reading more lines while a
    /// quoted field is open.
    fn csv_fields(&mut self) -> Result<Vec<Option<String>>> {
        let opened = self.line;
        // Quotes alternate open and closed, a doubled one included.
        while self.buffer.iter().filter(|&&b| b == b'"').count() % 2 == 1 {
            if !self.read_line()? {
                return Err(invalid(&self.path, opened, "a quoted field never ends"));
            }
        }
        let end = line_end(&self.buffer);
        let text = self.text(&self.buffer[..end])?;
        let mut fields = Vec::new();
        let mut chars = text.chars().peekable();
        loop {
            let mut field = String::new();
            if chars.peek() == Some(&'"') {
                chars.next();
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            field.push('"');
                        }
                        Some('"') | None => break,
                        Some(c) => field.push(c),
                    }
                }
                if chars.peek().is_some_and(|&c| c != ',') {
                    let what = "a quoted field has text after its closing quote";
                    return Err(invalid(&self.path, opened, what));
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == ',' {
                        break;
                    }
                    if c == '"' {
                        let what = "a quote in the middle of an unquoted field";
                        return Err(invalid(&self.path, opened, what));
                    }
                    field.push(c);
                    chars.next();
                }
            }
            fields.push((!field.is_empty()).then_some(field));
            if chars.next().is_none() {
                return Ok(fields);
            }
        }
    }

    fn tsv_fields(&self) -> Result<Vec<Option<String>>> {
        let end = line_end(&self.buffer);
        let text = self.text(&self.buffer[..end])?;
        Ok(text
            .split('\t')
            .map(|f| (!f.is_empty()).then(|| unescape_tsv(f)))
            .collect())
    }
}

fn is_blank(line: &[u8]) -> bool {
    line[..line_end(line)].is_empty()
}

/// The length of `line` without its line ending.
fn line_end(line: &[u8]) -> usize {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    end
}

fn unescape_tsv(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A cursor over a JSON text.
struct Json<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl Json<'_> {
    fn skip_space(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        self.skip_space();
        let found = self.peek() == Some(b);
        self.pos += usize::from(found);
        found
    }

    fn hex4(&mut self) -> std::result::Result<u32, String> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .ok_or("a \\u escape is cut short")?;
        self.pos += 4;
        u32::from_str_radix(digits, 16).map_err(|_| "a \\u escape isn't hexadecimal".to_string())
    }

    fn string(&mut self) -> std::result::Result<String, String> {
        // The opening quote has been read.
        let mut out = String::new();
        loop {
            let rest = &self.text[self.pos..];
            let Some(n) = rest.find(['"', '\\']) else {
                return Err("a string never ends".into());
            };
            out.push_str(&rest[..n]);
            self.pos += n + 1;
            if rest.as_bytes()[n] == b'"' {
                return Ok(out);
            }
            let escape = self.peek().ok_or("a string never ends")?;
            self.pos += 1;
            match escape {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let mut c = self.hex4()?;
                    if (0xd800..0xdc00).contains(&c) {
                        if self.text.get(self.pos..self.pos + 2) != Some("\\u") {
                            return Err("a lone surrogate in a \\u escape".into());
                        }
                        self.pos += 2;
                        let low = self.hex4()?;
                        if !(0xdc00..0xe000).contains(&low) {
                            return Err("a lone surrogate in a \\u escape".into());
                        }
                        c = 0x10000 + ((c - 0xd800) << 10) + (low - 0xdc00);
                    }
                    out.push(char::from_u32(c).ok_or("a \\u escape names no character")?);
                }
                _ => return Err("an unknown escape in a string".into()),
            }
        }
    }

    /// A scalar: its text, or `None` for null.
    fn scalar(&mut self) -> std::result::Result<Option<String>, String> {
        self.skip_space();
        match self.peek() {
            Some(b'"') => {
                self.pos += 1;
                self.string().map(Some)
            }
            Some(b'{' | b'[') => Err("a nested object or array can't be a column".into()),
            Some(_) => {
                let rest = &self.text[self.pos..];
                let end = rest
                    .find(|c: char| c == ',' || c == '}' || c.is_ascii_whitespace())
                    .unwrap_or(rest.len());
                let word = &rest[..end];
                self.pos += end;
                match word {
                    "null" => Ok(None),
                    "true" | "false" => Ok(Some(word.to_string())),
                    w if w.parse::<f64>().is_ok() => Ok(Some(w.to_string())),
                    w => Err(format!("{w} is not a JSON value")),
                }
            }
            None => Err("the line ends where a value should be".into()),
        }
    }
}

/// The fields of a flat JSON object, null ones left out.
fn parse_object(text: &str) -> std::result::Result<HashMap<String, String>, String> {
    let mut json = Json {
        bytes: text.as_bytes(),
        text,
        pos: 0,
    };
    let mut object = HashMap::new();
    if !json.eat(b'{') {
        return Err("expected a JSON object".into());
    }
    if !json.eat(b'}') {
        loop {
            if !json.eat(b'"') {
                return Err("expected a quoted key".into());
            }
            let key = json.string()?;
            if !json.eat(b':') {
                return Err("expected ':' after a key".into());
            }
            if let Some(value) = json.scalar()?
                && !value.is_empty()
            {
                object.insert(key, value);
            }
            if json.eat(b'}') {
                break;
            }
            if !json.eat(b',') {
                return Err("expected ',' or '}' after a value".into());
            }
        }
    }
    json.skip_space();
    if json.pos != text.len() {
        return Err("text after the object".into());
    }
    Ok(object)
}

/// The lines of the file at `path`, to size a load by.
pub(super) fn count_lines(path: &str) -> Result<u64> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut buffer = vec![0u8; 1 << 16];
    let mut lines = 0;
    loop {
        let n = file.read(&mut buffer).map_err(|e| io_error(path, e))?;
        if n == 0 {
            return Ok(lines);
        }
        lines += buffer[..n].iter().filter(|&&b| b == b'\n').count() as u64;
    }
}

/// Writes `rows`, with a header of `columns` for CSV and TSV.  A field of
/// `None` is left empty, or null in JSON lines.
pub(super) fn write_rows<'a>(
    path: &str,
    format: FileFormat,
    columns: &[String],
    rows: impl Iterator<Item = Vec<Option<String>>> + 'a,
) -> Result<usize> {
    let file = File::create(path).map_err(|e| io_error(path, e))?;
    let mut out = BufWriter::new(file);
    let write = |out: &mut BufWriter<File>, fields: Vec<Option<&str>>| -> io::Result<()> {
        match format {
            FileFormat::Csv | FileFormat::Tsv => {
                let separator = if format == FileFormat::Csv { "," } else { "\t" };
                let line: Vec<String> = fields
                    .iter()
                    .map(|f| f.map_or(String::new(), |f| quoted(format, f)))
                    .collect();
                writeln!(out, "{}", line.join(separator))
            }
            FileFormat::Jsonl => {
                let members: Vec<String> = columns
                    .iter()
                    .zip(&fields)
                    .map(|(c, f)| {
                        let value = f.map_or("null".to_string(), json_string);
                        format!("{}:{value}", json_string(c))
                    })
                    .collect();
                writeln!(out, "{{{}}}", members.join(","))
            }
        }
    };
    if format != FileFormat::Jsonl {
        let header = columns.iter().map(|c| Some(c.as_str())).collect();
        write(&mut out, header).map_err(|e| io_error(path, e))?;
    }
    let mut count = 0;
    for row in rows {
        let fields = row.iter().map(Option::as_deref).collect();
        write(&mut out, fields).map_err(|e| io_error(path, e))?;
        count += 1;
    }
    out.flush().map_err(|e| io_error(path, e))?;
    Ok(count)
}

fn quoted(format: FileFormat, field: &str) -> String {
    match format {
        FileFormat::Csv if field.contains([',', '"', '\n', '\r']) => {
            format!("\"{}\"", field.replace('"', "\"\""))
        }
        FileFormat::Tsv => field
            .replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
            .replace('\r', "\\r"),
        _ => field.to_string(),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(name: &str, text: &str, format: FileFormat, header: bool) -> (Records, Vec<Record>) {
        let path =
            std::env::temp_dir().join(format!("graqlite-files-{}-{name}", std::process::id()));
        std::fs::write(&path, text).unwrap();
        let p = path.to_str().unwrap();
        let mut r = Records::open(p, format, header).unwrap();
        let mut all = Vec::new();
        while let Some(record) = r.next_record().unwrap() {
            all.push(record);
        }
        std::fs::remove_file(&path).unwrap();
        (r, all)
    }

    fn get<'a>(r: &Records, record: &'a Record, column: &str) -> Option<&'a str> {
        r.field(record, column)
    }

    #[test]
    fn csv_quoting_and_line_endings() {
        let text = "\u{feff}a,b,c\r\n1,\"x, \"\"y\"\"\",\r\n\n\"two\nlines\",,z";
        let (r, all) = records("csv", text, FileFormat::Csv, true);
        assert_eq!(all.len(), 2);
        assert_eq!(get(&r, &all[0], "a"), Some("1"));
        assert_eq!(get(&r, &all[0], "b"), Some("x, \"y\""));
        assert_eq!(get(&r, &all[0], "c"), None);
        assert_eq!(get(&r, &all[1], "1"), Some("two\nlines"));
        assert_eq!(get(&r, &all[1], "3"), Some("z"));
        assert_eq!(get(&r, &all[1], "4"), None);
    }

    #[test]
    fn csv_without_a_header_is_by_position() {
        let (r, all) = records("nohead", "p,q\nr,s\n", FileFormat::Csv, false);
        assert_eq!(all.len(), 2);
        assert_eq!(get(&r, &all[1], "2"), Some("s"));
        assert!(r.has_column("9"));
    }

    #[test]
    fn csv_errors_name_the_line() {
        let open = |name: &str, text: &str| {
            let path =
                std::env::temp_dir().join(format!("graqlite-files-{}-{name}", std::process::id()));
            std::fs::write(&path, text).unwrap();
            let p = path.to_str().unwrap().to_string();
            let mut r = Records::open(&p, FileFormat::Csv, true).unwrap();
            let mut message = String::new();
            loop {
                match r.next_record() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        message = e.to_string();
                        break;
                    }
                }
            }
            std::fs::remove_file(&path).unwrap();
            message
        };
        assert!(open("short", "a,b\n1,2\n3\n").contains(":3: expected 2 fields, found 1"));
        assert!(open("open", "a\n\"x\n").contains(":2: a quoted field never ends"));
        assert!(open("after", "a\n\"x\"y\n").contains("after its closing quote"));
        assert!(open("mid", "a\nx\"y\"\n").contains("middle of an unquoted"));
    }

    #[test]
    fn tsv_escapes() {
        let (r, all) = records("tsv", "a\tb\nx\\ty\t\\\\\n", FileFormat::Tsv, true);
        assert_eq!(get(&r, &all[0], "a"), Some("x\ty"));
        assert_eq!(get(&r, &all[0], "b"), Some("\\"));
    }

    #[test]
    fn jsonl_objects() {
        let text =
            "{\"a\": \"x\\n\\u00e9\\ud83d\\ude00\", \"b\": 12.5, \"c\": null, \"d\": true}\n\n{}\n";
        let (r, all) = records("jsonl", text, FileFormat::Jsonl, true);
        assert_eq!(all.len(), 2);
        assert_eq!(get(&r, &all[0], "a"), Some("x\né😀"));
        assert_eq!(get(&r, &all[0], "b"), Some("12.5"));
        assert_eq!(get(&r, &all[0], "c"), None);
        assert_eq!(get(&r, &all[0], "d"), Some("true"));
        assert!(!r.has_column("1"));
        for bad in [
            "[1]",
            "{\"a\": {}}",
            "{\"a\" 1}",
            "{\"a\": nope}",
            "{} x",
            "{\"a\": \"x",
        ] {
            assert!(parse_object(bad).is_err(), "{bad}");
        }
    }
}
