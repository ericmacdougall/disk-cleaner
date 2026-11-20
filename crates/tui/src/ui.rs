//! Drawing. Only the rows on screen are built, so a tree of a million entries draws as fast as
//! a small one.

use crate::app::{AnalysisState, App, DevRow, Tab, WalkState};
use disk_cleaner_view::dev::GROUPS;
use disk_cleaner_view::table::SortKey;
use disk_cleaner_view::tree::EntryKind;
use disk_cleaner_view::units;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

/// The allocated column's header: on Windows the value is the file length, as in core.
pub fn allocated_header() -> &'static str {
    if cfg!(windows) {
        "Length"
    } else {
        "Allocated"
    }
}

const HELP: &[(&str, &str)] = &[
    (
        "Tab, 1, 2",
        "switch between the size tree and the developer view",
    ),
    ("Up, Down, j, k", "move"),
    (
        "PgUp, PgDn, Home, End",
        "move by a page, to the start or the end",
    ),
    ("Enter, Right, l", "open the selected folder (drill down)"),
    ("Left, Backspace, h", "go up one folder"),
    ("Space", "expand or collapse the selected folder in place"),
    ("s", "sort by the next column ({sort})"),
    ("r", "reverse the sort order"),
    (
        "e",
        "export the plan as JSON to a new file in the current folder",
    ),
    (
        "i",
        "show every notice of the developer view (Up and Down scroll it)",
    ),
    ("?", "show or hide this help"),
    ("q, Ctrl-C", "quit"),
];

/// The help's last line.
const NOTHING_CHANGES: &str = "Nothing here deletes, moves or backs up anything.";

/// The order `s` goes through, as the help names it.
fn sort_order() -> String {
    format!(
        "{}, size, files, modified, name",
        allocated_header().to_lowercase()
    )
}

/// What the size tree's columns mean (F8: Size and % parent are easy to misread).
fn columns_note() -> String {
    let first = if cfg!(windows) {
        "Length: file length."
    } else {
        "Allocated: disk space used."
    };
    format!(
        "{first} Size: apparent size (file lengths, folders' own entries included, as du \
         --apparent-size). % parent: share of the parent folder's allocated space, so it \
         orders like {}; not a sort key.",
        allocated_header()
    )
}

/// Display width of a string.
fn width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Display width of a character.
fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    width(c.encode_utf8(&mut buf))
}

/// Word-wrap `text` to `cols` display columns. A word wider than a line is broken by width,
/// never inside a character.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        let w = width(word);
        if used > 0 && used + 1 + w <= cols {
            line.push(' ');
            line.push_str(word);
            used += 1 + w;
            continue;
        }
        if used > 0 {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
        if w <= cols {
            line.push_str(word);
            used = w;
            continue;
        }
        for c in word.chars() {
            let cw = char_width(c);
            if used > 0 && used + cw > cols {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += cw;
        }
    }
    if used > 0 || out.is_empty() {
        out.push(line);
    }
    out
}

/// `s` if it fits in `cols` display columns, else an ellipsis and as much of its end as fits
/// (the end of a path names the folder).
pub fn fit_tail(s: &str, cols: usize) -> String {
    if width(s) <= cols {
        return s.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if used + cw > cols {
            break;
        }
        used += cw;
        tail.push(c);
    }
    let mut out = String::from("\u{2026}");
    out.extend(tail.iter().rev());
    out
}

/// Draw the whole screen.
pub fn draw(f: &mut Frame, app: &mut App) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, app, head);
    match app.tab {
        Tab::Tree => draw_tree(f, app, body),
        Tab::Dev => draw_dev(f, app, body),
    }
    draw_footer(f, app, foot);
    if app.details {
        draw_details(f, app, f.area());
    }
    if app.help {
        draw_help(f, f.area());
    }
}

fn tab_span(label: &str, on: bool) -> Span<'static> {
    let text = if on {
        format!("[{label}]")
    } else {
        format!(" {label} ")
    };
    let style = if on {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
    };
    Span::styled(text, style)
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::styled("PruneDisk ", Style::default().add_modifier(Modifier::BOLD)),
        tab_span("1 Size tree", app.tab == Tab::Tree),
        Span::raw(" "),
        tab_span("2 Developer view", app.tab == Tab::Dev),
        Span::raw("   read-only   ? help"),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let text = if !app.status.is_empty() {
        app.status.clone()
    } else {
        match (&app.walk, &app.analysis) {
            (WalkState::Walking(_), _) => "Scanning sizes... q quits".to_string(),
            (_, AnalysisState::Running) => {
                "Analysing repositories and building the plan (git runs read-only)...".to_string()
            }
            (_, AnalysisState::Failed(e)) => format!("Analysis failed: {e}"),
            _ => "Enter opens a folder, Left goes up, s sorts, e exports the plan, ? help"
                .to_string(),
        }
    };
    f.render_widget(Paragraph::new(text), area);
}

