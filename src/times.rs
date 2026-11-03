//! When ids were committed: a map between times and ids, so that queries
//! can see the graph as it was at a time.
//!
//! Ids increase as entries are appended, so an id is already a time in the
//! order of the log.  [`Times`] records, at each commit, the wall-clock time
//! and the id after the last entry committed, so a time maps to the last id
//! committed by then, and an id to the time it was committed.  Recorded times
//! never decrease, even if the clock does.
//!
//! A graph on disk keeps them in `{prefix}times`: a header of the magic, the
//! version and the log's uuid, then a record of the time and next id for each
//! commit, as little-endian words.  The file is appended to after each
//! commit and not flushed, since it can be rebuilt: when it is missing,
//! belongs to another log, or ends before the log's last commit (after a
//! crash), the missing commits are rebuilt from the creation times of the
//! links appended in them.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::local_record_log::LocalRecordLog;
use crate::mmap::suffixed;
use crate::record::{LinkTime, Uuid};

const FILE_MAGIC: u64 = u64::from_le_bytes(*b"grqltime");
const FILE_VERSION: u64 = 1;
const HEADER_WORDS: usize = 4;

/// The times of the commits of a graph.
#[derive(Debug, Default)]
pub struct Times {
    /// Each commit's time, and the id after its last entry, both
    /// increasing.
    commits: Arc<RwLock<Vec<(LinkTime, u64)>>>,
    /// For a reader (see [`reader`](Times::reader)): how many of the
    /// commits it sees, those when it was made.
    len: Option<usize>,
    /// The file the commits are appended to, for a graph on disk.
    file: Option<File>,
}

/// The times file of the graph at `prefix`.
pub(crate) fn path(prefix: &Path) -> PathBuf {
    suffixed(prefix, "times")
}

