//! Dry-run plan from a scan (design sections 3, 4 and 6). Nothing here touches the disk.
//!
//! The plan is advice: per classified directory an action, a plain-language reason and the
//! bytes that would be reclaimed. There is no built-in age: `idle_days` and the active set are
//! inputs. A repo whose facts are not all `Yes` never gets a removing action.

use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What the plan proposes for one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Leave it alone.
    Keep,
    /// Delete a regenerable or cache directory.
    RemoveRegenerable,
    /// Save unsaved work to recovery refs or a bundle, re-check, then archive.
    RecoverThenArchive,
    /// Every fact is safe: the repo can be archived and removed.
    Archive,
    /// Back the item up to storage, verify, then remove it.
    BackupThenRemove,
    /// A person decides.
    Ask,
}

impl Action {
    /// JSON and display name.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Keep => "keep",
            Action::RemoveRegenerable => "remove-regenerable",
            Action::RecoverThenArchive => "recover-then-archive",
            Action::Archive => "archive",
            Action::BackupThenRemove => "backup-then-remove",
            Action::Ask => "ask",
        }
    }

    /// Does the action end with something being removed from disk?
    pub fn removes(self) -> bool {
        matches!(
            self,
            Action::RemoveRegenerable
                | Action::RecoverThenArchive
                | Action::Archive
                | Action::BackupThenRemove
        )
    }
}

/// Inputs of the plan. Both thresholds are the user's.
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// Paths the user says are active. Anything under one, or enclosing one, is kept.
    pub active: Vec<PathBuf>,
    /// A project is idle when its newest activity is at least this many days old.
    pub idle_days: u64,
    /// Current time, unix seconds (an input so plans are reproducible).
    pub now: i64,
}

/// One line of the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    /// Index into the scan's `nodes`.
    pub node: usize,
    pub path: String,
    pub class: Class,
    pub action: Action,
    /// Plain-language reason.
    pub reason: String,
    /// Bytes freed if the action completes (0 for keep and ask).
    pub reclaimable_bytes: u64,
    pub bytes_total: u64,
    /// Days since the newest activity evidence, when there is any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_days: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<Facts>,
}

/// Count and bytes for one action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionTotal {
    pub items: u64,
    pub reclaimable_bytes: u64,
}

/// Plan totals. Nested items are not double counted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanTotals {
    /// Bytes freed if every removing action completed.
    pub reclaimable_bytes: u64,
    /// Bytes freed by actions that need no recovery step (`remove-regenerable`, `archive`).
    pub ready_bytes: u64,
    pub by_action: BTreeMap<String, ActionTotal>,
}

/// A dry-run plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub schema_version: u32,
    /// Always true in this phase: a plan changes nothing.
    pub dry_run: bool,
    pub generated_at: i64,
    pub scan_generated_at: i64,
    pub idle_days: u64,
    pub active: Vec<String>,
    pub totals: PlanTotals,
    pub items: Vec<PlanItem>,
}

fn under_active(path: &Path, active: &[PathBuf]) -> Option<String> {
    active
        .iter()
        .find(|a| path.starts_with(a) || a.starts_with(path))
        .map(|a| a.to_string_lossy().into_owned())
}

