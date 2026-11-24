//! Application state and event loop.
//!
//! The UI thread never does unbounded work: anything that may need to read
//! a lot of data (filtered windows, stream windows, match counts) runs on a
//! worker thread that can be cancelled and reports progress.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::clock;
use crate::export;
use crate::filter::{self, Filter};
use crate::hidden;
use crate::source::{Killer, Scan, Source, natural_cmp};
use crate::summary::{self, Report};
use crate::ui;
use crate::window::{self, Appended, Line, LineReader, Target, Window};

/// How long typing must pause before the viewer filter is applied.
const TYPE_DELAY: Duration = Duration::from_millis(250);

pub struct Job {
    pub id: u64,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<AtomicU64>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum Msg {
    Window(u64, io::Result<Window>),
    Count(usize, io::Result<u64>),
    /// A finished analysis; None when it was cancelled.
    Summary(u64, Option<Box<Report>>),
    /// A finished export; None when it was cancelled.
    Export(u64, Option<Result<export::Done, String>>),
}

/// Background reader for a live docker stream. Lines are filtered on that
/// thread; a bounded channel gives backpressure.
struct Follower {
    rx: Receiver<Vec<Line>>,
    killer: Killer,
}

impl Follower {
    fn spawn(
        reader: Box<dyn io::Read + Send>,
        killer: Killer,
        filter: Option<Arc<Filter>>,
    ) -> Follower {
        let (tx, rx): (SyncSender<Vec<Line>>, _) = mpsc::sync_channel(32);
        thread::spawn(move || {
            let mut lr = LineReader::new(reader, 0);
            let mut buf = Vec::new();
            let mut batch = Vec::new();
            loop {
                match lr.next_line(&mut buf) {
                    Ok(Some((off, _))) => {
                        if filter.as_ref().is_none_or(|f| f.is_match(&buf)) {
                            batch.push(Line {
                                off,
                                data: buf.as_slice().into(),
                            });
                        }
                        if (lr.drained() || batch.len() >= 64)
                            && !batch.is_empty()
                            && tx.send(std::mem::take(&mut batch)).is_err()
                        {
                            return;
                        }
                    }
                    _ => {
                        let _ = tx.send(batch);
                        return;
                    }
                }
            }
        });
        Follower { rx, killer }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.killer.kill();
    }
}

/// What to do with the view once a requested window arrives.
enum After {
    Bottom,
    Top,
    Anchor { off: u64, delta: isize },
    AnchorContent { data: Box<[u8]>, delta: isize },
}

pub struct Viewer {
    pub src: Source,
    pub win: Window,
    pub top: usize,
    pub hscroll: usize,
    pub live: bool,
    /// The view is held at the end (newest line at the bottom).
    pub pinned: bool,
    follower: Option<Follower>,
    pub loading: Option<Job>,
    after: After,
    /// The filter in effect and the text being typed (applied after a pause).
    pub filter: Option<Arc<Filter>>,
    pub query: String,
    query_changed: Option<Instant>,
    pub query_error: Option<String>,
    pub notice: String,
    /// Offset of the line the viewer was opened at from the summary.
    pub mark: Option<u64>,
    last_poll: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Viewer,
    Summary,
}

/// Summary tabs: one per level, then the per-file breakdown.
pub const TABS: usize = 5;
pub const FILES_TAB: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TypeSort {
    Count,
    Latest,
    Oldest,
}

impl TypeSort {
    pub fn label(self) -> &'static str {
        match self {
            TypeSort::Count => "count",
            TypeSort::Latest => "last seen",
            TypeSort::Oldest => "first seen",
        }
    }
}

/// The message type opened with Enter, and the selected occurrence
/// (0 = newest).
pub struct Detail {
    pub level: usize,
    pub group: usize,
    pub sel: usize,
}

pub struct Summary {
    /// The running analysis.
    pub job: Option<Job>,
    pub done: Arc<AtomicU64>,
    pub total: usize,
    pub report: Option<Box<Report>>,
    pub tab: usize,
    pub sel: [usize; TABS],
    pub sort: TypeSort,
    pub query: String,
    /// Visible rows of the current tab: indices into its types or files.
    pub rows: Vec<usize>,
    pub detail: Option<Detail>,
    /// A one-off note shown in the status bar until the next key.
    pub notice: String,
    /// Message types selected for export: (level, group).
    pub marked: HashSet<(usize, usize)>,
    /// The running export, files read so far and files to read.
    pub export: Option<Job>,
    pub export_done: Arc<AtomicU64>,
    pub export_total: usize,
    /// The files and time range the analysis covers.
    scope: (Vec<usize>, Option<usize>, String, String),
}

/// Which field of the file list's top bar receives typing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Search,
    From,
    To,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Newest,
    Name,
    Largest,
    Matches,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Newest => "newest",
            Sort::Largest => "largest",
            Sort::Matches => "matches",
        }
    }
}

/// Time range presets cycled with Left/Right: (label, seconds back; 0 = all).
pub const PRESETS: &[(&str, i64)] = &[
    ("All time", 0),
    ("Last 15 min", 15 * 60),
    ("Last 1 hour", 3600),
    ("Last 6 hours", 6 * 3600),
    ("Last 24 hours", 24 * 3600),
    ("Last 3 days", 3 * 86400),
    ("Last 7 days", 7 * 86400),
    ("Last 30 days", 30 * 86400),
];

#[derive(Clone, Copy)]
pub enum Count {
    Pending,
    Done(u64),
    Failed,
}

pub struct App {
    pub title: String,
    pub sources: Vec<Source>,
    pub order: Vec<usize>,
    pub sel: usize,
    pub sort: Sort,
    pub counts: Vec<Count>,
    pub grep: Option<Arc<Filter>>,
    pub screen: Screen,
    pub viewer: Option<Viewer>,
    pub summary: Option<Summary>,
    /// Message types hidden from the summary, saved in hidden::path().
    pub hidden: Vec<String>,
    /// Types hidden in this session, latest last (Ctrl+Z shows them again).
    undo_hide: Vec<String>,
    /// The viewer was opened from the summary: Esc goes back there.
    from_summary: bool,
    pub help: bool,
    /// Wrap long lines in the viewer (Ctrl+R toggles).
    pub wrap: bool,
    /// Text columns of the viewer body.
    pub cols: usize,
    // File list filters.
    pub search: String,
    pub focus: Focus,
    /// Index into PRESETS, or None for a custom range typed by hand.
    pub preset: Option<usize>,
    pub from_text: String,
    pub to_text: String,
    pub range_error: bool,
    pub message: String,
    pub rows: usize,
    tail: usize,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    next_id: u64,
    count_cancel: Arc<AtomicBool>,
    quit: bool,
    backlog: bool,
}

