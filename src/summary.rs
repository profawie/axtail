//! Log analysis behind the Summary dashboard.
//!
//! Every selected log is streamed once, line by line, on a worker thread.
//! Each entry gets a level (ERROR / WARN / INFO / DEBUG) and a timestamp, and
//! entries of the same level are grouped into message types: the variable
//! parts of a message (numbers, ids, addresses, quoted values) are replaced
//! by placeholders so the same failure thrown many times counts as one type.
//!
//! Memory stays bounded: a capped number of types per level, a ring of the
//! latest occurrences per type and a global budget for their text.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use memchr::memmem;

use crate::clock;
use crate::source::Source;
use crate::ui::sanitize;
use crate::window::LineReader;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

pub const LEVEL_NAMES: [&str; 4] = ["ERROR", "WARN", "INFO", "DEBUG"];

/// Distinct message types kept per level; the rest share one overflow row.
const MAX_GROUPS: [usize; 4] = [1000, 1000, 400, 400];
/// Latest occurrences kept per type.
const MAX_OCC: [usize; 4] = [300, 300, 100, 100];
/// Bytes of message text kept per occurrence.
const OCC_TEXT: usize = 240;
/// Global budget for stored occurrences.
const OCC_BUDGET: usize = 48 * 1024 * 1024;
const OCC_OVERHEAD: usize = 40;
/// Stack trace lines kept for the first occurrence of a type.
const TRACE_LINES: usize = 20;
const PATTERN_MAX: usize = 200;

pub struct Occ {
    pub src: usize,
    /// 1-based line number in the (decompressed) log.
    pub line: u64,
    pub off: u64,
    pub ts: Option<i64>,
    pub text: Box<str>,
}

pub struct Group {
    pub pattern: String,
    pub count: u64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// Number of files the type was seen in.
    pub files: u32,
    last_src: usize,
    /// Latest occurrences, oldest first.
    pub occ: VecDeque<Occ>,
    /// Lines that followed the first occurrence (stack trace).
    pub trace: Vec<String>,
}