/// The active path that protects node `idx`: a direct hit, or a hit on a classified directory
/// that encloses it (an active project keeps its regenerable directories too).
fn active_for(scan: &ScanResult, idx: usize, active: &[PathBuf]) -> Option<String> {
    let node = &scan.nodes[idx];
    under_active(Path::new(&node.path), active)
        .or_else(|| node.parent.and_then(|p| active_for(scan, p, active)))
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Newest activity evidence of the project a node belongs to: file modification times and the
/// newest commit. Regenerable directories use their owning project.
fn activity(scan: &ScanResult, idx: usize) -> Option<i64> {
    let node = &scan.nodes[idx];
    if node.class.is_artifact() {
        if let Some(p) = node.parent {
            return activity(scan, p);
        }
    }
    let commit = node.repo.as_ref().and_then(|r| r.last_commit_time);
    max_opt(node.newest_mtime, commit)
}

fn idle_days_of(last: Option<i64>, now: i64) -> Option<u64> {
    last.map(|t| (now.saturating_sub(t).max(0) / 86_400) as u64)
}

fn join_names(names: &[&str]) -> String {
    names.join(", ")
}

/// Build a dry-run plan from a scan.
pub fn plan(scan: &ScanResult, opts: &PlanOptions) -> Plan {
    let mut items: Vec<PlanItem> = Vec::new();
    for (idx, node) in scan.nodes.iter().enumerate() {
        if matches!(node.class, Class::Output | Class::Other) {
            continue;
        }
        let last = activity(scan, idx);
        let idle = idle_days_of(last, opts.now);
        let is_idle = idle.map(|d| d >= opts.idle_days);
        let facts = node.repo.as_ref().map(|r| r.facts);

        let (action, reason, reclaim) = if let Some(a) = active_for(scan, idx, &opts.active) {
            (
                Action::Keep,
                format!("active: under or containing the active path {a}"),
                0,
            )
        } else {
            decide(scan, idx, idle, is_idle, opts)
        };
        items.push(PlanItem {
            node: idx,
            path: node.path.clone(),
            class: node.class,
            action,
            reason,
            reclaimable_bytes: reclaim,
            bytes_total: node.bytes_total,
            idle_days: idle,
            facts,
        });
    }

    // Totals: an item inside another removing item is already counted by the outer one.
    let mut removing = vec![false; scan.nodes.len()];
    for it in &items {
        if it.action.removes() {
            removing[it.node] = true;
        }
    }
    let mut totals = PlanTotals::default();
    for it in &items {
        let slot = totals.by_action.entry(it.action.as_str().to_string()).or_default();
        slot.items += 1;
        let mut nested = false;
        let mut cur = scan.nodes[it.node].parent;
        while let Some(p) = cur {
            if removing[p] {
                nested = true;
                break;
            }
            cur = scan.nodes[p].parent;
        }
        if it.action.removes() && !nested {
            slot.reclaimable_bytes += it.reclaimable_bytes;
            totals.reclaimable_bytes += it.reclaimable_bytes;
            if matches!(it.action, Action::RemoveRegenerable | Action::Archive) {
                totals.ready_bytes += it.reclaimable_bytes;
            }
        }
    }

    Plan {
        schema_version: SCHEMA_VERSION,
        dry_run: true,
        generated_at: opts.now,
        scan_generated_at: scan.generated_at,
        idle_days: opts.idle_days,
        active: opts
            .active
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        totals,
        items,
    }
}

fn decide(
    scan: &ScanResult,
    idx: usize,
    idle: Option<u64>,
    is_idle: Option<bool>,
    opts: &PlanOptions,
) -> (Action, String, u64) {
    let node = &scan.nodes[idx];
    let idle_txt = match idle {
        Some(d) => format!("{d} days"),
        None => "unknown".to_string(),
    };
    match node.class {
        Class::Regenerable | Class::Cache => {
            let what = node.rule.as_deref().unwrap_or("artifact");
            if node.contains_outputs {
                let n = scan.outputs.iter().filter(|o| o.node == Some(idx)).count();
                return (
                    Action::Ask,
                    format!(
                        "{what} directory holds {n} output file(s) (video, archive, model...) \
                         that are not regenerable; not offered for removal until they are \
                         backed up or moved"
                    ),
                    0,
                );
            }
            match is_idle {
                None => (
                    Action::Ask,
                    "no activity evidence for the owning project".to_string(),
                    0,
                ),
                Some(false) => (
                    Action::Keep,
                    format!(
                        "owning project was active {idle_txt} ago, less than the {} day idle threshold",
                        opts.idle_days
                    ),
                    0,
                ),
                Some(true) => (
                    Action::RemoveRegenerable,
                    format!(
                        "{what} is rebuilt by the project's tooling; owning project idle {idle_txt} (threshold {})",
                        opts.idle_days
                    ),
                    node.bytes_total,
                ),
            }
        }
        Class::Repo | Class::Worktree => {
            let facts = match node.repo.as_ref() {
                Some(r) => r.facts,
                None => {
                    return (
                        Action::Ask,
                        "repo facts are missing from the scan".to_string(),
                        0,
                    )
                }
            };
            match is_idle {
                None => (
                    Action::Ask,
                    "no activity evidence (no files and no commits found)".to_string(),
                    0,
                ),
                Some(false) => (
                    Action::Keep,
                    format!(
                        "active {idle_txt} ago, less than the {} day idle threshold",
                        opts.idle_days
                    ),
                    0,
                ),
                Some(true) => {
                    if facts.any_unknown() {
                        (
                            Action::Ask,
                            format!(
                                "cannot prove it is safe: unknown {}",
                                join_names(&facts.unknown())
                            ),
                            0,
                        )
                    } else if !facts.all_yes() {
                        (
                            Action::RecoverThenArchive,
                            format!(
                                "idle {idle_txt}, but not fully saved elsewhere ({}); recover first, then archive",
                                join_names(&facts.failing())
                            ),
                            node.bytes_total,
                        )
                    } else {
                        (
                            Action::Archive,
                            format!(
                                "idle {idle_txt}; every commit is on a network remote, nothing unsaved, no dependents"
                            ),
                            node.bytes_total,
                        )
                    }
                }
            }
        }
        Class::BareRepo => {
            let dependents = node
                .repo
                .as_ref()
                .map(|r| r.facts.no_dependents)
                .unwrap_or(Tri::Unknown);
            if dependents == Tri::No {
                (
                    Action::Keep,
                    "linked worktrees or clones depend on this object store".to_string(),
                    0,
                )
            } else {
                (
                    Action::Ask,
                    "bare repos are never removed automatically".to_string(),
                    0,
                )
            }
        }
        Class::LooseCode => (
            Action::Ask,
            "source outside any repo: connect an origin, back it up raw, remove it, or leave it"
                .to_string(),
            0,
        ),
        Class::Output | Class::Other => (Action::Keep, "analysis only".to_string(), 0),
    }
}
