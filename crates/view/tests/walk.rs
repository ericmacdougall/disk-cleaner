//! The size walk against core's scan on the same synthetic fixture (spec AC-2), and its cancel,
//! progress-rate and job-cap behaviour (AC-3, NEG-17).

// Tests build fixtures on disk.
#![allow(clippy::disallowed_methods)]

use disk_cleaner_core::{scan, ScanOptions};
use disk_cleaner_view::jobs::{JobState, Jobs, StartRefused};
use disk_cleaner_view::walk::{walk, WalkError, WalkOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A small tree with nested folders, files of several sizes, an empty folder, and on Unix a
/// hard link and a symlink (to a folder, which must not be followed).
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write(&root.join("a/one.bin"), 10_000);
    write(&root.join("a/b/two.bin"), 100_000);
    write(&root.join("a/b/c/three.bin"), 1);
    write(&root.join("big.bin"), 1_000_000);
    write(&root.join("proj/node_modules/pkg/index.js"), 5_000);
    std::fs::create_dir_all(root.join("empty")).unwrap();
    #[cfg(unix)]
    {
        std::fs::hard_link(root.join("big.bin"), root.join("a/big-link.bin")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("link-to-a")).unwrap();
    }
    (dir, root)
}

fn core_scan(root: &Path) -> disk_cleaner_core::ScanResult {
    let mut opts = ScanOptions::new(vec![root.to_path_buf()]);
    opts.collect_facts = false;
    opts.one_file_system = true;
    scan(&opts).unwrap()
}

fn view_walk(root: &Path) -> disk_cleaner_view::Tree {
    let cancel = AtomicBool::new(false);
    walk(
        &WalkOptions::new(vec![root.to_path_buf()]),
        &cancel,
        &mut |_| {},
    )
    .unwrap()
}

#[test]
fn the_walk_agrees_with_core_totals() {
    let (_d, root) = fixture();
    let core = core_scan(&root);
    let tree = view_walk(&root);
    let r = tree.roots()[0];
    // Unix: blocks times 512, hard links once. Windows: core fills allocated_bytes with
    // logical sizes, and so does the walk.
    assert_eq!(tree.alloc(r), core.totals.allocated_bytes, "allocated");
    assert_eq!(tree.stats.allocated_bytes, core.totals.allocated_bytes);
    assert_eq!(
        tree.stats.logical_bytes, core.totals.logical_bytes,
        "logical"
    );
    assert_eq!(tree.stats.files, core.totals.files, "files");
    assert_eq!(tree.stats.dirs, core.totals.dirs, "dirs");
    assert_eq!(tree.stats.symlinks, core.totals.symlinks, "symlinks");
    assert_eq!(
        tree.stats.hardlink_duplicates_skipped,
        core.totals.hardlink_duplicates_skipped
    );
    #[cfg(unix)]
    {
        assert_eq!(tree.stats.hardlink_duplicates_skipped, 1);
        let link = tree.find_path(&root.join("link-to-a")).unwrap();
        assert_eq!(tree.kind(link), disk_cleaner_view::tree::EntryKind::Symlink);
        assert!(
            tree.children(link).is_empty(),
            "a symlink is never followed"
        );
    }
    // The root's file count is every non-folder entry.
    assert_eq!(tree.files(r), core.totals.files);
    // Top files are the biggest, largest first (the 1 MB file or its hard link).
    assert!(tree.top_files[0].bytes >= 1_000_000, "{:?}", tree.top_files);
    assert!(tree.top_files.windows(2).all(|w| w[0].bytes >= w[1].bytes));
}

#[test]
fn the_comparison_is_not_vacuous() {
    // A changed fixture must change the totals: guards against comparing two zeros.
    let (_d, root) = fixture();
    let before = view_walk(&root);
    write(&root.join("more.bin"), 300_000);
    let after = view_walk(&root);
    assert!(after.alloc(after.roots()[0]) > before.alloc(before.roots()[0]));
    assert!(before.stats.files >= 5);
}

#[cfg(unix)]
#[test]
fn an_unreadable_folder_is_counted_and_shown() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = fixture();
    let locked = root.join("locked");
    write(&locked.join("secret.bin"), 1000);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root ignores permissions; the check means nothing then.
    let readable_anyway = std::fs::read_dir(&locked).is_ok();
    let tree = view_walk(&root);
    let core = core_scan(&root);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable_anyway {
        eprintln!("running as a user that can read mode 000 folders; skipped");
        return;
    }
    let id = tree.find_path(&locked).unwrap();
    assert!(tree.flags(id) & disk_cleaner_view::tree::flags::UNREADABLE != 0);
    assert!(tree.stats.unreadable >= 1);
    assert!(core.coverage.unreadable_dirs >= 1);
    assert_eq!(tree.stats.allocated_bytes, core.totals.allocated_bytes);
}

