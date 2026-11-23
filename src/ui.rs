//! Rendering: header, filter bar, bordered body and a one-line status bar.
//! The full key list lives in the help dialog (Ctrl+G) to save space.

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Count, Focus, PRESETS, Screen, Viewer};
use crate::clock;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Rows available for content: total minus header, filter bar, borders and
/// status bar.
pub fn body_rows(term_height: u16) -> usize {
    (term_height as usize).saturating_sub(5).max(1)
}

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn age(t: Option<SystemTime>) -> String {
    let Some(secs) = t.and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()) else {
        return "-".into();
    };
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Keeps the tail of a long name, which is the most telling part of a path.
fn fit_left(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        format!("{s:<width$}")
    } else {
        let tail: String = s.chars().skip(n - width.saturating_sub(2)).collect();
        format!("..{tail}")
    }
}

/// Makes raw log bytes safe to print: lossy UTF-8, no ANSI escapes, no
/// control characters, tabs expanded.
fn sanitize(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn level_style(line: &str) -> Style {
    let head: Vec<u8> = line
        .bytes()
        .take(200)
        .map(|b| b.to_ascii_uppercase())
        .collect();
    let has = |needle: &str| memchr::memmem::find(&head, needle.as_bytes()).is_some();
    if has("ERROR")
        || has("FATAL")
        || has("CRIT")
        || has("PANIC")
        || has("EXCEPTION")
        || has("SEVERE")
    {
        Style::new().fg(Color::LightRed)
    } else if has("WARN") {
        Style::new().fg(Color::Yellow)
    } else if has("DEBUG") || has("TRACE") {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new()
    }
}

/// Builds one visible row: horizontal scroll, level colour, match highlight.
fn render_line(
    data: &[u8],
    hscroll: usize,
    width: usize,
    highlight: Option<&regex::Regex>,
) -> Line<'static> {
    let text = sanitize(data);
    let base = level_style(&text);
    // Byte range of the visible columns.
    let mut idx = text.char_indices().map(|(i, _)| i).skip(hscroll);
    let lo = idx.next().unwrap_or(text.len());
    let hi = idx.nth(width.saturating_sub(1)).unwrap_or(text.len());
    let hit = Style::new().fg(Color::Black).bg(Color::Yellow);
    let mut spans = Vec::new();
    let mut pos = lo;
    if let Some(re) = highlight {
        for found in re.find_iter(&text) {
            let (s, e) = (found.start().max(lo), found.end().min(hi));
            if s >= e || s < pos {
                continue;
            }
            if s > pos {
                spans.push(Span::styled(text[pos..s].to_string(), base));
            }
            spans.push(Span::styled(text[s..e].to_string(), hit));
            pos = e;
        }
    }
    if pos < hi {
        spans.push(Span::styled(text[pos..hi].to_string(), base));
    }
    Line::from(spans).style(base)
}

fn badge(text: &str, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::new()
            .fg(Color::Black)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )
}

const DIM: Style = Style::new().fg(Color::DarkGray);
const LABEL: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const CURSOR: &str = "\u{2588}";

pub fn draw(f: &mut Frame, app: &App) {
    let [header, bar, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let viewer = app.viewer.as_ref().filter(|_| app.screen == Screen::Viewer);
    let subject = viewer.map_or(app.title.as_str(), |v| v.src.name.as_str());
    f.render_widget(
        Paragraph::new(format!(" axtail {VERSION} - {subject}")).style(
            Style::new()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );
    match viewer {
        Some(v) => {
            draw_viewer_bar(f, bar, v);
            draw_viewer(f, body, v);
        }
        None => {
            draw_list_bar(f, bar, app);
            draw_list(f, body, app);
        }
    }
    draw_status(f, status, app, viewer);
    if app.help {
        draw_help(f, f.area(), viewer.is_some());
    }
}

/// Renders `left` and right-aligns `right` on the same row.
fn two_sided(f: &mut Frame, area: Rect, left: Vec<Span<'static>>, right: Vec<Span<'static>>) {
    let rw: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let [l, r] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(rw as u16)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), l);
    f.render_widget(Paragraph::new(Line::from(right)), r);
}

