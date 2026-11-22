//! Log sources (plain file, gzip, zip entry, docker) and discovery.
//!
//! Everything here is strictly read-only: files are opened with
//! `File::open`, and docker is only ever asked to `logs`, `ls`, `cat`, `tail`.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use flate2::read::{DeflateDecoder, MultiGzDecoder};

/// Upper bound on listed entries, to keep the list itself bounded.
pub const MAX_ENTRIES: usize = 20_000;

#[derive(Clone)]
pub enum Kind {
    Plain(PathBuf),
    Gzip(PathBuf),
    ZipEntry {
        archive: PathBuf,
        data_start: u64,
        comp_size: u64,
        deflated: bool,
    },
    DockerLogs {
        container: String,
    },
    DockerFile {
        container: String,
        path: String,
        comp: Comp,
    },
}

/// Compression of a file inside a container.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Comp {
    None,
    Gzip,
    Zip,
}

impl Comp {
    fn of(path: &str) -> Comp {
        match lower_ext(path).as_str() {
            "gz" | "gzip" => Comp::Gzip,
            "zip" => Comp::Zip,
            _ => Comp::None,
        }
    }
}

#[derive(Clone)]
pub struct Source {
    pub kind: Kind,
    pub name: String,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

/// Kills a spawned docker process (used to stop a blocked follower).
#[derive(Clone)]
pub struct Killer(Arc<Mutex<Child>>);

impl Killer {
    pub fn kill(&self) {
        if let Ok(mut c) = self.0.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct ChildReader {
    killer: Killer,
    reader: io::PipeReader,
}

impl Read for ChildReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Drop for ChildReader {
    fn drop(&mut self) {
        self.killer.kill();
    }
}

fn docker_bin() -> String {
    std::env::var("AXTAIL_DOCKER").unwrap_or_else(|_| "docker".into())
}

/// Spawns docker with stdout and stderr merged into one pipe.
fn spawn_docker(args: &[&str]) -> io::Result<(Box<dyn Read + Send>, Killer)> {
    let (reader, writer) = io::pipe()?;
    let mut cmd = Command::new(docker_bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("cannot run docker: {e}")))?;
    drop(cmd); // close our copies of the write end so EOF is delivered
    let killer = Killer(Arc::new(Mutex::new(child)));
    Ok((
        Box::new(ChildReader {
            killer: killer.clone(),
            reader,
        }),
        killer,
    ))
}

impl Source {
    /// Seekable sources can be windowed without reading from the start.
    pub fn plain_path(&self) -> Option<&Path> {
        match &self.kind {
            Kind::Plain(p) => Some(p),
            _ => None,
        }
    }

    pub fn is_docker(&self) -> bool {
        matches!(self.kind, Kind::DockerLogs { .. } | Kind::DockerFile { .. })
    }

    pub fn can_live(&self) -> bool {
        match &self.kind {
            Kind::Plain(_) | Kind::DockerLogs { .. } => true,
            Kind::DockerFile { comp, .. } => *comp == Comp::None,
            _ => false,
        }
    }

    /// The `docker logs` stream itself, as opposed to a file in a container.
    pub fn is_docker_logs(&self) -> bool {
        matches!(self.kind, Kind::DockerLogs { .. })
    }

    pub fn type_label(&self) -> &'static str {
        match &self.kind {
            Kind::Plain(_) => "text",
            Kind::Gzip(_) => "gzip",
            Kind::ZipEntry { .. } => "zip",
            Kind::DockerLogs { .. } => "docker",
            Kind::DockerFile {
                comp: Comp::Gzip, ..
            } => "dk-gzip",
            Kind::DockerFile {
                comp: Comp::Zip, ..
            } => "dk-zip",
            Kind::DockerFile { .. } => "dk-file",
        }
    }

