//! Rendering: header, filter bar, bordered body and a one-line status bar.
//! The full key list lives in the help dialog (Ctrl+G) to save space.

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::app::{App, Count, FILES_TAB, Focus, PRESETS, Screen, Summary, Viewer};
use crate::clock;
use crate::summary::{Group, LEVEL_NAMES, Report};

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
pub fn sanitize(data: &[u8]) -> String {
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

/// Builds the spans for the byte range `lo..hi` of `text`, highlighting matches.
fn row_spans(
    text: &str,
    lo: usize,
    hi: usize,
    base: Style,
    hits: &[(usize, usize)],
) -> Line<'static> {
    let hit = Style::new().fg(Color::Black).bg(Color::Yellow);
    let mut spans = Vec::new();
    let mut pos = lo;
    for &(s, e) in hits {
        let (s, e) = (s.max(lo), e.min(hi));
        if s >= e || s < pos {
            continue;
        }
        if s > pos {
            spans.push(Span::styled(text[pos..s].to_string(), base));
        }
        spans.push(Span::styled(text[s..e].to_string(), hit));
        pos = e;
    }
    if pos < hi {
        spans.push(Span::styled(text[pos..hi].to_string(), base));
    }
    Line::from(spans).style(base)
}

fn matches(text: &str, highlight: Option<&regex::Regex>) -> Vec<(usize, usize)> {
    highlight.map_or_else(Vec::new, |re| {
        re.find_iter(text).map(|m| (m.start(), m.end())).collect()
    })
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
    row_spans(&text, lo, hi, base, &matches(&text, highlight))
}

/// Builds all the rows of one log line, broken at the terminal width.
fn wrap_line(data: &[u8], width: usize, highlight: Option<&regex::Regex>) -> Vec<Line<'static>> {
    let text = sanitize(data);
    let base = level_style(&text);
    let hits = matches(&text, highlight);
    let width = width.max(1);
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).step_by(width).collect();
    if starts.is_empty() {
        return vec![Line::raw("").style(base)];
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &lo)| {
            let hi = starts.get(n + 1).copied().unwrap_or(text.len());
            row_spans(&text, lo, hi, base, &hits)
        })
        .collect()
}

/// How many screen rows a log line takes when wrapped at `width` columns.
pub fn wrapped_rows(data: &[u8], width: usize) -> usize {
    sanitize(data).chars().count().div_ceil(width.max(1)).max(1)
}

/// Text columns inside the viewer's border.
pub fn body_cols(term_width: u16) -> usize {
    term_width.saturating_sub(2) as usize
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
    let summary = app
        .summary
        .as_ref()
        .filter(|_| app.screen == Screen::Summary);
    let subject = match (viewer, summary) {
        (Some(v), _) => v.src.name.clone(),
        (_, Some(_)) => format!("Summary of {}", app.title),
        _ => app.title.clone(),
    };
    f.render_widget(
        Paragraph::new(format!(" axtail {VERSION} - {subject}")).style(
            Style::new()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );
    match (viewer, summary) {
        (Some(v), _) => {
            draw_viewer_bar(f, bar, v);
            draw_viewer(f, body, v, app.wrap);
        }
        (_, Some(s)) => {
            draw_summary_bar(f, bar, app, s);
            match (&s.report, &s.detail) {
                (Some(r), Some(d)) => {
                    draw_detail(f, body, app, &r.groups[d.level][d.group], d.level, d.sel)
                }
                (Some(r), None) => draw_summary(f, body, app, s, r),
                (None, _) => draw_analysing(f, body, s),
            }
        }
        _ => {
            draw_list_bar(f, bar, app);
            draw_list(f, body, app);
        }
    }
    draw_status(f, status, app, viewer, summary);
    if app.help {
        draw_help(f, f.area(), app.screen);
    }
}

// ----- summary ---------------------------------------------------------------

/// One colour per summary tab: ERROR, WARN, INFO, DEBUG, FILES.
const TAB_COLORS: [Color; 5] = [
    Color::LightRed,
    Color::Yellow,
    Color::LightGreen,
    Color::LightBlue,
    Color::LightMagenta,
];
const PLACEHOLDER: Style = Style::new().fg(Color::LightMagenta);
const SELECTED: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Cyan)
    .add_modifier(Modifier::BOLD);
