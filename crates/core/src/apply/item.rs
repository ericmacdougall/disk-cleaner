//! The per-item sequence (class C and the quarantine design):
//!
//! 1. pre-checks (a directory, not a link; no mount point at or below; not reachable by a process);
//! 2. `quarantining`, then a no-replace rename to `<parent>/.prunedisk-q-<plan id>-<idx>`, then
//!    `quarantined` with the identity of the root and the digest of the inventory;
//! 3. checks on the quarantined copy (the scan's classifier, protection, reach, mounts);
//! 4. the last check: the digest is unchanged and nothing new reaches the item;
//! 5. `removing`, then the bottom-up deletion, then `removed`.
//!
//! Any doubt at 3 or 4 restores the item in place (`restoring`, a no-replace rename back,
//! `restored-in-place`, or `kept-in-quarantine` when the original path is taken). Every mutation
//! goes through [`super::fsops`] with the proof an fsynced intent record gives.

use super::classify::Classifier;
use super::fsops::{self, delete_tree, inventory, Dir, FileId, Inventory, Kind, RenameError};
use super::journal::{Journal, SkipReason};
use super::mounts::{at_or_below, MountTable};
use super::{Reach, ReachQuery, Result};
use crate::plan::QUARANTINE_PREFIX;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Everything one item needs from the run.
pub struct ItemCtx<'a> {
    pub journal: &'a mut Journal,
    pub mounts: &'a dyn MountTable,
    pub reach: &'a dyn Reach,
    pub classifier: &'a Classifier<'a>,
    /// Identities of protected roots: never entered by a walk.
    pub forbidden: &'a HashSet<FileId>,
    pub plan_id: &'a str,
}

/// How an item ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Removed(u64),
    Skipped(SkipReason, String),
    Restored(String),
    Kept(String),
}

/// The quarantine name of item `idx` of plan `plan_id`.
pub fn quarantine_name(plan_id: &str, idx: usize) -> String {
    format!("{QUARANTINE_PREFIX}{plan_id}-{idx}")
}

/// Canonical form of `parent`/`name` for comparison with the mount table (the parent is
/// resolved, the name is not).
fn canonical_child(parent: &Path, name: &OsStr) -> PathBuf {
    std::fs::canonicalize(parent)
        .unwrap_or_else(|_| parent.to_path_buf())
        .join(name)
}

/// Is a mount point at or below `path`? An unreadable table counts as one (fail closed).
fn mount_hit(mounts: &dyn MountTable, path: &Path) -> Option<String> {
    match mounts.mount_points() {
        Ok(m) => at_or_below(&m, path).map(|p| format!("mount point {}", p.display())),
        Err(e) => Some(format!("the mount table could not be read: {e}")),
    }
}

fn skip(
    cx: &mut ItemCtx,
    idx: usize,
    path: &str,
    reason: SkipReason,
    detail: String,
) -> Result<Outcome> {
    cx.journal.skip(idx, path, reason, &detail)?;
    Ok(Outcome::Skipped(reason, detail))
}

/// Rename the quarantine back. `restoring` first; `kept-in-quarantine` when the original path is
/// occupied or the rename fails.
fn restore(
    cx: &mut ItemCtx,
    idx: usize,
    parent: &Dir,
    name: &OsStr,
    qname: &OsStr,
    why: &str,
) -> Result<Outcome> {
    let proof = cx.journal.intent_restore(idx, why)?;
    match fsops::rename_noreplace(&proof, parent, qname, parent, name) {
        Ok(()) => {
            let _ = parent.sync();
            cx.journal.restored_in_place(idx, why)?;
            Ok(Outcome::Restored(why.to_string()))
        }
        Err(e) => {
            let msg = format!("{why}; could not restore: {e}");
            cx.journal.kept_in_quarantine(idx, &msg)?;
            Ok(Outcome::Kept(msg))
        }
    }
}