pub struct FileStat {
    pub src: usize,
    pub lines: u64,
    /// Entries per level; index 4 = no level found.
    pub counts: [u64; 5],
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Report {
    pub files: Vec<FileStat>,
    pub totals: [u64; 5],
    pub lines: u64,
    pub bytes: u64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub level_last: [Option<i64>; 4],
    pub groups: [Vec<Group>; 4],
    /// The level has more distinct types than are kept.
    pub capped: [bool; 4],
    /// Entries left out because they are outside the time range.
    pub skipped: u64,
    pub secs: f32,
}

// ----- timestamps ----------------------------------------------------------

fn num(b: &[u8], i: usize, n: usize) -> Option<i64> {
    let s = b.get(i..i + n)?;
    let mut v = 0i64;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    Some(v)
}

fn month(b: &[u8], i: usize) -> Option<i64> {
    const NAMES: [&[u8; 3]; 12] = [
        b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov",
        b"dec",
    ];
    let s = b.get(i..i + 3)?;
    NAMES
        .iter()
        .position(|n| n.eq_ignore_ascii_case(s))
        .map(|p| p as i64 + 1)
}

/// "HH:MM:SS" at `i`, plus an optional fraction. Returns seconds of day and end.
fn clock_time(b: &[u8], i: usize) -> Option<(i64, i64, i64, usize)> {
    let h = num(b, i, 2)?;
    if b.get(i + 2) != Some(&b':') || b.get(i + 5) != Some(&b':') {
        return None;
    }
    let mi = num(b, i + 3, 2)?;
    let s = num(b, i + 6, 2)?;
    let mut e = i + 8;
    if matches!(b.get(e), Some(b'.' | b',')) && b.get(e + 1).is_some_and(u8::is_ascii_digit) {
        e += 1;
        while b.get(e).is_some_and(u8::is_ascii_digit) {
            e += 1;
        }
    }
    Some((h, mi, s, e))
}

/// A zone suffix at `e`: "Z", "+08:00", "+0800" or "+08".
fn zone(b: &[u8], e: usize) -> Option<(i64, usize)> {
    match b.get(e)? {
        b'Z' | b'z' => Some((0, e + 1)),
        &c @ (b'+' | b'-') => {
            let h = num(b, e + 1, 2)?;
            let (m, end) = if b.get(e + 3) == Some(&b':') {
                (num(b, e + 4, 2)?, e + 6)
            } else if let Some(m) = num(b, e + 3, 2) {
                (m, e + 5)
            } else {
                (0, e + 3)
            };
            if h > 14 || m > 59 || b.get(end).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            let secs = h * 3600 + m * 60;
            Some((if c == b'-' { -secs } else { secs }, end))
        }
        _ => None,
    }
}

/// "2026-11-22 13:05:09", "2026-11-22T13:05:09.123+08:00", "2026/11/22 13:05:09".
fn iso(b: &[u8], i: usize) -> Option<(i64, usize)> {
    let y = num(b, i, 4)?;
    let sep = *b.get(i + 4)?;
    if !matches!(sep, b'-' | b'/') || b.get(i + 7) != Some(&sep) {
        return None;
    }
    let mo = num(b, i + 5, 2)?;
    let d = num(b, i + 8, 2)?;
    if !matches!(b.get(i + 10), Some(b'T' | b' ' | b'_')) {
        return None;
    }
    let (h, mi, s, e) = clock_time(b, i + 11)?;
    let (off, e) = zone(b, e).unwrap_or((clock::offset_secs(), e));
    Some((clock::civil_to_unix(y, mo, d, h, mi, s, off)?, e))
}

/// "22/Nov/2026:13:05:09 +0800" (access logs) and "22-Nov-2026 13:05:09".
fn dmy(b: &[u8], i: usize) -> Option<(i64, usize)> {
    let d = num(b, i, 2)?;
    let sep = *b.get(i + 2)?;
    if !matches!(sep, b'/' | b'-') || b.get(i + 6) != Some(&sep) {
        return None;
    }
    let mo = month(b, i + 3)?;
    let y = num(b, i + 7, 4)?;
    if !matches!(b.get(i + 11), Some(b':' | b' ')) {
        return None;
    }
    let (h, mi, s, e) = clock_time(b, i + 12)?;
    let spaced = if b.get(e) == Some(&b' ') {
        zone(b, e + 1)
    } else {
        None
    };
    let (off, e) = spaced
        .or_else(|| zone(b, e))
        .unwrap_or((clock::offset_secs(), e));
    Some((clock::civil_to_unix(y, mo, d, h, mi, s, off)?, e))
}

/// Year-less syslog time at the start of a line: "Nov 22 13:05:09".
fn syslog(b: &[u8], year: i32, not_after: i64) -> Option<(i64, usize)> {
    let mo = month(b, 0)?;
    if b.get(3) != Some(&b' ') {
        return None;
    }
    let d = match (b.get(4)?, b.get(5)?) {
        (b' ', c) if c.is_ascii_digit() => (c - b'0') as i64,
        _ => num(b, 4, 2)?,
    };
    if b.get(6) != Some(&b' ') {
        return None;
    }
    let (h, mi, s, e) = clock_time(b, 7)?;
    let at = |y: i32| clock::civil_to_unix(y as i64, mo, d, h, mi, s, clock::offset_secs());
    let t = at(year)?;
    // A December line read in January belongs to the year before.
    let t = if t > not_after + 86_400 {
        at(year - 1)?
    } else {
        t
    };
    Some((t, e))
}

/// Finds the first timestamp near the start of a line.
fn scan_ts(b: &[u8], year: i32, not_after: i64) -> Option<(i64, usize)> {
    if let Some(r) = syslog(b, year, not_after) {
        return Some(r);
    }
    let head = b.len().min(96);
    (0..head)
        .filter(|&i| b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()))
        .find_map(|i| iso(b, i).or_else(|| dmy(b, i)))
}

// ----- levels --------------------------------------------------------------

