//! Exports message types from the Summary dashboard to a Markdown report.
//!
//! The summary keeps only a short preview of each occurrence. An export reads
//! the logs again, only the files that hold the exported occurrences, and
//! takes each occurrence's full line and every stack trace line under it.
//! Stack traces that differ only in variable parts (numbers, ids) are shown
//! once, with how many occurrences share them.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::clock;
use crate::source::Source;
use crate::summary::{self, Group, LEVEL_NAMES};
use crate::ui::{VERSION, commas, sanitize, span_text, stamp};
use crate::window::LineReader;

/// Stack trace lines kept per occurrence.
const TRACE_MAX: usize = 500;
/// Bytes kept per line.
const LINE_MAX: usize = 4000;
/// Global budget for the text of distinct stack traces.
const TRACE_BUDGET: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct OccRef {
    pub src: usize,
    pub line: u64,
    pub off: u64,
    pub ts: Option<i64>,
}

/// One message type to export, copied from the summary.
pub struct Item {
    pub level: usize,
    pub pattern: String,
    pub count: u64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub files: u32,
    /// The kept occurrences, oldest first.
    pub occ: Vec<OccRef>,
}

impl Item {
    pub fn new(level: usize, g: &Group) -> Item {
        Item {
            level,
            pattern: g.pattern.clone(),
            count: g.count,
            first: g.first,
            last: g.last,
            files: g.files,
            occ: g
                .occ
                .iter()
                .map(|o| OccRef {
                    src: o.src,
                    line: o.line,
                    off: o.off,
                    ts: o.ts,
                })
                .collect(),
        }
    }
}

pub struct Request {
    /// What was analysed, as shown in the title bar.
    pub title: String,
    /// What is exported, e.g. "all 71 ERROR types".
    pub what: String,
    pub range: String,
    pub search: String,
    pub analysed_files: usize,
    pub totals: [u64; 5],
    pub items: Vec<Item>,
    /// Every listed source by index (only the needed ones are read).
    pub sources: Vec<Source>,
    pub path: PathBuf,
}

pub struct Done {
    pub path: PathBuf,
    pub types: usize,
    pub entries: usize,
    /// Occurrences whose line could not be read again.
    pub missing: usize,
    pub errors: Vec<String>,
}

/// One distinct stack trace of a type.
struct Variant {
    trace: Vec<String>,
    more: bool,
    count: usize,
    /// The occurrence the trace was taken from.
    example: usize,
}

#[derive(Default)]
struct Captured {
    /// Full line of each occurrence (same order as `Item::occ`).
    lines: Vec<Option<String>>,
    /// Variant index of each occurrence.
    variant: Vec<Option<usize>>,
    variants: Vec<Variant>,
    keys: HashMap<String, usize>,
}

struct Open {
    item: usize,
    occ: usize,
    trace: Vec<String>,
    more: bool,
}

fn clip(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut e = max;
        while !s.is_char_boundary(e) {
            e -= 1;
        }
        s.truncate(e);
        s.push_str(" ...");
    }
    s
}

struct Capture {
    out: Vec<Captured>,
    budget: usize,
}

impl Capture {
    fn close(&mut self, o: Open) {
        let mut key = String::new();
        for l in &o.trace {
            summary::normalize(l, &mut key);
            key.push('\n');
        }
        let c = &mut self.out[o.item];
        let v = match c.keys.get(&key) {
            Some(&v) => v,
            None => {
                let bytes: usize = o.trace.iter().map(String::len).sum();
                let (trace, more) = if bytes <= self.budget {
                    self.budget -= bytes;
                    (o.trace, o.more)
                } else {
                    (
                        vec!["(trace not kept: export size limit reached)".into()],
                        false,
                    )
                };
                c.variants.push(Variant {
                    trace,
                    more,
                    count: 0,
                    example: o.occ,
                });
                c.keys.insert(key, c.variants.len() - 1);
                c.variants.len() - 1
            }
        };
        c.variants[v].count += 1;
        c.variant[o.occ] = Some(v);
    }

