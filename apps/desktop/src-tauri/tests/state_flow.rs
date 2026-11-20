//! The app state's job, settings and import flows through `Inner` (no window, no worker).
//!
//! PR-gate findings of 406987d:
//! - F2: installing an analysis must not hold the session lock while tags are prepared, and
//!   must stay linear in a wide tree. Thresholds registered before the first measurement
//!   (WAL, round 6): with 20,000 planned sibling folders in a debug build on a CI runner,
//!   installation takes at most 5 s, and no page request waits more than 250 ms; while tag
//!   preparation is held for 300 ms, a page request and Cancel are each answered within 250 ms.
//!   The same 250 ms applies to Cancel pressed during the 20,000-folder install (round 7, same
//!   threshold, registered before that test's first run).
//! - F3, F4: the plan as Export writes it carries the new window too.
//! - F3: a Save whose plan settings were written but whose scan settings were not says so and
//!   still replans.
//! - F4: a Save that finds its values already on disk (another window wrote them), and a reload
//!   that finds the file changed, both replan.
//! - F5: imports and scans share one admission: a second import, or a scan, is refused while an
//!   import runs, and a failed import frees the slot.
#![cfg(debug_assertions)]
// The tests write their own fixtures (scan files, config files) in temporary folders.
#![allow(clippy::disallowed_methods)]

use disk_cleaner_view::export::write_plan_new;
use disk_cleaner_view::settings::{AppSettings, CoreChanges};
use disk_cleaner_view::synth::{synthetic_scan, wide_dir_scan, SEED};
use prunedisk_desktop_lib::state::{Emit, Inner, ScanSettings};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const WIDE_DIRS: usize = 20_000;
const MAX_INSTALL: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_millis(250);
const HELD: Duration = Duration::from_millis(300);

// Tests in this file share hooks on separate `Inner`s, but timing tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn no_emit() -> Emit {
    Arc::new(|_| {})
}

fn inner_in(dir: &Path) -> Arc<Inner> {
    let inner = Inner::new(Some(dir.join("cfg").join("config.toml")));
    // A scan must never start a real worker here.
    inner.set_worker(dir.join("no-such-worker"), "git".into());
    inner
}