/// The level a word stands for; the flag is false for a mere hint such as
/// a word ending in "Exception".
fn word_level(tok: &[u8]) -> Option<(Level, bool)> {
    let is = |names: &[&str]| names.iter().any(|n| n.as_bytes().eq_ignore_ascii_case(tok));
    if is(&[
        "error", "err", "fatal", "critical", "crit", "panic", "severe", "emerg", "alert",
    ]) {
        return Some((Level::Error, true));
    }
    if is(&["warn", "warning"]) {
        return Some((Level::Warn, true));
    }
    if is(&["info", "notice"]) {
        return Some((Level::Info, true));
    }
    if is(&["debug", "trace", "verbose"]) {
        return Some((Level::Debug, true));
    }
    let ends = |s: &[u8]| tok.len() > s.len() && tok[tok.len() - s.len()..].eq_ignore_ascii_case(s);
    if ends(b"exception") || ends(b"error") {
        return Some((Level::Error, false));
    }
    None
}

struct Found {
    level: Level,
    strong: bool,
    /// Where the message starts when the level word is a prefix to strip.
    strip: Option<usize>,
}

/// Looks for the first level word in the 160 bytes after `from`.
fn find_level(b: &[u8], from: usize) -> Option<Found> {
    let glue = |c: u8, extra: &[u8]| c.is_ascii_alphanumeric() || extra.contains(&c);
    let end = b.len().min(from + 160);
    let mut i = from;
    while i < end {
        if !b[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j].is_ascii_alphabetic() {
            j += 1;
        }
        let prev = if i > 0 { b[i - 1] } else { b' ' };
        let next = b.get(j).copied().unwrap_or(b' ');
        if let Some((level, strong)) = word_level(&b[i..j]) {
            // Part of a path, a key or an identifier: not a level.
            let free = !glue(prev, b"_/.-\\") && !glue(next, b"_/.-\\=");
            if !strong && !next.is_ascii_digit() && next != b'_' {
                return Some(Found {
                    level,
                    strong,
                    strip: None,
                });
            }
            if strong && free {
                let upper = b[i..j].iter().all(u8::is_ascii_uppercase);
                let prefix = i - from <= 64
                    && (upper || b"[<(\"=|".contains(&prev) || b"]>:|".contains(&next));
                return Some(Found {
                    level,
                    strong,
                    strip: prefix.then_some(j),
                });
            }
        }
        i = j;
    }
    None
}

// ----- structured lines ----------------------------------------------------

/// The value of a top-level-looking JSON key, as a byte range of the line.
fn json_val(b: &[u8], keys: &[&[u8]]) -> Option<(usize, usize)> {
    for key in keys {
        for at in memmem::find_iter(b, key) {
            let mut i = at + key.len();
            while b.get(i) == Some(&b' ') {
                i += 1;
            }
            if b.get(i) != Some(&b':') {
                continue;
            }
            i += 1;
            while b.get(i) == Some(&b' ') {
                i += 1;
            }
            if b.get(i) == Some(&b'"') {
                let start = i + 1;
                let mut e = start;
                while e < b.len() && b[e] != b'"' {
                    e += if b[e] == b'\\' { 2 } else { 1 };
                }
                return Some((start, e.min(b.len())));
            }
            let e = b[i..]
                .iter()
                .position(|c| matches!(c, b',' | b'}' | b' '))
                .map_or(b.len(), |p| i + p);
            return Some((i, e));
        }
    }
    None
}

/// An access-log request: level from the HTTP status, message from the route.
fn access(b: &[u8]) -> Option<(Level, i64, String)> {
    let h = memmem::find(b, b"HTTP/")?;
    let q = h + 5 + b.get(h + 5..)?.iter().take(4).position(|&c| c == b'"')?;
    if b.get(q + 1) != Some(&b' ') {
        return None;
    }
    let status = num(b, q + 2, 3)?;
    if b.get(q + 5).is_some_and(|c| *c != b' ') {
        return None;
    }
    let s = memchr::memrchr(b'"', &b[..h])?;
    let req = String::from_utf8_lossy(&b[s + 1..h]);
    let mut parts = req.split_whitespace();
    let method = parts.next()?;
    let path = parts.next().unwrap_or("-");
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let path: String = path.chars().take(120).collect();
    let level = match status {
        500.. => Level::Error,
        400..500 => Level::Warn,
        _ => Level::Info,
    };
    Some((level, status, format!("{method} {path}")))
}