    /// Reads one log and takes the wanted entries with their stack traces.
    fn file(
        &mut self,
        source: &Source,
        wants: &HashMap<u64, (usize, usize)>,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> io::Result<()> {
        let min = wants.keys().copied().min().unwrap_or(0);
        let (r, base): (Box<dyn Read>, u64) = match source.plain_path() {
            Some(p) => {
                let mut f = File::open(p)?;
                f.seek(SeekFrom::Start(min))?;
                (Box::new(f), min)
            }
            None => (Box::new(source.open_stream()?), 0),
        };
        let mtime = source.modified.map_or(clock::now_secs(), clock::secs);
        let year = clock::year(mtime);
        let start = progress.load(Ordering::Relaxed);
        let mut lr = LineReader::new(r, base);
        let mut buf = Vec::with_capacity(1024);
        let mut had_ts = false;
        let mut open: Option<Open> = None;
        let mut found = 0;
        let mut n = 0u64;
        loop {
            n += 1;
            if n.is_multiple_of(512) {
                if cancel.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                progress.store(start + lr.off - base, Ordering::Relaxed);
            }
            let Some((off, _)) = lr.next_line(&mut buf)? else {
                break;
            };
            if let Some(&(item, occ)) = wants.get(&off) {
                if let Some(o) = open.take() {
                    self.close(o);
                }
                summary::continues(&buf, year, mtime, &mut had_ts);
                self.out[item].lines[occ] = Some(clip(sanitize(&buf), LINE_MAX));
                open = Some(Open {
                    item,
                    occ,
                    trace: Vec::new(),
                    more: false,
                });
                found += 1;
                continue;
            }
            let cont = summary::continues(&buf, year, mtime, &mut had_ts);
            if let Some(o) = &mut open {
                if cont {
                    if buf.is_empty() {
                        continue;
                    }
                    if o.trace.len() < TRACE_MAX {
                        o.trace.push(clip(sanitize(&buf), LINE_MAX));
                    } else {
                        o.more = true;
                    }
                    continue;
                }
                let o = open.take().unwrap();
                self.close(o);
            }
            if found == wants.len() {
                break;
            }
        }
        if let Some(o) = open.take() {
            self.close(o);
        }
        progress.store(start + lr.off - base, Ordering::Relaxed);
        Ok(())
    }
}

/// The longest run of backticks in `text`, so a code fence can be longer.
fn fence(lines: &[&str]) -> String {
    let mut longest = 0;
    for l in lines {
        let mut run = 0;
        for c in l.chars() {
            run = if c == '`' { run + 1 } else { 0 };
            longest = longest.max(run);
        }
    }
    "`".repeat(longest.max(2) + 1)
}

fn code(w: &mut impl Write, lines: &[&str]) -> io::Result<()> {
    let f = fence(lines);
    writeln!(w, "{f}text")?;
    for l in lines {
        writeln!(w, "{l}")?;
    }
    writeln!(w, "{f}\n")
}

/// Runs an export. Returns None when cancelled.
pub fn run(
    req: Request,
    cancel: &AtomicBool,
    progress: &AtomicU64,
    done_files: &AtomicU64,
) -> Option<Result<Done, String>> {
    // Which entries to take from which file.
    let mut per_src: BTreeMap<usize, HashMap<u64, (usize, usize)>> = BTreeMap::new();
    for (i, item) in req.items.iter().enumerate() {
        for (k, o) in item.occ.iter().enumerate() {
            per_src.entry(o.src).or_default().insert(o.off, (i, k));
        }
    }
    let mut cap = Capture {
        out: req
            .items
            .iter()
            .map(|it| Captured {
                lines: vec![None; it.occ.len()],
                variant: vec![None; it.occ.len()],
                ..Default::default()
            })
            .collect(),
        budget: TRACE_BUDGET,
    };
    let mut errors = Vec::new();
    for (src, wants) in &per_src {
        let source = &req.sources[*src];
        match cap.file(source, wants, cancel, progress) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return None,
            Err(e) => errors.push(format!("{}: {e}", source.name)),
            Ok(()) => {}
        }
        done_files.fetch_add(1, Ordering::Relaxed);
    }
    Some(write_report(&req, &cap.out, errors).map_err(|e| format!("{}: {e}", req.path.display())))
}

