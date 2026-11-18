//! Dry-run plan from a scan (design sections 3, 4 and 6). Nothing here changes anything on disk.
//!
//! The plan is advice: per classified directory an action, a plain-language reason and the
//! bytes that would be reclaimed. There is no built-in age: the active window and the active
//! paths are inputs. A project is active when an `--active` path covers it, when its newest
//! activity is inside the window, or when a running process has its working directory inside it.
//! A repo whose facts are not all `Yes` never gets a removing action.

use crate::model::*;
use crate::procs::ProcessCheck;
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

/// Whether a project was judged active, idle, or could not be judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectState {
    Active,
    Idle,
    Unknown,
}

/// Inputs of the plan. All three are the user's.
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// Paths the user says are active. Anything under one, or enclosing one, is kept.
    pub active: Vec<PathBuf>,
    /// A project is active when its newest activity is at most this many days old.
    pub active_window_days: u64,
    /// Where the window came from (`default`, a config file path, `--active-window-days`).
    pub window_source: String,
    /// Current time, unix seconds (an input so plans are reproducible).
    pub now: i64,
    /// Working directories of running processes.
    pub process: ProcessCheck,
}

impl PlanOptions {
    /// No active paths, no running processes known, window from the caller.
    pub fn new(active_window_days: u64, now: i64) -> PlanOptions {
        PlanOptions {
            active: Vec::new(),
            active_window_days,
            window_source: "input".to_string(),
            now,
            process: ProcessCheck::none_running(),
        }
    }
}

/// One line of the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    /// Index into the scan's `nodes`.
    pub node: usize,
    pub path: String,
    pub class: Class,
    pub action: Action,
    /// Was the project judged active, idle or unknown, and why.
    pub state: ProjectState,
    pub state_reason: String,
    /// Plain-language reason for the action.
    pub reason: String,
    /// Bytes freed if the action completes (0 for keep and ask).
    pub reclaimable_bytes: u64,
    pub bytes_total: u64,
    /// Whole days since the newest activity evidence, when there is any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days_since_activity: Option<u64>,
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
    /// Bytes freed by `remove-regenerable` (needs no recovery or remote verification step).
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
    /// The window used to decide activity.
    pub active_window_days: u64,
    /// Where the window came from.
    pub window_source: String,
    /// `ok`, `partial` or `unknown`: could running processes be inspected?
    pub process_check: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_check_note: Option<String>,
    /// Always false in this phase: remote facts come from local remote-tracking refs, which can
    /// be stale. Apply must re-check them with `git ls-remote` before removing anything.
    pub remote_facts_verified: bool,
    pub remote_facts_note: String,
    pub active: Vec<String>,
    pub totals: PlanTotals,
    pub items: Vec<PlanItem>,
}

/// Real path of `p`, without the `\\?\` prefix Windows canonicalization adds (process working
/// directories are reported without it).
fn canon(p: &Path) -> PathBuf {
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let text = real.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => real,
    }
}

fn under_active(path: &Path, active: &[PathBuf]) -> Option<String> {
    if active.is_empty() {
        return None;
    }
    // Compare real paths as well as the stored spelling: a scan taken through a symlink or
    // relative path must still match an `--active` path spelled another way.
    let real = canon(path);
    active
        .iter()
        .find(|a| {
            let real_a = canon(a);
            path.starts_with(a)
                || a.starts_with(path)
                || real.starts_with(&real_a)
                || real_a.starts_with(&real)
        })
        .map(|a| a.to_string_lossy().into_owned())
}

/// Resolve the user's `--active` paths: each must exist, is made absolute and symlink-free, and
/// must lie inside or around something the scan covered. A mistyped path must not silently
/// protect nothing.
pub fn resolve_active(
    scan: &ScanResult,
    raw: &[PathBuf],
) -> std::result::Result<Vec<PathBuf>, crate::Error> {
    let covered: Vec<PathBuf> = scan.roots.iter().map(|r| canon(Path::new(r))).collect();
    let mut out = Vec::new();
    for p in raw {
        let real = std::fs::canonicalize(p).map_err(|e| crate::Error::Active {
            path: p.clone(),
            message: format!("cannot use this --active path: {e}"),
        })?;
        let real = canon(&real);
        let related = covered
            .iter()
            .any(|r| real.starts_with(r) || r.starts_with(&real))
            || scan
                .nodes
                .iter()
                .any(|n| canon(Path::new(&n.path)).starts_with(&real));
        if !related {
            return Err(crate::Error::Active {
                path: p.clone(),
                message: "this --active path is not inside or around anything the scan covered"
                    .to_string(),
            });
        }
        out.push(real);
    }
    Ok(out)
}

