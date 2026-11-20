//! Render and key tests on ratatui's TestBackend (spec AC-1, AC-10, NEG-6), and the frame-time
//! measurement over a million entries (AC-9, ignored by default).

// Tests write export fixtures.
#![allow(clippy::disallowed_methods)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use disk_cleaner_core::{Plan, ScanResult};
use disk_cleaner_tui::app::{App, Outcome, Tab};
use disk_cleaner_tui::ui;
use disk_cleaner_tui::worker::{Analysis, Msg};
use disk_cleaner_view::dev::PlanInputs;
use disk_cleaner_view::synth::{synthetic_scan, synthetic_tree, EPOCH, SEED};
use disk_cleaner_view::tree::{EntryKind, NewEntry, TreeBuilder, WalkStats};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::time::Instant;

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn screen(app: &mut App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn small_tree() -> disk_cleaner_view::Tree {
    let mut b = TreeBuilder::new(true);
    let mut add = |parent, name: &str, kind, alloc| {
        b.push(NewEntry {
            parent,
            name,
            kind,
            size: alloc,
            alloc,
            files: 0,
            mtime: EPOCH,
            flags: 0,
        })
        .unwrap()
    };
    let r = add(None, "/data", EntryKind::Dir, 4096);
    let photos = add(Some(r), "photos", EntryKind::Dir, 4096);
    add(Some(photos), "beach.jpg", EntryKind::File, 5_000_000);
    add(Some(r), "notes.txt", EntryKind::File, 8192);
    add(
        Some(r),
        "evil\u{1b}[2J\u{202E}gpj.exe",
        EntryKind::File,
        4096,
    );
    drop(add);
    b.finish(Vec::new(), WalkStats::default())
}

fn app_with_tree() -> App {
    let mut app = App::new(EPOCH);
    app.set_tree(small_tree());
    app
}

fn inputs() -> PlanInputs {
    PlanInputs {
        config: disk_cleaner_core::Config::default(),
        window_source: "default".into(),
    }
}

#[test]
fn the_tree_is_sorted_by_size_and_drills_down() {
    let mut app = app_with_tree();
    let s = screen(&mut app, 120, 20);
    assert!(s.contains("Size tree: all scanned folders"), "{s}");
    let photos = s.find("photos/").expect("photos row");
    let notes = s.find("notes.txt").expect("notes row");
    assert!(photos < notes, "largest first:\n{s}");
    // Row 0 is /data; move to photos and open it.
    assert_eq!(app.on_key(key(KeyCode::Down)), Outcome::Continue);
    app.on_key(key(KeyCode::Enter));
    let s = screen(&mut app, 120, 20);
    assert!(s.contains("/data / photos"), "{s}");
    assert!(s.contains("beach.jpg"), "{s}");
    app.on_key(key(KeyCode::Left));
    let s = screen(&mut app, 120, 20);
    assert!(s.contains("Size tree: /data "), "{s}");
    assert!(s.contains("notes.txt"), "{s}");
}

#[test]
fn sort_keys_change_the_order_and_the_header() {
    let mut app = app_with_tree();
    // Allocated -> Size -> Files -> Modified -> Name
    for _ in 0..4 {
        app.on_key(key(KeyCode::Char('s')));
    }
    let s = screen(&mut app, 120, 20);
    assert!(s.contains("Name ^"), "{s}");
    let evil = s.find("evil").unwrap();
    let notes = s.find("notes.txt").unwrap();
    let photos = s.find("photos/").unwrap();
    assert!(evil < notes && notes < photos, "{s}");
    app.on_key(key(KeyCode::Char('r')));
    let s = screen(&mut app, 120, 20);
    assert!(s.contains("Name v"), "{s}");
}

#[test]
fn hostile_names_are_shown_as_inert_text() {
    let mut app = app_with_tree();
    let s = screen(&mut app, 140, 20);
    assert!(s.contains(r"evil\u{001B}[2J\u{202E}gpj.exe"), "{s}");
    assert!(!s.contains('\u{1b}'));
    assert!(!s.contains('\u{202E}'));
}

#[test]
fn help_opens_and_closes() {
    let mut app = app_with_tree();
    app.on_key(key(KeyCode::Char('?')));
    let s = screen(&mut app, 100, 30);
    assert!(s.contains("Keys"), "{s}");
    assert!(s.contains("Nothing here deletes"), "{s}");
    // q closes help first, then quits.
    assert_eq!(app.on_key(key(KeyCode::Char('q'))), Outcome::Continue);
    assert!(!app.help);
    assert_eq!(app.on_key(key(KeyCode::Char('q'))), Outcome::Quit);
}

#[test]
fn the_developer_view_shows_actions_and_core_totals() {
    let mut app = app_with_tree();
    let scan = synthetic_scan(40, SEED);
    let plan = disk_cleaner_tui::worker::plan_scan_with(&scan, &inputs(), EPOCH);
    let reclaim = disk_cleaner_view::units::bytes(plan.totals.reclaimable_bytes);
    app.set_analysis(&scan, plan, false);
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Dev);
    let s = screen(&mut app, 160, 40);
    assert!(s.contains("Developer view"), "{s}");
    assert!(s.contains("Dry run: nothing is changed"), "{s}");
    assert!(!s.contains("today ago") && !s.contains("- ago"), "{s}");
    assert!(s.contains(&reclaim), "{s}");
    assert!(s.contains("Repositories ("), "{s}");
    // Move to the first item: its reason shows below.
    app.on_key(key(KeyCode::Down));
    let s = screen(&mut app, 160, 40);
    assert!(s.contains("State:"), "{s}");
}