fn scan_file(dir: &Path, name: &str, nodes: usize) -> PathBuf {
    let path = dir.join(name);
    let bytes = serde_json::to_vec(&synthetic_scan(nodes, SEED)).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn shown_window(inner: &Inner) -> u64 {
    let s = inner.session();
    s.analysis.as_ref().expect("an analysis is shown").plan.active_window_days
}

/// The window of the plan as Export writes it: the shown plan, through export_plan's writer.
fn exported_window(inner: &Inner, dir: &Path, name: &str) -> u64 {
    let plan = {
        let s = inner.session();
        Arc::clone(&s.analysis.as_ref().expect("an analysis is shown").plan)
    };
    let path = dir.join(name);
    write_plan_new(&path, &plan).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    v["active_window_days"].as_u64().expect("the exported plan has a window")
}

fn max_ms(d: &AtomicU64) -> f64 {
    d.load(Ordering::SeqCst) as f64 / 1e6
}

/// Request pages of `view` until `stop`, recording the longest wait.
fn pager(inner: Arc<Inner>, view: u64, stop: Arc<AtomicBool>, longest: Arc<AtomicU64>) -> std::thread::JoinHandle<usize> {
    std::thread::spawn(move || {
        let mut n = 0;
        while !stop.load(Ordering::SeqCst) {
            let t = Instant::now();
            if let Ok(s) = inner.view(view) {
                let _ = s.tree.as_ref().map(|t| t.page(0, 200));
            }
            let ns = t.elapsed().as_nanos() as u64;
            longest.fetch_max(ns, Ordering::SeqCst);
            n += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        n
    })
}

#[test]
fn installing_a_wide_analysis_stays_fast_and_leaves_pages_answered() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let inner = inner_in(tmp.path());
    let (tree, scan) = wide_dir_scan(WIDE_DIRS, SEED);
    let ticket = inner.jobs.start().unwrap();
    let view = inner.show_walk(ticket.id, tree, &[PathBuf::from("/wide")]).expect("walk shown");

    let stop = Arc::new(AtomicBool::new(false));
    let longest = Arc::new(AtomicU64::new(0));
    let pages = pager(Arc::clone(&inner), view, Arc::clone(&stop), Arc::clone(&longest));
    let t = Instant::now();
    let shown = inner.show_scan(ticket.id, view, Arc::new(scan)).unwrap();
    let took = t.elapsed();
    std::thread::sleep(Duration::from_millis(20));
    stop.store(true, Ordering::SeqCst);
    let answered = pages.join().unwrap();

    let s = inner.session();
    let items = s.analysis.as_ref().unwrap().plan.items.len();
    let rows = s.tree.as_ref().unwrap().page(1, 50);
    let tagged = rows.iter().filter(|r| r.tag.as_deref() == Some("regenerable")).count();
    drop(s);
    eprintln!(
        "PERF install_wide dirs={WIDE_DIRS} plan_items={items} install_ms={:.1} longest_page_wait_ms={:.1} pages={answered} limits: {} ms, {} ms",
        took.as_secs_f64() * 1e3,
        max_ms(&longest),
        MAX_INSTALL.as_millis(),
        MAX_WAIT.as_millis()
    );
    assert!(shown, "the analysis was not installed");
    assert!(items >= WIDE_DIRS, "the plan has {items} items for {WIDE_DIRS} folders");
    assert_eq!(tagged, rows.len(), "every planned folder is tagged: {tagged} of {}", rows.len());
    assert!(took <= MAX_INSTALL, "installing took {took:?}");
    assert!(
        max_ms(&longest) <= MAX_WAIT.as_secs_f64() * 1e3,
        "a page request waited {:.1} ms while the analysis was installed",
        max_ms(&longest)
    );
}

/// The wide install again, with Cancel pressed while it runs (once the analysis is being
/// marked) and a pager running throughout: Cancel and every page are answered within 250 ms,
/// and the cancelled analysis is not shown.
#[test]
fn cancel_during_a_wide_install_is_answered_promptly() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let inner = inner_in(tmp.path());
    let (tree, scan) = wide_dir_scan(WIDE_DIRS, SEED);
    let ticket = inner.jobs.start().unwrap();
    let id = ticket.id;
    let view = inner.show_walk(id, tree, &[PathBuf::from("/wide")]).expect("walk shown");

    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    inner.set_prepare_hook(Some(Arc::new(move || {
        let _ = entered_tx.lock().unwrap().send(());
    })));
    let stop = Arc::new(AtomicBool::new(false));
    let longest = Arc::new(AtomicU64::new(0));
    let pages = pager(Arc::clone(&inner), view, Arc::clone(&stop), Arc::clone(&longest));
    let installer = {
        let inner = Arc::clone(&inner);
        std::thread::spawn(move || inner.show_scan(id, view, Arc::new(scan)))
    };
    entered_rx.recv_timeout(Duration::from_secs(60)).expect("the analysis was marked");
    let t = Instant::now();
    let cancelled = inner.cancel(id, &no_emit());
    let cancel_wait = t.elapsed();
    let shown = installer.join().unwrap().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    stop.store(true, Ordering::SeqCst);
    let answered = pages.join().unwrap();
    inner.set_prepare_hook(None);

    eprintln!(
        "PERF cancel_wide dirs={WIDE_DIRS} cancel_wait_ms={:.3} longest_page_wait_ms={:.1} pages={answered} limits: {} ms, {} ms",
        cancel_wait.as_secs_f64() * 1e3,
        max_ms(&longest),
        MAX_WAIT.as_millis(),
        MAX_WAIT.as_millis()
    );
    assert!(cancel_wait <= MAX_WAIT, "Cancel waited {cancel_wait:?} during a wide install");
    assert!(cancelled, "Cancel did not take");
    assert!(
        max_ms(&longest) <= MAX_WAIT.as_secs_f64() * 1e3,
        "a page request waited {:.1} ms during a wide install",
        max_ms(&longest)
    );
    assert!(!shown, "a cancelled job's analysis was installed");
    assert!(inner.session().analysis.is_none(), "a cancelled job's analysis is on screen");
}