/// Run one `remove-regenerable` item through the sequence.
pub fn process(cx: &mut ItemCtx, idx: usize, path: &str, rule_id: Option<&str>) -> Result<Outcome> {
    let original = Path::new(path);
    let (Some(parent_path), Some(name)) = (original.parent(), original.file_name()) else {
        return skip(cx, idx, path, SkipReason::NotADirectory, "no parent".into());
    };
    let qname = quarantine_name(cx.plan_id, idx);
    let qpath = parent_path.join(&qname);

    // 1. Pre-checks.
    let parent = match Dir::open_path(parent_path) {
        Ok(d) => d,
        Err(e) => {
            return skip(
                cx,
                idx,
                path,
                SkipReason::NotADirectory,
                format!("parent: {e}"),
            );
        }
    };
    let meta = match parent.stat_child(name) {
        Ok(m) => m,
        Err(e) => return skip(cx, idx, path, SkipReason::NotADirectory, e.to_string()),
    };
    if meta.kind != Kind::Dir {
        return skip(
            cx,
            idx,
            path,
            SkipReason::NotADirectory,
            "not a directory (a link or a file now)".into(),
        );
    }
    if cx.forbidden.contains(&meta.id) {
        return skip(
            cx,
            idx,
            path,
            SkipReason::Protected,
            "a protected root".into(),
        );
    }
    if let Some(hit) = mount_hit(cx.mounts, &canonical_child(parent_path, name)) {
        return skip(cx, idx, path, SkipReason::Mountpoint, hit);
    }
    if let Some(f) = cx.reach.reaches(&ReachQuery {
        original,
        quarantine: None,
        ids: None,
    }) {
        return skip(cx, idx, path, SkipReason::InUseUnknown, f.to_string());
    }
    if parent.stat_child(OsStr::new(&qname)).is_ok() {
        return skip(
            cx,
            idx,
            path,
            SkipReason::Rename,
            format!("{qname} already exists"),
        );
    }

    // 2. Intent, no-replace rename, identity and inventory of the quarantined directory.
    let proof =
        cx.journal
            .intent_quarantine(idx, path, &qpath.to_string_lossy(), &parent, rule_id)?;
    if let Err(e) = fsops::rename_noreplace(&proof, &parent, name, &parent, OsStr::new(&qname)) {
        let detail = match &e {
            RenameError::Unsupported => format!("{e} (counted)"),
            _ => e.to_string(),
        };
        return skip(cx, idx, path, SkipReason::Rename, detail);
    }
    let _ = parent.sync();
    let q_os = OsStr::new(&qname);
    let root = match parent.open_child(q_os) {
        Ok(r) if r.id() == meta.id => r,
        Ok(_) => {
            let why = "the directory changed identity at the rename";
            return keep_unrecorded(cx, idx, why);
        }
        Err(e) => return keep_unrecorded(cx, idx, &format!("cannot open the quarantine: {e}")),
    };
    let inv = match inventory(&root, cx.forbidden) {
        Ok(i) => i,
        Err(stop) => {
            return restore(cx, idx, &parent, name, q_os, &format!("walk: {stop}"));
        }
    };
    cx.journal
        .quarantined(idx, root.id(), &inv.digest, inv.entries, inv.bytes)?;
    #[cfg(test)]
    super::hooks::stage(super::hooks::Stage::AfterQuarantined, &qpath);

    // 3. Checks on the copy.
    if let Err(why) = classify_copy(cx, rule_id, &parent, &qname, &qpath) {
        return restore(cx, idx, &parent, name, q_os, &why);
    }
    if let Some(why) = still_clear(cx, original, &qpath, parent_path, &qname, &inv) {
        return restore(cx, idx, &parent, name, q_os, &why);
    }
    #[cfg(test)]
    super::hooks::stage(super::hooks::Stage::BeforeLastCheck, &qpath);

    // 4. Last check, immediately before the destructive call.
    match inventory(&root, cx.forbidden) {
        Ok(fresh) if fresh.digest == inv.digest => {}
        Ok(_) => {
            return restore(cx, idx, &parent, name, q_os, "changed in quarantine");
        }
        Err(stop) => {
            return restore(cx, idx, &parent, name, q_os, &format!("walk: {stop}"));
        }
    }
    match parent.stat_child(q_os) {
        Ok(m) if m.id == root.id() => {}
        _ => return restore(cx, idx, &parent, name, q_os, "the quarantine moved"),
    }
    if let Some(why) = still_clear(cx, original, &qpath, parent_path, &qname, &inv) {
        return restore(cx, idx, &parent, name, q_os, &why);
    }

    // 5. Intent, then the deletion.
    let proof = cx.journal.intent_remove(idx)?;
    match delete_tree(&proof, &parent, q_os, root.id(), Some(&inv.ids)) {
        Ok(()) => {
            cx.journal.removed(idx, inv.bytes)?;
            Ok(Outcome::Removed(inv.bytes))
        }
        Err(stop) => {
            let why = format!("deletion stopped: {stop}");
            cx.journal.kept_in_quarantine(idx, &why)?;
            Ok(Outcome::Kept(why))
        }
    }
}

/// The directory sits in quarantine but its identity could not be trusted: leave it there.
fn keep_unrecorded(cx: &mut ItemCtx, idx: usize, why: &str) -> Result<Outcome> {
    cx.journal.kept_in_quarantine(idx, why)?;
    Ok(Outcome::Kept(why.to_string()))
}

fn classify_copy(
    cx: &ItemCtx,
    rule_id: Option<&str>,
    parent: &Dir,
    qname: &str,
    qpath: &Path,
) -> std::result::Result<(), String> {
    let rule = rule_id.ok_or_else(|| "the plan item has no rule identity".to_string())?;
    cx.classifier.verify_copy(rule, parent, qname, qpath)
}

/// Mounts and processes, against both the original path and the quarantine path. `Some(reason)`
/// when something now stands in the way.
fn still_clear(
    cx: &ItemCtx,
    original: &Path,
    qpath: &Path,
    parent_path: &Path,
    qname: &str,
    inv: &Inventory,
) -> Option<String> {
    let name = original.file_name().unwrap_or_default();
    if let Some(h) = mount_hit(cx.mounts, &canonical_child(parent_path, name)) {
        return Some(h);
    }
    if let Some(h) = mount_hit(cx.mounts, &canonical_child(parent_path, OsStr::new(qname))) {
        return Some(h);
    }
    cx.reach
        .reaches(&ReachQuery {
            original,
            quarantine: Some(qpath),
            ids: Some(&inv.ids),
        })
        .map(|f| f.to_string())
}