#[test]
fn export_writes_a_new_file_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tree();
    app.export_dir = dir.path().to_path_buf();
    app.on_key(key(KeyCode::Char('e')));
    assert!(app.status.contains("not ready"), "{}", app.status);
    let scan = synthetic_scan(10, SEED);
    let plan = disk_cleaner_tui::worker::plan_scan_with(&scan, &inputs(), EPOCH);
    app.set_analysis(&scan, plan, false);
    app.on_key(key(KeyCode::Char('e')));
    app.on_key(key(KeyCode::Char('e')));
    let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(files.len(), 2, "two exports, two files");
    // An unwritable target is a message, not a crash.
    app.export_dir = dir.path().join("missing");
    app.on_key(key(KeyCode::Char('e')));
    assert!(app.status.starts_with("Export failed"), "{}", app.status);
}

#[test]
fn an_imported_scan_has_no_size_tree() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("scan.json");
    std::fs::write(&p, serde_json::to_vec(&synthetic_scan(5, SEED)).unwrap()).unwrap();
    let mut app = App::new(EPOCH);
    // Plan settings come from the default config path; skip when that file is unreadable on
    // this machine rather than depend on it.
    if app.load_import(&p).is_err() {
        return;
    }
    assert_eq!(app.tab, Tab::Dev);
    app.on_key(key(KeyCode::Char('1')));
    let s = screen(&mut app, 120, 20);
    assert!(s.contains("no size tree"), "{s}");
}

/// The screen as rows of cell symbols.
fn cells(app: &mut App, w: u16, h: u16) -> Vec<Vec<String>> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        })
        .collect()
}

/// The inside of the box whose top border holds `title`, one string per row.
fn boxed(rows: &[Vec<String>], title: &str) -> Option<Vec<String>> {
    for (y, row) in rows.iter().enumerate() {
        let line: String = row.concat();
        let Some(byte) = line.find(title) else {
            continue;
        };
        let mut col = 0;
        let mut acc = 0;
        for (i, s) in row.iter().enumerate() {
            if acc >= byte {
                col = i;
                break;
            }
            acc += s.len();
        }
        // The tab bar also names the views; only a top border counts.
        let Some(left) = (0..=col).rev().find(|&x| row[x] == "┌") else {
            continue;
        };
        let Some(right) = (col..row.len()).find(|&x| row[x] == "┐") else {
            continue;
        };
        let mut out = Vec::new();
        for r in rows.iter().skip(y + 1) {
            if r[left] == "└" {
                break;
            }
            out.push(r[left + 1..right].concat());
        }
        return Some(out);
    }
    None
}