    /// Opens a decompressed byte stream from the very beginning.
    pub fn open_stream(&self) -> io::Result<Box<dyn Read + Send>> {
        match &self.kind {
            Kind::Plain(p) => Ok(Box::new(File::open(p)?)),
            Kind::Gzip(p) => Ok(Box::new(MultiGzDecoder::new(File::open(p)?))),
            Kind::ZipEntry {
                archive,
                data_start,
                comp_size,
                deflated,
            } => {
                let mut f = File::open(archive)?;
                f.seek(SeekFrom::Start(*data_start))?;
                let raw = f.take(*comp_size);
                Ok(if *deflated {
                    Box::new(DeflateDecoder::new(raw))
                } else {
                    Box::new(raw)
                })
            }
            Kind::DockerLogs { container } => Ok(spawn_docker(&["logs", container])?.0),
            Kind::DockerFile {
                container,
                path,
                comp,
            } => {
                let r = spawn_docker(&["exec", container, "cat", "--", path])?.0;
                Ok(match comp {
                    Comp::None => r,
                    Comp::Gzip => Box::new(MultiGzDecoder::new(r)),
                    Comp::Zip => unzip_stream(r)?,
                })
            }
        }
    }

    /// Opens a live stream that starts with the last `tail` lines and then
    /// follows new output (docker sources only).
    pub fn open_follow(&self, tail: usize) -> io::Result<(Box<dyn Read + Send>, Killer)> {
        let n = tail.to_string();
        match &self.kind {
            Kind::DockerLogs { container } => {
                spawn_docker(&["logs", "--tail", &n, "-f", container])
            }
            Kind::DockerFile {
                container,
                path,
                comp: Comp::None,
            } => spawn_docker(&["exec", container, "tail", "-n", &n, "-F", "--", path]),
            _ => Err(io::Error::other("this source cannot be followed live")),
        }
    }
}

/// Decompresses every entry of a zip read as a plain stream (no seeking, so
/// it works through `docker exec cat`). Entries are concatenated, each
/// preceded by a `==> name <==` line like `tail` prints for several files.
/// Runs on its own thread; dropping the returned reader stops it.
fn unzip_stream(mut r: Box<dyn Read + Send>) -> io::Result<Box<dyn Read + Send>> {
    let (reader, mut writer) = io::pipe()?;
    std::thread::spawn(move || {
        use std::io::Write;
        loop {
            match zip::read::read_zipfile_from_stream(&mut r) {
                Ok(Some(mut entry)) => {
                    if entry.is_dir() {
                        continue;
                    }
                    let name = entry.name().map(|n| n.into_owned()).unwrap_or_default();
                    if writeln!(writer, "==> {name} <==").is_err()
                        || io::copy(&mut entry, &mut writer).is_err()
                    {
                        return;
                    }
                    // Make sure the next header starts on its own line.
                    let _ = writeln!(writer);
                }
                Ok(None) => return,
                Err(e) => {
                    let _ = writeln!(writer, "axtail: cannot read zip stream: {e}");
                    return;
                }
            }
        }
    });
    Ok(Box::new(reader))
}

pub struct Scan {
    pub sources: Vec<Source>,
    pub warnings: Vec<String>,
    pub title: String,
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// Compares names so that embedded numbers sort by value:
/// `prod.2026-11-21.9.log` comes before `prod.2026-11-21.10.log`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let (x, y) = (&a[si..i], &b[sj..j]);
            let strip = |d: &[u8]| -> usize { d.iter().take_while(|&&c| c == b'0').count() };
            let (x, y) = (
                &x[strip(x).min(x.len() - 1)..],
                &y[strip(y).min(y.len() - 1)..],
            );
            let ord = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            let ord = a[i].to_ascii_lowercase().cmp(&b[j].to_ascii_lowercase());
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

/// Minimal wildcard match supporting `*` and `?`.
pub fn wild_match(pat: &str, name: &str) -> bool {
    let fold = cfg!(windows);
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let eq = |a: char, b: char| a == b || (fold && a.to_lowercase().eq(b.to_lowercase()));
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || eq(p[pi], n[ni])) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

fn lower_ext(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

struct Collector {
    sources: Vec<Source>,
    warnings: Vec<String>,
    seen: HashSet<PathBuf>,
    truncated: bool,
}

impl Collector {
    fn full(&mut self) -> bool {
        if self.sources.len() >= MAX_ENTRIES {
            self.truncated = true;
        }
        self.truncated
    }

    fn add_file(&mut self, path: &Path) {
        if self.full() || !self.seen.insert(path.to_path_buf()) {
            return;
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) if m.is_file() => m,
            _ => return,
        };
        let name = path.to_string_lossy().into_owned();
        let name = name
            .strip_prefix("./")
            .or_else(|| name.strip_prefix(".\\"))
            .unwrap_or(&name)
            .to_string();
        let modified = meta.modified().ok();
        match lower_ext(&name).as_str() {
            "zip" => self.add_zip(path, &name, modified),
            "gz" | "gzip" => self.sources.push(Source {
                kind: Kind::Gzip(path.to_path_buf()),
                name,
                size: Some(meta.len()),
                modified,
            }),
            _ => self.sources.push(Source {
                kind: Kind::Plain(path.to_path_buf()),
                name,
                size: Some(meta.len()),
                modified,
            }),
        }
    }

    fn add_zip(&mut self, path: &Path, name: &str, modified: Option<SystemTime>) {
        let mut archive = match File::open(path)
            .map_err(|e| e.to_string())
            .and_then(|f| zip::ZipArchive::new(io::BufReader::new(f)).map_err(|e| e.to_string()))
        {
            Ok(a) => a,
            Err(e) => return self.warnings.push(format!("{name}: {e}")),
        };
        for i in 0..archive.len() {
            if self.full() {
                return;
            }
            let Ok(entry) = archive.by_index_raw(i) else {
                continue;
            };
            if entry.is_dir() {
                continue;
            }
            let inner = entry
                .name()
                .map(|n| n.into_owned())
                .unwrap_or_else(|_| format!("entry-{i}"));
            let deflated = match entry.compression() {
                zip::CompressionMethod::Stored => false,
                zip::CompressionMethod::Deflated => true,
                other => {
                    self.warnings
                        .push(format!("{name}:{inner}: unsupported compression {other}"));
                    continue;
                }
            };
            if entry.encrypted() {
                self.warnings
                    .push(format!("{name}:{inner}: encrypted entry skipped"));
                continue;
            }
            let Some(data_start) = entry.data_start() else {
                continue;
            };
            self.sources.push(Source {
                kind: Kind::ZipEntry {
                    archive: path.to_path_buf(),
                    data_start,
                    comp_size: entry.compressed_size(),
                    deflated,
                },
                name: format!("{name}:{inner}"),
                size: Some(entry.size()),
                modified,
            });
        }
    }

    fn add_dir(&mut self, dir: &Path, pattern: Option<&str>) {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => return self.warnings.push(format!("{}: {e}", dir.display())),
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in rd.flatten() {
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            let ok = match pattern {
                Some(p) => wild_match(p, &fname),
                None => !fname.starts_with('.'),
            };
            if ok {
                paths.push(entry.path());
            }
            if paths.len() > MAX_ENTRIES {
                break;
            }
        }
        paths.sort();
        for p in paths {
            self.add_file(&p);
        }
    }
}

/// Resolves command-line filters (files, directories, wildcards) to sources.
pub fn scan_local(filters: &[String]) -> Scan {
    let mut c = Collector {
        sources: Vec::new(),
        warnings: Vec::new(),
        seen: HashSet::new(),
        truncated: false,
    };
    for arg in filters {
        let path = Path::new(arg);
        if path.is_dir() {
            c.add_dir(path, None);
        } else if path.is_file() {
            c.add_file(path);
        } else if has_glob(arg) {
            let pattern = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let parent = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            if has_glob(&parent.to_string_lossy()) {
                c.warnings.push(format!(
                    "{arg}: wildcards are only supported in the file name"
                ));
            } else {
                c.add_dir(parent, Some(&pattern));
            }
        } else {
            c.warnings.push(format!("{arg}: not found"));
        }
    }
    if c.truncated {
        c.warnings
            .push(format!("list truncated to {MAX_ENTRIES} entries"));
    }
    Scan {
        sources: c.sources,
        warnings: c.warnings,
        title: title_of(filters),
    }
}

fn title_of(filters: &[String]) -> String {
    match filters {
        [] => String::new(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} (+{} more)", rest.len()),
    }
}

/// Where log files are usually kept inside a container. The container's
/// working directory is searched too.
const DOCKER_ROOTS: &[&str] = &[
    "/var/log",
    "/opt",
    "/app",
    "/srv",
    "/data",
    "/logs",
    "/log",
    "/var/www",
    "/usr/src/app",
    "/home",
];

/// How deep `find` descends below each searched directory.
const DOCKER_MAX_DEPTH: u32 = 8;

/// Default test for "is this a log file": *.log, rotated *.log.N(.gz),
/// *.out, every file under /var/log except binary accounting files, and
/// text or archives in any path that mentions "log".
const DOCKER_LOG_TEST: &str = "\\( -name '*.log' -o -name '*.log.*' -o -name '*.out' \
-o \\( -path '/var/log/*' ! -name 'wtmp*' ! -name 'btmp*' ! -name 'lastlog' ! -name '*.journal*' \\) \
-o \\( -path '*log*' \\( -name '*.txt' -o -name '*.gz' -o -name '*.zip' \\) \\) \\) \
! -name '*.tar.gz' ! -name '*.tgz' ! -name '*.pid' ! -name '*.lock' \
! -name faillog ! -name '*.xz' ! -name '*.bz2' ! -name '*.zst'";

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Builds a POSIX sh script, run inside the container, that lists matching
/// files as `size|mtime|path`. It only reads: `find` plus `stat`, with
/// `head` bounding the output. Works with GNU and BusyBox tools; when
/// `stat` or `find -exec {} +` is unavailable it falls back to bare paths.
fn find_script(roots: &[String], with_workdir: bool, test: &str) -> String {
    let mut dirs: Vec<String> = roots.iter().map(|r| sh_quote(r)).collect();
    if with_workdir {
        dirs.push("\"$PWD\"".into());
    }
    let prune = "\\( -name node_modules -o -name .git -o -name site-packages -o -name __pycache__ \
-o -name .cache -o -name proc \\) -prune";
    format!(
        "set --; for d in {dirs}; do case \"$d\" in /) continue;; esac; [ -e \"$d\" ] && set -- \"$@\" \"$d\"; done; \
[ $# -gt 0 ] || exit 0; \
out=$(find \"$@\" -maxdepth {depth} -xdev {prune} -o -type f {test} -exec stat -c '%s|%Y|%n' {{}} + 2>/dev/null | head -n {limit}); \
if [ -n \"$out\" ]; then printf '%s\\n' \"$out\"; \
else find \"$@\" -maxdepth {depth} -xdev {prune} -o -type f {test} -print 2>/dev/null | head -n {limit}; fi",
        dirs = dirs.join(" "),
        depth = DOCKER_MAX_DEPTH,
        limit = MAX_ENTRIES,
    )
}

/// Parses one `size|mtime|path` line (or a bare path).
fn parse_found(line: &str) -> Option<(Option<u64>, Option<SystemTime>, String)> {
    let mut parts = line.splitn(3, '|');
    if let (Some(a), Some(b), Some(path)) = (parts.next(), parts.next(), parts.next())
        && let (Ok(size), Ok(secs)) = (a.parse::<u64>(), b.parse::<u64>())
    {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        return Some((Some(size), Some(t), path.to_string()));
    }
    line.starts_with('/')
        .then(|| (None, None, line.to_string()))
}

/// Docker mode: the container's log stream first, then log files found
/// inside the container. Without filters the usual log directories are
/// searched recursively; a filter narrows that down:
///   `*.gz`, `prod.2026-11-21*`   name pattern, searched in the usual places
///   `/opt/app/logs/*.gz`         name pattern under one directory
///   `/opt/app/logs`              every file under a directory, or one file
pub fn scan_docker(container: &str, filters: &[String]) -> Scan {
    let mut sources = vec![Source {
        kind: Kind::DockerLogs {
            container: container.to_string(),
        },
        name: format!("{container} (docker logs)"),
        size: None,
        modified: None,
    }];
    let mut warnings = Vec::new();
    let defaults: Vec<String> = DOCKER_ROOTS.iter().map(|s| s.to_string()).collect();
    let mut searches: Vec<(&str, Vec<String>, bool, String)> = Vec::new();
    if filters.is_empty() {
        searches.push(("", defaults.clone(), true, DOCKER_LOG_TEST.to_string()));
    }
    for arg in filters {
        if has_glob(arg) {
            match arg.rsplit_once('/') {
                Some((d, p)) => {
                    let dir = if d.is_empty() { "/" } else { d };
                    searches.push((
                        arg,
                        vec![dir.to_string()],
                        false,
                        format!("-name {}", sh_quote(p)),
                    ));
                }
                None => searches.push((
                    arg,
                    defaults.clone(),
                    true,
                    format!("-name {}", sh_quote(arg)),
                )),
            }
        } else {
            searches.push((arg, vec![arg.clone()], false, String::new()));
        }
    }
    let mut seen = HashSet::new();
    'search: for (label, roots, workdir, test) in &searches {
        let script = find_script(roots, *workdir, test);
        let out = Command::new(docker_bin())
            .args(["exec", container, "sh", "-c", &script])
            .stdin(Stdio::null())
            .output();
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                warnings.push(format!("cannot run docker: {e}"));
                break;
            }
        };
        if !out.status.success() && out.stdout.is_empty() {
            let msg = String::from_utf8_lossy(&out.stderr);
            warnings.push(
                msg.trim()
                    .lines()
                    .last()
                    .unwrap_or("docker exec failed")
                    .to_string(),
            );
            continue;
        }
        let before = sources.len();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if sources.len() > MAX_ENTRIES {
                warnings.push(format!("list truncated to {MAX_ENTRIES} entries"));
                break 'search;
            }
            let Some((size, modified, path)) = parse_found(line) else {
                continue;
            };
            if !seen.insert(path.clone()) {
                continue;
            }
            sources.push(Source {
                name: path.clone(),
                kind: Kind::DockerFile {
                    container: container.to_string(),
                    comp: Comp::of(&path),
                    path,
                },
                size,
                modified,
            });
        }
        if sources.len() == before && !label.is_empty() {
            warnings.push(format!("{label}: no files found in the container"));
        }
    }
    let title = if filters.is_empty() {
        format!("docker:{container}")
    } else {
        format!("docker:{container} {}", title_of(filters))
    };
    Scan {
        sources,
        warnings,
        title,
    }
}