fn draw_viewer_bar(f: &mut Frame, area: Rect, v: &Viewer) {
    let mut left = vec![
        Span::styled(" Filter \u{25b8} ", LABEL),
        Span::raw(v.query.clone()),
        Span::raw(CURSOR),
    ];
    if v.query.is_empty() {
        left.push(Span::styled(
            " type to show matching lines only: error timeout   +must  -skip  \"exact phrase\"  /regex/",
            DIM,
        ));
    }
    if let Some(e) = &v.query_error {
        left.push(Span::styled(
            format!("   {e}"),
            Style::new().fg(Color::LightRed),
        ));
    }
    let right = match (&v.filter, v.loading.is_some()) {
        (Some(_), false) => {
            let n = v.win.lines.len();
            let more = if v.win.at_start { "" } else { ", more above" };
            vec![Span::styled(
                format!("{n} matching line{}{more} ", if n == 1 { "" } else { "s" }),
                Style::new().fg(Color::LightYellow),
            )]
        }
        _ => Vec::new(),
    };
    two_sided(f, area, left, right);
}

fn field(text: &str, placeholder: &str, focused: bool) -> Vec<Span<'static>> {
    let style = if focused {
        Style::new().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::new().fg(Color::White)
    };
    let mut out = if text.is_empty() && !focused {
        vec![Span::styled(placeholder.to_string(), DIM)]
    } else {
        vec![Span::styled(text.to_string(), style)]
    };
    if focused {
        out.push(Span::raw(CURSOR));
    }
    out
}

fn draw_list_bar(f: &mut Frame, area: Rect, app: &App) {
    let mut left = vec![Span::styled(" Search \u{25b8} ", LABEL)];
    left.extend(field(
        &app.search,
        "type a name",
        app.focus == Focus::Search,
    ));
    let preset = match app.preset {
        Some(i) => PRESETS[i].0,
        None => "Custom",
    };
    left.push(Span::styled("   Time ", LABEL));
    left.push(Span::styled(
        format!("\u{25c2} {preset} \u{25b8}"),
        Style::new().fg(Color::LightYellow),
    ));
    left.push(Span::styled("  From ", LABEL));
    left.extend(field(&app.from_text, "any", app.focus == Focus::From));
    left.push(Span::styled("  To ", LABEL));
    left.extend(field(&app.to_text, "now", app.focus == Focus::To));
    if app.range_error {
        left.push(Span::styled(
            "  use YYYY-MM-DD HH:MM",
            Style::new().fg(Color::LightRed),
        ));
    }
    let right = vec![Span::styled(
        format!(
            "{} of {} ({}) ",
            app.order.len(),
            app.sources.len(),
            clock::zone_label()
        ),
        DIM,
    )];
    two_sided(f, area, left, right);
}