enum Msg {
    Range(usize, usize),
    /// An HTTP request: status and "METHOD /path".
    Access(i64, String),
}

struct Parsed {
    ts: Option<i64>,
    level: Option<Level>,
    strong: bool,
    msg: Msg,
}

fn skip_punct(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| b"]>\":|- \t".contains(c)) {
        i += 1;
    }
    i
}

fn parse_line(b: &[u8], year: i32, not_after: i64) -> Parsed {
    if b.first() == Some(&b'{') {
        let ts = json_val(
            b,
            &[b"\"time\"", b"\"timestamp\"", b"\"@timestamp\"", b"\"ts\""],
        )
        .and_then(|(s, e)| {
            let v = &b[s..e];
            if let Some(r) = scan_ts(v, year, not_after) {
                return Some(r.0);
            }
            // Epoch seconds or milliseconds.
            let n: f64 = std::str::from_utf8(v).ok()?.parse().ok()?;
            let secs = if n > 1e11 { n / 1000.0 } else { n };
            (secs > 1e8).then_some(secs as i64)
        });
        let level = json_val(b, &[b"\"level\"", b"\"severity\"", b"\"lvl\""])
            .and_then(|(s, e)| word_level(&b[s..e]))
            .filter(|l| l.1);
        let msg = json_val(b, &[b"\"msg\"", b"\"message\""]);
        if level.is_some() || msg.is_some() {
            let found = level.or_else(|| find_level(b, 0).map(|f| (f.level, f.strong)));
            let (s, e) = msg.unwrap_or((0, b.len()));
            return Parsed {
                ts,
                level: found.map(|l| l.0),
                strong: found.is_some_and(|l| l.1),
                msg: Msg::Range(s, e),
            };
        }
    }
    let (ts, ts_end) = match scan_ts(b, year, not_after) {
        Some((t, e)) => (Some(t), e),
        None => (None, 0),
    };
    if let Some((level, status, request)) = access(b) {
        return Parsed {
            ts,
            level: Some(level),
            strong: true,
            msg: Msg::Access(status, request),
        };
    }
    let found = find_level(b, ts_end);
    let mut start = skip_punct(b, found.as_ref().and_then(|f| f.strip).unwrap_or(ts_end));
    let mut end = b.len();
    // logfmt: msg="..."
    if let Some(at) = memmem::find(&b[ts_end..], b"msg=\"") {
        let s = ts_end + at + 5;
        let mut e = s;
        while e < b.len() && b[e] != b'"' {
            e += if b[e] == b'\\' { 2 } else { 1 };
        }
        (start, end) = (s, e.min(b.len()));
    }
    if start >= end {
        (start, end) = (0, b.len());
    }
    Parsed {
        ts,
        level: found.as_ref().map(|f| f.level),
        strong: found.is_some_and(|f| f.strong),
        msg: Msg::Range(start, end),
    }
}

// ----- message types -------------------------------------------------------

/// Length of a quoted single token (no spaces) closed by `q`.
fn quoted_token(rest: &[u8], q: u8) -> Option<usize> {
    for (k, &c) in rest.iter().take(120).enumerate() {
        if c == q {
            let after_ok = q != b'\'' || !rest.get(k + 1).is_some_and(u8::is_ascii_alphanumeric);
            return (k > 0 && after_ok).then_some(k);
        }
        if c.is_ascii_whitespace() {
            return None;
        }
    }
    None
}

fn uuid_len(b: &[u8]) -> Option<usize> {
    let s = b.get(..36)?;
    let ok = s.iter().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => *c == b'-',
        _ => c.is_ascii_hexdigit(),
    });
    (ok && !b.get(36).is_some_and(u8::is_ascii_alphanumeric)).then_some(36)
}

