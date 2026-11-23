//! `--resume`: finish or roll back the unfinished items of one plan id's journal.
//!
//! It needs no fresh plan, age or schema, and is not gated by the guard state (an interrupted run
//! must always be finishable). For each unfinished item it opens the parent by path, verifies the
//! parent's and the quarantine root's (dev, ino) against the journal, and then acts relative to
//! those fds:
//!
//! - `removing` without `removed`: finish deleting;
//! - `restoring`, `quarantined` or `quarantining` without `removing`: rename back when the original
//!   path is free, else keep the quarantine.
//!
//! A `.prunedisk-q-*` directory that no journal names is never touched; it is listed.

use super::fsops::{self, delete_tree, Dir, Kind};
use super::journal::{self, fold, read_entries, ItemLog, Journal, Record, State};
use super::mounts::{at_or_below, MountTable};
use super::{Report, Result};
use crate::plan::QUARANTINE_PREFIX;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Every quarantine path (`to`) any journal in the state directory names.
fn journaled_targets(state_dir: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let Ok(rd) = std::fs::read_dir(Journal::dir_of(state_dir)) else {
        return out;
    };
    for e in rd.flatten() {
        if let Ok(entries) = read_entries(&e.path()) {
            for en in entries {
                if let Record::Quarantining { to, .. } = en.rec {
                    out.insert(PathBuf::from(to));
                }
            }
        }
    }
    out
}

/// Reconcile every unfinished item of `journal` (the journal of `plan_id`). `extra_parents` are
/// further directories to look in for unjournaled quarantines (the parents of the plan's items).
pub fn reconcile(
    state_dir: &Path,
    journal: &mut Journal,
    mounts: &dyn MountTable,
    extra_parents: &[PathBuf],
    report: &mut Report,
) -> Result<()> {
    let entries = read_entries(journal.path())?;
    let logs = fold(&entries);
    let mut pending: Vec<&ItemLog> = logs.iter().filter(|l| !l.state.is_final()).collect();
    pending.sort_by_key(|l| l.idx);
    for log in pending {
        let line = reconcile_item(journal, mounts, log, report)?;
        report.lines.push(line);
    }
    // Unjournaled quarantines next to journaled items or plan items.
    let named = journaled_targets(state_dir);
    let mut parents: Vec<PathBuf> = logs
        .iter()
        .filter_map(|l| Path::new(&l.from).parent().map(Path::to_path_buf))
        .collect();
    parents.extend(extra_parents.iter().cloned());
    parents.sort();
    parents.dedup();
    for p in parents {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name();
            if n.to_string_lossy().starts_with(QUARANTINE_PREFIX) && !named.contains(&e.path()) {
                report.unjournaled.push(e.path());
            }
        }
    }
    report.unjournaled.sort();
    Ok(())
}

fn kept(journal: &mut Journal, log: &ItemLog, report: &mut Report, why: String) -> Result<String> {
    journal.kept_in_quarantine(log.idx, &why)?;
    report.kept.push((log.idx, log.from.clone(), why.clone()));
    Ok(format!(
        "item {}: {} kept in quarantine: {why}",
        log.idx, log.from
    ))
}

fn restored(
    journal: &mut Journal,
    log: &ItemLog,
    report: &mut Report,
    note: &str,
) -> Result<String> {
    journal.restored_in_place(log.idx, note)?;
    report
        .restored
        .push((log.idx, log.from.clone(), note.to_string()));
    Ok(format!(
        "item {}: {} restored in place ({note})",
        log.idx, log.from
    ))
}

