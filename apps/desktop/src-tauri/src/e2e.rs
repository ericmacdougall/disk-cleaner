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