fn progress_text(p: &disk_cleaner_view::walk::Progress) -> String {
    format!(
        "Scanning: {} files, {} folders, {}, {:.1} s\n{}",
        units::count(p.files),
        units::count(p.dirs),
        units::bytes(p.bytes),
        p.elapsed_ms as f64 / 1000.0,
        p.current
    )
}

fn header_cell(
    label: &str,
    key: Option<SortKey>,
    sort: disk_cleaner_view::table::Sort,
) -> Cell<'static> {
    let mark = match key {
        Some(k) if k == sort.key => {
            if sort.descending {
                " v"
            } else {
                " ^"
            }
        }
        _ => "",
    };
    Cell::from(format!("{label}{mark}")).style(Style::default().add_modifier(Modifier::BOLD))
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(view) = app.tree.as_ref() else {
        let text = match &app.walk {
            WalkState::Walking(p) => progress_text(p),
            WalkState::Failed(e) => format!("The size walk failed: {e}"),
            WalkState::NotRun => "This scan was read from a file, so there is no size tree. \
                                  Run `disk-cleaner tui <folders>` for one. Press 2 for the \
                                  developer view."
                .to_string(),
            WalkState::Done => String::new(),
        };
        f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title(" Size tree ")),
            area,
        );
        return;
    };
    let tree = view.tree();
    let crumbs: Vec<String> = view.breadcrumb().into_iter().map(|c| c.name).collect();
    let place = if crumbs.is_empty() {
        "all scanned folders".to_string()
    } else {
        crumbs.join(" / ")
    };
    let title = format!(
        " Size tree: {place}  (source: size walk, {} files, {} in {:.1} s) ",
        units::count(tree.stats.files),
        units::bytes(tree.stats.allocated_bytes),
        tree.stats.elapsed_ms as f64 / 1000.0
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    // One line for the column header.
    let visible = inner.height.saturating_sub(1) as usize;
    app.page = visible.max(1);
    app.scroll_into_view();
    let view = app.tree.as_ref().expect("checked above");
    let sort = view.sort();
    let rows: Vec<Row> = view
        .page(app.top, visible)
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let marker = match (r.kind, r.has_children, r.expanded) {
                (EntryKind::Dir, true, true) => "- ",
                (EntryKind::Dir, true, false) => "+ ",
                (EntryKind::Dir, false, _) => "  ",
                (EntryKind::Symlink, _, _) => "@ ",
                _ => "  ",
            };
            let mut name = format!("{}{}{}", "  ".repeat(r.depth as usize), marker, r.name);
            if r.kind == EntryKind::Dir {
                name.push('/');
            }
            for (on, note) in [
                (r.unreadable, " [unreadable]"),
                (r.mount_skipped, " [other file system, not entered]"),
                (r.hardlink_duplicate, " [hard link, counted once]"),
                (r.kind == EntryKind::Symlink, " [link, not followed]"),
            ] {
                if on {
                    name.push_str(note);
                }
            }
            let style = if app.top + i == app.sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(name),
                Cell::from(units::bytes(r.alloc)),
                Cell::from(units::bytes(r.size)),
                Cell::from(units::count(r.files)),
                Cell::from(units::age(r.mtime, app.now)),
                Cell::from(units::percent(r.percent_of_parent)),
            ])
            .style(style)
        })
        .collect();
    let header = Row::new(vec![
        header_cell("Name", Some(SortKey::Name), sort),
        header_cell(allocated_header(), Some(SortKey::Allocated), sort),
        header_cell("Size", Some(SortKey::Size), sort),
        header_cell("Files", Some(SortKey::Files), sort),
        header_cell("Modified", Some(SortKey::Modified), sort),
        header_cell("% parent", None, sort),
    ]);
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(10),
            Constraint::Length(9),
        ],
    )
    .header(header);
    f.render_widget(table, inner);
}

/// Said when core reports incomplete coverage without a note.
const NO_COVERAGE_DETAIL: &str = "Coverage is incomplete (the scan gave no detail), so links \
                                  or clones in what was not read were not checked.";

