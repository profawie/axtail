//! Application state and event loop.
//!
//! The UI thread never does unbounded work: anything that may need to read
//! a lot of data (filtered windows, stream windows, match counts) runs on a
//! worker thread that can be cancelled and reports progress.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::clock;
use crate::filter::{self, Filter};
use crate::source::{Killer, Scan, Source, natural_cmp};
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
    follower: Option<Follower>,
    pub loading: Option<Job>,
    after: After,
    /// The filter in effect and the text being typed (applied after a pause).
    pub filter: Option<Arc<Filter>>,
    pub query: String,
    query_changed: Option<Instant>,
    pub query_error: Option<String>,
    pub notice: String,
    last_poll: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Viewer,
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
    pub help: bool,
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
            help: false,
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
            self.rows = ui::body_rows(terminal.size()?.height);
            self.open(0);
        }
        let mut dirty = true;
        while !self.quit {
            self.rows = ui::body_rows(terminal.size()?.height);
            if dirty {
                terminal.draw(|f| ui::draw(f, self))?;
                dirty = false;
            }
            let typing = self
                .viewer
                .as_ref()
                .is_some_and(|v| v.query_changed.is_some());
            let busy = self.viewer.as_ref().is_some_and(|v| v.loading.is_some());
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
        match self.screen {
            Screen::List => self.on_list_key(key),
            Screen::Viewer => self.on_viewer_key(key),
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
            KeyCode::Char('l') if ctrl(&key, 'l') => self.back_to_list(),
            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),
            KeyCode::PageUp => self.scroll(-page),
            KeyCode::PageDown => self.scroll(page),
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
                    self.back_to_list();
                }
            }
            _ => {}
        }
    }

    fn back_to_list(&mut self) {
        self.viewer = None; // drops follower and cancels jobs
        self.screen = Screen::List;
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
        let src = self.sources[idx].clone();
        let can_live = src.can_live();
        let filter = self.grep.clone();
        let query = filter.as_ref().map_or(String::new(), |f| f.text.clone());
        self.viewer = Some(Viewer {
            src,
            win: Window::default(),
            top: 0,
            hscroll: 0,
            live: false,
            follower: None,
            loading: None,
            after: After::Bottom,
            filter,
            query,
            query_changed: None,
            query_error: None,
            notice: String::new(),
            last_poll: Instant::now(),
        });
        self.screen = Screen::Viewer;
        if can_live {
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
        match after {
            After::Bottom => v.top = max_top as usize,
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
        let v = self.viewer.as_mut().unwrap();
        let max_top = v.win.lines.len().saturating_sub(rows) as isize;
        let want = v.top as isize + delta;
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
