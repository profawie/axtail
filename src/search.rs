//! Streaming, cancellable search with bounded memory.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::source::Source;

const CHUNK: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Text,
    TextCase,
    Regex,
    RegexNoCase,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Text => "text",
            Mode::TextCase => "text, case-sensitive",
            Mode::Regex => "regex",
            Mode::RegexNoCase => "regex, ignore case",
        }
    }

    pub fn next(self) -> Mode {
        match self {
            Mode::Text => Mode::TextCase,
            Mode::TextCase => Mode::Regex,
            Mode::Regex => Mode::RegexNoCase,
            Mode::RegexNoCase => Mode::Text,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Towards older lines (start of file).
    Back,
    /// Towards newer lines (end of file).
    Fwd,
}

pub struct Matcher {
    pub bytes: regex::bytes::Regex,
    pub text: regex::Regex,
    pub query: String,
    pub mode: Mode,
}

impl Matcher {
    pub fn new(query: &str, mode: Mode) -> Result<Matcher, String> {
        let pattern = match mode {
            Mode::Text | Mode::TextCase => regex::escape(query),
            Mode::Regex | Mode::RegexNoCase => query.to_string(),
        };
        let nocase = matches!(mode, Mode::Text | Mode::RegexNoCase);
        let short = |e: regex::Error| {
            e.to_string()
                .lines()
                .last()
                .unwrap_or("invalid pattern")
                .to_string()
        };
        let bytes = regex::bytes::RegexBuilder::new(&pattern)
            .case_insensitive(nocase)
            .multi_line(true)
            .crlf(true)
            .unicode(true)
            .build()
            .map_err(short)?;
        let text = regex::RegexBuilder::new(&pattern)
            .case_insensitive(nocase)
            .multi_line(true)
            .build()
            .map_err(short)?;
        Ok(Matcher {
            bytes,
            text,
            query: query.to_string(),
            mode,
        })
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

/// Calls `f(chunk_relative_line_start)` once per matching line in `hay`.
/// Stops early when `f` returns false. Returns false if stopped.
fn each_match_line(re: &regex::bytes::Regex, hay: &[u8], mut f: impl FnMut(usize) -> bool) -> bool {
    let mut pos = 0;
    while pos < hay.len() {
        let Some(m) = re.find_at(hay, pos) else { break };
        if m.start() >= hay.len() {
            break;
        }
        let line_start = memchr::memrchr(b'\n', &hay[..m.start()]).map_or(0, |i| i + 1);
        if !f(line_start) {
            return false;
        }
        match memchr::memchr(b'\n', &hay[m.start()..]) {
            Some(i) => pos = m.start() + i + 1,
            None => break,
        }
    }
    true
}

/// Scans a stream chunk by chunk, calling `f(absolute_line_offset)` for each
/// matching line until it returns false or `stop_at` bytes were scanned.
fn scan_stream(
    r: &mut dyn Read,
    mut base: u64,
    re: &regex::bytes::Regex,
    stop_at: u64,
    cancel: &AtomicBool,
    progress: &AtomicU64,
    mut f: impl FnMut(u64) -> bool,
) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut filled = 0;
    let mut eof = false;
    while !eof && base < stop_at {
        while filled < CHUNK {
            match r.read(&mut buf[filled..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
        }
        // Only scan whole lines; carry the unfinished tail to the next chunk.
        let end = if eof {
            filled
        } else {
            memchr::memrchr(b'\n', &buf[..filled]).map_or(filled, |i| i + 1)
        };
        if !each_match_line(re, &buf[..end], |ls| f(base + ls as u64)) {
            return Ok(());
        }
        buf.copy_within(end..filled, 0);
        filled -= end;
        base += end as u64;
        progress.store(base, Ordering::Relaxed);
    }
    Ok(())
}

/// Last matching line that starts before `hi`, reading the file backwards.
fn plain_back(
    path: &Path,
    re: &regex::bytes::Regex,
    hi: u64,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> io::Result<Option<u64>> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut end = hi.min(size);
    let top = end;
    let mut buf = vec![0u8; CHUNK];
    while end > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let start = end.saturating_sub(CHUNK as u64);
        let len = (end - start) as usize;
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut buf[..len])?;
        // Skip the partial first line; it belongs to the next (earlier) chunk.
        let mut first = if start == 0 {
            0
        } else {
            memchr::memchr(b'\n', &buf[..len]).map_or(0, |i| i + 1)
        };
        if first >= len {
            first = 0;
        }
        let mut last = None;
        each_match_line(re, &buf[first..len], |ls| {
            last = Some(ls);
            true
        });
        if let Some(ls) = last {
            return Ok(Some(start + (first + ls) as u64));
        }
        end = start + first as u64;
        progress.store(top - end, Ordering::Relaxed);
    }
    Ok(None)
}

/// Finds the nearest matching line.
/// `Back`: last line starting before `pos`. `Fwd`: first line starting at or after `pos`.
pub fn find(
    src: &Source,
    m: &Matcher,
    pos: u64,
    dir: Dir,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> io::Result<Option<u64>> {
    let mut found = None;
    match (dir, src.plain_path()) {
        (Dir::Back, Some(path)) => return plain_back(path, &m.bytes, pos, cancel, progress),
        (Dir::Back, None) => {
            let mut r = src.open_stream()?;
            scan_stream(&mut r, 0, &m.bytes, pos, cancel, progress, |off| {
                if off < pos {
                    found = Some(off);
                }
                off < pos
            })?;
        }
        (Dir::Fwd, _) if pos == u64::MAX => {}
        (Dir::Fwd, path) => {
            let (mut r, base): (Box<dyn Read + Send>, u64) = match path {
                Some(p) => {
                    let mut f = File::open(p)?;
                    let base = pos.saturating_sub(1);
                    f.seek(SeekFrom::Start(base))?;
                    (Box::new(f), base)
                }
                None => (src.open_stream()?, 0),
            };
            scan_stream(&mut r, base, &m.bytes, u64::MAX, cancel, progress, |off| {
                if off >= pos {
                    found = Some(off);
                }
                off < pos
            })?;
        }
    }
    Ok(found)
}

/// Counts matching lines in a whole source.
pub fn count(src: &Source, m: &Matcher, cancel: &AtomicBool) -> io::Result<u64> {
    let mut r = src.open_stream()?;
    let mut n = 0u64;
    let progress = AtomicU64::new(0);
    scan_stream(&mut r, 0, &m.bytes, u64::MAX, cancel, &progress, |_| {
        n += 1;
        true
    })?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{Kind, Source};
    use std::io::Write;

    fn offsets(data: &[u8], pat: &str, mode: Mode) -> Vec<u64> {
        let m = Matcher::new(pat, mode).unwrap();
        let mut out = Vec::new();
        let (c, p) = (AtomicBool::new(false), AtomicU64::new(0));
        scan_stream(&mut &data[..], 0, &m.bytes, u64::MAX, &c, &p, |o| {
            out.push(o);
            true
        })
        .unwrap();
        out
    }

    #[test]
    fn reports_each_matching_line_once() {
        let data = b"ok\nerror error\nfine\r\nError: x\nlast error";
        assert_eq!(offsets(data, "error", Mode::Text), vec![3, 21, 30]);
        assert_eq!(offsets(data, "error", Mode::TextCase), vec![3, 30]);
        assert_eq!(offsets(data, "^fine$", Mode::Regex), vec![15]);
        assert_eq!(offsets(data, "a.b", Mode::Text), Vec::<u64>::new());
    }

    #[test]
    fn finds_across_chunk_boundaries_in_both_directions() {
        let path = std::env::temp_dir().join(format!("axtail-search-{}.log", std::process::id()));
        let mut f = File::create(&path).unwrap();
        let mut hits = Vec::new();
        let mut off = 0u64;
        for i in 0..150_000 {
            let line = if i % 50_000 == 49_999 {
                format!("{i} NEEDLE here\n")
            } else {
                format!("{i} filler filler filler\n")
            };
            if line.contains("NEEDLE") {
                hits.push(off);
            }
            off += line.len() as u64;
            f.write_all(line.as_bytes()).unwrap();
        }
        drop(f);
        assert!(off > 2 * CHUNK as u64);
        let m = Matcher::new("needle", Mode::Text).unwrap();
        let (c, p) = (AtomicBool::new(false), AtomicU64::new(0));
        let plain = Source {
            kind: Kind::Plain(path.clone()),
            name: String::new(),
            size: None,
            modified: None,
        };
        let run = |pos, dir| find(&plain, &m, pos, dir, &c, &p).unwrap();
        assert_eq!(run(u64::MAX, Dir::Back), Some(hits[2]));
        assert_eq!(run(hits[2], Dir::Back), Some(hits[1]));
        assert_eq!(run(hits[0], Dir::Back), None);
        assert_eq!(run(0, Dir::Fwd), Some(hits[0]));
        assert_eq!(run(hits[0], Dir::Fwd), Some(hits[0]));
        assert_eq!(run(hits[0] + 1, Dir::Fwd), Some(hits[1]));
        assert_eq!(run(hits[2] + 1, Dir::Fwd), None);
        assert_eq!(count(&plain, &m, &c).unwrap(), 3);
        std::fs::remove_file(&path).unwrap();
    }
}