#[cfg(test)]
mod tests {
    use super::wild_match;

    #[test]
    fn natural_order() {
        use super::natural_cmp;
        let mut v = vec![
            "prod.2026-11-21.10.log",
            "prod.2026-11-21.9.log",
            "prod.2026-11-20.100.log",
            "a.log",
        ];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            [
                "a.log",
                "prod.2026-11-20.100.log",
                "prod.2026-11-21.9.log",
                "prod.2026-11-21.10.log"
            ]
        );
    }

    #[test]
    fn parses_find_output() {
        use super::parse_found;
        let (size, t, p) = parse_found("1234|1763800000|/opt/app/logs/a|b.log").unwrap();
        assert_eq!((size, p.as_str()), (Some(1234), "/opt/app/logs/a|b.log"));
        assert!(t.is_some());
        assert_eq!(parse_found("/var/log/syslog").unwrap().2, "/var/log/syslog");
        assert!(parse_found("find: warning").is_none());
    }

    #[test]
    fn wildcards() {
        assert!(wild_match("*.log", "api.log"));
        assert!(wild_match("api-*.log", "api-2026-11.log"));
        assert!(!wild_match("api-*.log", "web-1.log"));
        assert!(wild_match("app.log.?", "app.log.1"));
        assert!(!wild_match("*.log", "api.log.gz"));
        assert!(wild_match("*", ""));
    }
}