const TABLE_HEAD: Style = Style::new()
    .fg(Color::Cyan)
    .add_modifier(Modifier::BOLD.union(Modifier::UNDERLINED));

/// 1234567 -> "1,234,567".
fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn stamp(t: Option<i64>) -> String {
    t.map_or("-".into(), clock::format_secs)
}

/// "3d 4h", "2h 5m", "40s": how long a type has been occurring.
fn span_text(secs: i64) -> String {
    match secs {
        ..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        3600..86400 => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d {}h", secs / 86400, secs % 86400 / 3600),
    }
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 || part == 0 {
        return "0%".into();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p < 0.1 {
        "<0.1%".into()
    } else if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

/// A message type with its placeholders (<n>, <ip>, ...) in their own colour.
fn pattern_spans(text: &str, base: Style, mark: Style) -> Vec<Span<'static>> {
    const MARKS: [&str; 6] = ["<n>", "<ip>", "<uuid>", "<hex>", "<id>", "<*>"];
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next = MARKS
            .iter()
            .filter_map(|m| rest.find(m).map(|at| (at, m.len())))
            .min();
        match next {
            Some((at, len)) => {
                if at > 0 {
                    spans.push(Span::styled(rest[..at].to_string(), base));
                }
                spans.push(Span::styled(rest[at..at + len].to_string(), mark));
                rest = &rest[at + len..];
            }
            None => {
                spans.push(Span::styled(rest.to_string(), base));
                break;
            }
        }
    }
    spans
}

/// First visible row so that the selection stays on screen.
fn scroll_top(sel: usize, rows: usize) -> usize {
    (sel + 1).saturating_sub(rows)
}

fn draw_summary_bar(f: &mut Frame, area: Rect, app: &App, s: &Summary) {
    let preset = match app.preset {
        Some(i) => PRESETS[i].0,
        None => "Custom",
    };
    let time = [
        Span::styled("   Time ", LABEL),
        Span::styled(
            format!("\u{25c2} {preset} \u{25b8}"),
            Style::new().fg(Color::LightYellow),
        ),
    ];
    let mut left;
    let mut right = Vec::new();
    if let (Some(d), Some(r)) = (&s.detail, &s.report) {
        let color = TAB_COLORS[d.level];
        let at = s
            .rows
            .iter()
            .position(|&g| g == d.group)
            .map_or(0, |p| p + 1);
        left = vec![
            Span::styled(" Summary \u{25b8} ", LABEL),
            Span::styled(
                LEVEL_NAMES[d.level],
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" \u{25b8} ", LABEL),
            Span::raw(format!("type {at} of {}", s.rows.len())),
        ];
        left.extend(time);
        right.push(Span::styled(
            format!("{} occurrences ", commas(r.groups[d.level][d.group].count)),
            Style::new().fg(color),
        ));
    } else {
        left = vec![
            Span::styled(" Find \u{25b8} ", LABEL),
            Span::raw(s.query.clone()),
            Span::raw(CURSOR),
        ];
        if s.query.is_empty() {
            left.push(Span::styled(" type to search", DIM));
        }
        left.extend(time);
        if let Some(r) = &s.report {
            right.push(Span::styled(
                format!("{} read in {:.2}s ", human(r.bytes), r.secs),
                DIM,
            ));
        }
    }
    two_sided(f, area, left, right);
}

fn draw_analysing(f: &mut Frame, area: Rect, s: &Summary) {
    let block = frame_block(" Summary ".into(), String::new());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = match &s.job {
        Some(job) => vec![
            Line::raw(""),
            Line::from(vec![
                Span::raw("  "),
                badge("ANALYSING", Color::LightMagenta),
                Span::styled(
                    format!(
                        "  {} of {} files",
                        s.done.load(Ordering::Relaxed).min(s.total as u64),
                        s.total
                    ),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  \u{b7}  {} read",
                        human(job.progress.load(Ordering::Relaxed))
                    ),
                    Style::new().fg(Color::LightCyan),
                ),
            ]),
            Line::raw(""),
            Line::styled(
                "  Counting lines per level and grouping messages into types...",
                DIM,
            ),
            Line::styled("  Esc cancels.", DIM),
        ],
        None => vec![
            Line::raw(""),
            Line::styled("  The analysis was cancelled. Ctrl+R runs it again.", DIM),
        ],
    };
    f.render_widget(Paragraph::new(lines), inner);
}