fn reconcile_item(
    journal: &mut Journal,
    mounts: &dyn MountTable,
    log: &ItemLog,
    report: &mut Report,
) -> Result<String> {
    let from = Path::new(&log.from);
    let to = Path::new(&log.to);
    let (Some(parent_path), Some(name), Some(qname)) =
        (from.parent(), from.file_name(), to.file_name())
    else {
        return kept(
            journal,
            log,
            report,
            "the journal's paths are unusable".into(),
        );
    };
    let parent = match Dir::open_path(parent_path) {
        Ok(d) => d,
        Err(e) => return kept(journal, log, report, format!("cannot open the parent: {e}")),
    };
    if parent.id() != log.parent {
        return kept(
            journal,
            log,
            report,
            "the parent is not the directory the journal recorded (swapped)".into(),
        );
    }
    let qmeta = parent.stat_child(qname);
    match log.state {
        State::Removing => {
            let Ok(qm) = qmeta else {
                // The quarantine is gone: the deletion had finished before `removed` was written.
                journal.removed(log.idx, log.bytes)?;
                report.removed += 1;
                report.bytes += log.bytes;
                return Ok(format!(
                    "item {}: {} was already removed ({} bytes)",
                    log.idx, log.from, log.bytes
                ));
            };
            let Some(root_id) = log.root else {
                return kept(journal, log, report, "no root identity was recorded".into());
            };
            if qm.id != root_id || qm.kind != Kind::Dir {
                return kept(
                    journal,
                    log,
                    report,
                    "the quarantine is not the directory the journal recorded".into(),
                );
            }
            if let Ok(m) = mounts.mount_points() {
                if let Some(hit) = at_or_below(&m, &canonical(parent_path, qname)) {
                    return kept(
                        journal,
                        log,
                        report,
                        format!("mount point {} below the quarantine", hit.display()),
                    );
                }
            } else {
                return kept(
                    journal,
                    log,
                    report,
                    "the mount table could not be read".into(),
                );
            }
            let proof = journal.intent_remove(log.idx)?;
            match delete_tree(&proof, &parent, qname, root_id, None) {
                Ok(()) => {
                    journal.removed(log.idx, log.bytes)?;
                    report.removed += 1;
                    report.bytes += log.bytes;
                    Ok(format!(
                        "item {}: {} removal finished ({} bytes)",
                        log.idx, log.from, log.bytes
                    ))
                }
                Err(stop) => kept(journal, log, report, format!("deletion stopped: {stop}")),
            }
        }
        State::Restoring | State::Quarantined | State::Quarantining => {
            let original = parent.stat_child(name);
            let Ok(qm) = qmeta else {
                return match (log.state, original) {
                    (State::Quarantining, _) => restored(journal, log, report, "nothing was moved"),
                    (_, Ok(_)) => restored(journal, log, report, "the rename back had completed"),
                    _ => restored(journal, log, report, "neither path exists now"),
                };
            };
            if qm.kind != Kind::Dir || log.root.is_some_and(|r| r != qm.id) {
                return kept(
                    journal,
                    log,
                    report,
                    "the quarantine is not the directory the journal recorded".into(),
                );
            }
            if original.is_ok() {
                return kept(
                    journal,
                    log,
                    report,
                    "the original path was recreated while the item sat in quarantine".into(),
                );
            }
            let proof = journal.intent_restore(log.idx, "resume")?;
            match fsops::rename_noreplace(&proof, &parent, qname, &parent, OsStr::new(name)) {
                Ok(()) => {
                    let _ = parent.sync();
                    restored(journal, log, report, "resume")
                }
                Err(e) => kept(journal, log, report, format!("could not rename back: {e}")),
            }
        }
        _ => Ok(String::new()),
    }
}

fn canonical(parent: &Path, name: &OsStr) -> PathBuf {
    std::fs::canonicalize(parent)
        .unwrap_or_else(|_| parent.to_path_buf())
        .join(name)
}

/// For tests and the CLI: the unfinished items of a plan id's journal, without changing anything.
pub fn pending(state_dir: &Path, plan_id: &str) -> Result<Vec<ItemLog>> {
    journal::unfinished(&Journal::path_of(state_dir, plan_id))
}

#[allow(dead_code)]
fn _uses(_: fsops::FileId) {}
