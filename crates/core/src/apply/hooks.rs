//! Test-only hooks. This module exists only under `cfg(test)`: no kill injection, trace or
//! callback is compiled into the library or the binary.

use super::journal::{Entry, Record};
use rustix::io::Errno;
use std::cell::{Cell, RefCell};
use std::path::Path;

/// Where an item is when a stage hook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// After `quarantined` was journaled and before the checks on the copy.
    AfterQuarantined,
    /// After the copy's checks passed and before the last check.
    BeforeLastCheck,
}

type StageFn = Box<dyn FnMut(Stage, &Path)>;
type PurgeFn = Box<dyn FnMut(&str)>;

thread_local! {
    static TRACE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static STAGE: RefCell<Option<StageFn>> = const { RefCell::new(None) };
    static PURGE: RefCell<Option<PurgeFn>> = const { RefCell::new(None) };
    static FORCE_RENAME: Cell<Option<Errno>> = const { Cell::new(None) };
}

/// Make the next no-replace renames of this thread fail with `e` (a file system without the flag).
pub fn force_rename_error(e: Option<Errno>) {
    FORCE_RENAME.with(|c| c.set(e));
}

pub fn forced_rename_error() -> Option<Errno> {
    FORCE_RENAME.with(|c| c.get())
}

/// The record's kind name, as it appears in the journal.
pub fn rec_name(r: &Record) -> &'static str {
    match r {
        Record::Start { .. } => "start",
        Record::Skip { .. } => "skip",
        Record::Quarantining { .. } => "quarantining",
        Record::Quarantined { .. } => "quarantined",
        Record::Restoring { .. } => "restoring",
        Record::RestoredInPlace { .. } => "restored-in-place",
        Record::KeptInQuarantine { .. } => "kept-in-quarantine",
        Record::Removing { .. } => "removing",
        Record::Removed { .. } => "removed",
        Record::End { .. } => "end",
    }
}

/// Called by the journal after a record is fsynced: traces it, and in a child process started by
/// the resume matrix, parks forever after the named record so the parent can SIGKILL it there.
pub fn journal_synced(e: &Entry) {
    let name = rec_name(&e.rec);
    TRACE.with(|t| t.borrow_mut().push(format!("J:{name}")));
    if let Ok(spec) = std::env::var("PD_TEST_PARK_AFTER") {
        // `<record name>:<nth occurrence, from 1>:<marker file>`
        let mut p = spec.splitn(3, ':');
        let (want, nth, marker) = (p.next(), p.next(), p.next());
        if let (Some(want), Some(nth), Some(marker)) = (want, nth, marker) {
            if want == name {
                let n: usize = TRACE.with(|t| {
                    t.borrow()
                        .iter()
                        .filter(|x| **x == format!("J:{name}"))
                        .count()
                });
                if Some(n) == nth.parse().ok() {
                    let _ = std::fs::write(marker, b"parked");
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                }
            }
        }
    }
}

/// Called by `fsops` immediately before a mutating system call.
pub fn syscall(name: &str) {
    TRACE.with(|t| t.borrow_mut().push(format!("S:{name}")));
}

pub fn take_trace() -> Vec<String> {
    TRACE.with(|t| std::mem::take(&mut *t.borrow_mut()))
}

pub fn set_stage(f: Option<StageFn>) {
    STAGE.with(|s| *s.borrow_mut() = f);
}

pub fn stage(st: Stage, path: &Path) {
    STAGE.with(|s| {
        if let Some(f) = s.borrow_mut().as_mut() {
            f(st, path);
        }
    });
}

pub fn set_purge(f: Option<PurgeFn>) {
    PURGE.with(|s| *s.borrow_mut() = f);
}

pub fn before_purge(rel: &[u8]) {
    // The resume matrix parks a child inside the deletion, below the root, to be SIGKILLed.
    if let Ok(marker) = std::env::var("PD_TEST_PARK_PURGE") {
        if !rel.is_empty() {
            let _ = std::fs::write(marker, b"parked");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }
    PURGE.with(|s| {
        if let Some(f) = s.borrow_mut().as_mut() {
            f(&String::from_utf8_lossy(rel));
        }
    });
}