/// The five tiles at the top: one per level plus the files.
fn draw_tiles(f: &mut Frame, area: Rect, s: &Summary, r: &Report) {
    let tiles: [Rect; 5] = Layout::horizontal([Constraint::Ratio(1, 5); 5]).areas(area);
    let entries: u64 = r.totals.iter().sum();
    for (i, rect) in tiles.into_iter().enumerate() {
        let color = TAB_COLORS[i];
        let on = s.tab == i;
        let name = if i == FILES_TAB {
            "FILES"
        } else {
            LEVEL_NAMES[i]
        };
        let title = if on {
            badge(name, color)
        } else {
            Span::styled(
                format!(" {name} "),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            )
        };
        let block = Block::bordered()
            .border_type(if on {
                BorderType::Double
            } else {
                BorderType::Plain
            })
            .border_style(if on {
                Style::new().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(color)
            })
            .title(title);
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let big = Style::new().fg(color).add_modifier(Modifier::BOLD);
        let wide = inner.width >= 26;
        let lines = if i == FILES_TAB {
            let failed = r.files.iter().filter(|x| x.error.is_some()).count();
            vec![
                Line::styled(format!(" {}", commas(r.files.len() as u64)), big),
                Line::raw(format!(" {} lines", commas(r.lines))),
                if failed > 0 {
                    Line::styled(
                        format!(" {failed} unreadable"),
                        Style::new().fg(Color::LightRed),
                    )
                } else {
                    Line::styled(format!(" {} no level", commas(r.totals[4])), DIM)
                },
            ]
        } else {
            let types = r.groups[i].len();
            let kinds = format!(
                "{}{} type{}",
                types,
                if r.capped[i] { "+" } else { "" },
                if types == 1 { "" } else { "s" }
            );
            let share = percent(r.totals[i], entries);
            let big = Line::styled(format!(" {}", commas(r.totals[i])), big);
            if wide {
                let last = r.level_last[i]
                    .map_or(" -".into(), |t| format!(" last {}", clock::format_secs(t)));
                vec![
                    big,
                    Line::raw(format!(" {share}  \u{b7}  {kinds}")),
                    Line::styled(last, DIM),
                ]
            } else {
                vec![
                    big,
                    Line::raw(format!(" {share} of lines")),
                    Line::styled(format!(" {kinds}"), DIM),
                ]
            }
        };
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn draw_summary(f: &mut Frame, area: Rect, app: &App, s: &Summary, r: &Report) {
    let tiles_h = if area.height >= 11 { 5 } else { 0 };
    let [tiles, table] =
        Layout::vertical([Constraint::Length(tiles_h), Constraint::Min(3)]).areas(area);
    if tiles_h > 0 {
        draw_tiles(f, tiles, s, r);
    }
    let color = TAB_COLORS[s.tab];
    let files = s.tab == FILES_TAB;
    let title = if files {
        format!(" FILES \u{b7} {} analysed ", s.rows.len())
    } else {
        format!(
            " {} \u{b7} {} message type{} ",
            LEVEL_NAMES[s.tab],
            s.rows.len(),
            if s.rows.len() == 1 { "" } else { "s" }
        )
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(color))
        .title(Span::styled(
            title,
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ))
        .title(
            Line::from(Span::styled(
                if files {
                    " most errors first ".to_string()
                } else {
                    format!(" sort: {} ", s.sort.label())
                },
                Style::new().fg(Color::Gray),
            ))
            .right_aligned(),
        );
    let inner = block.inner(table);
    f.render_widget(block, table);
    if inner.height < 2 {
        return;
    }
    let width = inner.width as usize;
    let rows = inner.height as usize - 1;
    let sel = s.sel[s.tab];
    let top = scroll_top(sel, rows);
    let mut lines = Vec::with_capacity(rows + 1);
    if files {
        let name_w = width.saturating_sub(1 + 6 * 12).clamp(12, 60);
        lines.push(Line::styled(
            format!(
                " {:<name_w$} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11}",
                "NAME", "LINES", "ERROR", "WARN", "INFO", "DEBUG", "NO LEVEL"
            ),
            TABLE_HEAD,
        ));
        for (row, &i) in s.rows.iter().enumerate().skip(top).take(rows) {
            let st = &r.files[i];
            let on = row == sel;
            let pick = |style: Style| if on { SELECTED } else { style };
            let mut spans = vec![Span::styled(
                format!(" {}", fit_left(&app.sources[st.src].name, name_w)),
                pick(Style::new()),
            )];
            if let Some(e) = &st.error {
                spans.push(Span::styled(
                    format!("  cannot read: {e}"),
                    pick(Style::new().fg(Color::LightRed)),
                ));
            } else {
                spans.push(Span::styled(
                    format!(" {:>11}", commas(st.lines)),
                    pick(Style::new().fg(Color::White)),
                ));
                for (lv, &n) in st.counts.iter().enumerate() {
                    let style = match (n, lv) {
                        (0, _) => DIM,
                        (_, 4) => Style::new().fg(Color::Gray),
                        _ => Style::new().fg(TAB_COLORS[lv]),
                    };
                    spans.push(Span::styled(format!(" {:>11}", commas(n)), pick(style)));
                }
            }
            let line = Line::from(spans);
            lines.push(if on { pad_selected(line, width) } else { line });
        }
    } else {
        let groups = &r.groups[s.tab];
        let (show_first, show_files) = (width >= 100, width >= 76);
        let mut head = format!(" {:>10}  {:<19}", "COUNT", "LAST SEEN");
        if show_first {
            head.push_str(&format!("  {:<19}", "FIRST SEEN"));
        }
        if show_files {
            head.push_str(&format!("  {:>5}", "FILES"));
        }
        head.push_str("  MESSAGE TYPE");
        lines.push(Line::styled(head, TABLE_HEAD));
        for (row, &i) in s.rows.iter().enumerate().skip(top).take(rows) {
            let g = &groups[i];
            let on = row == sel;
            let pick = |style: Style| if on { SELECTED } else { style };
            let mut spans = vec![
                Span::styled(
                    format!(" {:>10}", commas(g.count)),
                    pick(Style::new().fg(color).add_modifier(Modifier::BOLD)),
                ),
                Span::styled(
                    format!("  {:<19}", stamp(g.last)),
                    pick(Style::new().fg(Color::White)),
                ),
            ];
            if show_first {
                spans.push(Span::styled(format!("  {:<19}", stamp(g.first)), pick(DIM)));
            }
            if show_files {
                spans.push(Span::styled(
                    format!("  {:>5}", g.files),
                    pick(Style::new().fg(Color::LightCyan)),
                ));
            }
            spans.push(Span::styled("  ", pick(Style::new())));
            spans.extend(pattern_spans(
                &g.pattern,
                pick(Style::new()),
                pick(PLACEHOLDER),
            ));
            let line = Line::from(spans);
            lines.push(if on { pad_selected(line, width) } else { line });
        }
    }
    if s.rows.is_empty() {
        let (text, style) = if !s.query.is_empty() {
            (
                " Nothing matches the search. Esc clears it.".to_string(),
                DIM,
            )
        } else if s.tab == 0 {
            (
                " No ERROR lines found. \u{2714}".to_string(),
                Style::new().fg(Color::LightGreen),
            )
        } else if files {
            (" No files to analyse.".to_string(), DIM)
        } else {
            (format!(" No {} lines found.", LEVEL_NAMES[s.tab]), DIM)
        };
        lines.push(Line::styled(text, style));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Extends the highlight of a selected row to the full width.
fn pad_selected(mut line: Line<'static>, width: usize) -> Line<'static> {
    let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    line.spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        SELECTED,
    ));
    line
}

/// One message type: what it is, when it happened and every kept occurrence.
fn draw_detail(f: &mut Frame, area: Rect, app: &App, g: &Group, level: usize, sel: usize) {
    let color = TAB_COLORS[level];
    let key = |k: &str| Span::styled(format!("  {k:<10}"), LABEL);
    // Newest first on screen.
    let shown = g.occ.len();
    let picked = shown.checked_sub(1 + sel).and_then(|i| g.occ.get(i));
    let mut info = vec![
        Line::from(
            [key("Type")]
                .into_iter()
                .chain(pattern_spans(
                    &g.pattern,
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                    PLACEHOLDER,
                ))
                .collect::<Vec<_>>(),
        ),
        Line::from(vec![
            key("Count"),
            Span::styled(
                commas(g.count),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " in {} file{}",
                g.files,
                if g.files == 1 { "" } else { "s" }
            )),
        ]),
        Line::from(vec![
            key("First"),
            Span::raw(stamp(g.first)),
            Span::styled("    Last  ", LABEL),
            Span::raw(stamp(g.last)),
            Span::styled("    Span  ", LABEL),
            Span::raw(match (g.first, g.last) {
                (Some(a), Some(b)) => span_text(b - a),
                _ => "-".into(),
            }),
        ]),
    ];
    if let Some(o) = picked {
        info.push(Line::from(vec![
            key("Message"),
            Span::styled(o.text.to_string(), Style::new().fg(color)),
        ]));
    }
    if !g.trace.is_empty() {
        info.push(Line::styled("  Stack trace of the first occurrence", LABEL));
        info.extend(g.trace.iter().map(|t| {
            Line::styled(
                format!("    {}", t.trim_start()),
                Style::new().fg(Color::Gray),
            )
        }));
    }
    let inner_w = area.width.saturating_sub(2).max(20) as usize;
    let need: usize = info
        .iter()
        .map(|l| l.width().div_ceil(inner_w).max(1))
        .sum();
    let top_h = (need as u16 + 2).min(area.height.saturating_sub(6)).max(3);
    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_h), Constraint::Min(3)]).areas(area);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(color))
        .title(badge(&format!("{} type", LEVEL_NAMES[level]), color));
    let inner = block.inner(top);
    f.render_widget(block, top);
    f.render_widget(Paragraph::new(info).wrap(Wrap { trim: false }), inner);

    let kept = if (shown as u64) < g.count {
        format!(" latest {} of {} ", commas(shown as u64), commas(g.count))
    } else {
        String::new()
    };
    let block = frame_block(format!(" Occurrences ({}) ", commas(shown as u64)), kept);
    let inner = block.inner(bottom);
    f.render_widget(block, bottom);
    if inner.height < 2 {
        return;
    }
    let width = inner.width as usize;
    let name_w = (width / 4).clamp(12, 36);
    let rows = inner.height as usize - 1;
    let mut lines = vec![Line::styled(
        format!(
            " {:<19}  {:<name_w$}  {:>9}  MESSAGE",
            "TIMESTAMP", "FILE", "LINE"
        ),
        TABLE_HEAD,
    )];
    for (row, o) in g
        .occ
        .iter()
        .rev()
        .enumerate()
        .skip(scroll_top(sel, rows))
        .take(rows)
    {
        let on = row == sel;
        let pick = |style: Style| if on { SELECTED } else { style };
        let line = Line::from(vec![
            Span::styled(
                format!(" {:<19}", stamp(o.ts)),
                pick(Style::new().fg(Color::White)),
            ),
            Span::styled(
                format!("  {}", fit_left(&app.sources[o.src].name, name_w)),
                pick(Style::new().fg(Color::LightCyan)),
            ),
            Span::styled(format!("  {:>9}", commas(o.line)), pick(DIM)),
            Span::styled(format!("  {}", o.text), pick(Style::new().fg(color))),
        ]);
        lines.push(if on { pad_selected(line, width) } else { line });
    }
    f.render_widget(Paragraph::new(lines), inner);
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
        format!("{} of {} files ", app.order.len(), app.sources.len()),
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