fn ip_len(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    for part in 0..4 {
        let start = i;
        while i - start < 3 && b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
        if part < 3 {
            if b.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
    }
    let more = b.get(i).is_some_and(u8::is_ascii_alphanumeric)
        || (b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit));
    (!more).then_some(i)
}

/// Turns a message into its type: variable parts become placeholders.
///   <n> number   <ip> address   <uuid>   <hex>   <id> generated id
///   '<*>' a quoted single value
pub fn normalize(msg: &str, out: &mut String) {
    let b = msg.as_bytes();
    let mut i = 0;
    while i < b.len() && out.len() < PATTERN_MAX {
        let c = b[i];
        if c >= 0x80 {
            let ch = msg[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8().max(1);
        } else if c.is_ascii_whitespace() || c.is_ascii_control() {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            i += 1;
        } else if matches!(c, b'"' | b'\'' | b'`') {
            let opens = c != b'\'' || i == 0 || !b[i - 1].is_ascii_alphanumeric();
            match quoted_token(&b[i + 1..], c).filter(|_| opens) {
                Some(len) => {
                    out.push(c as char);
                    out.push_str("<*>");
                    out.push(c as char);
                    i += len + 2;
                }
                None => {
                    out.push(c as char);
                    i += 1;
                }
            }
        } else if c.is_ascii_alphanumeric() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_alphanumeric() {
                j += 1;
            }
            let tok = &b[i..j];
            let digits = tok.iter().filter(|c| c.is_ascii_digit()).count();
            let letters = tok.len() - digits;
            if let Some(n) = uuid_len(&b[i..]) {
                out.push_str("<uuid>");
                i += n;
            } else if digits == 0 {
                out.push_str(&msg[i..j]);
                i = j;
            } else if letters == 0 {
                if let Some(n) = ip_len(&b[i..]) {
                    out.push_str("<ip>");
                    i += n;
                    continue;
                }
                // Swallow a decimal part: 3.25 is one number.
                if b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                    while b.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                    }
                }
                out.push_str("<n>");
                i = j;
            } else {
                let hex = tok.iter().all(u8::is_ascii_hexdigit);
                if tok.len() > 2 && tok[0] == b'0' && tok[1] | 0x20 == b'x' {
                    out.push_str("<hex>");
                } else if (hex && tok.len() >= 6) || (tok.len() >= 8 && digits >= 2 && letters >= 2)
                {
                    out.push_str("<id>");
                } else {
                    let mut in_num = false;
                    for &t in tok {
                        if t.is_ascii_digit() {
                            if !in_num {
                                out.push_str("<n>");
                            }
                            in_num = true;
                        } else {
                            out.push(t as char);
                            in_num = false;
                        }
                    }
                }
                i = j;
            }
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    let keep = out.trim_end().len();
    out.truncate(keep);
}

fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut e = max;
    while !s.is_char_boundary(e) {
        e -= 1;
    }
    &s[..e]
}

// ----- analysis ------------------------------------------------------------

pub struct Analyzer {
    report: Report,
    index: [HashMap<String, usize>; 4],
    overflow: [Option<usize>; 4],
    occ_bytes: usize,
    range: (Option<i64>, Option<i64>),
    pat: String,
}

impl Analyzer {
    pub fn new(range: (Option<i64>, Option<i64>)) -> Analyzer {
        Analyzer {
            report: Report::default(),
            index: Default::default(),
            overflow: [None; 4],
            occ_bytes: 0,
            range,
            pat: String::new(),
        }
    }

    /// Index of the type `self.pat` belongs to, creating it when new.
    fn group(&mut self, lv: usize) -> (usize, bool) {
        if let Some(&g) = self.index[lv].get(self.pat.as_str()) {
            return (g, false);
        }
        let groups = &mut self.report.groups[lv];
        let new = |pattern: String| Group {
            pattern,
            count: 0,
            first: None,
            last: None,
            files: 0,
            last_src: usize::MAX,
            occ: VecDeque::new(),
            trace: Vec::new(),
        };
        if groups.len() >= MAX_GROUPS[lv] {
            self.report.capped[lv] = true;
            let g = *self.overflow[lv].get_or_insert_with(|| {
                groups.push(new(format!(
                    "(other messages: more than {} distinct types)",
                    MAX_GROUPS[lv]
                )));
                groups.len() - 1
            });
            return (g, false);
        }
        groups.push(new(self.pat.clone()));
        self.index[lv].insert(self.pat.clone(), groups.len() - 1);
        (groups.len() - 1, true)
    }

