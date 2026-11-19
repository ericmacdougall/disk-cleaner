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
    (
        "s",
        "sort by the next column (allocated, size, files, modified, name)",
    ),
    ("r", "reverse the sort order"),
    (
        "e",
        "export the plan as JSON to a new file in the current folder",
    ),
    ("?", "show or hide this help"),
    ("q, Ctrl-C", "quit"),
];

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

fn dev_summary(app: &App) -> Vec<Line<'static>> {
    let Some(dev) = &app.dev else {
        let text = match &app.analysis {
            AnalysisState::Waiting => "The analysis starts after the size walk.".to_string(),
            AnalysisState::Running => "Analysing repositories and building the plan...".to_string(),
            AnalysisState::Failed(e) => format!("The analysis failed: {e}"),
            AnalysisState::Done => String::new(),
        };
        return vec![Line::from(text)];
    };
    let s = &dev.summary;
    let t = &s.plan_totals;
    let mut lines = vec![
        Line::from(format!(
            "Source: analysis (core scan and plan), scan made {}. Dry run: nothing is changed.",
            units::ago(Some(s.scan_generated_at), app.now)
        )),
        Line::from(format!(
            "Reclaimable if every removing action completed: {}  (ready now, regenerable only: {})",
            units::bytes(t.reclaimable_bytes),
            units::bytes(t.ready_bytes)
        )),
    ];
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
    lines.push(Line::from(format!("By action: {}", by_action.join(", "))));
    lines.push(Line::from(format!(
        "Active window: {} days ({}). Remote facts not verified: {}",
        s.active_window_days, s.window_source, s.remote_facts_note
    )));
    if let Some(n) = &s.coverage_note {
        lines.push(Line::from(format!("Coverage: {n}")));
    }
    if let Some(n) = &s.stale_warning {
        lines.push(Line::from(format!("Warning: {n}")));
    }
    if let Some(n) = &s.imported_note {
        lines.push(Line::from(n.clone()));
    }
    lines
}

fn draw_dev(f: &mut Frame, app: &mut App, area: Rect) {
    let summary = dev_summary(app);
    let [top, list, detail] = Layout::vertical([
        Constraint::Length(summary.len() as u16 + 2),
        Constraint::Min(3),
        Constraint::Length(6),
    ])
    .areas(area);
    f.render_widget(
        Paragraph::new(summary).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Developer view "),
        ),
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
                    it.path
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
                    s.path
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

fn draw_help(f: &mut Frame, area: Rect) {
    let w = area.width.min(78);
    let h = (HELP.len() as u16 + 4).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    let mut lines: Vec<Line> = HELP
        .iter()
        .map(|(k, v)| Line::from(format!("{k:<24}{v}")))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Nothing here deletes, moves or backs up anything.",
    ));
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Keys ")),
        rect,
    );
}
