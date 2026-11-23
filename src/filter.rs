//! Line filter typed in the header ("logcat style") and the chunked
//! scanning used to apply it to large files with bounded memory.
//!
//! Query syntax, always case-insensitive:
//!   error timeout      lines containing "error" OR "timeout"
//!   +db error timeout  ...and that also contain "db"
//!   error -healthcheck lines with "error" but without "healthcheck"
//!   "connection reset" an exact phrase (quotes keep the space)
//!   /time(d)?out \d+/  a regular expression (the whole query between slashes)

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};

use regex::bytes::{Regex, RegexBuilder};

use crate::source::Source;

pub const CHUNK: usize = 1024 * 1024;

pub struct Filter {
    pub text: String,
    any: Option<Regex>,
    all: Vec<Regex>,
    none: Vec<Regex>,
    /// Positive terms, for highlighting matches on screen.
    pub highlight: Option<regex::Regex>,
}

fn build(pattern: &str) -> Result<Regex, String> {
    // Multi-line so ^ and $ mean line start/end when scanning whole chunks.
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .multi_line(true)
        .crlf(true)
        .unicode(true)
        .build()
        .map_err(|e| {
            e.to_string()
                .lines()
                .last()
                .unwrap_or("invalid pattern")
                .to_string()
        })
}

fn build_text(pattern: &str) -> Option<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .ok()
}