    /// Streams one log. `year` and `not_after` place year-less timestamps.
    pub fn file(
        &mut self,
        stat: &mut FileStat,
        r: impl Read,
        year: i32,
        not_after: i64,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> io::Result<()> {
        let base = self.report.bytes;
        let mut lr = LineReader::new(r, 0);
        let mut buf = Vec::with_capacity(1024);
        let mut last_ts: Option<i64> = None;
        let mut had_ts = false;
        // The entry that still collects its stack trace lines.
        let mut open: Option<(usize, usize)> = None;
        let res = loop {
            if stat.lines.is_multiple_of(512) {
                if cancel.load(Ordering::Relaxed) {
                    break Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                progress.store(base + lr.off, Ordering::Relaxed);
            }
            let off = match lr.next_line(&mut buf) {
                Ok(Some((off, _))) => off,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            stat.lines += 1;
            if buf.is_empty() {
                continue;
            }
            let p = parse_line(&buf, year, not_after);
            // Indented lines, and lines without a time or a clear level in a
            // log that has times, continue the entry above (stack traces).
            if matches!(buf[0], b' ' | b'\t') || (p.ts.is_none() && had_ts && !p.strong) {
                if let Some((lv, g)) = open {
                    let trace = &mut self.report.groups[lv][g].trace;
                    if trace.len() < TRACE_LINES {
                        trace.push(clip(&sanitize(&buf), OCC_TEXT).to_string());
                    }
                }
                continue;
            }
            open = None;
            if let Some(t) = p.ts {
                had_ts = true;
                last_ts = Some(t);
                let r = &mut self.report;
                r.first = Some(r.first.map_or(t, |f| f.min(t)));
                r.last = Some(r.last.map_or(t, |l| l.max(t)));
            }
            let ts = p.ts.or(last_ts);
            if let Some(t) = ts
                && (self.range.0.is_some_and(|f| t < f) || self.range.1.is_some_and(|e| t > e))
            {
                self.report.skipped += 1;
                continue;
            }
            let Some(level) = p.level else {
                stat.counts[4] += 1;
                continue;
            };
            let lv = level as usize;
            stat.counts[lv] += 1;
            if let Some(t) = ts {
                let l = &mut self.report.level_last[lv];
                *l = Some(l.map_or(t, |x| x.max(t)));
            }
            self.pat.clear();
            let text = match p.msg {
                Msg::Range(s, e) => sanitize(&buf[s..e]),
                Msg::Access(status, request) => {
                    // The status is part of the type, ids in the path are not.
                    self.pat.push_str(&format!("HTTP {status} "));
                    format!("HTTP {status} {}", sanitize(request.as_bytes()))
                }
            };
            let skip = self.pat.len();
            normalize(&text[skip..], &mut self.pat);
            let (gi, created) = self.group(lv);
            let g = &mut self.report.groups[lv][gi];
            g.count += 1;
            if let Some(t) = ts {
                g.first = Some(g.first.map_or(t, |f| f.min(t)));
                g.last = Some(g.last.map_or(t, |l| l.max(t)));
            }
            if g.last_src != stat.src {
                g.last_src = stat.src;
                g.files += 1;
            }
            if created {
                open = Some((lv, gi));
            }
            let over = self.occ_bytes > OCC_BUDGET && !g.occ.is_empty();
            if (g.occ.len() >= MAX_OCC[lv] || over)
                && let Some(old) = g.occ.pop_front()
            {
                self.occ_bytes -= old.text.len() + OCC_OVERHEAD;
            }
            let text: Box<str> = clip(&text, OCC_TEXT).into();
            self.occ_bytes += text.len() + OCC_OVERHEAD;
            g.occ.push_back(Occ {
                src: stat.src,
                line: stat.lines,
                off,
                ts,
                text,
            });
        };
        self.report.bytes = base + lr.off;
        res
    }

    pub fn finish(mut self) -> Report {
        let r = &mut self.report;
        for f in &r.files {
            r.lines += f.lines;
            for (t, c) in r.totals.iter_mut().zip(f.counts) {
                *t += c;
            }
        }
        for g in r.groups.iter_mut().flatten() {
            g.occ
                .make_contiguous()
                .sort_by_key(|o| (o.ts, o.src, o.line));
        }
        self.report
    }
}

/// Analyses the given logs in order (oldest first keeps the latest
/// occurrences). Returns None when cancelled. `done` counts finished files.
pub fn analyse(
    sources: Vec<(usize, Source)>,
    range: (Option<i64>, Option<i64>),
    cancel: &AtomicBool,
    progress: &AtomicU64,
    done: &AtomicU64,
) -> Option<Report> {
    let start = Instant::now();
    let now = clock::now_secs();
    let mut a = Analyzer::new(range);
    for (src, source) in sources {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let mtime = source.modified.map_or(now, clock::secs);
        let mut stat = FileStat {
            src,
            lines: 0,
            counts: [0; 5],
            error: None,
        };
        let res = source
            .open_stream()
            .and_then(|r| a.file(&mut stat, r, clock::year(mtime), mtime, cancel, progress));
        match res {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return None,
            Err(e) => stat.error = Some(e.to_string()),
            Ok(()) => {}
        }
        a.report.files.push(stat);
        done.fetch_add(1, Ordering::Relaxed);
    }
    let mut report = a.finish();
    report.secs = start.elapsed().as_secs_f32();
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        let mut out = String::new();
        normalize(s, &mut out);
        out
    }

    fn parse(s: &str) -> (Option<i64>, Option<Level>, String) {
        let p = parse_line(s.as_bytes(), 2026, i64::MAX / 2);
        let msg = match p.msg {
            Msg::Range(a, b) => s[a..b].to_string(),
            Msg::Access(status, m) => format!("HTTP {status} {m}"),
        };
        (p.ts, p.level, msg)
    }

    #[test]
    fn variable_parts_become_placeholders() {
        assert_eq!(
            norm("Connection to 10.0.0.5:5432 timed out after 3000ms"),
            "Connection to <ip>:<n> timed out after <n>ms"
        );
        assert_eq!(
            norm("order 8812 failed for user 'alice' (req 3f2a9c1e-11aa-4bcd-9e0f-0123456789ab)"),
            "order <n> failed for user '<*>' (req <uuid>)"
        );
        assert_eq!(
            norm("can't open file \"/data/a.csv\": took 3.25s"),
            "can't open file \"<*>\": took <n>s"
        );
        assert_eq!(
            norm("ptr 0xDEADBEEF session a81f3c9d2e worker7"),
            "ptr <hex> session <id> worker<n>"
        );
        assert_eq!(
            norm("said \"hello big world\" 5 times"),
            "said \"hello big world\" <n> times"
        );
        assert_eq!(norm("  spaced \t out  "), "spaced out");
        assert_eq!(
            norm("http2 stream reset v1.2.3"),
            "http<n> stream reset v<n>.<n>"
        );
    }

    #[test]
    fn timestamps_and_levels() {
        let t = clock::civil_to_unix(2026, 11, 22, 13, 5, 9, 0).unwrap();
        let (ts, lv, msg) = parse("2026-11-22T13:05:09.123Z ERROR [db] pool exhausted");
        assert_eq!(
            (ts, lv, msg.as_str()),
            (Some(t), Some(Level::Error), "[db] pool exhausted")
        );
        let (ts, lv, msg) = parse("2026-11-22 21:05:09,001+08:00 [main] WARN c.f.Svc - slow call");
        assert_eq!(
            (ts, lv, msg.as_str()),
            (Some(t), Some(Level::Warn), "c.f.Svc - slow call")
        );
        let (ts, lv, msg) = parse("2026/11/22 13:05:09 [error] 31#0: *7 connect() failed");
        assert_eq!(lv, Some(Level::Error));
        assert_eq!(msg, "31#0: *7 connect() failed");
        assert!(ts.is_some());
        let (ts, lv, msg) = parse(
            "10.1.2.3 - bob [22/Nov/2026:13:05:09 +0000] \"GET /api/orders/17?x=1 HTTP/1.1\" 502 17 \"-\" \"curl\"",
        );
        assert_eq!(
            (ts, lv, msg.as_str()),
            (Some(t), Some(Level::Error), "HTTP 502 GET /api/orders/17")
        );
        let (ts, lv, _) = parse("Nov 22 13:05:09 host sshd[812]: debug: session opened");
        assert_eq!(lv, Some(Level::Debug));
        assert_eq!(
            ts,
            clock::civil_to_unix(2026, 11, 22, 13, 5, 9, clock::offset_secs())
        );
        let (ts, lv, msg) = parse(
            "{\"time\":\"2026-11-22T13:05:09Z\",\"level\":\"info\",\"msg\":\"started on 8080\"}",
        );
        assert_eq!(
            (ts, lv, msg.as_str()),
            (Some(t), Some(Level::Info), "started on 8080")
        );
        let (_, lv, msg) = parse("ts=1 level=error msg=\"dial tcp refused\" err=x");
        assert_eq!((lv, msg.as_str()), (Some(Level::Error), "dial tcp refused"));
        // Paths, keys and prose without a level word stay unlevelled.
        assert_eq!(parse("opening /var/log/error.log now").1, None);
        assert_eq!(parse("error_count=0 all good").1, None);
        assert_eq!(parse("plain line").1, None);
        assert_eq!(
            parse("java.lang.NullPointerException: boom").1,
            Some(Level::Error)
        );
    }

    #[test]
    fn groups_errors_with_trace_and_range() {
        let log = "\
2026-11-22 10:00:00 INFO  server started on port 8080
2026-11-22 10:00:01 ERROR Connection to 10.0.0.5:5432 timed out after 3000ms
java.net.SocketTimeoutException: connect timed out
\tat com.demo.Db.connect(Db.java:42)
2026-11-22 10:00:02 DEBUG cache hit key=77
2026-11-22 10:00:03 ERROR Connection to 10.0.0.9:5432 timed out after 5000ms
2026-11-22 10:00:04 WARN  disk usage 91%
2026-11-22 10:00:05 ERROR Payment 5521 declined
no level here
";
        let (c, p) = (AtomicBool::new(false), AtomicU64::new(0));
        let run = |range| {
            let mut a = Analyzer::new(range);
            let mut stat = FileStat {
                src: 0,
                lines: 0,
                counts: [0; 5],
                error: None,
            };
            a.file(&mut stat, log.as_bytes(), 2026, i64::MAX / 2, &c, &p)
                .unwrap();
            a.report.files.push(stat);
            a.finish()
        };
        let r = run((None, None));
        assert_eq!(r.lines, 9);
        assert_eq!(r.totals, [3, 1, 1, 1, 0]);
        let errors = &r.groups[0];
        assert_eq!(errors.len(), 2);
        let g = &errors[0];
        assert_eq!(g.pattern, "Connection to <ip>:<n> timed out after <n>ms");
        assert_eq!((g.count, g.files, g.occ.len()), (2, 1, 2));
        assert_eq!(g.trace.len(), 2);
        assert_eq!((g.occ[0].line, g.occ[1].line), (2, 6));
        assert_eq!(g.last.unwrap() - g.first.unwrap(), 2);
        assert!(g.occ[1].text.contains("10.0.0.9"));

        let from = clock::civil_to_unix(2026, 11, 22, 10, 0, 3, clock::offset_secs());
        let r = run((from, None));
        assert_eq!(r.totals, [2, 1, 0, 0, 0]);
        assert_eq!(r.skipped, 3);
    }
}
