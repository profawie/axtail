//! Rendering: header, bordered body, status line and a nano-style key bar.

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Count, PromptKind, Screen, Sort, Viewer};
use crate::search::Matcher;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Rows available for content: total minus header, borders, status, key bar.
pub fn body_rows(term_height: u16) -> usize {
    (term_height as usize).saturating_sub(6).max(1)
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
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
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
    matcher: Option<&Matcher>,
    current: bool,
) -> Line<'static> {
    let text = sanitize(data);
    let mut base = level_style(&text);
    if current {
        base = base.bg(Color::Indexed(236));
    }
    // Byte range of the visible columns.
    let mut idx = text.char_indices().map(|(i, _)| i).skip(hscroll);
    let lo = idx.next().unwrap_or(text.len());
    let hi = idx.nth(width.saturating_sub(1)).unwrap_or(text.len());
    let hit = Style::new().fg(Color::Black).bg(if current {
        Color::LightYellow
    } else {
        Color::Yellow
    });
    let mut spans = Vec::new();
    let mut pos = lo;
    if let Some(m) = matcher {
        for found in m.text.find_iter(&text) {
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

pub fn draw(f: &mut Frame, app: &App) {
    let [header, body, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(2),
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
        Some(v) => draw_viewer(f, body, v),
        None => draw_list(f, body, app),
    }
    draw_status(f, status, app, viewer);
    draw_keys(f, keys, app, viewer);
    if app.about {
        draw_about(f, f.area());
    }
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
    let shown = app.order.len();
    let total = app.sources.len();
    let title = if shown == total {
        format!(" Log files ({total}) ")
    } else {
        format!(
            " Log files ({shown} of {total}, filter: {}) ",
            app.name_filter
        )
    };
    let block = frame_block(title, format!(" sort: {} ", app.sort.label()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let grep = app.grep.is_some();
    let fixed = 8 + 11 + 10 + if grep { 10 } else { 0 } + 4;
    let name_w = (inner.width as usize).saturating_sub(fixed).max(8);
    let head = format!(
        " {:<name_w$} {:<8} {:>10} {:>9}{}",
        "NAME",
        "TYPE",
        "SIZE",
        "MODIFIED",
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
    let top = app.sel.saturating_sub(rows.saturating_sub(1)).min(app.sel);
    let top = if app.sel >= rows { top } else { 0 };
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
            " {:<8} {:>10} {:>9}{}",
            s.type_label(),
            s.size.map_or("-".into(), human),
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
            " No log files match.",
            Style::new().fg(Color::DarkGray),
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
    let lines: Vec<Line> = w
        .lines
        .iter()
        .skip(v.top)
        .take(inner.height as usize)
        .map(|l| {
            render_line(
                &l.data,
                v.hscroll,
                width,
                v.matcher.as_deref(),
                v.cur_match == Some(l.off),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App, viewer: Option<&Viewer>) {
    let dim = Style::new().fg(Color::Gray);
    let mut spans = Vec::new();
    if let Some(p) = &app.prompt {
        let label = match p.kind {
            PromptKind::Search => format!("Search [{}]", p.mode.label()),
            PromptKind::ListFilter => "Filter by name".to_string(),
        };
        spans.push(badge(&label, Color::LightMagenta));
        spans.push(Span::raw(format!(" {}", p.text)));
        spans.push(Span::styled(
            "_",
            Style::new().add_modifier(Modifier::SLOW_BLINK),
        ));
        if let Some(v) = viewer.filter(|v| !v.notice.is_empty()) {
            spans.push(Span::styled(
                format!("   {}", v.notice),
                Style::new().fg(Color::LightRed),
            ));
        }
    } else if let Some(v) = viewer {
        let job = v.searching.as_ref().or(v.loading.as_ref());
        if let Some(job) = job {
            let done = job.progress.load(Ordering::Relaxed);
            spans.push(badge(&job.label.to_uppercase(), Color::LightMagenta));
            spans.push(Span::styled(
                format!(" {} scanned, Esc to cancel", human(done)),
                dim,
            ));
        } else if v.live {
            spans.push(badge("LIVE", Color::LightGreen));
            spans.push(Span::styled(" streaming in real time", dim));
        } else if v.src.can_live() {
            spans.push(badge("PAUSED", Color::Yellow));
            spans.push(Span::styled(" Enter resumes live", dim));
        } else {
            spans.push(badge("STATIC", Color::Gray));
            spans.push(Span::styled(" compressed log", dim));
        }
        if let Some(m) = &v.matcher {
            spans.push(Span::styled(
                format!("  search: \"{}\" [{}]", m.query, m.mode.label()),
                Style::new().fg(Color::LightYellow),
            ));
        }
        if !v.notice.is_empty() {
            spans.push(Span::styled(
                format!("  {}", v.notice),
                Style::new().fg(Color::LightCyan),
            ));
        }
    } else {
        spans.push(badge("LIST", Color::Cyan));
        if let Some(m) = &app.grep {
            let pending = app
                .counts
                .iter()
                .filter(|c| matches!(c, Count::Pending))
                .count();
            let state = if pending > 0 && app.sort != Sort::Matches {
                ", counting..."
            } else {
                ""
            };
            spans.push(Span::styled(
                format!(" grep: \"{}\" [{}]{state}", m.query, m.mode.label()),
                Style::new().fg(Color::LightYellow),
            ));
        }
        if !app.message.is_empty() {
            spans.push(Span::styled(
                format!("  {}", app.message),
                Style::new().fg(Color::LightRed),
            ));
        } else {
            spans.push(Span::styled(" select a log and press Enter", dim));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_keys(f: &mut Frame, area: Rect, app: &App, viewer: Option<&Viewer>) {
    let keys: &[(&str, &str)] = if let Some(p) = &app.prompt {
        match p.kind {
            PromptKind::Search => &[
                ("Enter", "Search"),
                ("Tab", "Change Mode"),
                ("Esc", "Cancel"),
                ("^X", "Exit"),
            ],
            PromptKind::ListFilter => &[("Enter", "Apply"), ("Esc", "Clear"), ("^X", "Exit")],
        }
    } else if viewer.is_some() {
        &[
            ("^F", "Search"),
            ("^P", "Older Match"),
            ("^N", "Newer Match"),
            ("Enter", "Live/Pause"),
            ("^L", "File List"),
            ("Up/Dn", "Scroll"),
            ("Lt/Rt", "Pan"),
            ("Home/End", "Top/Bottom"),
            ("^G", "About"),
            ("^X", "Exit"),
        ]
    } else {
        &[
            ("Enter", "Open Log"),
            ("Up/Dn", "Select"),
            ("Lt/Rt", "Sort"),
            ("^F", "Filter Names"),
            ("^G", "About"),
            ("^X", "Exit"),
        ]
    };
    let per_row = keys.len().div_ceil(2).max(1);
    let cell = (area.width as usize / per_row).max(1);
    let key_style = Style::new()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let lines: Vec<Line> = keys
        .chunks(per_row)
        .map(|row| {
            let mut spans = Vec::new();
            for (k, label) in row {
                let pad = cell.saturating_sub(k.len() + 1);
                spans.push(Span::styled(k.to_string(), key_style));
                spans.push(Span::raw(format!(
                    " {:<pad$}",
                    label.chars().take(pad).collect::<String>()
                )));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_about(f: &mut Frame, area: Rect) {
    let text = [
        "",
        &format!("axtail {VERSION}"),
        "advanced tail for sysadmins",
        "",
        "Find the root cause in plain, gzip, zip",
        "and Docker logs, straight from a terminal.",
        "",
        "Read-only: axtail never writes to a log.",
        "Bounded memory: files are never loaded whole.",
        "",
        "Written in Rust.",
        env!("CARGO_PKG_REPOSITORY"),
        "",
        "Press any key to close",
    ];
    let (w, h) = (52.min(area.width), (text.len() as u16 + 2).min(area.height));
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let lines: Vec<Line> = text
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let style = match i {
                1 => Style::new()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
                13 => Style::new().fg(Color::DarkGray),
                _ => Style::new(),
            };
            Line::styled(t.to_string(), style).centered()
        })
        .collect();
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(Style::new().fg(Color::LightCyan))
                .title(" About "),
        ),
        rect,
    );
}