impl Drop for App {
    fn drop(&mut self) {
        self.count_cancel.store(true, Ordering::Relaxed);
    }
}

fn ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

fn typed(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

/// Removes the last word (and the spaces after it), like Ctrl+W in a shell.
/// The first `max` characters of `s`, with an ellipsis when cut.
fn clip_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\u{2026}", &s[..i]),
        None => s.to_string(),
    }
}

fn delete_word(s: &mut String) {
    let trimmed = s.trim_end().len();
    s.truncate(trimmed);
    let cut = s.rfind(' ').map_or(0, |i| i + 1);
    s.truncate(cut);
}

fn err_text(e: &io::Error) -> String {
    e.to_string()
}

impl App {
    pub fn new(scan: Scan, grep: Option<Filter>, tail: usize) -> App {
        let (tx, rx) = mpsc::channel();
        let grep = grep.map(Arc::new);
        let docker = scan.sources.iter().any(|s| s.is_docker());
        let has_times = scan.sources.iter().any(|s| s.modified.is_some());
        let mut app = App {
            title: scan.title,
            counts: vec![Count::Pending; scan.sources.len()],
            order: Vec::new(),
            sources: scan.sources,
            sel: 0,
            // Rotated logs are easiest to browse newest first.
            sort: if docker || has_times {
                Sort::Newest
            } else {
                Sort::Name
            },
            grep,
            screen: Screen::List,
            viewer: None,
            summary: None,
            hidden: hidden::load(),
            undo_hide: Vec::new(),
            from_summary: false,
            help: false,
            wrap: true,
            cols: 78,
            search: String::new(),
            focus: Focus::Search,
            preset: Some(0),
            from_text: String::new(),
            to_text: String::new(),
            range_error: false,
            message: match scan.warnings.len() {
                0 => String::new(),
                1 => scan.warnings[0].clone(),
                n => format!("{} (+{} more warnings)", scan.warnings[0], n - 1),
            },
            rows: 20,
            tail,
            tx,
            rx,
            next_id: 1,
            count_cancel: Arc::new(AtomicBool::new(false)),
            quit: false,
            backlog: false,
        };
        app.rebuild_order();
        app.start_counts();
        app
    }