#[test]
fn bad_roots_are_refused() {
    let cancel = AtomicBool::new(false);
    let (_d, root) = fixture();
    for bad in [
        PathBuf::from("relative"),
        root.join("missing"),
        root.join("big.bin"),
    ] {
        let r = walk(&WalkOptions::new(vec![bad.clone()]), &cancel, &mut |_| {});
        assert!(matches!(r, Err(WalkError::Root { .. })), "{bad:?}: {r:?}");
    }
    assert_eq!(
        walk(&WalkOptions::new(vec![]), &cancel, &mut |_| {}).unwrap_err(),
        WalkError::NoRoots
    );
}

fn many_files(n: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("many");
    for d in 0..(n / 100).max(1) {
        let sub = root.join(format!("d{d}"));
        std::fs::create_dir_all(&sub).unwrap();
        for f in 0..100 {
            std::fs::write(sub.join(format!("f{f}")), b"").unwrap();
        }
    }
    (dir, root)
}

#[test]
fn cancel_stops_the_walk_quickly() {
    let (_d, root) = many_files(3000);
    let cancel = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicUsize::new(0));
    let mut opts = WalkOptions::new(vec![root]);
    let (s, c) = (Arc::clone(&seen), Arc::clone(&cancel));
    // Slow every entry down and cancel after 200 of them.
    opts.entry_hook = Some(Arc::new(move |_p: &Path| {
        if s.fetch_add(1, Ordering::Relaxed) == 200 {
            c.store(true, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(1));
    }));
    opts.threads = 2;
    let start = Instant::now();
    let r = walk(&opts, &cancel, &mut |_| {});
    let took = start.elapsed();
    assert_eq!(r.unwrap_err(), WalkError::Cancelled);
    assert!(took < Duration::from_secs(2), "took {took:?}");
    assert!(
        seen.load(Ordering::Relaxed) < 3000,
        "the walk went on after cancel"
    );
}

#[test]
fn progress_is_reported_at_most_ten_times_a_second() {
    let (_d, root) = many_files(2000);
    let mut opts = WalkOptions::new(vec![root]);
    opts.entry_hook = Some(Arc::new(|_p: &Path| {
        std::thread::sleep(Duration::from_micros(500));
    }));
    opts.threads = 2;
    let cancel = AtomicBool::new(false);
    let mut events = 0u64;
    let start = Instant::now();
    let tree = walk(&opts, &cancel, &mut |_| events += 1).unwrap();
    let ms = start.elapsed().as_millis() as u64;
    assert!(tree.stats.files >= 2000);
    // One per 100 ms, plus the first and the final report.
    assert!(events <= ms / 100 + 2, "{events} events in {ms} ms");
    assert!(events >= 1);
}

#[test]
fn repeated_cancel_and_start_keep_at_most_two_walks() {
    let (_d, root) = many_files(200);
    let jobs = Jobs::new();
    // Every walk blocks in its first metadata call until released.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let running = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    let mut ids = Vec::new();
    let mut refused = None;
    for _ in 0..3 {
        let ticket = match jobs.start() {
            Ok(t) => t,
            Err(e) => {
                refused = Some(e);
                break;
            }
        };
        ids.push(ticket.id);
        let mut opts = WalkOptions::new(vec![root.clone()]);
        opts.threads = 1;
        let rx = Arc::clone(&release_rx);
        let blocked = Arc::new(AtomicBool::new(false));
        let b = Arc::clone(&blocked);
        opts.entry_hook = Some(Arc::new(move |_p: &Path| {
            if !b.swap(true, Ordering::SeqCst) {
                let _ = rx.lock().unwrap().recv();
            }
        }));
        let jobs2 = jobs.clone();
        let running2 = Arc::clone(&running);
        running.fetch_add(1, Ordering::SeqCst);
        handles.push(std::thread::spawn(move || {
            let r = walk(&opts, &ticket.cancel, &mut |_| {});
            // A cancelled job's result is dropped by id.
            let accepted = r.is_ok() && jobs2.advance(ticket.id, JobState::Done);
            running2.fetch_sub(1, Ordering::SeqCst);
            drop(ticket.guard);
            accepted
        }));
        assert!(jobs.cancel(*ids.last().unwrap()));
    }
    assert_eq!(refused, Some(StartRefused::StillStopping));
    assert_eq!(ids.len(), 2);
    assert_eq!(running.load(Ordering::SeqCst), 2);
    assert_eq!(jobs.live(), 2);
    drop(release_tx);
    for h in handles {
        assert!(!h.join().unwrap(), "a cancelled walk's result was accepted");
    }
    assert_eq!(jobs.live(), 0);
    let t = jobs.start().unwrap();
    assert!(jobs.is_current(t.id));
}