#[test]
fn tags_are_prepared_without_the_session_lock() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let inner = inner_in(tmp.path());
    let (tree, scan) = wide_dir_scan(200, SEED);
    let ticket = inner.jobs.start().unwrap();
    let id = ticket.id;
    let view = inner.show_walk(id, tree, &[PathBuf::from("/wide")]).expect("walk shown");

    let (entered_tx, entered_rx) = mpsc::channel::<bool>();
    let entered_tx = Mutex::new(entered_tx);
    let me = Arc::clone(&inner);
    inner.set_prepare_hook(Some(Arc::new(move || {
        let free = me.session_is_free();
        let _ = entered_tx.lock().unwrap().send(free);
        std::thread::sleep(HELD);
    })));
    let installer = {
        let inner = Arc::clone(&inner);
        std::thread::spawn(move || inner.show_scan(id, view, Arc::new(scan)))
    };
    let free = entered_rx.recv_timeout(Duration::from_secs(30)).expect("tags were prepared");

    let t = Instant::now();
    let page = inner.view(view).map(|s| s.tree.as_ref().map(|t| t.page(0, 10).len()));
    let page_wait = t.elapsed();
    let t = Instant::now();
    let cancelled = inner.cancel(id, &no_emit());
    let cancel_wait = t.elapsed();
    let shown = installer.join().unwrap().unwrap();
    inner.set_prepare_hook(None);

    eprintln!(
        "lock free while preparing: {free}; page wait {page_wait:?}; cancel wait {cancel_wait:?}; cancelled {cancelled}; shown {shown}"
    );
    assert!(free, "the session lock was held while the tags were prepared");
    assert!(page.is_ok(), "the page request was refused");
    assert!(page_wait <= MAX_WAIT, "a page request waited {page_wait:?} for tag preparation");
    assert!(cancel_wait <= MAX_WAIT, "Cancel waited {cancel_wait:?} for tag preparation");
    assert!(cancelled, "Cancel did not take");
    assert!(!shown, "a cancelled job's analysis was installed");
    assert!(inner.session().analysis.is_none(), "a cancelled job's analysis is on screen");
}

/// `config.toml`'s folder, with the app settings file made unwritable (a folder in its place).
fn break_app_settings(dir: &Path) {
    std::fs::create_dir_all(dir.join("cfg").join("prunedisk-app.json")).unwrap();
}

fn other_app_settings() -> AppSettings {
    AppSettings {
        top_files: AppSettings::default().top_files + 5,
        ..AppSettings::default()
    }
}

#[test]
fn a_save_that_writes_only_the_plan_settings_says_so_and_replans() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let inner = inner_in(tmp.path());
    inner.import_file(&scan_file(tmp.path(), "scan.json", 50)).unwrap();
    inner.load_settings().unwrap();
    let before = shown_window(&inner);
    let want = before + 30;
    break_app_settings(tmp.path());

    let changes = CoreChanges {
        active_window_days: Some(want),
        max_git_admin_bytes: None,
    };
    let report = inner
        .save_settings(changes, Some(other_app_settings()), false)
        .expect("a partial save is reported, not refused");
    let text = std::fs::read_to_string(tmp.path().join("cfg").join("config.toml")).unwrap();
    assert!(text.contains(&format!("active_window_days = {want}")), "config.toml: {text}");
    assert_eq!(report.outcome, "partial", "{report:?}");
    let note = report.note.clone().unwrap_or_default();
    assert!(note.contains("scan settings were not saved"), "{note}");
    assert!(report.replanned, "the plan was not recomputed: {report:?}");
    assert_eq!(shown_window(&inner), want, "the plan shown keeps the old window");
    assert_eq!(
        exported_window(&inner, tmp.path(), "plan.json"),
        want,
        "the exported plan keeps the old window"
    );
    assert_eq!(inner.app_settings(), AppSettings::default(), "unsaved scan settings were adopted");

    // A retry is not refused as "changed since loaded": the saved text is the loaded text now.
    let again = inner
        .save_settings(changes, Some(other_app_settings()), false)
        .expect("the retry is reported, not refused");
    assert_eq!(again.outcome, "partial", "{again:?}");
}