/// Deletes the times file of the graph at `prefix`, if it has one.
pub(crate) fn remove(prefix: &Path) -> io::Result<()> {
    match fs::remove_file(path(prefix)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl Times {
    /// The times of the commits of `log`, at `prefix` or in memory, read
    /// from its times file and rebuilt where that is missing or behind.
    pub(crate) fn open(
        prefix: Option<&Path>,
        uuid: Uuid,
        log: &LocalRecordLog,
    ) -> io::Result<Self> {
        let next_id = log.metadata().next_id;
        let mut commits = prefix.and_then(|p| read(p, uuid)).unwrap_or_default();
        // A commit can't be past the log's end; the rest were rolled back.
        commits.retain(|&(_, id)| id <= next_id);
        let read_commits = commits.len();
        let from = commits.last().map_or(log.start_id(), |&(_, id)| id);
        rebuild(log, from, next_id, &mut commits);
        let file = match prefix {
            None => None,
            Some(p) => Some(write(p, uuid, &commits, read_commits)?),
        };
        Ok(Times {
            commits: Arc::new(RwLock::new(commits)),
            len: None,
            file,
        })
    }

    /// Records a commit at `time` of the entries before `next_id`, unless
    /// there are none since the last.  Its time is no earlier than the
    /// last's.
    pub(crate) fn record(&mut self, time: LinkTime, next_id: u64) {
        let mut commits = self.commits.write().unwrap_or_else(|e| e.into_inner());
        let last = commits.last().copied();
        if last.is_some_and(|(_, id)| id >= next_id) {
            return;
        }
        let time = last.map_or(time, |(t, _)| time.max(t));
        commits.push((time, next_id));
        drop(commits);
        // The file can be rebuilt, so a failure to write it is not the
        // commit's.
        if let Some(file) = &mut self.file {
            let _ = file.write_all(&record_bytes(time, next_id));
        }
    }

    /// A reader of the times, sharing them, which sees the commits recorded
    /// when it was made.
    pub(crate) fn reader(&self) -> Times {
        let len = self.len.unwrap_or_else(|| self.with(|c| c.len()));
        Times {
            commits: Arc::clone(&self.commits),
            len: Some(len),
            file: None,
        }
    }

    /// Calls `f` with the commits this sees.
    fn with<T>(&self, f: impl FnOnce(&[(LinkTime, u64)]) -> T) -> T {
        let commits = self.commits.read().unwrap_or_else(|e| e.into_inner());
        let n = self.len.unwrap_or(commits.len()).min(commits.len());
        f(&commits[..n])
    }

    /// The last id committed at or before `time`, or `None` if nothing was.
    pub fn id_at(&self, time: LinkTime) -> Option<u64> {
        self.with(|c| {
            let n = c.partition_point(|&(t, _)| t <= time);
            n.checked_sub(1).map(|i| c[i].1 - 1)
        })
    }

    /// When `id` was committed, or `None` if it is not yet.
    pub fn time_of(&self, id: u64) -> Option<LinkTime> {
        self.with(|c| {
            let n = c.partition_point(|&(_, next)| next <= id);
            c.get(n).map(|&(t, _)| t)
        })
    }

    /// The number of commits recorded.
    pub fn len(&self) -> usize {
        self.with(|c| c.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The times of the first and last commits.
    pub fn span(&self) -> Option<(LinkTime, LinkTime)> {
        self.with(|c| Some((c.first()?.0, c.last()?.0)))
    }
}

/// Adds commits for the entries of `log` from `from` to `to`, at the creation
/// times of their links: one commit for each time, covering the entries up
/// to the last link created then.  Entries after the last link go with it.
fn rebuild(log: &LocalRecordLog, from: u64, to: u64, commits: &mut Vec<(LinkTime, u64)>) {
    if from >= to {
        return;
    }
    let mut last_time = commits.last().map(|&(t, _)| t);
    let mut pending: Option<(LinkTime, u64)> = None;
    let mut end = from;
    for record in log.records(from) {
        let Ok(record) = record else { break };
        if record.id() >= to {
            break;
        }
        end = record.next_id();
        let Some(link) = record.as_link() else {
            continue;
        };
        let time = last_time.map_or(link.when(), |t| link.when().max(t));
        match &mut pending {
            Some((t, next)) if *t == time => *next = end,
            _ => {
                commits.extend(pending.take());
                pending = Some((time, end));
            }
        }
        last_time = Some(time);
    }
    match pending {
        Some((t, _)) => commits.push((t, to.max(end))),
        // No links: the entries can only be given the last time known.
        None => {
            if let Some(t) = last_time {
                commits.push((t, to));
            }
        }
    }
}

fn record_bytes(time: LinkTime, next_id: u64) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&time.to_le_bytes());
    bytes[8..].copy_from_slice(&next_id.to_le_bytes());
    bytes
}

/// The commits in the times file at `prefix`, if it is for the log with
/// `uuid`, ignoring a last record that is incomplete.
fn read(prefix: &Path, uuid: Uuid) -> Option<Vec<(LinkTime, u64)>> {
    let mut bytes = Vec::new();
    File::open(path(prefix))
        .ok()?
        .read_to_end(&mut bytes)
        .ok()?;
    let words: Vec<u64> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&c| u64::from_le_bytes(c))
        .collect();
    let (header, records) = words.split_at_checked(HEADER_WORDS)?;
    if header != [FILE_MAGIC, FILE_VERSION, uuid.uuid0, uuid.uuid1] {
        return None;
    }
    let mut commits: Vec<(LinkTime, u64)> = Vec::new();
    for &[time, id] in records.as_chunks::<2>().0 {
        // Keep the file's invariant even if it was damaged.
        if commits.last().is_some_and(|&(t, i)| time < t || id <= i) {
            break;
        }
        commits.push((time, id));
    }
    Some(commits)
}

/// Opens the times file at `prefix` for appending, holding `commits`: if its
/// first `kept` are already in it, by appending the rest, and otherwise by
/// writing it again.
fn write(prefix: &Path, uuid: Uuid, commits: &[(LinkTime, u64)], kept: usize) -> io::Result<File> {
    let path = path(prefix);
    let expected_len = ((HEADER_WORDS + 2 * kept) * 8) as u64;
    let intact = kept > 0 && fs::metadata(&path).is_ok_and(|m| m.len() == expected_len);
    let mut file = if intact {
        OpenOptions::new().append(true).open(&path)?
    } else {
        let mut file = File::create(&path)?;
        for word in [FILE_MAGIC, FILE_VERSION, uuid.uuid0, uuid.uuid1] {
            file.write_all(&word.to_le_bytes())?;
        }
        for &(t, id) in &commits[..kept] {
            file.write_all(&record_bytes(t, id))?;
        }
        file
    };
    for &(t, id) in &commits[kept..] {
        file.write_all(&record_bytes(t, id))?;
    }
    Ok(file)
}

/// The time in milliseconds since 1970 written by `text`, in ISO 8601:
/// `2026-10-01`, `2026-10-01T12:30`, `2026-10-01T12:30:15.250`, with `Z` or
/// an offset such as `+02:00`, and UTC if neither.  A space may stand for
/// `T`.  A date alone is the start of that day.
pub fn parse_time(text: &str) -> Option<LinkTime> {
    let b = text.as_bytes();
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = b.get(range)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut ms = days_from_civil(year, month, day) * 86_400_000;
    let mut i = 10;
    if matches!(b.get(i), Some(b'T' | b't' | b' ')) {
        let (hour, minute) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
        if b.get(i + 3) != Some(&b':') || hour > 23 || minute > 59 {
            return None;
        }
        ms += (hour * 60 + minute) * 60_000;
        i += 6;
        if b.get(i) == Some(&b':') {
            let second = num(i + 1..i + 3)?;
            if second > 59 {
                return None;
            }
            ms += second * 1000;
            i += 3;
            if b.get(i) == Some(&b'.') {
                let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                if digits == 0 {
                    return None;
                }
                let fraction = &text[i + 1..i + 1 + digits];
                let millis: String = fraction.chars().chain("000".chars()).take(3).collect();
                ms += millis.parse::<i64>().ok()?;
                i += 1 + digits;
            }
        }
    }
    match &b[i..] {
        [] | [b'Z' | b'z'] => {}
        [sign @ (b'+' | b'-'), rest @ ..] => {
            let rest = std::str::from_utf8(rest).ok()?;
            let (h, m) = match rest.split_once(':') {
                Some((h, m)) => (h, m),
                None if rest.len() == 4 => rest.split_at(2),
                None => return None,
            };
            if h.len() != 2 || m.len() != 2 {
                return None;
            }
            let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
            if h > 23 || m > 59 {
                return None;
            }
            let offset = (h * 60 + m) * 60_000;
            ms += if *sign == b'+' { -offset } else { offset };
        }
        _ => return None,
    }
    LinkTime::try_from(ms).ok()
}

/// `time`, in milliseconds since 1970, in ISO 8601 in UTC:
/// `2026-10-01T12:30:15Z`, with milliseconds if there are any.
pub fn format_time(time: LinkTime) -> String {
    let ms = time as i64;
    let (days, of_day) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (year, month, day) = civil_from_days(days);
    let (h, m, s, millis) = (
        of_day / 3_600_000,
        of_day / 60_000 % 60,
        of_day / 1000 % 60,
        of_day % 1000,
    );
    if millis == 0 {
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The days since 1970-01-01 of a date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a number of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_and_formatting_times() {
        for (text, ms) in [
            ("1970-01-01", 0),
            ("1970-01-01T00:00:00Z", 0),
            ("2026-10-31", 1_793_404_800_000),
            ("2026-10-31T12:30", 1_793_449_800_000),
            ("2026-10-31 12:30:15", 1_793_449_815_000),
            ("2026-10-31T12:30:15.25Z", 1_793_449_815_250),
            ("2026-10-31T12:30:15.123456Z", 1_793_449_815_123),
            ("2026-10-31T14:30:15+02:00", 1_793_449_815_000),
            ("2026-10-31T07:30:15-0500", 1_793_449_815_000),
            ("2000-02-29", 951_782_400_000),
        ] {
            assert_eq!(parse_time(text), Some(ms), "{text}");
        }
        for text in [
            "",
            "2026",
            "2026-13-01",
            "2026-02-30",
            "1900-02-29",
            "2026-10-31T24:00",
            "2026-10-31T12",
            "2026-10-31T12:30:15.",
            "2026-10-31Tx",
            "2026-10-31 junk",
            "2026-10-31+2",
            "1969-12-31",
        ] {
            assert_eq!(parse_time(text), None, "{text}");
        }
        for (ms, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (1_793_449_815_000, "2026-10-31T12:30:15Z"),
            (1_793_449_815_250, "2026-10-31T12:30:15.250Z"),
            (951_782_400_000, "2000-02-29T00:00:00Z"),
        ] {
            assert_eq!(format_time(ms), text);
            assert_eq!(parse_time(text), Some(ms));
        }
    }

    #[test]
    fn mapping_times_and_ids() {
        let mut times = Times::default();
        assert_eq!(times.id_at(100), None);
        times.record(100, 10);
        times.record(200, 20);
        // Nothing new: not a commit.
        times.record(250, 20);
        // The clock went back: the time stays in order.
        times.record(150, 30);
        assert_eq!(times.len(), 3);
        assert_eq!(times.id_at(99), None);
        assert_eq!(times.id_at(100), Some(9));
        assert_eq!(times.id_at(199), Some(9));
        assert_eq!(times.id_at(200), Some(29));
        assert_eq!(times.id_at(u64::MAX), Some(29));
        assert_eq!(times.time_of(1), Some(100));
        assert_eq!(times.time_of(9), Some(100));
        assert_eq!(times.time_of(10), Some(200));
        assert_eq!(times.time_of(29), Some(200));
        assert_eq!(times.time_of(30), None);
        assert_eq!(times.span(), Some((100, 200)));
    }
}