fn write_report(req: &Request, cap: &[Captured], errors: Vec<String>) -> io::Result<Done> {
    if let Some(dir) = req.path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut w = BufWriter::new(File::create(&req.path)?);
    let name = |src: usize| req.sources.get(src).map_or("?", |s| s.name.as_str());
    let entries: usize = cap.iter().map(|c| c.lines.iter().flatten().count()).sum();
    let missing: usize = cap
        .iter()
        .map(|c| c.lines.iter().filter(|l| l.is_none()).count())
        .sum();

    writeln!(w, "# axtail export: {}\n", req.what)?;
    writeln!(w, "| | |\n|---|---|")?;
    writeln!(w, "| Source | {} |", req.title.replace('|', "\\|"))?;
    writeln!(
        w,
        "| Created | {} ({}) by axtail {VERSION} |",
        clock::format_secs(clock::now_secs()),
        clock::offset_label()
    )?;
    writeln!(w, "| Time range | {} |", req.range)?;
    if !req.search.is_empty() {
        writeln!(w, "| Search | {} |", req.search.replace('|', "\\|"))?;
    }
    writeln!(
        w,
        "| Files analysed | {} |",
        commas(req.analysed_files as u64)
    )?;
    writeln!(
        w,
        "| Entries | ERROR {} · WARN {} · INFO {} · DEBUG {} · no level {} |",
        commas(req.totals[0]),
        commas(req.totals[1]),
        commas(req.totals[2]),
        commas(req.totals[3]),
        commas(req.totals[4])
    )?;
    writeln!(w, "| Contains | {} |\n", req.what)?;
    writeln!(
        w,
        "Each type lists its latest occurrences (up to 300 per type, as kept by the \
         summary) with the full log line, and every distinct stack trace under them. \
         Traces that differ only in numbers or ids count as one.\n"
    )?;
    if !errors.is_empty() {
        writeln!(w, "**Could not read again:**\n")?;
        for e in &errors {
            writeln!(w, "- {}", e.replace('\n', " "))?;
        }
        writeln!(w)?;
    }

    // Contents.
    writeln!(w, "## Contents\n")?;
    let rows: Vec<String> = req
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            format!(
                "{:>3}. {:<5} {:>10}  last {}  {}",
                i + 1,
                LEVEL_NAMES[it.level],
                commas(it.count),
                stamp(it.last),
                it.pattern
            )
        })
        .collect();
    code(&mut w, &rows.iter().map(String::as_str).collect::<Vec<_>>())?;

    for (i, (it, c)) in req.items.iter().zip(cap).enumerate() {
        writeln!(
            w,
            "---\n\n## {}. {} · {} occurrence{}\n",
            i + 1,
            LEVEL_NAMES[it.level],
            commas(it.count),
            if it.count == 1 { "" } else { "s" }
        )?;
        writeln!(w, "**Type**\n")?;
        code(&mut w, &[&it.pattern])?;
        let span = match (it.first, it.last) {
            (Some(a), Some(b)) => span_text(b - a),
            _ => "-".into(),
        };
        writeln!(w, "| | |\n|---|---|")?;
        writeln!(w, "| Level | {} |", LEVEL_NAMES[it.level])?;
        writeln!(
            w,
            "| Count | {} in {} file{} |",
            commas(it.count),
            it.files,
            if it.files == 1 { "" } else { "s" }
        )?;
        writeln!(w, "| First seen | {} |", stamp(it.first))?;
        writeln!(w, "| Last seen | {} |", stamp(it.last))?;
        writeln!(w, "| Span | {span} |")?;
        writeln!(
            w,
            "| Kept | latest {} of {} |",
            commas(it.occ.len() as u64),
            commas(it.count)
        )?;
        writeln!(w, "| Stack traces | {} distinct |\n", c.variants.len())?;

        // Files of the kept occurrences, most first.
        let mut files: BTreeMap<usize, (usize, Option<i64>)> = BTreeMap::new();
        for o in &it.occ {
            let e = files.entry(o.src).or_default();
            e.0 += 1;
            e.1 = e.1.max(o.ts);
        }
        let mut files: Vec<_> = files.into_iter().collect();
        files.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(b.1.1.cmp(&a.1.1)));
        writeln!(w, "**Files** (of the kept occurrences)\n")?;
        let lines: Vec<String> = files
            .iter()
            .map(|(src, (n, last))| format!("{:>6}  last {}  {}", n, stamp(*last), name(*src)))
            .collect();
        code(
            &mut w,
            &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;

        // The newest message in full.
        if let Some(line) = c.lines.iter().rev().flatten().next() {
            writeln!(w, "**Latest message**\n")?;
            code(&mut w, &[line])?;
        }

        // Distinct stack traces, most frequent first.
        let mut order: Vec<usize> = (0..c.variants.len()).collect();
        order.sort_by_key(|&v| std::cmp::Reverse(c.variants[v].count));
        let label: HashMap<usize, usize> =
            order.iter().enumerate().map(|(n, &v)| (v, n + 1)).collect();
        for (n, &v) in order.iter().enumerate() {
            let var = &c.variants[v];
            let ex = &it.occ[var.example];
            writeln!(
                w,
                "### Stack trace {} · {} of {} kept occurrence{}\n",
                n + 1,
                commas(var.count as u64),
                commas(it.occ.len() as u64),
                if it.occ.len() == 1 { "" } else { "s" }
            )?;
            writeln!(
                w,
                "Example: {}  ·  {} line {}\n",
                stamp(ex.ts),
                name(ex.src),
                commas(ex.line)
            )?;
            let mut lines: Vec<&str> = Vec::with_capacity(var.trace.len() + 2);
            if let Some(l) = &c.lines[var.example] {
                lines.push(l);
            }
            if var.trace.is_empty() {
                lines.push("    (no stack trace under this line)");
            }
            lines.extend(var.trace.iter().map(String::as_str));
            let more = format!("    ... more than {TRACE_MAX} lines, the rest is not shown");
            if var.more {
                lines.push(&more);
            }
            code(&mut w, &lines)?;
        }

        // Every kept occurrence, newest first.
        writeln!(
            w,
            "**Occurrences** (latest {} of {}, newest first)\n",
            commas(it.occ.len() as u64),
            commas(it.count)
        )?;
        let lines: Vec<String> = it
            .occ
            .iter()
            .enumerate()
            .rev()
            .map(|(k, o)| {
                let trace = c.variant[k].map_or("-".into(), |v| format!("#{}", label[&v]));
                format!(
                    "{}  {}:{}  trace {}  {}",
                    stamp(o.ts),
                    name(o.src),
                    o.line,
                    trace,
                    c.lines[k]
                        .as_deref()
                        .unwrap_or("(line could not be read again)")
                )
            })
            .collect();
        code(
            &mut w,
            &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
    }
    w.flush()?;
    Ok(Done {
        path: req.path.clone(),
        types: req.items.len(),
        entries,
        missing,
        errors,
    })
}

