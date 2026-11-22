//! Application state and event loop.
//!
//! The UI thread never does unbounded work: anything that may need to read
//! a lot of data (stream windows, searches, match counts) runs on a worker
//! thread that can be cancelled and reports progress.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::search::{self, Dir, Matcher, Mode};
use crate::source::{Killer, Scan, Source};
use crate::ui;
use crate::window::{self, Appended, Line, LineReader, Target, Window};

pub struct Job {
    pub id: u64,
    pub label: &'static str,
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
    Found(u64, Dir, io::Result<Option<u64>>),
    Count(usize, io::Result<u64>),
}

/// Background reader for a live docker stream. Bounded channel = backpressure.
struct Follower {
    rx: Receiver<Vec<Line>>,
    killer: Killer,
}

impl Follower {
    fn spawn(reader: Box<dyn io::Read + Send>, killer: Killer) -> Follower {
        let (tx, rx): (SyncSender<Vec<Line>>, _) = mpsc::sync_channel(32);
        thread::spawn(move || {
            let mut lr = LineReader::new(reader, 0);
            let mut buf = Vec::new();
            let mut batch = Vec::new();
            loop {
                match lr.next_line(&mut buf) {
                    Ok(Some((off, _))) => {
                        batch.push(Line {
                            off,
                            data: buf.as_slice().into(),
                        });
                        if (lr.drained() || batch.len() >= 64)
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
    Match(u64),
}

pub struct Viewer {
    pub src: Source,
    pub win: Window,
    pub top: usize,
    pub hscroll: usize,
    pub live: bool,
    follower: Option<Follower>,
    pub loading: Option<Job>,
    pub searching: Option<Job>,
    after: After,
    pub matcher: Option<Arc<Matcher>>,
    pub cur_match: Option<u64>,
    pub notice: String,
    last_poll: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Viewer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Search,
    ListFilter,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub text: String,
    pub mode: Mode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Name,
    Newest,
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
    pub name_filter: String,
    pub counts: Vec<Count>,
    pub grep: Option<Arc<Matcher>>,
    pub screen: Screen,
    pub viewer: Option<Viewer>,
    pub about: bool,
    pub prompt: Option<Prompt>,
    pub message: String,
    pub rows: usize,
    tail: usize,
    last_mode: Mode,
    last_query: String,
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

fn err_text(e: &io::Error) -> String {
    e.to_string()
}

impl App {
    pub fn new(scan: Scan, grep: Option<Matcher>, tail: usize) -> App {
        let (tx, rx) = mpsc::channel();
        let grep = grep.map(Arc::new);
        let docker = scan.sources.iter().any(|s| s.is_docker());
        let mut app = App {
            title: scan.title,
            counts: vec![Count::Pending; scan.sources.len()],
            order: Vec::new(),
            sources: scan.sources,
            sel: 0,
            // In a container there are often hundreds of rotated files:
            // show the most recent first.
            sort: if docker { Sort::Newest } else { Sort::Name },
            name_filter: String::new(),
            last_mode: grep.as_ref().map_or(Mode::Text, |m| m.mode),
            last_query: grep.as_ref().map_or(String::new(), |m| m.query.clone()),
            grep,
            screen: Screen::List,
            viewer: None,
            about: false,
            prompt: None,
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
                        .send(Msg::Count(i, search::count(&src, &m, &cancel)))
                        .is_err()
                {
                    return;
                }
            }
        });
    }

    fn rebuild_order(&mut self) {
        let needle = self.name_filter.to_lowercase();
        let mut order: Vec<usize> = (0..self.sources.len())
            .filter(|&i| needle.is_empty() || self.sources[i].name.to_lowercase().contains(&needle))
            .collect();
        let s = &self.sources;
        match self.sort {
            // Natural order on the full path keeps each folder together.
            Sort::Name => {
                order.sort_by(|&a, &b| crate::source::natural_cmp(&s[a].name, &s[b].name))
            }
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
        order.sort_by_key(|&i| !self.sources[i].is_docker_logs());
        self.order = order;
        self.sel = self.sel.min(self.order.len().saturating_sub(1));
    }

    fn new_job(&mut self, label: &'static str) -> Job {
        self.next_id += 1;
        Job {
            id: self.next_id,
            label,
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
            let busy = self
                .viewer
                .as_ref()
                .is_some_and(|v| v.loading.is_some() || v.searching.is_some());
            let timeout = Duration::from_millis(if self.backlog {
                0
            } else if busy {
                50
            } else {
                200
            });
            if event::poll(timeout)? {
                // Drain a burst of input (key repeat) before redrawing once.
                for _ in 0..64 {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        _ => {}
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
                dirty = true;
            }
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
        if self.about {
            self.about = false;
            return;
        }
        if self.prompt.is_some() {
            return self.on_prompt_key(key);
        }
        if ctrl(&key, 'g') || key.code == KeyCode::F(1) {
            self.about = true;
            return;
        }
        match self.screen {
            Screen::List => self.on_list_key(key),
            Screen::Viewer => self.on_viewer_key(key),
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        let Some(p) = &mut self.prompt else { return };
        let kind = p.kind;
        match key.code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Tab if kind == PromptKind::Search => p.mode = p.mode.next(),
            KeyCode::Backspace => {
                p.text.pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if p.text.len() < 512 {
                    p.text.push(c);
                }
            }
            KeyCode::Enter => {
                let p = self.prompt.take().unwrap();
                match kind {
                    PromptKind::ListFilter => {}
                    PromptKind::Search => return self.submit_search(p),
                }
            }
            _ => {}
        }
        if kind == PromptKind::ListFilter {
            if let Some(p) = &self.prompt {
                self.name_filter = p.text.clone();
            } else if key.code == KeyCode::Esc {
                self.name_filter.clear();
            }
            self.rebuild_order();
        }
    }

    fn submit_search(&mut self, p: Prompt) {
        self.last_mode = p.mode;
        self.last_query = p.text.clone();
        let Some(v) = &mut self.viewer else { return };
        v.cur_match = None;
        if p.text.is_empty() {
            v.matcher = None;
            v.notice = "Search cleared".into();
            return;
        }
        match Matcher::new(&p.text, p.mode) {
            Ok(m) => {
                v.matcher = Some(Arc::new(m));
                self.start_search(Dir::Back);
            }
            Err(e) => {
                v.notice = format!("Invalid pattern: {e}");
                self.prompt = Some(p);
            }
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        let last = self.order.len().saturating_sub(1);
        let page = self.rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(page),
            KeyCode::PageDown => self.sel = (self.sel + page).min(last),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = last,
            KeyCode::Left | KeyCode::Right => {
                let all: &[Sort] = if self.grep.is_some() {
                    &[Sort::Name, Sort::Newest, Sort::Largest, Sort::Matches]
                } else {
                    &[Sort::Name, Sort::Newest, Sort::Largest]
                };
                let i = all.iter().position(|&s| s == self.sort).unwrap_or(0);
                let step = if key.code == KeyCode::Right {
                    1
                } else {
                    all.len() - 1
                };
                self.sort = all[(i + step) % all.len()];
                self.rebuild_order();
            }
            KeyCode::Enter => {
                if let Some(&i) = self.order.get(self.sel) {
                    self.open(i);
                }
            }
            KeyCode::Esc if !self.name_filter.is_empty() => {
                self.name_filter.clear();
                self.rebuild_order();
            }
            KeyCode::Char('/') => self.open_filter_prompt(),
            KeyCode::Char('f') if ctrl(&key, 'f') => self.open_filter_prompt(),
            _ => {}
        }
    }

    fn open_filter_prompt(&mut self) {
        self.prompt = Some(Prompt {
            kind: PromptKind::ListFilter,
            text: self.name_filter.clone(),
            mode: Mode::Text,
        });
    }

    fn on_viewer_key(&mut self, key: KeyEvent) {
        let page = self.rows.saturating_sub(1).max(1) as isize;
        let Some(v) = &mut self.viewer else { return };
        v.notice.clear();
        match key.code {
            KeyCode::Char('f') if ctrl(&key, 'f') => self.open_search_prompt(),
            KeyCode::Char('/') => self.open_search_prompt(),
            KeyCode::Char('l') if ctrl(&key, 'l') => self.back_to_list(),
            KeyCode::Char('p') if ctrl(&key, 'p') => self.start_search(Dir::Back),
            KeyCode::Char('n') if ctrl(&key, 'n') => self.start_search(Dir::Fwd),
            KeyCode::Char('N') => self.start_search(Dir::Back),
            KeyCode::Char('n') => self.start_search(Dir::Fwd),
            KeyCode::F(3) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.start_search(Dir::Back)
            }
            KeyCode::F(3) => self.start_search(Dir::Fwd),
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
                if v.loading.is_some() || v.searching.is_some() {
                    v.loading = None;
                    v.searching = None;
                    v.notice = "Cancelled".into();
                } else if v.matcher.is_some() {
                    v.matcher = None;
                    v.cur_match = None;
                    v.notice = "Search cleared".into();
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

    fn open_search_prompt(&mut self) {
        // Searching freezes the view so new lines do not disturb it.
        self.set_live(false);
        self.prompt = Some(Prompt {
            kind: PromptKind::Search,
            text: self.last_query.clone(),
            mode: self.last_mode,
        });
    }

    // ----- viewer ----------------------------------------------------------

    fn open(&mut self, idx: usize) {
        let src = self.sources[idx].clone();
        let can_live = src.can_live();
        self.viewer = Some(Viewer {
            src,
            win: Window::default(),
            top: 0,
            hscroll: 0,
            live: false,
            follower: None,
            loading: None,
            searching: None,
            after: After::Bottom,
            matcher: self.grep.clone(),
            cur_match: None,
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
        v.searching = None;
        v.cur_match = None;
        if let Some(path) = v.src.plain_path() {
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
                    v.follower = Some(Follower::spawn(reader, killer));
                    v.live = true;
                }
                Err(e) => v.notice = err_text(&e),
            }
        }
    }

    fn start_load(&mut self, target: Target, after: After) {
        let tail = self.tail;
        let Some(src) = self.viewer.as_ref().map(|v| v.src.clone()) else {
            return;
        };
        if let Some(path) = src.plain_path() {
            // Seekable: a bounded read around the target, done inline.
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
        let job = self.new_job("Loading");
        let (id, cancel, progress, tx) = (
            job.id,
            job.cancel.clone(),
            job.progress.clone(),
            self.tx.clone(),
        );
        thread::spawn(move || {
            let res = src
                .open_stream()
                .and_then(|r| window::load_stream(r, target, tail, &cancel, &progress));
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
            After::Match(off) => {
                v.cur_match = Some(off);
                v.top = place(v.win.index_of(off), -((rows / 3) as isize));
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
            // tail with real offsets and re-anchor on the same line.
            let data = top_line.data.clone();
            self.start_load(Target::End, After::AnchorContent { data, delta });
        } else {
            let off = top_line.off;
            self.start_load(Target::Around(off), After::Anchor { off, delta });
        }
    }

    fn start_search(&mut self, dir: Dir) {
        let rows = self.rows;
        let Some(v) = &mut self.viewer else { return };
        let Some(m) = v.matcher.clone() else {
            v.notice = "No search pattern yet: press ^F".into();
            return;
        };
        if v.live {
            self.set_live(false);
        }
        let job = self.new_job("Searching");
        let v = self.viewer.as_mut().unwrap();
        let w = &v.win;
        // Continue from the current match only while it is still on screen;
        // after scrolling away, search from what the user is looking at.
        let visible = |off: u64| w.lines.iter().skip(v.top).take(rows).any(|l| l.off == off);
        let pos = match (dir, v.cur_match.filter(|&c| visible(c))) {
            (Dir::Back, Some(c)) => c,
            (Dir::Fwd, Some(c)) => c + 1,
            (_, None) if w.relative => u64::MAX,
            (Dir::Back, None) => match w.lines.get(v.top + rows) {
                Some(l) => l.off,
                None if w.at_end => u64::MAX,
                None => w.lines.back().map_or(u64::MAX, |l| l.off + 1),
            },
            (Dir::Fwd, None) => w.lines.get(v.top).map_or(0, |l| l.off),
        };
        let (id, cancel, progress, tx, src) = (
            job.id,
            job.cancel.clone(),
            job.progress.clone(),
            self.tx.clone(),
            v.src.clone(),
        );
        thread::spawn(move || {
            let res = search::find(&src, &m, pos, dir, &cancel, &progress);
            let _ = tx.send(Msg::Found(id, dir, res));
        });
        v.loading = None;
        v.searching = Some(job);
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
                        Err(e) => v.notice = err_text(&e),
                    }
                }
                Msg::Found(id, dir, res) => {
                    let Some(v) = &mut self.viewer else { continue };
                    if v.searching.as_ref().map(|j| j.id) != Some(id) {
                        continue;
                    }
                    v.searching = None;
                    match res {
                        Ok(Some(off)) => self.start_load(Target::Around(off), After::Match(off)),
                        Ok(None) => {
                            v.notice = match dir {
                                Dir::Back => "No older match".into(),
                                Dir::Fwd => "No newer match".into(),
                            }
                        }
                        Err(e) => v.notice = err_text(&e),
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
        if !v.live {
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
            match window::append_plain(path, &mut v.win) {
                Ok(Appended::Nothing) => {}
                Ok(Appended::Grew) => changed = true,
                Ok(Appended::Reload) => {
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
            v.top = v.win.lines.len().saturating_sub(rows);
        }
        changed
    }
}