/// The active path that covers node `idx` or a classified directory enclosing it.
fn active_for(scan: &ScanResult, idx: usize, active: &[PathBuf]) -> Option<String> {
    let node = &scan.nodes[idx];
    under_active(Path::new(&node.path), active)
        .or_else(|| node.parent.and_then(|p| active_for(scan, p, active)))
}

/// A running process's working directory inside node `idx` or an enclosing classified directory.
fn process_in(scan: &ScanResult, idx: usize, process: &ProcessCheck) -> Option<String> {
    let node = &scan.nodes[idx];
    let raw = Path::new(&node.path);
    let real = canon(raw);
    process
        .inside(raw)
        .or_else(|| process.inside(&real))
        .map(|c| format!("{} (working directory of a running process)", c.display()))
        .or_else(|| node.parent.and_then(|p| process_in(scan, p, process)))
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

fn join_names(names: &[&str]) -> String {
    names.join(", ")
}

/// Judge one node: active (path, process or recent activity), idle or unknown, with the reason.
fn judge(scan: &ScanResult, idx: usize, opts: &PlanOptions) -> (ProjectState, String, Option<u64>) {
    let last = activity(scan, idx);
    let days = last.map(|t| (opts.now.saturating_sub(t).max(0) / 86_400) as u64);
    if let Some(a) = active_for(scan, idx, &opts.active) {
        return (
            ProjectState::Active,
            format!("covered by the active path {a}"),
            days,
        );
    }
    if let Some(p) = process_in(scan, idx, &opts.process) {
        return (ProjectState::Active, format!("in use: {p}"), days);
    }
    let window = opts.active_window_days;
    match (last, days) {
        (Some(t), Some(d)) => {
            let age = opts.now.saturating_sub(t).max(0);
            // An absurdly large window saturates instead of wrapping negative.
            let window_secs = i64::try_from(window)
                .unwrap_or(i64::MAX)
                .saturating_mul(86_400);
            if age <= window_secs {
                (
                    ProjectState::Active,
                    format!(
                        "newest activity {d} day(s) ago, within the {window}-day active window"
                    ),
                    days,
                )
            } else {
                (
                    ProjectState::Idle,
                    format!(
                        "newest activity {d} day(s) ago, outside the {window}-day active window"
                    ),
                    days,
                )
            }
        }
        _ => (
            ProjectState::Unknown,
            "no activity evidence (no files and no commits found)".to_string(),
            None,
        ),
    }
}

/// Build a dry-run plan from a scan.
pub fn plan(scan: &ScanResult, opts: &PlanOptions) -> Plan {
    let mut items: Vec<PlanItem> = Vec::new();
    for (idx, node) in scan.nodes.iter().enumerate() {
        if matches!(node.class, Class::Output | Class::Other) {
            continue;
        }
        let (state, state_reason, days) = judge(scan, idx, opts);
        let (mut action, mut reason, mut reclaim) = decide(scan, idx, state, &state_reason);
        if action.removes() {
            if let Some(why) = node.ask_reason.as_deref() {
                reason = format!("{reason}; but it {why}");
                action = Action::Ask;
                reclaim = 0;
            } else if node.unreadable > 0 {
                reason = format!(
                    "{reason}; but {} entr{} inside could not be read, so the scan of it is \
                     incomplete",
                    node.unreadable,
                    if node.unreadable == 1 { "y" } else { "ies" }
                );
                action = Action::Ask;
                reclaim = 0;
            } else if let Some(n) = node
                .repo
                .as_ref()
                .map(|r| r.submodule_gitdirs)
                .filter(|&n| {
                    n > scan
                        .nodes
                        .iter()
                        .filter(|c| c.parent == Some(idx) && c.class.is_git())
                        .count()
                })
            {
                reason = format!(
                    "{reason}; but {n} submodule git director(ies) under .git/modules are not \
                     individually checked, and their stash or local branches would be deleted"
                );
                action = Action::Ask;
                reclaim = 0;
            }
        }
        if action.removes() {
            if let ProcessCheck::Unknown { reason: why } = &opts.process {
                // Cannot prove nothing is using it: a person decides.
                reason = format!("{reason}; but running processes could not be inspected ({why})");
                action = Action::Ask;
                reclaim = 0;
            }
        }
        items.push(PlanItem {
            node: idx,
            path: node.path.clone(),
            class: node.class,
            action,
            state,
            state_reason,
            reason,
            reclaimable_bytes: reclaim,
            bytes_total: node.bytes_total,
            days_since_activity: days,
            facts: node.repo.as_ref().map(|r| r.facts),
        });
    }

    // An enclosing directory goes with everything inside it. When a nested node is kept, asked
    // about, needs recovery, or holds something that cannot be removed, the enclosing node can
    // not be removed either: it becomes `ask`. Children come after parents, so walk backwards and
    // let a downgraded node block its own parent in turn.
    let mut pos: Vec<Option<usize>> = vec![None; scan.nodes.len()];
    for (k, it) in items.iter().enumerate() {
        pos[it.node] = Some(k);
    }
    for k in (0..items.len()).rev() {
        let (node_idx, action) = (items[k].node, items[k].action);
        if !matches!(
            action,
            Action::Keep | Action::Ask | Action::RecoverThenArchive
        ) {
            continue;
        }
        let parent = match scan.nodes[node_idx].parent.and_then(|p| pos[p]) {
            Some(pk) => pk,
            None => continue,
        };
        if items[parent].action.removes() {
            let child = items[k].path.clone();
            let why = match action {
                Action::Keep => "is kept",
                Action::Ask => "needs a person's decision",
                _ => "has unsaved work to recover first",
            };
            items[parent].reason = format!(
                "{}; but the nested {child} {why}, and everything inside goes with this one",
                items[parent].reason
            );
            items[parent].action = Action::Ask;
            items[parent].reclaimable_bytes = 0;
        }
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
        let slot = totals
            .by_action
            .entry(it.action.as_str().to_string())
            .or_default();
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
            // Only regenerable directories are ready without a recovery or verification step:
            // an archive rests on remote facts that stay unverified until apply.
            if it.action == Action::RemoveRegenerable {
                totals.ready_bytes += it.reclaimable_bytes;
            }
        }
    }

    Plan {
        schema_version: SCHEMA_VERSION,
        dry_run: true,
        generated_at: opts.now,
        scan_generated_at: scan.generated_at,
        active_window_days: opts.active_window_days,
        window_source: opts.window_source.clone(),
        process_check: opts.process.status().to_string(),
        process_check_note: opts.process.note(),
        remote_facts_verified: false,
        remote_facts_note:
            "remote facts come from local remote-tracking refs, which can be stale; \
                            they are unverified until apply re-checks them with git ls-remote"
                .to_string(),
        active: opts
            .active
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        totals,
        items,
    }
}