/// Where a new export goes: $AXTAIL_EXPORT_DIR, else the current folder,
/// else the home folder, else the temp folder (the first that is writable).
pub fn target(level: &str, selected: bool) -> PathBuf {
    let digits: String = clock::format_secs(clock::now_secs())
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let (day, time) = digits.split_at(digits.len().min(8));
    let name = format!(
        "axtail-{}-{}-{day}-{time}.md",
        level.to_lowercase(),
        if selected { "selected" } else { "all" }
    );
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(d) = var("AXTAIL_EXPORT_DIR") {
        return Path::new(&d).join(name);
    }
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::current_dir() {
        dirs.push(d);
    }
    if let Some(h) = var("HOME").or_else(|| var("USERPROFILE")) {
        dirs.push(PathBuf::from(h));
    }
    dirs.push(std::env::temp_dir());
    for d in &dirs {
        let probe = d.join(format!(".{name}.probe"));
        if File::create(&probe).is_ok() {
            let _ = fs::remove_file(&probe);
            return d.join(name);
        }
    }
    dirs[0].join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{Kind, Source};

    #[test]
    fn full_lines_and_distinct_traces() {
        let dir = std::env::temp_dir().join(format!("axtail-export-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("app.log");
        let text = "\
2026-11-22 10:00:00 INFO start
2026-11-22 10:00:01 ERROR Pool timeout after 5000ms (request 1)
java.sql.SQLException: timeout 1
\tat com.x.Pool.get(Pool.java:10)
\tat com.x.Svc.run(Svc.java:20)
2026-11-22 10:00:02 INFO ok
2026-11-22 10:00:03 ERROR Pool timeout after 5000ms (request 2)
java.sql.SQLException: timeout 2
\tat com.x.Pool.get(Pool.java:10)
\tat com.x.Svc.run(Svc.java:20)
2026-11-22 10:00:04 ERROR Pool timeout after 5000ms (request 3)
\tat com.x.Other.call(Other.java:7)
2026-11-22 10:00:05 INFO done
";
        fs::write(&log, text).unwrap();
        let off = |needle: &str| text.find(needle).unwrap() as u64;
        let occ = |n: &str, line: u64| OccRef {
            src: 0,
            line,
            off: off(n),
            ts: Some(0),
        };
        let item = Item {
            level: 0,
            pattern: "Pool timeout after <n>ms (request <n>)".into(),
            count: 3,
            first: Some(0),
            last: Some(3),
            files: 1,
            occ: vec![
                occ("2026-11-22 10:00:01", 2),
                occ("2026-11-22 10:00:03", 7),
                occ("2026-11-22 10:00:04", 11),
            ],
        };
        let out = dir.join("out.md");
        let req = Request {
            title: "test".into(),
            what: "all 1 ERROR types".into(),
            range: "All time".into(),
            search: String::new(),
            analysed_files: 1,
            totals: [3, 0, 3, 0, 0],
            items: vec![item],
            sources: vec![Source {
                kind: Kind::Plain(log.clone()),
                name: "app.log".into(),
                size: None,
                modified: None,
            }],
            path: out.clone(),
        };
        let (c, p) = (AtomicBool::new(false), AtomicU64::new(0));
        let done = run(req, &c, &p, &AtomicU64::new(0)).unwrap().unwrap();
        assert_eq!((done.types, done.entries, done.missing), (1, 3, 0));
        let md = fs::read_to_string(&out).unwrap();
        // Two traces: the SQLException one (twice) and the Other one.
        assert!(md.contains("### Stack trace 1 · 2 of 3 kept occurrences"));
        assert!(md.contains("### Stack trace 2 · 1 of 3 kept occurrences"));
        assert!(md.contains("    at com.x.Svc.run(Svc.java:20)"));
        assert!(md.contains("    at com.x.Other.call(Other.java:7)"));
        assert!(md.contains("app.log:11  trace #2  2026-11-22 10:00:04 ERROR Pool timeout"));
        assert!(!md.contains("INFO ok"));
        let _ = fs::remove_dir_all(&dir);
    }
}
