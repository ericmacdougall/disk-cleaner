//! Test builds only (the `e2e` feature; release bundles are built without it, and the bundle
//! job checks the release binary holds none of these names). The WebDriver checks cannot click
//! native dialogs, so each dialog answers from an environment variable instead, inside the
//! same command, which then goes on exactly as after a real pick. No command is added.
//!
//! - `PRUNEDISK_E2E_FOLDERS`: the folders the folder dialog picks (a path list, as `PATH`).
//! - `PRUNEDISK_E2E_OPEN`: the files the open dialog picks, one per call, in order; after the
//!   last, the dialog is cancelled.
//! - `PRUNEDISK_E2E_EXPORT`: the files the save dialog names, one per call, likewise.
//! - `PRUNEDISK_E2E_START`: folders (a path list) to scan, with git facts, as soon as the app
//!   starts, through the same offer and Start as the chooser (the window-close check,
//!   e2e/close/check_close.py, which drives the window only through the X server).
//! - `PRUNEDISK_E2E_SYNTH_NODES` and `PRUNEDISK_E2E_UNTRACED`: the AC-9 timing workload and its
//!   untraced timed launch (below).
//!
//! The run loop also reports the window and exit events it sees on stderr, one line each,
//! prefixed `e2e: event`, so the window-close check can say which way each route went.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static OPENED: AtomicUsize = AtomicUsize::new(0);
static EXPORTED: AtomicUsize = AtomicUsize::new(0);

fn list(var: &str) -> Option<Vec<PathBuf>> {
    std::env::var_os(var).map(|v| std::env::split_paths(&v).collect())
}

/// The folders to pick, when the variable is set.
pub fn folders() -> Option<Vec<PathBuf>> {
    list("PRUNEDISK_E2E_FOLDERS")
}

/// The folders to scan at start, when the variable is set.
pub fn start_roots() -> Option<Vec<PathBuf>> {
    list("PRUNEDISK_E2E_START")
}

/// `None` when `var` is not set (use the real dialog); otherwise the dialog's answer: the
/// list's next entry (counted by `counter`), or `None` (cancelled) once it is used up.
fn next(var: &str, counter: &AtomicUsize) -> Option<Option<PathBuf>> {
    let l = list(var)?;
    let n = counter.fetch_add(1, Ordering::SeqCst);
    Some(l.get(n).cloned())
}

/// The open dialog's answer, when the variable is set.
pub fn next_open() -> Option<Option<PathBuf>> {
    next("PRUNEDISK_E2E_OPEN", &OPENED)
}

/// The save dialog's answer, when the variable is set.
pub fn next_export() -> Option<Option<PathBuf>> {
    next("PRUNEDISK_E2E_EXPORT", &EXPORTED)
}

/// The untraced switch (spec AC-9, PR gate F9; CI only). `e2e/run.mjs` sets it to 1 for the
/// running-app scroll gate's timed launch, and `e2e/check.sh`'s app wrapper then starts that
/// launch without strace. The wrapper honours it only for a binary that holds this name, so only
/// for an e2e build: a release build holds no `PRUNEDISK_E2E_` name (check.sh's string check and
/// `e2e/check-shipped.sh`, which names this one too). The app checks the claim itself: when the
/// switch is set and a tracer is attached, it shows no synthetic tree, so a traced launch is never
/// timed as the untraced one.
const UNTRACED: &str = "PRUNEDISK_E2E_UNTRACED";

/// The pid tracing this process (Linux `TracerPid`; 0 when none), or `None` where it cannot be
/// read (no `/proc`, as on Windows, which has no tracer in the e2e run).
fn tracer_pid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("TracerPid:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Most files `PRUNEDISK_E2E_SYNTH_NODES` may ask for.
const MAX_SYNTH_NODES: usize = 10_000_000;

/// `PRUNEDISK_E2E_SYNTH_NODES` (spec AC-9 timings, PR gate F9): at start, show a synthetic size
/// tree of one folder holding this many files (`synth::wide_tree`, seed 16452), its root
/// expanded, through the scan path: a job is started as Start starts one, the tree is installed
/// with `show_walk`, and a one-folder synthetic scan finishes the job (`show_scan`), so the
/// window opens on the results, not on Analysing. No command is added and nothing is read from
/// or written to disk for the tree.
pub fn open_synthetic(inner: &crate::state::Inner) {
    use disk_cleaner_view::synth;
    let Some(v) = std::env::var_os("PRUNEDISK_E2E_SYNTH_NODES") else {
        return;
    };
    let n = match v.to_str().and_then(|s| s.parse::<usize>().ok()) {
        Some(n) if (1..=MAX_SYNTH_NODES).contains(&n) => n,
        _ => {
            eprintln!("e2e: PRUNEDISK_E2E_SYNTH_NODES must be 1 to {MAX_SYNTH_NODES}");
            return;
        }
    };
    if std::env::var_os(UNTRACED).is_some_and(|v| v == "1") {
        match tracer_pid() {
            Some(0) | None => eprintln!("e2e: {UNTRACED}=1: the untraced timing launch"),
            Some(pid) => {
                eprintln!("e2e: {UNTRACED}=1 but traced by pid {pid}: no synthetic tree");
                return;
            }
        }
    }
    // Held until the job is done, as a scan's thread holds it.
    let ticket = match inner.jobs.start() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("e2e: cannot start the synthetic job: {e}");
            return;
        }
    };
    let tree = synth::wide_tree(n, synth::SEED);
    let Some(view) = inner.show_walk(ticket.id, tree, &[PathBuf::from("/wide")]) else {
        eprintln!("e2e: the synthetic job is no longer current");
        return;
    };
    let scan = std::sync::Arc::new(synth::synthetic_scan(1, synth::SEED));
    match inner.show_scan(ticket.id, view, scan) {
        Ok(true) => {}
        Ok(false) => eprintln!("e2e: the synthetic view was replaced"),
        Err(e) => eprintln!("e2e: cannot finish the synthetic job: {e}"),
    }
    drop(ticket);
}
