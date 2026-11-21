//! Test builds only (the `e2e` feature; release bundles are built without it, and the bundle
//! job checks the release binary holds none of these names). The WebDriver checks cannot click
//! native dialogs, so each dialog answers from an environment variable instead, inside the
//! same command, which then goes on exactly as after a real pick. No command is added.
//!
//! - `PRUNEDISK_E2E_FOLDERS`: the folders the folder dialog picks (a path list, as `PATH`).
//! - `PRUNEDISK_E2E_OPEN`: the files the open dialog picks, one per call, in order; after the
//!   last, the dialog is cancelled.
//! - `PRUNEDISK_E2E_EXPORT`: the files the save dialog names, one per call, likewise.

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
