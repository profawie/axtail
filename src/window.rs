//! Bounded in-memory window over a log source.
//!
//! A log is never loaded whole. The viewer only ever holds a `Window`: a
//! bounded run of consecutive lines plus the byte offset of each line, so it
//! can ask for a neighbouring window when the user scrolls past an edge.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Longest line kept in one piece. Longer lines are split into several rows.
pub const MAX_LINE: usize = 8 * 1024;
/// Bytes read on each side of the target for seekable files.
pub const PLAIN_HALF: u64 = 512 * 1024;
/// Hard cap on the bytes a window may hold.
pub const MAX_WINDOW_BYTES: usize = 8 * 1024 * 1024;

pub struct Line {
    pub off: u64,
    pub data: Box<[u8]>,
}

#[derive(Clone, Copy, Debug)]
pub enum Target {
    Start,
    End,
    Around(u64),
}

#[derive(Default)]
pub struct Window {
    pub lines: VecDeque<Line>,
    pub bytes: usize,
    pub at_start: bool,
    pub at_end: bool,
    /// Offsets are relative to where a live stream was attached, not absolute.
    pub relative: bool,
    /// Offset where the next append starts (seekable files only).
    pub end_off: u64,
    /// The last line has no terminating newline yet.
    pub partial: bool,
    /// File size seen at the last load/append.
    pub seen_size: u64,
}

impl Window {
    pub fn push(&mut self, off: u64, data: &[u8]) {
        self.bytes += data.len();
        self.lines.push_back(Line {
            off,
            data: data.into(),
        });
    }

    pub fn pop_front(&mut self) {
        if let Some(l) = self.lines.pop_front() {
            self.bytes -= l.data.len();
            self.at_start = false;
        }
    }

    pub fn pop_back(&mut self) {
        if let Some(l) = self.lines.pop_back() {
            self.bytes -= l.data.len();
        }
    }

    /// Drop lines from the front until the window fits the limits.
    pub fn trim_front(&mut self, max_lines: usize, max_bytes: usize) {
        while self.lines.len() > max_lines || (self.bytes > max_bytes && self.lines.len() > 1) {
            self.pop_front();
        }
    }

    /// Index of the last line starting at or before `off`.
    pub fn index_of(&self, off: u64) -> usize {
        self.lines
            .partition_point(|l| l.off <= off)
            .saturating_sub(1)
    }
}

/// Splits a byte stream into lines while tracking byte offsets.
pub struct LineReader<R> {
    r: R,
    buf: Box<[u8]>,
    pos: usize,
    len: usize,
    pub off: u64,
}

impl<R: Read> LineReader<R> {
    pub fn new(r: R, base: u64) -> Self {
        Self {
            r,
            buf: vec![0u8; 64 * 1024].into(),
            pos: 0,
            len: 0,
            off: base,
        }
    }