fn draw_viewer(f: &mut Frame, area: Rect, v: &Viewer, wrap: bool) {
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
    let col = if v.hscroll > 0 && !wrap {
        format!(" col {} ", v.hscroll + 1)
    } else {
        String::new()
    };
    let block = frame_block(format!(" {} ", v.src.type_label()), format!("{col}{pos}"));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = inner.width as usize;
    let highlight = v.filter.as_ref().and_then(|f| f.highlight.as_ref());
    let height = inner.height as usize;
    // The line the summary jumped to stands out.
    let marked = |off: u64, mut line: Line<'static>| {
        if v.mark == Some(off) {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
        line
    };
    let mut lines: Vec<Line> = if !wrap {
        w.lines
            .iter()
            .skip(v.top)
            .take(height)
            .map(|l| marked(l.off, render_line(&l.data, v.hscroll, width, highlight)))
            .collect()
    } else if v.pinned {
        // At the end: fill from the bottom so the newest line is always whole.
        let mut rows: Vec<Line> = Vec::new();
        for l in w.lines.iter().rev() {
            if rows.len() >= height {
                break;
            }
            let part = wrap_line(&l.data, width, highlight);
            rows.extend(part.into_iter().rev().map(|p| marked(l.off, p)));
        }
        rows.truncate(height);
        rows.reverse();
        rows
    } else {
        let mut rows: Vec<Line> = Vec::new();
        for l in w.lines.iter().skip(v.top) {
            if rows.len() >= height {
                break;
            }
            let part = wrap_line(&l.data, width, highlight);
            rows.extend(part.into_iter().map(|p| marked(l.off, p)));
        }
        rows.truncate(height);
        rows
    };
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

fn draw_status(
    f: &mut Frame,
    area: Rect,
    app: &App,
    viewer: Option<&Viewer>,
    summary: Option<&Summary>,
) {
    let mut left = Vec::new();
    let tip = |t: &str| Span::styled(format!(" {t}"), Style::new().fg(Color::Gray));
    match (viewer, summary) {
        (None, Some(s)) => {
            if let Some(job) = &s.job {
                left.push(badge("ANALYSING", Color::LightMagenta));
                left.push(tip(&format!(
                    "{} of {} files  \u{b7}  {} read  \u{b7}  Esc cancels",
                    s.done.load(Ordering::Relaxed).min(s.total as u64),
                    s.total,
                    human(job.progress.load(Ordering::Relaxed))
                )));
            } else if let Some(d) = &s.detail {
                left.push(badge("DETAILS", TAB_COLORS[d.level]));
                left.push(tip("\u{2191}\u{2193} select  \u{b7}  Enter open the log at that line  \u{b7}  \u{2190}\u{2192} other types  \u{b7}  Esc back"));
            } else {
                left.push(badge("SUMMARY", Color::LightMagenta));
                left.push(tip(if s.tab == FILES_TAB {
                    "Tab next view  \u{b7}  \u{2191}\u{2193} select  \u{b7}  Enter open the log  \u{b7}  \u{2190}\u{2192} time range"
                } else {
                    "Tab next level  \u{b7}  \u{2191}\u{2193} select  \u{b7}  Enter details  \u{b7}  \u{2190}\u{2192} time range"
                }));
            }
        }
        (Some(v), _) => {
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
        (None, None) => {
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
    let mut right = Vec::new();
    let hot = Style::new().fg(Color::Black).bg(Color::LightMagenta);
    if summary.is_some_and(|s| s.detail.is_none() && s.tab != FILES_TAB) {
        right.push(Span::styled(" ^S ", hot));
        right.push(Span::styled(" Sort ", Style::new().fg(Color::LightMagenta)));
    } else if summary.is_none() {
        right.push(Span::styled(" ^D ", hot));
        right.push(Span::styled(
            " Summary ",
            Style::new().fg(Color::LightMagenta),
        ));
    }
    if viewer.is_some() {
        right.push(Span::styled(
            " ^R ",
            Style::new().fg(Color::Black).bg(Color::Gray),
        ));
        right.push(Span::styled(
            if app.wrap { " Wrap " } else { " No wrap " },
            Style::new().fg(Color::LightCyan),
        ));
        right.push(Span::raw(" "));
    }
    right.extend([
        Span::styled("\u{25f7} ", Style::new().fg(Color::Gray)),
        Span::styled(
            format!("{} ", clock::zone_label()),
            Style::new().fg(Color::LightCyan),
        ),
        Span::styled(" ^G ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::styled(" Help ", Style::new().fg(Color::Gray)),
        Span::styled(" ^X ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::styled(" Exit ", Style::new().fg(Color::Gray)),
    ]);
    two_sided(f, area, left, right);
}

fn draw_help(f: &mut Frame, area: Rect, screen: Screen) {
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
    let lines = vec![
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
    let viewer = vec![
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
        row("Ctrl+R", "wrap long lines on / off (default: wrap)"),
        row("\u{2190} \u{2192}", "pan long lines (when wrap is off)"),
        row("Ctrl+U / Ctrl+W", "clear filter / delete last word"),
        row("Ctrl+L", "back to the file list"),
    ];
    let summary = vec![
        Line::styled("Summary", head),
        row("Ctrl+D / F2", "analyse the listed logs (from any screen)"),
        row("Tab", "next view: ERROR, WARN, INFO, DEBUG, FILES"),
        row(
            "\u{2191} \u{2193} Enter",
            "select a message type, open its details",
        ),
        row("  in details", "Enter opens the log at that line"),
        row("type", "search message types or file names"),
        row("\u{2190} \u{2192}", "time range (analyses again)"),
        row("Ctrl+S / Ctrl+R", "sort types / analyse again"),
        row("Esc", "back"),
    ];
    let list = vec![
        Line::styled("File list", head),
        row("type", "search names (all words must match)"),
        row("\u{2191} \u{2193} Enter", "select and open a log"),
        row("\u{2190} \u{2192}", "time range: all, 15 min ... 30 days"),
        row("Tab", "type your own From / To date and time"),
        row("Ctrl+S", "sort: newest, name, largest"),
        row("Ctrl+D / F2", "summary of the listed logs"),
        row("Esc", "clear search, then time range"),
    ];
    let how = match clock::zone_source() {
        "system" => "detected from this machine",
        "--tz" => "set with --tz",
        _ => "set with AXTAIL_TZ",
    };
    let zone = vec![
        Line::styled("Time zone", head),
        Line::from(vec![
            Span::styled(format!("  {}", clock::zone_label()), key),
            Span::styled(format!("  {how}"), DIM),
        ]),
        row("", "used for MODIFIED, From and To"),
        row("--tz UTC", "start with another zone (UTC, local, +08:00)"),
    ];
    let general = vec![row("Ctrl+G / F1", "this help"), row("Ctrl+X", "exit")];
    let (first, second, third): (&[Line], &[Line], &[Line]) = match screen {
        Screen::Viewer => (&viewer, &summary, &list),
        Screen::Summary => (&summary, &list, &viewer),
        Screen::List => (&list, &summary, &viewer),
    };
    let footer = Line::styled(
        format!(
            "{}  \u{b7}  press any key to close",
            env!("CARGO_PKG_REPOSITORY")
        ),
        DIM,
    );
    // Two columns when the terminal is wide enough, so it fits short screens.
    let wide = area.width >= 140;
    let mut left = lines.clone();
    let mut right: Vec<Line> = Vec::new();
    left.extend(first.iter().cloned());
    left.push(Line::raw(""));
    left.extend(second.iter().cloned());
    left.push(Line::raw(""));
    if wide {
        right.extend(third.iter().cloned());
        right.push(Line::raw(""));
        right.extend(zone);
        right.push(Line::raw(""));
        right.extend(general);
    } else {
        left.extend(third.iter().cloned());
        left.push(Line::raw(""));
        left.extend(zone);
        left.push(Line::raw(""));
        left.extend(general);
    }
    left.push(Line::raw(""));
    left.push(footer);
    let col_w = 66u16;
    let w = if wide { 2 * col_w + 4 } else { col_w + 4 }.min(area.width);
    let h = (left.len().max(right.len()) as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::LightCyan))
        .title(" Help \u{b7} About ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if wide {
        let [l, r] =
            Layout::horizontal([Constraint::Length(col_w + 1), Constraint::Min(0)]).areas(inner);
        f.render_widget(Paragraph::new(left), l);
        f.render_widget(Paragraph::new(right), r);
    } else {
        f.render_widget(Paragraph::new(left), inner);
    }
}