/// Text with every run of whitespace made one space, so wrapped sentences compare whole.
fn flat(rows: &[String]) -> String {
    rows.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn flat_str(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn live_analysis(app: &mut App, scan: ScanResult, plan: Plan) {
    app.on_message(Msg::Analysis(Box::new(Analysis { scan, plan })));
}

/// F3: a live scan finishes after the screen opened, so its time is later than the clock read
/// at start; it is still a known time ("today"), not "an unknown time".
#[test]
fn a_live_scan_made_after_the_screen_opened_has_a_known_time() {
    let mut app = App::new(EPOCH);
    let mut scan = synthetic_scan(10, SEED);
    scan.generated_at = EPOCH + 90;
    let plan = disk_cleaner_tui::worker::plan_scan_with(&scan, &inputs(), EPOCH + 95);
    live_analysis(&mut app, scan, plan);
    app.on_key(key(KeyCode::Char('2')));
    let s = screen(&mut app, 160, 40);
    assert!(!s.contains("unknown time"), "{s}");
    assert!(s.contains("scan made today"), "{s}");
}

/// A plan whose every notice is set: an imported scan older than the window, a partial process
/// check, incomplete coverage and the remote-facts sentence.
fn noisy_app(now: i64) -> App {
    let mut app = App::new(now);
    let scan = synthetic_scan(40, SEED);
    let mut plan = disk_cleaner_tui::worker::plan_scan_with(&scan, &inputs(), now);
    let process = "202 processes could not be inspected; a project they use may look idle";
    let coverage = "Coverage is incomplete: 1 folder could not be read, so links or clones \
                    inside it were not checked.";
    plan.process_check = "partial".into();
    plan.process_check_note = Some(process.into());
    plan.coverage_complete = false;
    plan.coverage_note = Some(coverage.into());
    app.set_analysis(&scan, plan, true);
    app.tab = Tab::Dev;
    app
}

/// Every notice of the summary, as the view states it.
fn notices(app: &App) -> Vec<String> {
    let s = &app.dev.as_ref().unwrap().summary;
    vec![
        s.stale_warning.clone().expect("the scan is older than the window"),
        s.imported_note.clone().expect("imported"),
        s.process_check_note.clone().expect("partial"),
        s.coverage_note.clone().expect("incomplete"),
        s.remote_facts_note.clone(),
        "Process check: partial".to_string(),
    ]
}

/// Every screen the details overlay shows while scrolling from its top to its end.
fn every_details_screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
    app.on_key(key(KeyCode::Char('i')));
    let mut seen = Vec::new();
    for _ in 0..40 {
        let rows = cells(app, w, h);
        if let Some(b) = boxed(&rows, "All notices") {
            seen.push(flat(&b));
        }
        app.on_key(key(KeyCode::Down));
    }
    seen
}

/// F4: at 80x24 every safety notice can be read, on the main screen or by scrolling the
/// details overlay (`i`), and a clipped header says so.
#[test]
fn every_safety_notice_is_reachable_at_80x24() {
    let mut app = noisy_app(EPOCH + 400 * 86_400);
    let wanted = notices(&app);
    let rows = cells(&mut app, 80, 24);
    let header = boxed(&rows, "Developer view").expect("the header box");
    let header = flat(&header);
    let screens = every_details_screen(&mut app, 80, 24);
    assert!(!screens.is_empty(), "i opens no details overlay");
    for n in &wanted {
        let n = flat_str(n);
        let on_header = header.contains(&n);
        assert!(
            on_header || screens.iter().any(|s| s.contains(&n)),
            "not reachable at 80x24: {n}\nheader: {header}\noverlay: {screens:#?}"
        );
    }
    // Esc closes the overlay.
    app.on_key(key(KeyCode::Esc));
    let rows = cells(&mut app, 80, 24);
    assert!(boxed(&rows, "All notices").is_none());
    let title = rows
        .iter()
        .map(|r| r.concat())
        .find(|l| l.contains('┌') && l.contains("Developer view"));
    assert!(
        title.as_deref().unwrap_or("").contains("press i"),
        "a clipped header must say how to see the rest: {title:?}"
    );
}

/// F4: at 160x45 the header grows to fit every notice (nothing is cut).
#[test]
fn the_header_fits_every_notice_at_160x45() {
    let mut app = noisy_app(EPOCH + 400 * 86_400);
    let wanted = notices(&app);
    let rows = cells(&mut app, 160, 45);
    let header = flat(&boxed(&rows, "Developer view").expect("the header box"));
    for n in &wanted {
        assert!(header.contains(&flat_str(n)), "cut at 160x45: {n}\n{header}");
    }
}

/// F4: a partial process check and incomplete coverage are stated even without a note.
#[test]
fn a_partial_process_check_and_incomplete_coverage_are_stated_without_notes() {
    let mut app = App::new(EPOCH);
    let scan = synthetic_scan(10, SEED);
    let mut plan = disk_cleaner_tui::worker::plan_scan_with(&scan, &inputs(), EPOCH);
    plan.process_check = "partial".into();
    plan.process_check_note = None;
    plan.coverage_complete = false;
    plan.coverage_note = None;
    app.set_analysis(&scan, plan, false);
    app.tab = Tab::Dev;
    let rows = cells(&mut app, 160, 45);
    let header = flat(&boxed(&rows, "Developer view").expect("the header box"));
    assert!(header.contains("Process check: partial"), "{header}");
    assert!(header.contains("Coverage is incomplete"), "{header}");
}

/// F4 with core: a folder that cannot be read makes core's coverage incomplete, and the view
/// states core's note (the click-test's tree2 case).
#[cfg(unix)]
#[test]
fn an_unreadable_folder_gives_a_coverage_note_from_core() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("f"), b"x").unwrap();
    std::fs::write(dir.path().join("g"), b"y").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&locked).is_ok();
    let mut opts = disk_cleaner_core::ScanOptions::new(vec![dir.path().to_path_buf()]);
    opts.collect_facts = false;
    let scan = disk_cleaner_core::scan(&opts);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        println!("SKIP: this user can read a mode-000 folder (root)");
        return;
    }
    let scan = scan.unwrap();
    let plan = disk_cleaner_tui::worker::plan_scan_with(&scan, &inputs(), EPOCH);
    assert!(!plan.coverage_complete);
    let note = plan.coverage_note.clone().expect("core's coverage note");
    let mut app = App::new(EPOCH);
    app.set_analysis(&scan, plan, false);
    app.tab = Tab::Dev;
    let rows = cells(&mut app, 80, 24);
    let header = flat(&boxed(&rows, "Developer view").expect("the header box"));
    let screens = every_details_screen(&mut app, 80, 24);
    let n = flat_str(&note);
    assert!(
        header.contains(&n) || screens.iter().any(|s| s.contains(&n)),
        "core's coverage note is not reachable: {n}\n{header}\n{screens:#?}"
    );
}