fn decide(scan: &ScanResult, idx: usize, state: ProjectState, why: &str) -> (Action, String, u64) {
    let node = &scan.nodes[idx];
    if state == ProjectState::Active {
        return (Action::Keep, format!("active: {why}"), 0);
    }
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
            match state {
                ProjectState::Unknown => (
                    Action::Ask,
                    format!("cannot judge the owning project: {why}"),
                    0,
                ),
                _ => (
                    Action::RemoveRegenerable,
                    format!(
                        "{what} is rebuilt by the project's tooling; owning project idle: {why}"
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
            if state == ProjectState::Unknown {
                return (Action::Ask, format!("cannot judge activity: {why}"), 0);
            }
            if facts.any_unknown() {
                (
                    Action::Ask,
                    format!(
                        "idle ({why}) but cannot prove it is safe: unknown {}",
                        join_names(&facts.unknown())
                    ),
                    0,
                )
            } else if !facts.all_yes() {
                (
                    Action::RecoverThenArchive,
                    format!(
                        "idle ({why}), but not fully saved elsewhere ({}); recover first, then archive",
                        join_names(&facts.failing())
                    ),
                    node.bytes_total,
                )
            } else {
                (
                    Action::Archive,
                    format!(
                        "idle ({why}); local remote-tracking refs say every commit is on a \
                         network remote, nothing is unsaved and nothing depends on it \
                         (unverified until apply re-checks the remote with git ls-remote)"
                    ),
                    node.bytes_total,
                )
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
