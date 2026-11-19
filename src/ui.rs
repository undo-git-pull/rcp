//! Rendering. Everything here reads `App`; the only writes are scroll
//! offsets and page heights that depend on the terminal size.

use humansize::{BINARY, format_size};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::app::{App, JobStatus, Modal};
use crate::event::Side;
use crate::fs::CaseMode;
use crate::pane::Pane;
use crate::preview::{Body, Preview};
use crate::search::grep::{MAX_HITS, Row as GrepRow};
use crate::search::{Mode, SearchModal};

const ACCENT: Color = Color::Cyan;
const MATCH: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
/// Selected rows always set both colours, so the text contrasts with the
/// highlight whatever the terminal's default foreground is.
const SELECTED: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
const SELECTED_DIM: Style = Style::new().fg(Color::Black).bg(Color::Gray);
const MATCH_SELECTED: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Yellow)
    .add_modifier(Modifier::BOLD);

/// Match highlight that stays readable on both plain and selected rows.
fn match_style(base: Style) -> Style {
    base.patch(if base.bg.is_some() {
        MATCH_SELECTED
    } else {
        MATCH
    })
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, status, hints] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main);
    let active = app.active;
    app.preview_height = main.height.saturating_sub(2) as usize;

    for (side, area) in [(Side::Left, left), (Side::Right, right)] {
        if side != active
            && let Some(p) = &mut app.preview
        {
            draw_preview(f, area, p, Some("Preview"), false);
        } else {
            draw_pane(f, area, &mut app.panes[side.idx()], side == active);
        }
    }

    draw_status(f, status, app);
    f.render_widget(
        Paragraph::new(hint_line(&[
            ("Tab", "switch"),
            ("Enter", "open/preview"),
            ("Space", "copy"),
            ("d", "delete"),
            ("v", "mark"),
            ("/", "find"),
            ("^F", "grep"),
            ("?", "help"),
            ("q", "quit"),
        ])),
        hints,
    );

    if let Some(s) = &mut app.search {
        draw_search(f, s);
    }
    if let Some(m) = &app.modal {
        draw_modal(f, m, app);
    }
}

fn hint_line(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![];
    for (i, (k, v)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" · ").dark_gray());
        }
        spans.push(Span::raw(k.to_string()).fg(ACCENT));
        spans.push(Span::raw(format!(" {v}")));
    }
    Line::from(spans)
}

/// Keeps the end of `s` (the interesting part of a path) within `max` chars.
fn elide_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 2 {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

/// Fits `head + path + tail` in `max` chars by eliding the start of `path`
/// only, so labels like "Local: " survive long paths.
fn elide_path(head: &str, path: &str, tail: &str, max: usize) -> String {
    let fixed = head.chars().count() + tail.chars().count();
    if fixed + 2 > max {
        return elide_start(&format!("{head}{path}{tail}"), max);
    }
    format!("{head}{}{tail}", elide_start(path, max - fixed))
}

fn block(title: String, active: bool) -> Block<'static> {
    let style = if active {
        Style::new().fg(ACCENT)
    } else {
        Style::new().dark_gray()
    };
    Block::bordered()
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(style)
        .title(Span::styled(
            format!(" {title} "),
            style.add_modifier(Modifier::BOLD),
        ))
}