fn frame_block(title: String, right: String) -> Block<'static> {
    Block::bordered()
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ))
        .title(Line::from(Span::styled(right, Style::new().fg(Color::Gray))).right_aligned())
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let block = frame_block(
        format!(" Log files ({}) ", app.order.len()),
        format!(" sort: {} ", app.sort.label()),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let grep = app.grep.is_some();
    let fixed = 1 + 9 + 11 + 17 + 6 + if grep { 10 } else { 0 } + 1;
    let name_w = (inner.width as usize).saturating_sub(fixed).max(8);
    let head = format!(
        " {:<name_w$} {:<8} {:>10} {:<16} {:>5}{}",
        "NAME",
        "TYPE",
        "SIZE",
        "MODIFIED",
        "AGE",
        if grep {
            format!(" {:>9}", "MATCHES")
        } else {
            String::new()
        }
    );
    let mut lines = vec![Line::styled(
        head,
        Style::new()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )];
    let rows = inner.height as usize - 1;
    let top = if app.sel >= rows {
        app.sel + 1 - rows
    } else {
        0
    };
    for (row, &i) in app.order.iter().enumerate().skip(top).take(rows) {
        let s = &app.sources[i];
        let matches = if !grep {
            String::new()
        } else {
            let text = match app.counts[i] {
                _ if s.is_docker() => "-".to_string(),
                Count::Pending => "...".to_string(),
                Count::Done(n) => n.to_string(),
                Count::Failed => "error".to_string(),
            };
            format!(" {text:>9}")
        };
        let cell = fit_left(&s.name, name_w);
        let rest = format!(
            " {:<8} {:>10} {:<16} {:>5}{}",
            s.type_label(),
            s.size.map_or("-".into(), human),
            s.modified
                .map_or("-".into(), |t| clock::format(clock::secs(t))),
            age(s.modified),
            matches
        );
        let style = if row == app.sel {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if matches!(app.counts[i], Count::Done(n) if n > 0) && grep {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        };
        // Dim the folder part so file names in the same folder line up.
        let split = if s.is_docker_logs() {
            0
        } else {
            cell.trim_end().rfind(['/', '\\']).map_or(0, |i| i + 1)
        };
        let (dir, file) = cell.split_at(split);
        let dir_style = if row == app.sel {
            style
        } else {
            style.fg(Color::DarkGray)
        };
        let used = 1 + cell.chars().count() + rest.chars().count();
        let pad = (inner.width as usize).saturating_sub(used);
        lines.push(
            Line::from(vec![
                Span::styled(" ", style),
                Span::styled(dir.to_string(), dir_style),
                Span::styled(file.to_string(), style),
                Span::styled(format!("{rest}{}", " ".repeat(pad)), style),
            ])
            .style(style),
        );
    }
    if app.order.is_empty() {
        lines.push(Line::styled(
            " No log files match the search and time range. Esc clears them.",
            DIM,
        ));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_viewer(f: &mut Frame, area: Rect, v: &Viewer) {
    let w = &v.win;
    let pos = match (w.lines.get(v.top), v.src.size, w.relative) {
        (Some(l), Some(size), false) if size > 0 && v.src.plain_path().is_some() => {
            format!(
                " {} / {} ({}%) ",
                human(l.off),
                human(size),
                (l.off.min(size) * 100) / size
            )
        }
        (Some(l), _, false) => format!(" at {} ", human(l.off)),
        _ => String::new(),
    };
    let col = if v.hscroll > 0 {
        format!(" col {} ", v.hscroll + 1)
    } else {
        String::new()
    };
    let block = frame_block(format!(" {} ", v.src.type_label()), format!("{col}{pos}"));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = inner.width as usize;
    let highlight = v.filter.as_ref().and_then(|f| f.highlight.as_ref());
    let mut lines: Vec<Line> = w
        .lines
        .iter()
        .skip(v.top)
        .take(inner.height as usize)
        .map(|l| render_line(&l.data, v.hscroll, width, highlight))
        .collect();
    if lines.is_empty() && v.loading.is_none() {
        let msg = if v.filter.is_some() {
            if v.live {
                " No matching lines yet. Waiting for new ones..."
            } else {
                " No matching lines."
            }
        } else {
            " (empty)"
        };
        lines.push(Line::styled(msg, DIM));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App, viewer: Option<&Viewer>) {
    let mut left = Vec::new();
    let tip = |t: &str| Span::styled(format!(" {t}"), Style::new().fg(Color::Gray));
    match viewer {
        Some(v) => {
            if let Some(job) = &v.loading {
                let done = job.progress.load(Ordering::Relaxed);
                left.push(badge("SCANNING", Color::LightMagenta));
                left.push(tip(&format!("{} read  \u{b7}  Esc cancels", human(done))));
            } else {
                if v.live {
                    left.push(badge("LIVE", Color::LightGreen));
                } else if v.src.can_live() {
                    left.push(badge("PAUSED", Color::Yellow));
                } else {
                    left.push(badge("STATIC", Color::Gray));
                }
                if !v.notice.is_empty() {
                    left.push(Span::styled(
                        format!(" {}", v.notice),
                        Style::new().fg(Color::LightCyan),
                    ));
                } else if v.filter.is_some() {
                    left.push(tip(
                        "only matching lines are shown  \u{b7}  Esc clears the filter",
                    ));
                } else if v.live {
                    left.push(tip("type to filter  \u{b7}  \u{2191} pauses to scroll  \u{b7}  Esc back to list"));
                } else if v.src.can_live() {
                    left.push(tip("Enter or End resumes live  \u{b7}  type to filter  \u{b7}  Esc back to list"));
                } else {
                    left.push(tip(
                        "type to filter  \u{b7}  Home/End jump  \u{b7}  Esc back to list",
                    ));
                }
            }
        }
        None => {
            if !app.message.is_empty() {
                left.push(badge("NOTE", Color::LightRed));
                left.push(Span::styled(
                    format!(" {}", app.message),
                    Style::new().fg(Color::LightRed),
                ));
            } else {
                left.push(badge("FILES", Color::Cyan));
                left.push(tip(match app.focus {
                    Focus::Search => {
                        "\u{2191}\u{2193} select  \u{b7}  Enter open  \u{b7}  type to search  \u{b7}  \u{2190}\u{2192} time range  \u{b7}  Tab dates"
                    }
                    _ => "type a date: 2026-11-22 13:00  \u{b7}  Tab next field  \u{b7}  Esc back to search",
                }));
            }
        }
    }
    let right = vec![
        Span::styled(" ^G ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::styled(" Help ", Style::new().fg(Color::Gray)),
        Span::styled(" ^X ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::styled(" Exit ", Style::new().fg(Color::Gray)),
    ];
    two_sided(f, area, left, right);
}

fn draw_help(f: &mut Frame, area: Rect, in_viewer: bool) {
    let key = Style::new()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let head = Style::new()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let row = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<16}"), key),
            Span::raw(d.to_string()),
        ])
    };
    let mut lines = vec![
        Line::styled(
            format!("axtail {VERSION}  \u{b7}  advanced tail for sysadmins"),
            Style::new()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "Read-only. Bounded memory: logs are never loaded whole.",
            DIM,
        ),
        Line::raw(""),
    ];
    let viewer = [
        Line::styled("Log viewer", head),
        row("type", "filter lines (applies as you type)"),
        row("  error timeout", "lines with error OR timeout"),
        row("  +db -health", "must contain db / must not contain health"),
        row(
            "  \"conn reset\"",
            "exact phrase      /regex/  regular expression",
        ),
        row("Esc", "clear filter, then back to the file list"),
        row("\u{2191} \u{2193} PgUp PgDn", "scroll (pauses live)"),
        row("Enter", "pause / resume live"),
        row("Home / End", "start / end (End resumes live)"),
        row("\u{2190} \u{2192}", "pan long lines"),
        row("Ctrl+U / Ctrl+W", "clear filter / delete last word"),
        row("Ctrl+L", "back to the file list"),
    ];
    let list = [
        Line::styled("File list", head),
        row("type", "search names (all words must match)"),
        row("\u{2191} \u{2193} Enter", "select and open a log"),
        row("\u{2190} \u{2192}", "time range: all, 15 min ... 30 days"),
        row("Tab", "type your own From / To date and time"),
        row("Ctrl+S", "sort: newest, name, largest"),
        row("Esc", "clear search, then time range"),
    ];
    if in_viewer {
        lines.extend(viewer);
        lines.push(Line::raw(""));
        lines.extend(list);
    } else {
        lines.extend(list);
        lines.push(Line::raw(""));
        lines.extend(viewer);
    }
    lines.push(Line::raw(""));
    lines.push(row("Ctrl+G / F1", "this help"));
    lines.push(row("Ctrl+X", "exit"));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "{}  \u{b7}  press any key to close",
            env!("CARGO_PKG_REPOSITORY")
        ),
        DIM,
    ));
    let w = 70.min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(Style::new().fg(Color::LightCyan))
                .title(" Help \u{b7} About "),
        ),
        rect,
    );
}