/// Splits a query into terms; double quotes keep spaces inside a term.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Filter {
    /// Parses a typed query. Returns `Ok(None)` for an empty query.
    pub fn parse(text: &str) -> Result<Option<Filter>, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        if trimmed.len() > 2 && trimmed.starts_with('/') && trimmed.ends_with('/') {
            return Filter::regex(&trimmed[1..trimmed.len() - 1], text).map(Some);
        }
        let (mut any, mut all, mut none) = (Vec::new(), Vec::new(), Vec::new());
        for t in tokens(trimmed) {
            match t.as_str() {
                s if s.len() > 1 && s.starts_with('+') => all.push(regex::escape(&s[1..])),
                s if s.len() > 1 && s.starts_with('-') => none.push(regex::escape(&s[1..])),
                s => any.push(regex::escape(s)),
            }
        }
        if any.is_empty() && all.is_empty() && none.is_empty() {
            return Ok(None);
        }
        let positive: Vec<&String> = any.iter().chain(all.iter()).collect();
        let highlight = if positive.is_empty() {
            None
        } else {
            build_text(
                &positive
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("|"),
            )
        };
        Ok(Some(Filter {
            text: text.to_string(),
            any: if any.is_empty() {
                None
            } else {
                Some(build(&any.join("|"))?)
            },
            all: all.iter().map(|p| build(p)).collect::<Result<_, _>>()?,
            none: none.iter().map(|p| build(p)).collect::<Result<_, _>>()?,
            highlight,
        }))
    }

    /// A filter from one regular expression (used by `--grep`).
    pub fn regex(pattern: &str, text: &str) -> Result<Filter, String> {
        Ok(Filter {
            text: text.to_string(),
            any: Some(build(pattern)?),
            all: Vec::new(),
            none: Vec::new(),
            highlight: build_text(pattern),
        })
    }

    pub fn is_match(&self, line: &[u8]) -> bool {
        self.any.as_ref().is_none_or(|r| r.is_match(line))
            && self.all.iter().all(|r| r.is_match(line))
            && !self.none.iter().any(|r| r.is_match(line))
    }

    /// Calls `f(start, line)` for every matching line of `chunk`, which must
    /// hold whole lines. A fast regex search skips non-candidate lines, so a
    /// rare term costs about as much as a plain substring search. Stops when
    /// `f` returns false; returns false in that case.
    pub fn for_each_match(&self, chunk: &[u8], mut f: impl FnMut(usize, &[u8]) -> bool) -> bool {
        let candidate = self.any.as_ref().or(self.all.first());
        let mut pos = 0;
        while pos < chunk.len() {
            let (start, end) = match candidate {
                Some(re) => {
                    let Some(m) = re.find_at(chunk, pos) else {
                        break;
                    };
                    let start =
                        memchr::memrchr(b'\n', &chunk[pos..m.start()]).map_or(pos, |i| pos + i + 1);
                    let end = memchr::memchr(b'\n', &chunk[m.start()..])
                        .map_or(chunk.len(), |i| m.start() + i);
                    (start, end)
                }
                None => {
                    let end = memchr::memchr(b'\n', &chunk[pos..]).map_or(chunk.len(), |i| pos + i);
                    (pos, end)
                }
            };
            let mut line = &chunk[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if self.is_match(line) && !f(start, line) {
                return false;
            }
            pos = end + 1;
        }
        true
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

/// Reads a stream in chunks of whole lines, calling `f(chunk_base, chunk)`.
/// Stops when `f` returns false.
pub fn for_each_chunk(
    r: &mut dyn Read,
    mut base: u64,
    cancel: &AtomicBool,
    mut f: impl FnMut(u64, &[u8]) -> bool,
) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut filled = 0;
    let mut eof = false;
    while !eof {
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
        // Only whole lines; the unfinished tail moves to the next chunk.
        // A single line longer than a chunk is processed in pieces.
        let end = if eof {
            filled
        } else {
            memchr::memrchr(b'\n', &buf[..filled]).map_or(filled, |i| i + 1)
        };
        if !f(base, &buf[..end]) {
            return Ok(());
        }
        buf.copy_within(end..filled, 0);
        filled -= end;
        base += end as u64;
    }
    Ok(())
}

/// Counts matching lines in a whole source (for `--grep` in the file list).
pub fn count(src: &Source, filter: &Filter, cancel: &AtomicBool) -> io::Result<u64> {
    let mut r = src.open_stream()?;
    let mut n = 0u64;
    for_each_chunk(&mut r, 0, cancel, |_, chunk| {
        filter.for_each_match(chunk, |_, _| {
            n += 1;
            true
        });
        true
    })?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(q: &str, data: &str) -> Vec<String> {
        let f = Filter::parse(q).unwrap().unwrap();
        let mut out = Vec::new();
        f.for_each_match(data.as_bytes(), |_, l| {
            out.push(String::from_utf8_lossy(l).into_owned());
            true
        });
        out
    }

    const LOG: &str = "INFO start\nERROR db timeout\nWARN slow\nerror healthcheck failed\r\nINFO Timeout retry\nconnection reset by peer\n";

    #[test]
    fn terms_are_or_case_insensitive() {
        assert_eq!(
            lines("error timeout", LOG),
            [
                "ERROR db timeout",
                "error healthcheck failed",
                "INFO Timeout retry"
            ]
        );
    }

    #[test]
    fn required_excluded_and_phrases() {
        assert_eq!(lines("error +db", LOG), ["ERROR db timeout"]);
        assert_eq!(lines("error -healthcheck", LOG), ["ERROR db timeout"]);
        assert_eq!(lines("-info", LOG).len(), 4);
        assert_eq!(
            lines("\"connection reset\"", LOG),
            ["connection reset by peer"]
        );
        assert_eq!(lines("\"reset connection\"", LOG), Vec::<String>::new());
    }

    #[test]
    fn regex_and_empty_queries() {
        assert_eq!(lines("/^warn/", LOG), ["WARN slow"]);
        assert!(Filter::parse("   ").unwrap().is_none());
        assert!(Filter::parse("/(/").is_err());
        // Regex characters in plain terms are literal.
        assert_eq!(
            lines("db.timeout", "db timeout\ndb.timeout\n"),
            ["db.timeout"]
        );
    }

    #[test]
    fn chunks_hold_whole_lines() {
        let data: String = (0..200_000)
            .map(|i| format!("{i} {}\n", if i % 70_000 == 1 { "NEEDLE" } else { "hay" }))
            .collect();
        let f = Filter::parse("needle").unwrap().unwrap();
        let mut hits = Vec::new();
        for_each_chunk(
            &mut data.as_bytes(),
            0,
            &AtomicBool::new(false),
            |base, chunk| {
                f.for_each_match(chunk, |start, l| {
                    hits.push((base + start as u64, String::from_utf8_lossy(l).into_owned()));
                    true
                });
                true
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 3);
        for (off, line) in hits {
            assert!(data[off as usize..].starts_with(&line));
        }
    }
}