    fn fill(&mut self) -> io::Result<bool> {
        if self.pos < self.len {
            return Ok(true);
        }
        loop {
            match self.r.read(&mut self.buf) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pos = 0;
                    self.len = n;
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// True when the next `next_line` call would have to block on the source.
    pub fn drained(&self) -> bool {
        self.pos >= self.len
    }

    /// Reads the next line into `out` (without its line ending).
    /// Returns its start offset and whether it was terminated.
    pub fn next_line(&mut self, out: &mut Vec<u8>) -> io::Result<Option<(u64, bool)>> {
        out.clear();
        let start = self.off;
        loop {
            if !self.fill()? {
                return Ok(if self.off == start {
                    None
                } else {
                    Some((start, false))
                });
            }
            let avail = &self.buf[self.pos..self.len];
            let room = MAX_LINE - out.len();
            let scan = &avail[..avail.len().min(room)];
            if let Some(i) = memchr::memchr(b'\n', scan) {
                out.extend_from_slice(&scan[..i]);
                self.pos += i + 1;
                self.off += i as u64 + 1;
                if out.last() == Some(&b'\r') {
                    out.pop();
                }
                return Ok(Some((start, true)));
            }
            out.extend_from_slice(scan);
            self.pos += scan.len();
            self.off += scan.len() as u64;
            if out.len() >= MAX_LINE {
                // Forced split of an over-long line; swallow a newline that
                // happens to sit exactly at the split point.
                if self.fill()? && self.buf[self.pos] == b'\n' {
                    self.pos += 1;
                    self.off += 1;
                }
                return Ok(Some((start, true)));
            }
        }
    }
}

/// Loads a window from a non-seekable stream (gzip, zip entry, docker).
/// Reads from the very beginning, keeping only a bounded ring of lines.
pub fn load_stream(
    r: impl Read,
    target: Target,
    max_lines: usize,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> io::Result<Window> {
    let mut lr = LineReader::new(r, 0);
    let mut w = Window {
        at_start: true,
        ..Default::default()
    };
    let mut buf = Vec::with_capacity(1024);
    let mut n: u64 = 0;
    let mut after = 0usize; // lines at/after the target (Around only)
    let mut eof = false;
    loop {
        n += 1;
        if n.is_multiple_of(512) {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            progress.store(lr.off, Ordering::Relaxed);
        }
        let Some((off, _)) = lr.next_line(&mut buf)? else {
            eof = true;
            break;
        };
        match target {
            Target::Start => {
                if w.lines.len() >= max_lines || w.bytes + buf.len() > MAX_WINDOW_BYTES {
                    break;
                }
                w.push(off, &buf);
            }
            Target::End => {
                w.push(off, &buf);
                w.trim_front(max_lines, MAX_WINDOW_BYTES);
            }
            Target::Around(t) => {
                if off < t {
                    w.push(off, &buf);
                    w.trim_front(max_lines / 2, MAX_WINDOW_BYTES / 2);
                } else {
                    if after >= max_lines / 2 || w.bytes + buf.len() > MAX_WINDOW_BYTES {
                        break;
                    }
                    w.push(off, &buf);
                    after += 1;
                }
            }
        }
    }
    w.at_end = eof;
    w.end_off = lr.off;
    Ok(w)
}

/// Loads a window from a seekable file by reading only around the target.
pub fn load_plain(path: &Path, target: Target) -> io::Result<Window> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let (start, end) = match target {
        Target::Start => (0, size.min(2 * PLAIN_HALF)),
        Target::End => (size.saturating_sub(2 * PLAIN_HALF), size),
        Target::Around(t) => {
            let t = t.min(size);
            (t.saturating_sub(PLAIN_HALF), size.min(t + PLAIN_HALF))
        }
    };
    let mut w = Window {
        seen_size: size,
        ..Default::default()
    };
    let mut buf = Vec::with_capacity(1024);
    // Start one byte early and discard the first "line" so we begin exactly
    // on a line boundary.
    let seek_to = start.saturating_sub(1);
    f.seek(SeekFrom::Start(seek_to))?;
    let mut lr = LineReader::new(f, seek_to);
    if start > 0 {
        lr.next_line(&mut buf)?;
    }
    w.at_start = lr.off == 0;
    w.end_off = lr.off;
    let mut eof = false;
    while lr.off < end || w.lines.is_empty() {
        match lr.next_line(&mut buf)? {
            None => {
                eof = true;
                break;
            }
            Some((off, terminated)) => {
                w.push(off, &buf);
                if terminated {
                    w.end_off = lr.off;
                } else {
                    w.partial = true;
                    w.end_off = off;
                    eof = true;
                    break;
                }
            }
        }
    }
    w.at_end = eof || lr.off >= size;
    Ok(w)
}

pub enum Appended {
    Nothing,
    Grew,
    /// File shrank (truncated/rotated) or grew too much: reload from the end.
    Reload,
}

/// Appends newly written lines of a growing file to a window at its end.
pub fn append_plain(path: &Path, w: &mut Window) -> io::Result<Appended> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    if size == w.seen_size {
        return Ok(Appended::Nothing);
    }
    if size < w.end_off || size - w.end_off > 2 * PLAIN_HALF {
        return Ok(Appended::Reload);
    }
    w.seen_size = size;
    if w.partial {
        w.pop_back();
        w.partial = false;
    }
    f.seek(SeekFrom::Start(w.end_off))?;
    let mut lr = LineReader::new(f, w.end_off);
    let mut buf = Vec::with_capacity(1024);
    while let Some((off, terminated)) = lr.next_line(&mut buf)? {
        w.push(off, &buf);
        if terminated {
            w.end_off = lr.off;
        } else {
            w.partial = true;
            w.end_off = off;
            break;
        }
    }
    w.trim_front(usize::MAX, MAX_WINDOW_BYTES / 2);
    w.at_end = true;
    Ok(Appended::Grew)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn none() -> (AtomicBool, AtomicU64) {
        (AtomicBool::new(false), AtomicU64::new(0))
    }

    fn text(w: &Window) -> Vec<String> {
        w.lines
            .iter()
            .map(|l| String::from_utf8_lossy(&l.data).into_owned())
            .collect()
    }

    #[test]
    fn splits_lines_and_tracks_offsets() {
        let mut lr = LineReader::new(&b"a\r\nbb\n\nlast"[..], 10);
        let mut buf = Vec::new();
        let mut got = Vec::new();
        while let Some((off, term)) = lr.next_line(&mut buf).unwrap() {
            got.push((off, String::from_utf8(buf.clone()).unwrap(), term));
        }
        assert_eq!(
            got,
            vec![
                (10, "a".into(), true),
                (13, "bb".into(), true),
                (16, "".into(), true),
                (17, "last".into(), false)
            ]
        );
    }

    #[test]
    fn over_long_lines_are_split_not_buffered() {
        let data = vec![b'x'; MAX_LINE * 2 + 5];
        let (c, p) = none();
        let w = load_stream(&data[..], Target::End, 100, &c, &p).unwrap();
        assert_eq!(w.lines.len(), 3);
        assert!(w.lines.iter().all(|l| l.data.len() <= MAX_LINE));
    }

    #[test]
    fn stream_windows_stay_bounded() {
        let data: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let (c, p) = none();
        let end = load_stream(data.as_bytes(), Target::End, 10, &c, &p).unwrap();
        assert_eq!(text(&end).first().unwrap(), "line 990");
        assert!(end.at_end && !end.at_start);
        let start = load_stream(data.as_bytes(), Target::Start, 10, &c, &p).unwrap();
        assert_eq!(text(&start).last().unwrap(), "line 9");
        assert!(start.at_start && !start.at_end);
        let off = end.lines[0].off;
        let mid = load_stream(data.as_bytes(), Target::Around(off), 10, &c, &p).unwrap();
        assert_eq!(
            text(&mid),
            (985..995).map(|i| format!("line {i}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn plain_file_windows_and_live_append() {
        let path = std::env::temp_dir().join(format!("axtail-test-{}.log", std::process::id()));
        let mut f = File::create(&path).unwrap();
        for i in 0..200_000 {
            writeln!(f, "entry number {i}").unwrap();
        }
        f.flush().unwrap();
        let mut w = load_plain(&path, Target::End).unwrap();
        assert!(w.at_end && !w.at_start && w.bytes <= 2 * PLAIN_HALF as usize);
        assert_eq!(text(&w).last().unwrap(), "entry number 199999");
        // every line in the window is whole
        assert!(text(&w).iter().all(|l| l.starts_with("entry number ")));

        let off = w.lines[0].off;
        let around = load_plain(&path, Target::Around(off)).unwrap();
        assert_eq!(around.lines[around.index_of(off)].off, off);
        assert!(load_plain(&path, Target::Start).unwrap().at_start);

        write!(f, "appended\npartial").unwrap();
        f.flush().unwrap();
        assert!(matches!(
            append_plain(&path, &mut w).unwrap(),
            Appended::Grew
        ));
        assert_eq!(text(&w).last().unwrap(), "partial");
        writeln!(f, " done").unwrap();
        f.flush().unwrap();
        append_plain(&path, &mut w).unwrap();
        let t = text(&w);
        assert_eq!(&t[t.len() - 2..], ["appended", "partial done"]);
        assert!(matches!(
            append_plain(&path, &mut w).unwrap(),
            Appended::Nothing
        ));

        f.set_len(10).unwrap();
        assert!(matches!(
            append_plain(&path, &mut w).unwrap(),
            Appended::Reload
        ));
        std::fs::remove_file(&path).unwrap();
    }
}