    /// With `--grep`, count matching lines per file on one background thread.
    fn start_counts(&mut self) {
        let Some(m) = self.grep.clone() else { return };
        let jobs: Vec<(usize, Source)> = self
            .sources
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, s)| !s.is_docker())
            .collect();
        let (tx, cancel) = (self.tx.clone(), self.count_cancel.clone());
        thread::spawn(move || {
            for (i, src) in jobs {
                if cancel.load(Ordering::Relaxed)
                    || tx
                        .send(Msg::Count(i, filter::count(&src, &m, &cancel)))
                        .is_err()
                {
                    return;
                }
            }
        });
    }

    // ----- file list -------------------------------------------------------

    /// The active time range as (from, to) in seconds; None = open end.
    pub fn range(&self) -> (Option<i64>, Option<i64>) {
        match self.preset {
            Some(0) => (None, None),
            Some(i) => (Some(clock::now_secs() - PRESETS[i].1), None),
            None => (
                clock::parse(&self.from_text, false),
                clock::parse(&self.to_text, true),
            ),
        }
    }

    fn rebuild_order(&mut self) {
        let terms: Vec<String> = self
            .search
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let (from, to) = self.range();
        self.range_error = self.preset.is_none()
            && ((!self.from_text.trim().is_empty() && from.is_none())
                || (!self.to_text.trim().is_empty() && to.is_none()));
        let ranged = from.is_some() || to.is_some();
        let s = &self.sources;
        let name_ok = |i: usize| {
            let name = s[i].name.to_lowercase();
            terms.iter().all(|t| name.contains(t.as_str()))
        };
        let time = |i: usize| s[i].modified.map(clock::secs);
        let in_range = |t: i64| from.is_none_or(|f| t >= f) && to.is_none_or(|e| t <= e);
        let mut order: Vec<usize> = (0..s.len())
            .filter(|&i| name_ok(i))
            .filter(|&i| s[i].is_docker_logs() || !ranged || time(i).is_some_and(in_range))
            .collect();
        // A log written after the range's end may still hold its last lines:
        // per folder, also keep the first file modified after "To".
        if let Some(end) = to {
            let mut next: std::collections::HashMap<&str, (i64, usize)> = Default::default();
            for i in (0..s.len()).filter(|&i| name_ok(i)) {
                let Some(t) = time(i).filter(|&t| t > end && from.is_none_or(|f| t >= f)) else {
                    continue;
                };
                let dir = s[i].name.rfind(['/', '\\']).map_or("", |p| &s[i].name[..p]);
                let e = next.entry(dir).or_insert((t, i));
                if t < e.0 {
                    *e = (t, i);
                }
            }
            order.extend(next.values().map(|&(_, i)| i));
        }
        match self.sort {
            Sort::Name => order.sort_by(|&a, &b| natural_cmp(&s[a].name, &s[b].name)),
            Sort::Newest => order.sort_by(|&a, &b| s[b].modified.cmp(&s[a].modified)),
            Sort::Largest => order.sort_by(|&a, &b| s[b].size.cmp(&s[a].size)),
            Sort::Matches => {
                let n = |i: usize| match self.counts[i] {
                    Count::Done(n) => n,
                    _ => 0,
                };
                order.sort_by_key(|&i| std::cmp::Reverse(n(i)));
            }
        }
        // The live `docker logs` stream always stays at the top.
        order.sort_by_key(|&i| !s[i].is_docker_logs());
        self.order = order;
        self.sel = self.sel.min(self.order.len().saturating_sub(1));
    }

    fn set_preset(&mut self, i: usize) {
        self.preset = Some(i);
        self.focus = Focus::Search;
        if i == 0 {
            self.from_text.clear();
        } else {
            self.from_text = clock::format(clock::now_secs() - PRESETS[i].1);
        }
        self.to_text.clear();
        self.rebuild_order();
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        self.message.clear();
        let last = self.order.len().saturating_sub(1);
        let page = self.rows.saturating_sub(2).max(1);
        if let Some(c) = typed(&key) {
            match self.focus {
                Focus::Search => self.search.push(c),
                Focus::From | Focus::To => {
                    if c.is_ascii_digit() || matches!(c, '-' | ':' | ' ' | 'T') {
                        self.preset = None;
                        let field = if self.focus == Focus::From {
                            &mut self.from_text
                        } else {
                            &mut self.to_text
                        };
                        if field.len() < 19 {
                            field.push(c);
                        }
                    }
                }
            }
            self.sel = 0;
            return self.rebuild_order();
        }
        let field = match self.focus {
            Focus::Search => &mut self.search,
            Focus::From => &mut self.from_text,
            Focus::To => &mut self.to_text,
        };
        let date_field = self.focus != Focus::Search;
        match key.code {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(page),
            KeyCode::PageDown => self.sel = (self.sel + page).min(last),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = last,
            KeyCode::Enter => {
                if let Some(&i) = self.order.get(self.sel) {
                    self.open(i);
                }
            }
            KeyCode::Left | KeyCode::Right => {
                let n = PRESETS.len();
                let cur = self.preset.unwrap_or(0);
                let next = if key.code == KeyCode::Right {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                self.set_preset(next);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let order = [Focus::Search, Focus::From, Focus::To];
                let i = order.iter().position(|&f| f == self.focus).unwrap_or(0);
                let step = if key.code == KeyCode::Tab { 1 } else { 2 };
                self.focus = order[(i + step) % 3];
                // Start a custom range from what the preset showed.
                if self.focus != Focus::Search && self.preset.is_some_and(|p| p > 0) {
                    self.preset = None;
                }
            }
            KeyCode::Backspace => {
                field.pop();
                if date_field {
                    self.preset = None;
                }
                self.rebuild_order();
            }
            KeyCode::Char('u') if ctrl(&key, 'u') => {
                field.clear();
                if date_field {
                    self.preset = None;
                }
                self.rebuild_order();
            }
            KeyCode::Char('w') if ctrl(&key, 'w') => {
                delete_word(field);
                self.rebuild_order();
            }
            KeyCode::Char('s') if ctrl(&key, 's') => {
                let all: &[Sort] = if self.grep.is_some() {
                    &[Sort::Newest, Sort::Name, Sort::Largest, Sort::Matches]
                } else {
                    &[Sort::Newest, Sort::Name, Sort::Largest]
                };
                let i = all.iter().position(|&s| s == self.sort).unwrap_or(0);
                self.sort = all[(i + 1) % all.len()];
                self.rebuild_order();
            }
            KeyCode::Esc => {
                if date_field {
                    self.focus = Focus::Search;
                } else if !self.search.is_empty() {
                    self.search.clear();
                    self.rebuild_order();
                } else if self.preset != Some(0) {
                    self.set_preset(0);
                }
            }
            _ => {}
        }
    }

    fn new_job(&mut self) -> Job {
        self.next_id += 1;
        Job {
            id: self.next_id,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        if self.sources.len() == 1 {
            let size = terminal.size()?;
            self.rows = ui::body_rows(size.height);
            self.cols = ui::body_cols(size.width);
            self.open(0);
        }
        let mut dirty = true;
        while !self.quit {
            let size = terminal.size()?;
            self.rows = ui::body_rows(size.height);
            self.cols = ui::body_cols(size.width);
            if dirty {
                terminal.draw(|f| ui::draw(f, self))?;
                dirty = false;
            }
            let typing = self
                .viewer
                .as_ref()
                .is_some_and(|v| v.query_changed.is_some());
            let busy = self.viewer.as_ref().is_some_and(|v| v.loading.is_some())
                || self
                    .summary
                    .as_ref()
                    .is_some_and(|s| s.job.is_some() || s.export.is_some());
            let timeout = Duration::from_millis(if self.backlog {
                0
            } else if busy || typing {
                50
            } else {
                200
            });
            if event::poll(timeout)? {
                // Drain a burst of input (key repeat, paste) before redrawing once.
                for _ in 0..256 {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        Event::Resize(..) => {}
                        _ => continue,
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
                dirty = true;
            }
            dirty |= self.apply_typed_query();
            dirty |= self.drain_messages();
            dirty |= self.tick();
            dirty |= busy;
        }
        Ok(())
    }

    // ----- input -----------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if ctrl(&key, 'x') || ctrl(&key, 'c') {
            self.quit = true;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if ctrl(&key, 'g') || key.code == KeyCode::F(1) {
            self.help = true;
            return;
        }
        if (ctrl(&key, 'd') || key.code == KeyCode::F(2)) && self.screen != Screen::Summary {
            self.viewer = None;
            self.open_summary(false);
            return;
        }
        match self.screen {
            Screen::List => self.on_list_key(key),
            Screen::Viewer => self.on_viewer_key(key),
            Screen::Summary => self.on_summary_key(key),
        }
    }

    // ----- summary ---------------------------------------------------------

    /// Shows the summary, analysing the listed files (search and time range
    /// applied) unless that exact scope was analysed already.
    fn open_summary(&mut self, force: bool) {
        self.screen = Screen::Summary;
        let mut files = self.order.clone();
        files.sort_unstable();
        let scope = (
            files,
            self.preset,
            self.from_text.clone(),
            self.to_text.clone(),
        );
        let old = self.summary.take();
        if !force
            && let Some(s) = old.as_ref()
            && s.scope == scope
            && (s.report.is_some() || s.job.is_some())
        {
            self.summary = old;
            return;
        }
        // Oldest first, so the latest occurrences are the ones kept.
        let mut srcs: Vec<(usize, Source)> = scope
            .0
            .iter()
            .map(|&i| (i, self.sources[i].clone()))
            .collect();
        srcs.sort_by_key(|(_, s)| (s.is_docker_logs(), s.modified));
        let range = self.range();
        let job = self.new_job();
        let done = Arc::new(AtomicU64::new(0));
        let (id, cancel, progress, tx, counter) = (
            job.id,
            job.cancel.clone(),
            job.progress.clone(),
            self.tx.clone(),
            done.clone(),
        );
        let total = srcs.len();
        let hide = self.hidden.clone();
        thread::spawn(move || {
            let report = summary::analyse(srcs, range, hide, &cancel, &progress, &counter);
            let _ = tx.send(Msg::Summary(id, report.map(Box::new)));
        });
        self.summary = Some(Summary {
            job: Some(job),
            done,
            total,
            report: None,
            tab: old.as_ref().map_or(0, |s| s.tab),
            sel: [0; TABS],
            sort: old.as_ref().map_or(TypeSort::Count, |s| s.sort),
            query: old.map_or(String::new(), |s| s.query),
            rows: Vec::new(),
            detail: None,
            notice: String::new(),
            marked: HashSet::new(),
            export: None,
            export_done: Arc::new(AtomicU64::new(0)),
            export_total: 0,
            scope,
        });
    }

    /// Hides a message type from the summary, saves it and analyses again.
    fn hide_type(&mut self, level: usize, group: usize) {
        let Some(pattern) = self
            .summary
            .as_ref()
            .and_then(|s| s.report.as_ref())
            .and_then(|r| r.groups[level].get(group))
            .map(|g| g.pattern.clone())
        else {
            return;
        };
        if pattern.starts_with("(other messages:") {
            return;
        }
        let saved = hidden::add(&pattern);
        self.hidden.push(pattern.clone());
        self.undo_hide.push(pattern.clone());
        self.open_summary(true);
        let note = match saved {
            Ok(p) => format!(
                "Hidden: {}  \u{b7}  Ctrl+Z undo  \u{b7}  saved in {}",
                clip_chars(&pattern, 50),
                p.file_name()
                    .map_or(p.display().to_string(), |n| n.to_string_lossy().into())
            ),
            Err(e) => {
                format!("Hidden for this session only (could not save: {e})  \u{b7}  Ctrl+Z undo")
            }
        };
        if let Some(s) = &mut self.summary {
            s.notice = note;
        }
    }

    /// Shows the type hidden last in this session again.
    fn unhide_last(&mut self) {
        let Some(pattern) = self.undo_hide.pop() else {
            if let Some(s) = &mut self.summary {
                s.notice = match hidden::path() {
                    Some(p) => format!(
                        "Nothing to undo. Hidden types are listed in {}",
                        p.display()
                    ),
                    None => "Nothing to undo".into(),
                };
            }
            return;
        };
        self.hidden.retain(|h| *h != pattern);
        let saved = hidden::remove(&pattern);
        self.open_summary(true);
        if let Some(s) = &mut self.summary {
            s.notice = match saved {
                Ok(_) => format!("Shown again: {}", clip_chars(&pattern, 60)),
                Err(e) => format!("Shown again for this session (could not save: {e})"),
            };
        }
    }

    /// Selects or unselects a message type for export.
    fn toggle_mark(&mut self, level: usize, group: usize) {
        let Some(s) = &mut self.summary else { return };
        if !s.marked.remove(&(level, group)) {
            s.marked.insert((level, group));
        }
        s.notice = match s.marked.len() {
            0 => "No type selected".into(),
            n => format!(
                "{n} type{} selected  \u{b7}  Ctrl+E exports {}",
                if n == 1 { "" } else { "s" },
                if n == 1 { "it" } else { "them" }
            ),
        };
    }

    /// Exports message types to a Markdown report on a worker thread.
    /// `all`: every type listed in the current view. Otherwise the selected
    /// types, or the highlighted one when none is selected.
    fn start_export(&mut self, all: bool) {
        let range = match self.preset {
            Some(i) => PRESETS[i].0.to_string(),
            None => format!(
                "{} to {}",
                if self.from_text.trim().is_empty() {
                    "start"
                } else {
                    self.from_text.trim()
                },
                if self.to_text.trim().is_empty() {
                    "now"
                } else {
                    self.to_text.trim()
                }
            ),
        };
        let job = self.new_job();
        let (title, sources, tx) = (self.title.clone(), self.sources.clone(), self.tx.clone());
        let Some(s) = &mut self.summary else { return };
        let Some(r) = &s.report else { return };
        if s.job.is_some() || s.export.is_some() {
            return;
        }
        let detail = s.detail.as_ref().map(|d| (d.level, d.group));
        let level = detail.map_or(s.tab, |d| d.0);
        let by_count = |v: &mut Vec<(usize, usize)>| {
            v.sort_by_key(|&(lv, g)| (lv, std::cmp::Reverse(r.groups[lv][g].count)))
        };
        let picks: Vec<(usize, usize)> = if all {
            if level == FILES_TAB {
                s.notice = "Pick a level with Tab (ERROR, WARN, ...) to export its types".into();
                return;
            }
            s.rows.iter().map(|&g| (level, g)).collect()
        } else if !s.marked.is_empty() {
            let mut v: Vec<_> = s.marked.iter().copied().collect();
            by_count(&mut v);
            v
        } else if let Some(d) = detail {
            vec![d]
        } else if let Some(&g) = s.rows.get(s.sel[s.tab]).filter(|_| s.tab != FILES_TAB) {
            vec![(s.tab, g)]
        } else {
            s.notice = "Select a message type first (Ctrl+T)".into();
            return;
        };
        if picks.is_empty() {
            s.notice = "Nothing to export".into();
            return;
        }
        let one_level = picks.iter().all(|p| p.0 == picks[0].0);
        let lv_name = if one_level {
            summary::LEVEL_NAMES[picks[0].0]
        } else {
            ""
        };
        let n = picks.len();
        let plural = if n == 1 { "" } else { "s" };
        let what = if all {
            let mut w = format!("all {n} {lv_name} type{plural}");
            if !s.query.trim().is_empty() {
                w.push_str(&format!(" matching \"{}\"", s.query.trim()));
            }
            w
        } else if one_level {
            format!("{n} selected {lv_name} type{plural}")
        } else {
            format!("{n} selected type{plural}")
        };
        let items: Vec<export::Item> = picks
            .iter()
            .map(|&(lv, g)| export::Item::new(lv, &r.groups[lv][g]))
            .collect();
        let files: HashSet<usize> = items
            .iter()
            .flat_map(|i| i.occ.iter().map(|o| o.src))
            .collect();
        let req = export::Request {
            title,
            what,
            range,
            search: if all {
                s.query.trim().to_string()
            } else {
                String::new()
            },
            analysed_files: r.files.len(),
            totals: r.totals,
            items,
            sources,
            path: export::target(if one_level { lv_name } else { "mixed" }, !all),
        };
        let done = Arc::new(AtomicU64::new(0));
        let (id, cancel, progress, counter) = (
            job.id,
            job.cancel.clone(),
            job.progress.clone(),
            done.clone(),
        );
        thread::spawn(move || {
            let res = export::run(req, &cancel, &progress, &counter);
            let _ = tx.send(Msg::Export(id, res));
        });
        s.export = Some(job);
        s.export_done = done;
        s.export_total = files.len();
    }

    /// Filters and sorts the rows of the current summary tab.
    fn rebuild_summary_rows(&mut self) {
        let Some(s) = &mut self.summary else { return };
        s.rows.clear();
        let Some(r) = &s.report else { return };
        let terms: Vec<String> = s
            .query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let hit = |text: &str| {
            let text = text.to_lowercase();
            terms.iter().all(|t| text.contains(t.as_str()))
        };
        if s.tab == FILES_TAB {
            let name = |i: usize| self.sources[r.files[i].src].name.as_str();
            s.rows = (0..r.files.len()).filter(|&i| hit(name(i))).collect();
            // Most errors first, then most warnings.
            s.rows.sort_by(|&a, &b| {
                let key = |i: usize| (r.files[i].counts[0], r.files[i].counts[1]);
                key(b)
                    .cmp(&key(a))
                    .then_with(|| natural_cmp(name(a), name(b)))
            });
        } else {
            let g = &r.groups[s.tab];
            s.rows = (0..g.len()).filter(|&i| hit(&g[i].pattern)).collect();
            match s.sort {
                TypeSort::Count => s.rows.sort_by_key(|&i| std::cmp::Reverse(g[i].count)),
                TypeSort::Latest => s.rows.sort_by_key(|&i| std::cmp::Reverse(g[i].last)),
                TypeSort::Oldest => s.rows.sort_by_key(|&i| (g[i].first.is_none(), g[i].first)),
            }
        }
        let last = s.rows.len().saturating_sub(1);
        s.sel[s.tab] = s.sel[s.tab].min(last);
    }

    fn on_summary_key(&mut self, key: KeyEvent) {
        if self.summary.as_ref().is_some_and(|s| s.detail.is_some()) {
            return self.on_detail_key(key);
        }
        let page = self.rows.saturating_sub(8).max(1);
        let Some(s) = &mut self.summary else { return };
        s.notice.clear();
        if let Some(c) = typed(&key) {
            if s.query.len() < 200 {
                s.query.push(c);
                s.sel[s.tab] = 0;
            }
            return self.rebuild_summary_rows();
        }
        let last = s.rows.len().saturating_sub(1);
        let sel = &mut s.sel[s.tab];
        match key.code {
            KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Down => *sel = (*sel + 1).min(last),
            KeyCode::PageUp => *sel = sel.saturating_sub(page),
            KeyCode::PageDown => *sel = (*sel + page).min(last),
            KeyCode::Home => *sel = 0,
            KeyCode::End => *sel = last,
            KeyCode::Tab | KeyCode::BackTab => {
                let step = if key.code == KeyCode::Tab {
                    1
                } else {
                    TABS - 1
                };
                s.tab = (s.tab + step) % TABS;
                self.rebuild_summary_rows();
            }
            KeyCode::Left | KeyCode::Right => {
                let n = PRESETS.len();
                let cur = self.preset.unwrap_or(0);
                let next = if key.code == KeyCode::Right {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                self.set_preset(next);
                self.open_summary(false);
            }
            KeyCode::Enter => {
                let Some(&row) = s.rows.get(*sel) else { return };
                if s.tab == FILES_TAB {
                    if let Some(src) = s.report.as_ref().map(|r| r.files[row].src) {
                        self.from_summary = true;
                        self.open_with(src, None);
                    }
                } else {
                    s.detail = Some(Detail {
                        level: s.tab,
                        group: row,
                        sel: 0,
                    });
                }
            }
            KeyCode::Char('s') if ctrl(&key, 's') => {
                s.sort = match s.sort {
                    TypeSort::Count => TypeSort::Latest,
                    TypeSort::Latest => TypeSort::Oldest,
                    TypeSort::Oldest => TypeSort::Count,
                };
                self.rebuild_summary_rows();
            }
            KeyCode::Char('r') if ctrl(&key, 'r') => self.open_summary(true),
            KeyCode::Char('l') if ctrl(&key, 'l') => self.screen = Screen::List,
            KeyCode::Delete if s.tab != FILES_TAB && s.job.is_none() => {
                if let Some(&row) = s.rows.get(*sel) {
                    let tab = s.tab;
                    self.hide_type(tab, row);
                }
            }
            KeyCode::Char('z') if ctrl(&key, 'z') => self.unhide_last(),
            KeyCode::Char('t') | KeyCode::Insert
                if (ctrl(&key, 't') || key.code == KeyCode::Insert) && s.tab != FILES_TAB =>
            {
                if let Some(&row) = s.rows.get(*sel) {
                    *sel = (*sel + 1).min(last);
                    let tab = s.tab;
                    self.toggle_mark(tab, row);
                }
            }
            KeyCode::Char('e') if ctrl(&key, 'e') => self.start_export(false),
            KeyCode::Char('a') if ctrl(&key, 'a') => self.start_export(true),
            KeyCode::Esc if s.export.is_some() => {
                s.export = None;
                s.notice = "Export cancelled".into();
            }
            KeyCode::Backspace => {
                s.query.pop();
                self.rebuild_summary_rows();
            }
            KeyCode::Char('u') if ctrl(&key, 'u') => {
                s.query.clear();
                self.rebuild_summary_rows();
            }
            KeyCode::Char('w') if ctrl(&key, 'w') => {
                delete_word(&mut s.query);
                self.rebuild_summary_rows();
            }
            KeyCode::Esc => {
                if s.job.is_some() {
                    // Cancel; with nothing to show yet, leave the screen.
                    s.job = None;
                    self.screen = Screen::List;
                } else if !s.query.is_empty() {
                    s.query.clear();
                    self.rebuild_summary_rows();
                } else {
                    self.screen = Screen::List;
                }
            }
            _ => {}
        }
    }

    fn on_detail_key(&mut self, key: KeyEvent) {
        let page = self.rows.saturating_sub(12).max(1);
        let Some(s) = &mut self.summary else { return };
        let (Some(d), Some(r)) = (&mut s.detail, &s.report) else {
            return;
        };
        s.notice.clear();
        let occ = &r.groups[d.level][d.group].occ;
        let last = occ.len().saturating_sub(1);
        match key.code {
            KeyCode::Delete => {
                let (level, group) = (d.level, d.group);
                self.hide_type(level, group);
            }
            KeyCode::Up => d.sel = d.sel.saturating_sub(1),
            KeyCode::Down => d.sel = (d.sel + 1).min(last),
            KeyCode::PageUp => d.sel = d.sel.saturating_sub(page),
            KeyCode::PageDown => d.sel = (d.sel + page).min(last),
            KeyCode::Home => d.sel = 0,
            KeyCode::End => d.sel = last,
            KeyCode::Left | KeyCode::Right => {
                // The previous / next type of the list.
                let at = s.rows.iter().position(|&g| g == d.group).unwrap_or(0);
                let to = if key.code == KeyCode::Right {
                    (at + 1).min(s.rows.len().saturating_sub(1))
                } else {
                    at.saturating_sub(1)
                };
                if let Some(&g) = s.rows.get(to) {
                    s.sel[s.tab] = to;
                    *d = Detail {
                        level: d.level,
                        group: g,
                        sel: 0,
                    };
                }
            }
            KeyCode::Enter => {
                // Newest first on screen, oldest first in the ring.
                if let Some(o) = occ.get(last.saturating_sub(d.sel)) {
                    let (src, jump) = (o.src, (o.off, o.line));
                    self.from_summary = true;
                    self.open_with(src, Some(jump));
                }
            }
            KeyCode::Char('l') if ctrl(&key, 'l') => {
                s.detail = None;
                self.screen = Screen::List;
            }
            KeyCode::Char('t') | KeyCode::Insert
                if ctrl(&key, 't') || key.code == KeyCode::Insert =>
            {
                let (level, group) = (d.level, d.group);
                self.toggle_mark(level, group);
            }
            KeyCode::Char('e') if ctrl(&key, 'e') => self.start_export(false),
            KeyCode::Char('a') if ctrl(&key, 'a') => self.start_export(true),
            KeyCode::Esc if s.export.is_some() => {
                s.export = None;
                s.notice = "Export cancelled".into();
            }
            KeyCode::Esc | KeyCode::Backspace => s.detail = None,
            _ => {}
        }
    }

    fn on_viewer_key(&mut self, key: KeyEvent) {
        let page = self.rows.saturating_sub(1).max(1) as isize;
        let Some(v) = &mut self.viewer else { return };
        if let Some(c) = typed(&key) {
            if v.query.len() < 512 {
                v.query.push(c);
                v.query_changed = Some(Instant::now());
            }
            return;
        }
        v.notice.clear();
        match key.code {
            KeyCode::Backspace => {
                v.query.pop();
                v.query_changed = Some(Instant::now());
            }
            KeyCode::Char('u') if ctrl(&key, 'u') => {
                v.query.clear();
                v.query_changed = Some(Instant::now());
            }
            KeyCode::Char('w') if ctrl(&key, 'w') => {
                delete_word(&mut v.query);
                v.query_changed = Some(Instant::now());
            }
            KeyCode::Char('f') if ctrl(&key, 'f') => {
                v.notice = "Just type: the filter is at the top. Example: error timeout".into();
            }
            KeyCode::Char('l') if ctrl(&key, 'l') => self.leave_viewer(true),
            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),
            KeyCode::PageUp => self.scroll(-page),
            KeyCode::PageDown => self.scroll(page),
            KeyCode::Char('r') if ctrl(&key, 'r') => {
                self.wrap = !self.wrap;
                v.notice = if self.wrap {
                    "Wrap on: long lines continue on the next rows".into()
                } else {
                    "Wrap off: use \u{2190} \u{2192} to pan long lines".into()
                };
            }
            KeyCode::Left | KeyCode::Right if self.wrap => {
                v.notice = "Wrap is on, nothing to pan. Ctrl+R turns it off".into();
            }
            KeyCode::Left => v.hscroll = v.hscroll.saturating_sub(16),
            KeyCode::Right => v.hscroll = (v.hscroll + 16).min(window::MAX_LINE),
            KeyCode::Home => {
                self.set_live(false);
                self.start_load(Target::Start, After::Top);
            }
            KeyCode::End => {
                if v.src.can_live() {
                    self.set_live(true);
                } else {
                    self.start_load(Target::End, After::Bottom);
                }
            }
            KeyCode::Enter => {
                let on = !v.live;
                self.set_live(on);
            }
            KeyCode::Esc => {
                if v.loading.is_some() {
                    v.loading = None;
                    v.notice = "Cancelled".into();
                    if v.win.lines.is_empty() && v.filter.is_some() {
                        // A filter was being applied: drop it and go back.
                        v.query.clear();
                        v.filter = None;
                        self.refresh_view();
                    }
                } else if !v.query.is_empty() || v.filter.is_some() {
                    v.query.clear();
                    v.query_changed = Some(Instant::now() - TYPE_DELAY);
                } else {
                    self.leave_viewer(false);
                }
            }
            _ => {}
        }
    }

    /// Closes the viewer: back to the summary it was opened from, or to the
    /// file list.
    fn leave_viewer(&mut self, to_list: bool) {
        self.viewer = None; // drops follower and cancels jobs
        self.screen = if !to_list && self.from_summary && self.summary.is_some() {
            Screen::Summary
        } else {
            Screen::List
        };
    }

    /// Applies the typed filter once typing has paused.
    fn apply_typed_query(&mut self) -> bool {
        let Some(v) = &mut self.viewer else {
            return false;
        };
        match v.query_changed {
            Some(t) if t.elapsed() >= TYPE_DELAY => v.query_changed = None,
            _ => return false,
        }
        let parsed = match Filter::parse(&v.query) {
            Ok(f) => f,
            Err(e) => {
                v.query_error = Some(e);
                return true;
            }
        };
        v.query_error = None;
        let same = match (&parsed, &v.filter) {
            (None, None) => true,
            (Some(a), Some(b)) => a.text.trim() == b.text.trim(),
            _ => false,
        };
        if same {
            return true;
        }
        v.filter = parsed.map(Arc::new);
        self.refresh_view();
        // Never show the old lines under the new filter while it is applied.
        if let Some(v) = &mut self.viewer
            && v.loading.is_some()
        {
            v.win = Window::default();
        }
        true
    }

    /// Reloads the view after the filter changed, keeping the position.
    fn refresh_view(&mut self) {
        let Some(v) = &mut self.viewer else { return };
        if v.live {
            return self.set_live(true);
        }
        let target = v.win.lines.get(v.top).map(|l| (l.off, l.data.clone()));
        match target {
            Some((_, data)) if v.win.relative => {
                self.start_load(Target::End, After::AnchorContent { data, delta: 0 })
            }
            Some((off, _)) => self.start_load(Target::Around(off), After::Anchor { off, delta: 0 }),
            None if v.win.at_start && !v.win.at_end => self.start_load(Target::Start, After::Top),
            None => self.start_load(Target::End, After::Bottom),
        }
    }

    // ----- viewer ----------------------------------------------------------

    fn open(&mut self, idx: usize) {
        self.from_summary = false;
        self.open_with(idx, None);
    }

    /// Opens a log; `jump` = (offset, line number) of a line to show, paused.
    fn open_with(&mut self, idx: usize, jump: Option<(u64, u64)>) {
        let src = self.sources[idx].clone();
        let can_live = src.can_live();
        // A jump must show its line, so no filter may hide it.
        let filter = self.grep.clone().filter(|_| jump.is_none());
        let query = filter.as_ref().map_or(String::new(), |f| f.text.clone());
        self.viewer = Some(Viewer {
            src,
            win: Window::default(),
            top: 0,
            hscroll: 0,
            live: false,
            pinned: true,
            follower: None,
            loading: None,
            after: After::Bottom,
            filter,
            query,
            query_changed: None,
            query_error: None,
            notice: jump.map_or(String::new(), |(_, line)| {
                format!("Line {line}, highlighted  \u{b7}  Esc goes back to the summary")
            }),
            mark: jump.map(|j| j.0),
            last_poll: Instant::now(),
        });
        self.screen = Screen::Viewer;
        if let Some((off, _)) = jump {
            self.start_load(Target::Around(off), After::Anchor { off, delta: -2 });
        } else if can_live {
            self.set_live(true);
        } else {
            self.start_load(Target::End, After::Bottom);
        }
    }

    fn set_live(&mut self, on: bool) {
        let (rows, tail) = (self.rows, self.tail);
        let Some(v) = &mut self.viewer else { return };
        if !on {
            v.live = false;
            v.follower = None;
            return;
        }
        if !v.src.can_live() {
            v.notice = "Compressed log: live mode is not available".into();
            return;
        }
        v.loading = None;
        v.follower = None;
        if let Some(path) = v.src.plain_path() {
            if v.filter.is_some() {
                // Finding the latest matches may read far back: worker.
                v.live = true;
                return self.start_load(Target::End, After::Bottom);
            }
            match window::load_plain(path, Target::End) {
                Ok(w) => {
                    v.win = w;
                    v.live = true;
                    v.pinned = true;
                    v.top = v.win.lines.len().saturating_sub(rows);
                }
                Err(e) => v.notice = err_text(&e),
            }
        } else {
            match v.src.open_follow(tail) {
                Ok((reader, killer)) => {
                    v.win = Window {
                        relative: true,
                        at_end: true,
                        ..Default::default()
                    };
                    v.top = 0;
                    v.pinned = true;
                    v.follower = Some(Follower::spawn(reader, killer, v.filter.clone()));
                    v.live = true;
                }
                Err(e) => v.notice = err_text(&e),
            }
        }
    }

    fn start_load(&mut self, target: Target, after: After) {
        let tail = self.tail;
        let Some(v) = &self.viewer else { return };
        let (src, filter) = (v.src.clone(), v.filter.clone());
        if let (Some(path), None) = (src.plain_path(), &filter) {
            // Seekable and unfiltered: a bounded read around the target, inline.
            match window::load_plain(path, target) {
                Ok(w) => {
                    let v = self.viewer.as_mut().unwrap();
                    v.win = w;
                    v.loading = None;
                    self.apply_after(after);
                }
                Err(e) => self.viewer.as_mut().unwrap().notice = err_text(&e),
            }
            return;
        }
        let job = self.new_job();
        let (id, cancel, progress, tx) = (
            job.id,
            job.cancel.clone(),
            job.progress.clone(),
            self.tx.clone(),
        );
        thread::spawn(move || {
            let res = match (src.plain_path(), filter.as_deref()) {
                (Some(path), Some(f)) => {
                    window::load_plain_filtered(path, target, tail, f, &cancel, &progress)
                }
                _ => src.open_stream().and_then(|r| {
                    window::load_stream(r, target, tail, &cancel, &progress, filter.as_deref())
                }),
            };
            let _ = tx.send(Msg::Window(id, res));
        });
        let v = self.viewer.as_mut().unwrap();
        v.after = after;
        v.loading = Some(job);
    }

    fn apply_after(&mut self, after: After) {
        let rows = self.rows;
        let Some(v) = &mut self.viewer else { return };
        let len = v.win.lines.len();
        let max_top = len.saturating_sub(rows) as isize;
        let place =
            |idx: usize, delta: isize| (idx as isize + delta).clamp(0, max_top.max(0)) as usize;
        v.pinned = false;
        match after {
            After::Bottom => {
                v.top = max_top as usize;
                v.pinned = true;
            }
            After::Top => v.top = 0,
            After::Anchor { off, delta } => v.top = place(v.win.index_of(off), delta),
            After::AnchorContent { data, delta } => {
                let idx = v
                    .win
                    .lines
                    .iter()
                    .rposition(|l| l.data == data)
                    .unwrap_or(max_top as usize);
                v.top = place(idx, delta);
            }
        }
    }

    fn scroll(&mut self, delta: isize) {
        let rows = self.rows;
        let Some(v) = &mut self.viewer else { return };
        if v.loading.is_some() {
            return;
        }
        if v.live {
            self.set_live(false);
        }
        let (wrap, cols) = (self.wrap, self.cols);
        let v = self.viewer.as_mut().unwrap();
        let len = v.win.lines.len();
        let max_top = len.saturating_sub(rows) as isize;
        let mut want = v.top as isize + delta;
        if v.pinned {
            if delta > 0 && v.win.at_end {
                return;
            }
            if wrap && delta < 0 {
                // Leave the bottom-filled view from the line now at its top.
                let (mut used, mut first) = (0, len);
                while first > 0 && used < rows {
                    first -= 1;
                    used += ui::wrapped_rows(&v.win.lines[first].data, cols);
                }
                let cut = used > rows;
                want = first as isize + delta + cut as isize;
            }
            v.pinned = false;
        }
        if want >= max_top && v.win.at_end && delta > 0 {
            v.top = max_top.max(0) as usize;
            v.pinned = true;
            return;
        }
        let needs_more = (want < 0 && !v.win.at_start) || (want > max_top && !v.win.at_end);
        let Some(top_line) = v.win.lines.get(v.top).filter(|_| needs_more) else {
            v.top = want.clamp(0, max_top) as usize;
            return;
        };
        if v.win.relative {
            // A live docker window has no absolute position yet: reload the
            // full log with real offsets and re-anchor on the same line.
            let data = top_line.data.clone();
            self.start_load(Target::End, After::AnchorContent { data, delta });
        } else {
            let off = top_line.off;
            self.start_load(Target::Around(off), After::Anchor { off, delta });
        }
    }

    // ----- background results ----------------------------------------------

    fn drain_messages(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Count(i, res) => {
                    self.counts[i] = res.map_or(Count::Failed, Count::Done);
                    if self.sort == Sort::Matches && self.screen == Screen::List {
                        self.rebuild_order();
                    }
                }
                Msg::Summary(id, report) => {
                    let Some(s) = &mut self.summary else { continue };
                    if s.job.as_ref().map(|j| j.id) != Some(id) {
                        continue;
                    }
                    s.job = None;
                    s.report = report;
                    self.rebuild_summary_rows();
                }
                Msg::Export(id, res) => {
                    let Some(s) = &mut self.summary else { continue };
                    if s.export.as_ref().map(|j| j.id) != Some(id) {
                        continue;
                    }
                    s.export = None;
                    s.notice = match res {
                        Some(Ok(d)) => {
                            let mut n = format!(
                                "Exported {} type{} ({} occurrences) to {}",
                                d.types,
                                if d.types == 1 { "" } else { "s" },
                                d.entries,
                                d.path.display()
                            );
                            if !d.errors.is_empty() {
                                n.push_str(&format!(
                                    "  \u{b7}  could not read {} file{} again (listed in the report)",
                                    d.errors.len(),
                                    if d.errors.len() == 1 { "" } else { "s" }
                                ));
                            } else if d.missing > 0 {
                                n.push_str(&format!(
                                    "  \u{b7}  {} line{} could not be read again",
                                    d.missing,
                                    if d.missing == 1 { "" } else { "s" }
                                ));
                            }
                            n
                        }
                        Some(Err(e)) => format!("Export failed: {e}"),
                        None => "Export cancelled".into(),
                    };
                }
                Msg::Window(id, res) => {
                    let Some(v) = &mut self.viewer else { continue };
                    if v.loading.as_ref().map(|j| j.id) != Some(id) {
                        continue;
                    }
                    v.loading = None;
                    match res {
                        Ok(w) => {
                            v.win = w;
                            let after = std::mem::replace(&mut v.after, After::Bottom);
                            self.apply_after(after);
                        }
                        Err(e) => {
                            v.live = false;
                            v.notice = err_text(&e);
                        }
                    }
                }
            }
        }
        changed
    }

    /// Live mode upkeep: pick up appended data without re-reading the file.
    fn tick(&mut self) -> bool {
        let (rows, tail) = (self.rows, self.tail);
        let Some(v) = &mut self.viewer else {
            return false;
        };
        if !v.live || v.loading.is_some() {
            self.backlog = false;
            return false;
        }
        let mut changed = false;
        if let Some(f) = &v.follower {
            // Bounded work per tick; a backlog is finished on the next ticks.
            self.backlog = true;
            for _ in 0..512 {
                match f.rx.try_recv() {
                    Ok(batch) => {
                        for l in batch {
                            v.win.bytes += l.data.len();
                            v.win.lines.push_back(l);
                        }
                        changed = true;
                    }
                    Err(TryRecvError::Empty) => {
                        self.backlog = false;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.backlog = false;
                        v.follower = None;
                        v.live = false;
                        v.notice = "Stream ended".into();
                        changed = true;
                        break;
                    }
                }
            }
            if changed {
                v.win.trim_front(tail, window::MAX_WINDOW_BYTES);
            }
        } else if let Some(path) = v.src.plain_path() {
            if v.last_poll.elapsed() < Duration::from_millis(200) {
                return false;
            }
            v.last_poll = Instant::now();
            let filter = v.filter.clone();
            match window::append_plain(path, &mut v.win, filter.as_deref()) {
                Ok(Appended::Nothing) => {}
                Ok(Appended::Grew) => {
                    v.win
                        .trim_front(tail.max(rows), window::MAX_WINDOW_BYTES / 2);
                    changed = true;
                }
                Ok(Appended::Reload) => {
                    if filter.is_some() {
                        self.start_load(Target::End, After::Bottom);
                        return true;
                    }
                    match window::load_plain(path, Target::End) {
                        Ok(w) => v.win = w,
                        Err(e) => v.notice = err_text(&e),
                    }
                    changed = true;
                }
                Err(e) => {
                    let msg = err_text(&e);
                    changed = v.notice != msg;
                    v.notice = msg;
                }
            }
        }
        if changed {
            let v = self.viewer.as_mut().unwrap();
            v.top = v.win.lines.len().saturating_sub(rows);
        }
        changed
    }
}