fn draw_pane(f: &mut Frame, area: Rect, pane: &mut Pane, active: bool) {
    let head = format!("{}: ", pane.backend.label());
    let mut tail = String::new();
    if pane.loading {
        tail.push_str(" (loading…)");
    }
    if !pane.marked.is_empty() {
        tail.push_str(&format!(" [{} marked]", pane.marked.len()));
    }
    let path = pane.cwd.display().to_string();
    let b = block(
        elide_path(&head, &path, &tail, area.width.saturating_sub(6) as usize),
        active,
    );
    let inner = b.inner(area);
    f.render_widget(b, area);

    if let Some(err) = &pane.error {
        f.render_widget(
            Paragraph::new(err.as_str())
                .red()
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let wide = inner.width >= 56;
    let rows = pane.items.iter().map(|e| {
        let marked = pane.marked.contains(&e.name);
        let mut name = e.name.clone();
        if e.is_dir() && e.name != ".." {
            name.push('/');
        }
        if e.is_symlink {
            name.push_str(" →");
        }
        let name_style = match (marked, e.is_dir()) {
            (true, _) => Style::new().fg(Color::Yellow).bold(),
            (false, true) => Style::new().fg(Color::Blue).bold(),
            _ => Style::new(),
        };
        let size = if e.is_dir() {
            "<dir>".to_string()
        } else {
            format_size(e.size, BINARY)
        };
        let mut cells = vec![
            Cell::from(Span::styled(
                format!("{}{name}", if marked { "*" } else { " " }),
                name_style,
            )),
            Cell::from(Line::from(size).right_aligned()),
        ];
        if wide {
            let t = e
                .modified
                .map(|m| {
                    chrono::DateTime::<chrono::Local>::from(m)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            cells.push(Cell::from(t).dark_gray());
        }
        Row::new(cells)
    });
    let mut widths = vec![Constraint::Fill(1), Constraint::Length(10)];
    if wide {
        widths.push(Constraint::Length(16));
    }
    let highlight = if active { SELECTED } else { SELECTED_DIM };
    let table = Table::new(rows, widths)
        .row_highlight_style(highlight)
        .column_spacing(1);
    let mut state = TableState::new()
        .with_offset(pane.offset)
        .with_selected(Some(pane.cursor));
    f.render_stateful_widget(table, inner, &mut state);
    pane.offset = state.offset();
}

/// Text with the given byte ranges highlighted.
fn highlighted(text: &str, ranges: &[std::ops::Range<usize>], base: Style) -> Vec<Span<'static>> {
    let mut spans = vec![];
    let mut pos = 0;
    for r in ranges {
        let (start, end) = (r.start.max(pos).min(text.len()), r.end.min(text.len()));
        if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        spans.push(Span::styled(text[pos..start].to_string(), base));
        spans.push(Span::styled(
            text[start..end].to_string(),
            match_style(base),
        ));
        pos = end;
    }
    spans.push(Span::styled(text[pos..].to_string(), base));
    spans
}

fn draw_preview(f: &mut Frame, area: Rect, p: &mut Preview, label: Option<&str>, focused: bool) {
    let title = match label {
        Some(l) if !p.title.is_empty() => format!("{l}: {}", p.title),
        Some(l) => l.to_string(),
        None => p.title.clone(),
    };
    let mut b = block(title, focused);
    if !p.info.is_empty() {
        b = b.title_bottom(Line::from(format!(" {} ", p.info)).right_aligned());
    }
    if p.truncated {
        b = b.title_bottom(Line::from(" (truncated) ").left_aligned());
    }
    let inner = b.inner(area);
    f.render_widget(b, area);
    let height = inner.height as usize;
    let scroll = p.scroll.unwrap_or_else(|| p.auto_scroll(height));

    let lines: Vec<Line> = match &p.body {
        Body::Message(m) => vec![Line::from(m.clone()).dark_gray()],
        Body::Hex(lines) | Body::Dir(lines) => lines
            .iter()
            .skip(scroll)
            .take(height)
            .map(|l| Line::from(l.clone()))
            .collect(),
        Body::Text {
            first_line,
            lines,
            numbered,
        } => {
            let width = (first_line + lines.len() as u64).to_string().len();
            lines
                .iter()
                .enumerate()
                .skip(scroll)
                .take(height)
                .map(|(i, l)| {
                    let n = first_line + i as u64;
                    let focus = p.focus.as_ref().filter(|(fl, _)| *fl == n);
                    let base = if focus.is_some() {
                        SELECTED_DIM
                    } else {
                        Style::new()
                    };
                    let mut spans = vec![];
                    if *numbered {
                        spans.push(Span::styled(
                            format!("{n:>width$} "),
                            Style::new().dark_gray(),
                        ));
                    }
                    match focus {
                        Some((_, ranges)) => spans.extend(highlighted(l, ranges, base)),
                        None => spans.push(Span::raw(l.clone())),
                    }
                    Line::from(spans).style(base)
                })
                .collect()
        }
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let line = match &app.job {
        Some(j) => progress_line(j, app.queued.len(), area.width),
        None => {
            let style = if app.status_error {
                Style::new().red()
            } else {
                Style::new()
            };
            Line::styled(app.status.clone(), style)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn progress_line(j: &JobStatus, queued: usize, width: u16) -> Line<'static> {
    let verb = if j.is_delete { "Deleting" } else { "Copying" };
    let mut spans = vec![Span::raw(format!("{verb} {} ", j.current)).fg(ACCENT)];
    if j.total > 0 {
        let pct = (j.done as f64 / j.total as f64).min(1.0);
        let bar_w = (width as usize / 4).clamp(10, 40);
        let filled = (pct * bar_w as f64) as usize;
        let secs = j.started.elapsed().as_secs_f64().max(0.001);
        spans.push(Span::raw("█".repeat(filled)).fg(ACCENT));
        spans.push(Span::raw("░".repeat(bar_w - filled)).dark_gray());
        spans.push(Span::raw(format!(
            " {:>3.0}%  {} / {}  {}/s",
            pct * 100.0,
            format_size(j.done, BINARY),
            format_size(j.total, BINARY),
            format_size((j.done as f64 / secs) as u64, BINARY)
        )));
    } else if j.done > 0 {
        spans.push(Span::raw(format!(" {} item(s)", j.done)));
    }
    if queued > 0 {
        spans.push(Span::raw(format!("  (+{queued} queued)")).dark_gray());
    }
    spans.push(Span::raw("  Esc cancel").dark_gray());
    Line::from(spans)
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn draw_modal(f: &mut Frame, m: &Modal, app: &App) {
    let (title, lines): (&str, Vec<Line>) = match m {
        Modal::ConfirmDelete {
            side,
            what,
            summary,
            ..
        } => (
            "Delete",
            vec![
                Line::from(format!(
                    "Delete {what} from {}?",
                    app.pane(*side).backend.label()
                ))
                .bold(),
                Line::from(summary.clone().unwrap_or_else(|| "counting…".into())).dark_gray(),
                Line::from(""),
                Line::from(vec![
                    Span::raw("y").red().bold(),
                    Span::raw(" delete   any other key: cancel"),
                ]),
            ],
        ),
        Modal::Conflict { path, is_dir, .. } => (
            "Already exists",
            vec![
                Line::from(path.clone()).bold(),
                Line::from(if *is_dir {
                    "This directory already exists."
                } else {
                    "This file already exists."
                }),
                Line::from(""),
                hint_line(&[
                    ("o", if *is_dir { "merge" } else { "overwrite" }),
                    ("s", "skip"),
                    ("r", "rename"),
                    ("a", "overwrite all"),
                    ("c", "cancel"),
                ]),
            ],
        ),
        Modal::ConfirmQuit => (
            "Quit",
            vec![
                Line::from("A transfer is running. Cancel it and quit?"),
                Line::from(""),
                Line::from("y / N"),
            ],
        ),
        Modal::ConfirmCopy { what, dest, .. } => (
            "Copy",
            vec![
                Line::from(format!("Copy {what} to {dest}?")).bold(),
                Line::from(""),
                Line::from(vec![
                    Span::raw("y").fg(ACCENT).bold(),
                    Span::raw(" copy   any other key: cancel"),
                ]),
            ],
        ),
        Modal::ConfirmCancel => (
            "Cancel",
            vec![
                Line::from("Cancel the running transfer?"),
                Line::from(""),
                Line::from("y / N"),
            ],
        ),
        Modal::Help => ("Help", help_lines()),
    };
    let w = lines.iter().map(|l| l.width()).max().unwrap_or(20).max(30) as u16 + 4;
    let area = centered(f.area(), w, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block(title.into(), true)),
        area,
    );
}

fn help_lines() -> Vec<Line<'static>> {
    let keys = [
        ("Tab", "switch pane"),
        ("↑↓ j k, PgUp PgDn, g G", "move"),
        ("→ l Enter", "enter directory"),
        ("← h Backspace", "parent directory"),
        ("Enter", "open directory / toggle preview"),
        ("Space", "copy to other pane (asks for directories)"),
        ("d", "delete (asks first)"),
        ("v", "mark / unmark"),
        ("/  Ctrl-P", "find files (fuzzy)"),
        ("Ctrl-F", "search text"),
        (".", "toggle hidden files"),
        ("s", "cycle sort: name, size, time"),
        ("r", "refresh / reconnect"),
        ("Esc", "close preview / cancel transfer"),
        ("q", "quit"),
        ("", ""),
        ("In search", ""),
        ("Enter", "jump to result"),
        ("Ctrl-Y", "copy result(s) to other pane"),
        ("Tab", "mark result"),
        ("Shift-↑↓", "scroll preview"),
        ("Alt-H / Alt-I", "hidden files / ignore rules"),
        ("Alt-R Alt-C Alt-W", "regex / case / word (text search)"),
        ("Alt-G", "edit glob filter (text search)"),
    ];
    keys.iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::from(k.to_string()).bold()
            } else {
                Line::from(vec![
                    Span::raw(format!("{k:>24}  ")).fg(ACCENT),
                    Span::raw(v.to_string()),
                ])
            }
        })
        .collect()
}

fn tag(on: bool, label: &str) -> Span<'static> {
    if on {
        Span::raw(format!("[{label}] ")).fg(ACCENT)
    } else {
        Span::raw(String::new())
    }
}

fn draw_search(f: &mut Frame, s: &mut SearchModal) {
    let full = f.area();
    let area = centered(full, full.width * 9 / 10, full.height * 9 / 10);
    f.render_widget(Clear, area);
    let verb = if s.is_finder() {
        "Find files in"
    } else {
        "Search text in"
    };
    let head = format!("{verb} {}:", s.backend.label());
    let b = block(
        elide_path(
            &head,
            &s.root.display().to_string(),
            "",
            area.width.saturating_sub(6) as usize,
        ),
        true,
    );
    let inner = b.inner(area);
    f.render_widget(b, area);

    let glob_line = match &s.mode {
        Mode::Grep(g) => g.glob_focus || !g.glob.text.is_empty(),
        Mode::Finder(_) => false,
    };
    let [query, glob, body, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(glob_line as u16),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Query line: input on the left, counts and option tags on the right.
    let mut right = vec![];
    match &s.mode {
        Mode::Finder(fd) => {
            right.push(Span::raw(format!("{} / {}", fd.matched(), fd.total())));
        }
        Mode::Grep(g) => {
            right.push(tag(g.regex, "regex"));
            right.push(tag(!g.regex, "fixed"));
            right.push(tag(
                true,
                match g.case {
                    CaseMode::Smart => "smart-case",
                    CaseMode::Sensitive => "case",
                    CaseMode::Insensitive => "ignore-case",
                },
            ));
            right.push(tag(g.whole_word, "word"));
            let n = g.results.count;
            right.push(Span::raw(if g.truncated {
                format!("{MAX_HITS}+ hits")
            } else {
                format!("{n} hits")
            }));
        }
    }
    right.insert(0, tag(s.hidden, "hidden"));
    right.insert(1, tag(!s.respect_ignore, "no-ignore"));
    right.push(
        Span::raw(if s.running {
            " · searching…"
        } else {
            " · done"
        })
        .dark_gray(),
    );
    let right = Line::from(right).right_aligned();
    let rw = right.width() as u16;
    let [q_area, r_area] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(rw)]).areas(query);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("> ").fg(ACCENT).bold(),
            Span::raw(s.input.text.clone()),
        ])),
        q_area,
    );
    f.render_widget(Paragraph::new(right), r_area);

    let mut cursor = Position::new(q_area.x + 2 + s.input.cursor as u16, q_area.y);
    if let Mode::Grep(g) = &s.mode
        && glob_line
    {
        let style = if g.glob_focus {
            Style::new().fg(ACCENT)
        } else {
            Style::new().dark_gray()
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("glob ", style),
                Span::raw(g.glob.text.clone()),
            ])),
            glob,
        );
        if g.glob_focus {
            cursor = Position::new(glob.x + 5 + g.glob.cursor as u16, glob.y);
        }
    }
    f.set_cursor_position(cursor);

    let [list_area, preview_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
    let lb = Block::bordered().border_style(Style::new().dark_gray());
    let list_inner = lb.inner(list_area);
    f.render_widget(lb, list_area);
    let h = list_inner.height as usize;

    let mut lines: Vec<Line> = vec![];
    if let Some(e) = &s.error {
        lines.push(Line::from(e.clone()).red());
    }
    let h = h.saturating_sub(lines.len());
    match &mut s.mode {
        Mode::Finder(fd) => {
            if s.cursor < s.offset {
                s.offset = s.cursor;
            } else if s.cursor >= s.offset + h {
                s.offset = s.cursor + 1 - h;
            }
            for (i, (path, idx)) in fd
                .visible(s.offset as u32, (s.offset + h) as u32)
                .into_iter()
                .enumerate()
            {
                let selected = s.offset + i == s.cursor;
                let base = if selected { SELECTED } else { Style::new() };
                let mark = if s.marked.contains(&path) { "* " } else { "  " };
                let mut spans = vec![Span::styled(
                    mark,
                    if selected {
                        base
                    } else {
                        base.fg(Color::Yellow)
                    },
                )];
                let mut hi = idx.iter().peekable();
                for (ci, ch) in path.chars().enumerate() {
                    let m = hi.peek().is_some_and(|&&x| x as usize == ci);
                    if m {
                        hi.next();
                    }
                    spans.push(Span::styled(
                        ch.to_string(),
                        if m { match_style(base) } else { base },
                    ));
                }
                lines.push(Line::from(spans).style(base));
            }
        }
        Mode::Grep(g) => {
            let r = &g.results;
            let cur_row = r.row_of(s.cursor);
            // Keep the file header visible when the first hit is selected.
            let want_top = cur_row.saturating_sub(1);
            if want_top < s.offset {
                s.offset = want_top;
            } else if cur_row >= s.offset + h {
                s.offset = cur_row + 1 - h;
            }
            for (i, row) in r.rows(s.offset, h).into_iter().enumerate() {
                let selected = r.count > 0 && s.offset + i == cur_row;
                let base = if selected { SELECTED } else { Style::new() };
                lines.push(match row {
                    GrepRow::File(path, n) => {
                        let mark = if s.marked.contains(path) { "* " } else { "" };
                        Line::from(vec![
                            Span::raw(format!("{mark}{path}")).fg(Color::Magenta).bold(),
                            Span::raw(format!(" ({n})")).dark_gray(),
                        ])
                    }
                    GrepRow::Hit(hit) => {
                        let mut spans = vec![Span::styled(
                            format!("{:>6}: ", hit.line_no),
                            if selected {
                                base
                            } else {
                                base.fg(Color::Green)
                            },
                        )];
                        spans.extend(highlighted(
                            hit.line.trim_start(),
                            &shift(&hit.line, &hit.ranges),
                            base,
                        ));
                        Line::from(spans).style(base)
                    }
                });
            }
        }
    }
    f.render_widget(Paragraph::new(lines), list_inner);

    s.preview_height = preview_area.height.saturating_sub(2) as usize;
    match &mut s.preview {
        Some(p) => draw_preview(f, preview_area, p, None, false),
        None => f.render_widget(
            Block::bordered().border_style(Style::new().dark_gray()),
            preview_area,
        ),
    }

    let mut h = vec![
        ("Enter", "jump"),
        ("^Y", "copy"),
        ("Tab", "mark"),
        ("S-↑↓", "scroll preview"),
        ("M-h", "hidden"),
        ("M-i", "ignore"),
    ];
    if !s.is_finder() {
        h.extend([
            ("M-r", "regex"),
            ("M-c", "case"),
            ("M-w", "word"),
            ("M-g", "glob"),
        ]);
    }
    h.push(("Esc", "close"));
    f.render_widget(Paragraph::new(hint_line(&h)), hints);
}

/// Adjusts match ranges after the line's leading whitespace is trimmed.
fn shift(line: &str, ranges: &[std::ops::Range<usize>]) -> Vec<std::ops::Range<usize>> {
    let cut = line.len() - line.trim_start().len();
    ranges
        .iter()
        .filter(|r| r.end > cut)
        .map(|r| r.start.saturating_sub(cut)..r.end - cut)
        .collect()
}