/// The developer view's notices, one paragraph each: what the numbers are and how far to trust
/// them first, then the safety notices (they must never be cut off), then the breakdown.
pub fn dev_notes(app: &App) -> Vec<String> {
    let Some(dev) = &app.dev else {
        let text = match &app.analysis {
            AnalysisState::Waiting => "The analysis starts after the size walk.".to_string(),
            AnalysisState::Running => "Analysing repositories and building the plan...".to_string(),
            AnalysisState::Failed(e) => format!("The analysis failed: {e}"),
            AnalysisState::Done => String::new(),
        };
        return vec![text];
    };
    let s = &dev.summary;
    let t = &s.plan_totals;
    let mut lines = vec![
        format!(
            "Source: analysis (core scan and plan), scan made {}. Dry run: nothing is changed.",
            units::ago(Some(s.scan_generated_at), app.now)
        ),
        format!(
            "Reclaimable if every removing action completed: {}  (ready now, regenerable only: {})",
            units::bytes(t.reclaimable_bytes),
            units::bytes(t.ready_bytes)
        ),
    ];
    if let Some(n) = &s.stale_warning {
        lines.push(format!("Warning: {n}"));
    }
    if let Some(n) = &s.imported_note {
        lines.push(n.clone());
    }
    let process = match &s.process_check_note {
        Some(n) => format!("Process check: {}. {n}", s.process_check),
        None => format!("Process check: {}.", s.process_check),
    };
    lines.push(process);
    if let Some(n) = &s.coverage_note {
        lines.push(n.clone());
    } else if !s.coverage_complete {
        lines.push(NO_COVERAGE_DETAIL.to_string());
    }
    if let Some(view) = &app.tree {
        let st = &view.tree().stats;
        if st.unreadable > 0 {
            lines.push(format!(
                "Size walk: {} entries could not be read; their sizes are not counted.",
                units::count(st.unreadable)
            ));
        }
    }
    lines.push(format!(
        "Remote facts not verified: {}.",
        s.remote_facts_note
    ));
    let by_action: Vec<String> = t
        .by_action
        .iter()
        .map(|(a, x)| {
            format!(
                "{a} {} ({})",
                units::count(x.items),
                units::bytes(x.reclaimable_bytes)
            )
        })
        .collect();
    lines.push(format!("By action: {}", by_action.join(", ")));
    lines.push(format!(
        "Active window: {} days ({}).",
        s.active_window_days, s.window_source
    ));
    lines
}

/// Rows kept for the item list (at least) and the reason box under the header.
const LIST_MIN: u16 = 3;
const DETAIL_ROWS: u16 = 6;