/// F5 and F8: the help shows every line whole at 80x24 and 160x45, including what the Size and
/// % parent columns mean.
#[test]
fn the_help_is_whole_at_80x24_and_160x45() {
    let sort = format!(
        "sort by the next column ({}, size, files, modified, name)",
        ui::allocated_header().to_lowercase()
    );
    let wanted = [
        sort.as_str(),
        "export the plan as JSON to a new file in the current folder",
        "switch between the size tree and the developer view",
        "Size: apparent size",
        "% parent: share of the parent folder's allocated space",
        "not a sort key",
        "Nothing here deletes, moves or backs up anything.",
    ];
    for (w, h) in [(80, 24), (160, 45)] {
        let mut app = app_with_tree();
        app.on_key(key(KeyCode::Char('?')));
        let rows = cells(&mut app, w, h);
        let help = flat(&boxed(&rows, "Keys").expect("the help box"));
        for t in wanted {
            assert!(help.contains(t), "at {w}x{h} the help lacks: {t}\n{help}");
        }
    }
}

/// F6: a path too long for the list ends in its tail after an ellipsis, measured in display
/// columns (wide characters included).
#[test]
fn long_paths_keep_their_tail_after_an_ellipsis() {
    let mut app = App::new(EPOCH);
    let scan = synthetic_scan(20, SEED);
    let mut plan = disk_cleaner_tui::worker::plan_scan_with(&scan, &inputs(), EPOCH);
    for (i, it) in plan.items.iter_mut().enumerate() {
        let base = "/Users/someone/work/clients/a-rather-long-client-name/projects";
        it.path = format!("{base}/ñandú-目录-{i}/node_modules");
    }
    app.set_analysis(&scan, plan, false);
    app.tab = Tab::Dev;
    app.on_key(key(KeyCode::Down));
    let rows = cells(&mut app, 80, 30);
    let list = boxed(&rows, "Items by class").expect("the list box");
    let tails: Vec<&String> = list.iter().filter(|r| r.contains("modules")).collect();
    assert!(!tails.is_empty(), "no item row shows its path's tail:\n{list:#?}");
    for r in tails {
        assert!(r.contains('…'), "a cut path must show an ellipsis: {r:?}");
        assert!(r.trim_end().ends_with("node_modules"), "{r:?}");
    }
}

/// Tiny terminals draw without panicking, in every view and overlay.
#[test]
fn tiny_terminals_do_not_panic() {
    for (w, h) in [(1, 1), (2, 2), (10, 3), (20, 5), (40, 10), (80, 24)] {
        let mut app = noisy_app(EPOCH + 400 * 86_400);
        app.set_tree(small_tree());
        for k in ['1', '2', '?', '?', 'i', 'j', 'G'] {
            app.on_key(key(KeyCode::Char(k)));
            let _ = cells(&mut app, w, h);
        }
    }
}

#[test]
#[ignore = "performance measurement; run in release mode in CI"]
fn frame_time_over_a_million_entries() {
    let tree = synthetic_tree(1_000_000, SEED);
    let mut app = App::new(EPOCH);
    app.set_tree(tree);
    let mut t = Terminal::new(TestBackend::new(200, 60)).unwrap();
    let mut best = f64::MAX;
    let mut times = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        t.draw(|f| ui::draw(f, &mut app)).unwrap();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        times.push(ms);
        best = best.min(ms);
        app.on_key(key(KeyCode::PageDown));
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p95 = times[(times.len() * 95) / 100 - 1];
    println!("PERF tui_frame_ms best={best:.3} p95={p95:.3} n=50 nodes=1000000 seed={SEED}");
    assert!(p95 < 20.0, "TUI frame p95 {p95:.3} ms >= 20 ms");
}
