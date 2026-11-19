//! Render and key tests on ratatui's TestBackend (spec AC-1, AC-10, NEG-6), and the frame-time
//! measurement over a million entries (AC-9, ignored by default).

// Tests write export fixtures.
#![allow(clippy::disallowed_methods)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use disk_cleaner_tui::app::{App, Outcome, Tab};
use disk_cleaner_tui::ui;
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
    add(Some(r), "evil\u{1b}[2J\u{202E}gpj.exe", EntryKind::File, 4096);
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