#[test]
fn values_another_window_saved_are_adopted_and_replanned() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let inner = inner_in(tmp.path());
    inner.import_file(&scan_file(tmp.path(), "scan.json", 50)).unwrap();
    inner.load_settings().unwrap();
    let before = shown_window(&inner);
    let want = before + 30;
    // Another window saves the same value first.
    std::fs::create_dir_all(tmp.path().join("cfg")).unwrap();
    std::fs::write(
        tmp.path().join("cfg").join("config.toml"),
        format!("active_window_days = {want}\n"),
    )
    .unwrap();

    let report = inner
        .save_settings(
            CoreChanges {
                active_window_days: Some(want),
                max_git_admin_bytes: None,
            },
            None,
            false,
        )
        .unwrap();
    assert_eq!(report.outcome, "unchanged", "{report:?}");
    assert!(report.replanned, "the adopted values did not replan: {report:?}");
    assert_eq!(shown_window(&inner), want, "the plan shown keeps the old window");
    assert_eq!(
        exported_window(&inner, tmp.path(), "plan.json"),
        want,
        "the exported plan keeps the old window"
    );
}

#[test]
fn reloading_a_changed_config_replans() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let inner = inner_in(tmp.path());
    inner.import_file(&scan_file(tmp.path(), "scan.json", 50)).unwrap();
    inner.load_settings().unwrap();
    let want = shown_window(&inner) + 30;
    std::fs::create_dir_all(tmp.path().join("cfg")).unwrap();
    std::fs::write(
        tmp.path().join("cfg").join("config.toml"),
        format!("active_window_days = {want}\n"),
    )
    .unwrap();
    inner.load_settings().unwrap();
    assert!(inner.replan().unwrap(), "a changed config file did not replan");
    assert_eq!(shown_window(&inner), want);
    assert_eq!(exported_window(&inner, tmp.path(), "plan.json"), want);
}

#[test]
fn one_import_at_a_time_and_no_scan_while_importing() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let inner = inner_in(tmp.path());
    let first = scan_file(tmp.path(), "first.json", 50);
    let second = scan_file(tmp.path(), "second.json", 60);

    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let go_rx = Mutex::new(go_rx);
    inner.set_import_hook(Some(Arc::new(move || {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(30));
    })));
    let importer = {
        let inner = Arc::clone(&inner);
        let first = first.clone();
        std::thread::spawn(move || inner.import_file(&first))
    };
    entered_rx.recv_timeout(Duration::from_secs(30)).expect("the first import started");
    inner.set_import_hook(None);

    let overlapping = inner.import_file(&second);
    let scan = inner.start_scan(
        &[tmp.path().to_string_lossy().into_owned()],
        ScanSettings {
            collect_git_facts: false,
        },
        no_emit(),
    );
    go_tx.send(()).unwrap();
    let first_view = importer.join().unwrap();

    let refused = |r: &str| r.contains("a scan file is being opened");
    assert!(
        matches!(&overlapping, Err(e) if refused(&e.to_string())),
        "a second import ran alongside the first: {overlapping:?}"
    );
    assert!(
        matches!(&scan, Err(e) if refused(&e.to_string())),
        "a scan started while a file was being opened: {scan:?}"
    );
    let first_view = first_view.expect("the first import finished");
    assert_eq!(inner.session().view_id, first_view, "the first import is not the one shown");

    // A failed import frees the slot.
    let bad = tmp.path().join("bad.json");
    std::fs::write(&bad, b"not json").unwrap();
    assert!(inner.import_file(&bad).is_err());
    let after = inner.import_file(&second).expect("an import after a failed one is admitted");
    assert_eq!(inner.session().view_id, after);
}
