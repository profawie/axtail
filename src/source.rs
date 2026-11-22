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
        gz: bool,
    },
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
        self.plain_path().is_some() || self.is_docker()
    }

    pub fn type_label(&self) -> &'static str {
        match &self.kind {
            Kind::Plain(_) => "text",
            Kind::Gzip(_) => "gzip",
            Kind::ZipEntry { .. } => "zip",
            Kind::DockerLogs { .. } => "docker",
            Kind::DockerFile { gz: true, .. } => "dk-gzip",
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
                gz,
            } => {
                let r = spawn_docker(&["exec", container, "cat", "--", path])?.0;
                Ok(if *gz {
                    Box::new(MultiGzDecoder::new(r))
                } else {
                    r
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
                gz: false,
            } => spawn_docker(&["exec", container, "tail", "-n", &n, "-F", "--", path]),
            _ => Err(io::Error::other("this source cannot be followed live")),
        }
    }
}

pub struct Scan {
    pub sources: Vec<Source>,
    pub warnings: Vec<String>,
    pub title: String,
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?'])
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

/// Docker mode: the container's log stream, plus any files inside the
/// container matching the filters (listed with `docker exec <c> ls`).
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
    for arg in filters {
        let (dir, pattern) = if has_glob(arg) {
            match arg.rsplit_once('/') {
                Some((d, p)) => (if d.is_empty() { "/" } else { d }, Some(p)),
                None => (".", Some(arg.as_str())),
            }
        } else {
            (arg.as_str(), None)
        };
        let out = Command::new(docker_bin())
            .args(["exec", container, "ls", "-1Ap", "--", dir])
            .stdin(Stdio::null())
            .output();
        let out = match out {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr);
                warnings.push(format!(
                    "{arg}: {}",
                    msg.trim().lines().last().unwrap_or("docker exec ls failed")
                ));
                continue;
            }
            Err(e) => {
                warnings.push(format!("cannot run docker: {e}"));
                break;
            }
        };
        let listing = String::from_utf8_lossy(&out.stdout);
        for name in listing.lines() {
            if name.is_empty() || name.ends_with('/') || sources.len() >= MAX_ENTRIES {
                continue;
            }
            // `ls` on a single file prints the path itself.
            let path = if name == dir {
                name.to_string()
            } else {
                if pattern.is_some_and(|p| !wild_match(p, name)) {
                    continue;
                }
                format!("{}/{name}", dir.trim_end_matches('/'))
            };
            let ext = lower_ext(&path);
            if ext == "zip" {
                continue;
            }
            sources.push(Source {
                name: format!("{container}:{path}"),
                kind: Kind::DockerFile {
                    container: container.to_string(),
                    path,
                    gz: ext == "gz" || ext == "gzip",
                },
                size: None,
                modified: None,
            });
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
    fn wildcards() {
        assert!(wild_match("*.log", "api.log"));
        assert!(wild_match("api-*.log", "api-2026-11.log"));
        assert!(!wild_match("api-*.log", "web-1.log"));
        assert!(wild_match("app.log.?", "app.log.1"));
        assert!(!wild_match("*.log", "api.log.gz"));
        assert!(wild_match("*", ""));
    }
}