fn draw_dev(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = area.width.saturating_sub(2) as usize;
    let notes = dev_notes(app);
    let mut wrapped: Vec<String> = notes.iter().flat_map(|n| wrap(n, cols)).collect();
    // The header grows to fit its notices, up to what leaves the list and the reason box
    // their rows; whatever does not fit is in the `i` overlay, and the title says so.
    let most = area.height.saturating_sub(LIST_MIN + DETAIL_ROWS).max(3);
    let height = (wrapped.len() as u16).saturating_add(2).min(most);
    let shown = height.saturating_sub(2) as usize;
    let title = if wrapped.len() > shown {
        format!(
            " Developer view ({} more lines: press i for every notice) ",
            wrapped.len() - shown
        )
    } else {
        " Developer view (i: every notice) ".to_string()
    };
    let [top, list, detail] = Layout::vertical([
        Constraint::Length(height),
        Constraint::Min(LIST_MIN),
        Constraint::Length(DETAIL_ROWS),
    ])
    .areas(area);
    wrapped.truncate(shown);
    let summary: Vec<Line> = wrapped.into_iter().map(Line::from).collect();
    f.render_widget(
        Paragraph::new(summary).block(Block::default().borders(Borders::ALL).title(title)),
        top,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Items by class (action, reclaimable, path) ");
    let inner = block.inner(list);
    f.render_widget(block, list);
    app.page = (inner.height as usize).max(1);
    app.scroll_into_view();
    let Some(dev) = &app.dev else {
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    // Columns left for the path after "  {action:<27} {bytes:>10}  ".
    let path_cols = (inner.width as usize).saturating_sub(42);
    let end = (app.dev_top + inner.height as usize).min(app.dev_rows.len());
    for i in app.dev_top..end {
        let selected = i == app.dev_sel;
        let text = match app.dev_rows[i] {
            DevRow::Header(g) => {
                let grp = &dev.summary.groups[g];
                format!(
                    "{} ({} items, {} by core's class totals)",
                    grp.label,
                    units::count(grp.count as u64),
                    units::bytes(grp.class_bytes)
                )
            }
            DevRow::Item(g, k) => {
                let it = &dev.items(g)[k];
                format!(
                    "  {:<27} {:>10}  {}",
                    it.action,
                    units::bytes(it.reclaimable_bytes),
                    fit_tail(&it.path, path_cols)
                )
            }
            DevRow::SessionsHeader => format!(
                "Agent session folders ({}; from the size walk; no plan action yet)",
                app.sessions.len()
            ),
            DevRow::Session(k) => {
                let s = &app.sessions[k];
                format!(
                    "  {:<27} {:>10}  {}",
                    s.layout,
                    units::bytes(s.alloc),
                    fit_tail(&s.path, path_cols)
                )
            }
        };
        let mut style = Style::default();
        if matches!(app.dev_rows[i], DevRow::Header(_) | DevRow::SessionsHeader) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        lines.push(Line::styled(text, style));
    }
    f.render_widget(Paragraph::new(lines), inner);
    let detail_text = match app.dev_rows.get(app.dev_sel) {
        Some(DevRow::Item(g, k)) => {
            let it = &dev.items(*g)[*k];
            let mut t = format!(
                "{}: {}. State: {} ({}).",
                it.action, it.reason, it.state, it.state_reason
            );
            if !it.facts_no.is_empty() {
                t.push_str(&format!(" Facts that are no: {}.", it.facts_no.join(", ")));
            }
            if !it.facts_unknown.is_empty() {
                t.push_str(&format!(
                    " Facts that are unknown: {}.",
                    it.facts_unknown.join(", ")
                ));
            }
            t
        }
        Some(DevRow::Session(k)) => {
            let s = &app.sessions[*k];
            let mut t = format!(
                "{}: {} files, {}, last changed {}. {}.",
                s.layout,
                units::count(s.files),
                units::bytes(s.alloc),
                units::ago(s.newest_mtime, app.now),
                s.plan_note
            );
            if let Some(e) = &s.enclosing {
                t.push_str(&format!(" Inside {} ({}).", e.path, e.action));
            }
            t
        }
        Some(DevRow::Header(g)) => format!("{} (class {})", GROUPS[*g].1, GROUPS[*g].0.as_str()),
        _ => String::new(),
    };
    f.render_widget(
        Paragraph::new(detail_text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" Reason ")),
        detail,
    );
}

/// The help text wrapped to `cols`: keys in a left column, their text wrapped beside it with a
/// hanging indent, then what the columns mean.
fn help_lines(cols: usize) -> Vec<String> {
    let key_cols = (cols / 3).min(24);
    let text_cols = cols.saturating_sub(key_cols).max(1);
    let mut out = Vec::new();
    for (k, v) in HELP {
        let v = v.replace("{sort}", &sort_order());
        // A key list too wide for its column gets a line of its own.
        let mut key = *k;
        if width(key) >= key_cols {
            out.push(key.to_string());
            key = "";
        }
        for part in wrap(&v, text_cols) {
            out.push(format!("{key:<key_cols$}{part}"));
            key = "";
        }
    }
    out.push(String::new());
    out.extend(wrap(&columns_note(), cols));
    out.push(String::new());
    out.extend(wrap(NOTHING_CHANGES, cols));
    out
}

/// A box of `w` by `h` centred in `area` (never larger than it).
fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = area.width.min(78);
    let lines = help_lines(w.saturating_sub(2) as usize);
    let rect = centred(area, w, (lines.len() as u16).saturating_add(2));
    let lines: Vec<Line> = lines.into_iter().map(Line::from).collect();
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Keys ")),
        rect,
    );
}

/// Every notice of the developer view, scrollable (`i`): the header shows only what fits.
fn draw_details(f: &mut Frame, app: &mut App, area: Rect) {
    let rect = centred(area, 100, area.height);
    let cols = rect.width.saturating_sub(2) as usize;
    let mut lines: Vec<String> = Vec::new();
    for (i, n) in dev_notes(app).iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        lines.extend(wrap(n, cols));
    }
    let rows = rect.height.saturating_sub(2) as usize;
    app.details_page = rows.max(1);
    app.details_top = app.details_top.min(lines.len().saturating_sub(rows));
    let top = app.details_top;
    let end = (top + rows).min(lines.len());
    let (first, total) = (top + 1, lines.len());
    let title = format!(" All notices, lines {first}-{end} of {total} (Up/Down, Esc closes) ");
    let shown: Vec<Line> = lines[top..end].iter().cloned().map(Line::from).collect();
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(shown).block(Block::default().borders(Borders::ALL).title(title)),
        rect,
    );
}
