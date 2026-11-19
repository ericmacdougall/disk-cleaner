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
    /// The user accepts a partial process check (some processes could not be inspected) as
    /// enough to remove things. Off by default: an unreadable process may be working inside the
    /// candidate.
    pub trust_partial_process_check: bool,
    /// The user wants a scan that could not look everywhere (unreadable folders, mount points not
    /// entered) to block archiving: every repository's `no_dependents = yes` becomes `unknown`.
    /// Off by default: the plan then only discloses what was not read.
    pub require_full_coverage: bool,
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
            trust_partial_process_check: false,
            require_full_coverage: false,
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
    /// The user chose to treat a partial process check as sufficient
    /// (`trust_partial_process_check` / `--trust-partial-process-check`).
    #[serde(default)]
    pub trusted_partial_process_check: bool,
    /// Always false in this phase: remote facts come from local remote-tracking refs, which can
    /// be stale. Apply must re-check them with `git ls-remote` before removing anything.
    pub remote_facts_verified: bool,
    pub remote_facts_note: String,
    /// Scan roots inside a repository the scan did not cover: that repository was not assessed.
    #[serde(default)]
    pub enclosing_repos: Vec<EnclosingRepo>,
    /// `volume` when every scanned root is a mount point, else `subtree`. `no_dependents` means
    /// no dependents were found within the scanned roots; a subtree scan cannot see clones
    /// elsewhere.
    #[serde(default = "crate::model::default_scope")]
    pub scope: String,
    /// The scan read every folder and entered every mount point (its `coverage.complete`).
    pub coverage_complete: bool,
    /// When coverage is incomplete: one sentence saying what was not read and that links or
    /// clones there were not checked. Every archive and recover-then-archive reason repeats it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_note: Option<String>,
    /// The user asked for full coverage (`require_full_coverage` / `--require-full-coverage`):
    /// with incomplete coverage every repository's `no_dependents = yes` became `unknown`.
    #[serde(default)]
    pub required_full_coverage: bool,
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

/// A running process's working directory inside this node's directory.
fn process_hit(node: &Node, process: &ProcessCheck) -> Option<String> {
    let raw = Path::new(&node.path);
    let real = canon(raw);
    process
        .inside(raw)
        .or_else(|| process.inside(&real))
        .map(|c| format!("{} (working directory of a running process)", c.display()))
}

/// What the plan needs to know about every node, computed once with no recursion (a scan file can
/// describe a tree of any depth): parents first, then per-node results inherited down the tree.
///
/// Parent links that are out of range or form a cycle are cut (the scan loader rejects them; this
/// keeps `plan` total when a caller builds a scan by hand).
struct Ctx {
    /// Parent of each node, cycles and bad indexes removed.
    parent: Vec<Option<usize>>,
    /// Node indexes, every node after its parent.
    order: Vec<usize>,
    /// Active path covering the node or a node enclosing it.
    active: Vec<Option<String>>,
    /// Running process inside the node or a node enclosing it.
    process: Vec<Option<String>>,
    /// Newest activity evidence of the project the node belongs to.
    activity: Vec<Option<i64>>,
}

fn build_ctx(scan: &ScanResult, opts: &PlanOptions) -> Ctx {
    let n = scan.nodes.len();
    let mut parent: Vec<Option<usize>> = scan
        .nodes
        .iter()
        .enumerate()
        .map(|(i, nd)| nd.parent.filter(|p| *p < n && *p != i))
        .collect();
    let mut depth = vec![usize::MAX; n];
    let mut on_path = vec![false; n];
    for start in 0..n {
        if depth[start] != usize::MAX {
            continue;
        }
        let mut path: Vec<usize> = Vec::new();
        let mut cur = Some(start);
        let mut base = 0usize;
        while let Some(c) = cur {
            if depth[c] != usize::MAX {
                base = depth[c] + 1;
                break;
            }
            if on_path[c] {
                // A cycle: cut the link that closed it.
                if let Some(&last) = path.last() {
                    parent[last] = None;
                }
                base = 0;
                break;
            }
            on_path[c] = true;
            path.push(c);
            cur = parent[c];
        }
        for (off, &c) in path.iter().rev().enumerate() {
            depth[c] = base + off;
            on_path[c] = false;
        }
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| depth[i]);

    let mut active: Vec<Option<String>> = vec![None; n];
    let mut process: Vec<Option<String>> = vec![None; n];
    let mut activity: Vec<Option<i64>> = vec![None; n];
    for &i in &order {
        let node = &scan.nodes[i];
        let par = parent[i];
        active[i] = under_active(Path::new(&node.path), &opts.active)
            .or_else(|| par.and_then(|p| active[p].clone()));
        process[i] =
            process_hit(node, &opts.process).or_else(|| par.and_then(|p| process[p].clone()));
        // Regenerable directories use their owning project.
        activity[i] = match par {
            Some(p) if node.class.is_artifact() => activity[p],
            _ => {
                let commit = node.repo.as_ref().and_then(|r| r.last_commit_time);
                max_opt(node.newest_mtime, commit)
            }
        };
    }
    Ctx {
        parent,
        order,
        active,
        process,
        activity,
    }
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn join_names(names: &[&str]) -> String {
    names.join(", ")
}

/// Judge one node: active (path, process or recent activity), idle or unknown, with the reason.
fn judge(ctx: &Ctx, idx: usize, opts: &PlanOptions) -> (ProjectState, String, Option<u64>) {
    let last = ctx.activity[idx];
    let days = last.map(|t| (opts.now.saturating_sub(t).max(0) / 86_400) as u64);
    if let Some(a) = ctx.active[idx].as_deref() {
        return (
            ProjectState::Active,
            format!("covered by the active path {a}"),
            days,
        );
    }
    if let Some(p) = ctx.process[idx].as_deref() {
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

/// A submodule git directory under this repo's `.git/modules` that no scanned git node inside
/// the repo uses (matched by identity, not by count). Its stash and branches are deleted with
/// the repo and nobody has looked at them.
fn orphan_submodule_gitdir(scan: &ScanResult, idx: usize) -> Option<String> {
    let dirs = &scan.nodes[idx].repo.as_ref()?.submodule_gitdirs;
    if dirs.is_empty() {
        return None;
    }
    let inside = |j: usize| -> bool {
        let mut cur = scan.nodes[j].parent.filter(|p| *p < scan.nodes.len());
        let mut steps = 0usize;
        while let Some(p) = cur {
            if p == idx {
                return true;
            }
            steps += 1;
            if steps > scan.nodes.len() {
                return false;
            }
            cur = scan.nodes[p].parent.filter(|q| *q < scan.nodes.len());
        }
        false
    };
    let owned: Vec<PathBuf> = scan
        .nodes
        .iter()
        .enumerate()
        .filter(|(j, n)| n.class.is_git() && inside(*j))
        .filter_map(|(_, n)| {
            n.repo
                .as_ref()?
                .git_dir
                .as_ref()
                .map(|g| canon(Path::new(g)))
        })
        .collect();
    dirs.iter()
        .find(|d| !owned.iter().any(|o| *o == canon(Path::new(d.as_str()))))
        .cloned()
}

/// Build a dry-run plan from a scan.
pub fn plan(scan: &ScanResult, opts: &PlanOptions) -> Plan {
    let ctx = build_ctx(scan, opts);
    let complete = scan.coverage.complete;
    // Strict coverage: what was not read, when the user asked for that to block archiving.
    let strict: Option<String> = if opts.require_full_coverage && !complete {
        Some(coverage_gaps(&scan.coverage))
    } else {
        None
    };
    let mut items: Vec<PlanItem> = Vec::new();
    for (idx, node) in scan.nodes.iter().enumerate() {
        if matches!(node.class, Class::Output | Class::Other) {
            continue;
        }
        let (state, state_reason, days) = judge(&ctx, idx, opts);
        let (mut action, mut reason, mut reclaim) =
            decide(scan, idx, state, &state_reason, strict.as_deref());
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
            } else if node.skipped_mounts > 0 {
                let n = node.skipped_mounts;
                let (y, is, was) = if n == 1 {
                    ("y", "is", "was")
                } else {
                    ("ies", "are", "were")
                };
                reason = format!(
                    "{reason}; but {n} director{y} inside {is} on another file system and {was} \
                     not scanned (--one-file-system), so what is there was never looked at"
                );
                action = Action::Ask;
                reclaim = 0;
            } else if let Some(orphan) = orphan_submodule_gitdir(scan, idx) {
                reason = format!(
                    "{reason}; but the submodule git directory {orphan} under .git/modules has \
                     no checked-out, scanned worktree, so its stash or local branches would be \
                     deleted unseen"
                );
                action = Action::Ask;
                reclaim = 0;
            }
        }
        if action.removes() {
            match &opts.process {
                ProcessCheck::Unknown { reason: why } => {
                    // Cannot prove nothing is using it: a person decides.
                    reason =
                        format!("{reason}; but running processes could not be inspected ({why})");
                    action = Action::Ask;
                    reclaim = 0;
                }
                ProcessCheck::Partial { unreadable, .. } if !opts.trust_partial_process_check => {
                    // One of the processes nobody could look at may be working inside it.
                    reason = format!(
                        "{reason}; but {unreadable} running process(es) could not be inspected, \
                         so one of them may be using it (--trust-partial-process-check accepts \
                         that risk)"
                    );
                    action = Action::Ask;
                    reclaim = 0;
                }
                _ => {}
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
            facts: node
                .repo
                .as_ref()
                .map(|r| effective_facts(r.facts, strict.is_some())),
        });
    }

    // An enclosing directory goes with everything inside it. When a nested node is kept, asked
    // about, needs recovery, or holds something that cannot be removed, the enclosing node can
    // not be removed either: it becomes `ask`. Deepest nodes first, so a downgraded node blocks
    // its own parent in turn (no assumption about the order of `nodes`).
    let mut pos: Vec<Option<usize>> = vec![None; scan.nodes.len()];
    for (k, it) in items.iter().enumerate() {
        pos[it.node] = Some(k);
    }
    for &node_idx in ctx.order.iter().rev() {
        let k = match pos[node_idx] {
            Some(k) => k,
            None => continue,
        };
        let action = items[k].action;
        if !matches!(
            action,
            Action::Keep | Action::Ask | Action::RecoverThenArchive
        ) {
            continue;
        }
        let parent = match ctx.parent[node_idx].and_then(|p| pos[p]) {
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

    // Every decision that rests on "nothing depends on it" says where the scan could not look.
    // Last, so it is only added to items that are still archived or recovered.
    if let Some(gap) = coverage_reason(&scan.coverage, complete) {
        for it in items.iter_mut() {
            if matches!(it.action, Action::Archive | Action::RecoverThenArchive) {
                it.reason.push_str(&gap);
            }
        }
    }

    // Totals: an item inside another removing item is already counted by the outer one.
    let mut removing = vec![false; scan.nodes.len()];
    for it in &items {
        if it.action.removes() {
            removing[it.node] = true;
        }
    }
    // Is any enclosing node removing? Parents come first in `order`, so one pass is enough.
    let mut nested_in_removing = vec![false; scan.nodes.len()];
    for &i in &ctx.order {
        nested_in_removing[i] = ctx.parent[i]
            .map(|p| removing[p] || nested_in_removing[p])
            .unwrap_or(false);
    }
    let mut totals = PlanTotals::default();
    for it in &items {
        let slot = totals
            .by_action
            .entry(it.action.as_str().to_string())
            .or_default();
        slot.items += 1;
        if it.action.removes() && !nested_in_removing[it.node] {
            slot.reclaimable_bytes = slot.reclaimable_bytes.saturating_add(it.reclaimable_bytes);
            totals.reclaimable_bytes = totals
                .reclaimable_bytes
                .saturating_add(it.reclaimable_bytes);
            // Only regenerable directories are ready without a recovery or verification step:
            // an archive rests on remote facts that stay unverified until apply.
            if it.action == Action::RemoveRegenerable {
                totals.ready_bytes = totals.ready_bytes.saturating_add(it.reclaimable_bytes);
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
        trusted_partial_process_check: opts.trust_partial_process_check,
        remote_facts_verified: false,
        remote_facts_note:
            "remote facts come from local remote-tracking refs, which can be stale; \
                            they are unverified until apply re-checks them with git ls-remote"
                .to_string(),
        enclosing_repos: scan.enclosing_repos.clone(),
        scope: scan.scope.clone(),
        coverage_complete: complete,
        coverage_note: coverage_note(&scan.coverage, complete),
        required_full_coverage: opts.require_full_coverage,
        active: opts
            .active
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        totals,
        items,
    }
}

/// The places the scan could not look, each as (`"<n> folder(s)"`, verb phrase), and how many
/// there are in all.
fn gaps(c: &Coverage) -> (Vec<(String, &'static str)>, u64) {
    let kinds = [
        (
            c.unreadable_dirs,
            "folder",
            "folders",
            "could not be read",
            "could not be read",
        ),
        (
            c.other_unreadable_entries,
            "other entry",
            "other entries",
            "could not be read",
            "could not be read",
        ),
        (
            c.mount_points_skipped,
            "mount point",
            "mount points",
            "was not entered (--one-file-system)",
            "were not entered (--one-file-system)",
        ),
    ];
    let mut out = Vec::new();
    let mut total = 0u64;
    for (n, one, many, verb_one, verb_many) in kinds {
        if n > 0 {
            total = total.saturating_add(n);
            if n == 1 {
                out.push((format!("1 {one}"), verb_one));
            } else {
                out.push((format!("{n} {many}"), verb_many));
            }
        }
    }
    (out, total)
}

fn it_or_them(total: u64) -> &'static str {
    if total == 1 {
        "it"
    } else {
        "them"
    }
}

/// `1 folder could not be read and 2 mount points were not entered (--one-file-system)`.
fn coverage_gaps(c: &Coverage) -> String {
    let (parts, _) = gaps(c);
    if parts.is_empty() {
        return "the scan did not record where it could not look".to_string();
    }
    parts
        .iter()
        .map(|(noun, verb)| format!("{noun} {verb}"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The plan header's sentence for an incomplete scan.
fn coverage_note(c: &Coverage, complete: bool) -> Option<String> {
    if complete {
        return None;
    }
    let (_, total) = gaps(c);
    Some(format!(
        "Coverage is incomplete: {}, so links or clones inside {} were not checked.",
        coverage_gaps(c),
        it_or_them(total)
    ))
}

/// The ending of every archive and recover-then-archive reason when coverage is incomplete:
/// `; 1 folder in this scan could not be read; dependents inside it were not checked`.
fn coverage_reason(c: &Coverage, complete: bool) -> Option<String> {
    if complete {
        return None;
    }
    let (parts, total) = gaps(c);
    if parts.is_empty() {
        return Some(
            "; the scan did not record where it could not look, so dependents may have been \
             missed"
                .to_string(),
        );
    }
    let mut text = String::new();
    for (k, (noun, verb)) in parts.iter().enumerate() {
        if k == 0 {
            text.push_str(&format!("{noun} in this scan {verb}"));
        } else {
            text.push_str(&format!(" and {noun} {verb}"));
        }
    }
    Some(format!(
        "; {text}; dependents inside {} were not checked",
        it_or_them(total)
    ))
}

/// A repository's facts as the plan uses them: under strict coverage an unproven
/// `no_dependents = yes` is `unknown`.
fn effective_facts(mut facts: Facts, strict: bool) -> Facts {
    if strict && facts.no_dependents == Tri::Yes {
        facts.no_dependents = Tri::Unknown;
    }
    facts
}

/// Why a repository that others depend on asks: up to three of them are named.
fn dependents_reason(node: &Node) -> String {
    let mut who: Vec<String> = Vec::new();
    let mut precious = false;
    if let Some(r) = node.repo.as_ref() {
        who.extend(r.dependents.iter().cloned());
        for w in &r.worktrees {
            if !who.contains(w) {
                who.push(w.clone());
            }
        }
        precious = r.precious_objects;
    }
    if precious {
        let named = if who.is_empty() {
            String::new()
        } else {
            let shown = who.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            format!(" (known: {shown})")
        };
        return format!(
            "this repository sets extensions.preciousObjects, which declares that other \
             repositories borrow its objects{named}; deal with those first"
        );
    }
    if who.is_empty() {
        return "other checkouts or clones use this repository's objects or refs; deal with those \
                first"
            .to_string();
    }
    let shown = who.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let rest = who.len().saturating_sub(3);
    let more = if rest > 0 {
        format!(" and {rest} more")
    } else {
        String::new()
    };
    format!(
        "{} other checkouts or links use this repository's objects or refs: {shown}{more}; deal \
         with those first",
        who.len()
    )
}

/// For each unknown fact: what could not be checked and what would settle it. `strict` is what
/// the scan could not read when full coverage was required.
fn explain_unknown(node: &Node, facts: &Facts, strict: Option<&str>) -> String {
    let notes = node
        .repo
        .as_ref()
        .map(|r| r.notes.join(" "))
        .unwrap_or_default();
    // Was `no_dependents` itself unknown in the scan, or only made so by strict coverage?
    let scanned_unknown = node
        .repo
        .as_ref()
        .map(|r| r.facts.no_dependents == Tri::Unknown)
        .unwrap_or(true);
    let parts: Vec<String> = facts
        .unknown()
        .into_iter()
        .map(|name| {
            if let (Some(gap), "no_dependents", false) = (strict, name, scanned_unknown) {
                return format!(
                    "full coverage was required (require_full_coverage or \
                     --require-full-coverage) and {gap}, so links or clones there may depend \
                     on this repository; make them readable and scan again, or plan without \
                     that setting"
                );
            }
            let text: &str = match name {
                "has_network_remote" => {
                    "the remotes could not be read (git failed); run the scan again with git working"
                }
                "head_on_remote" => {
                    "HEAD could not be compared with the remote-tracking refs (git failed, or there \
                     is no commit yet); run the scan again"
                }
                "all_branches_on_remote" if notes.contains("has tags") => {
                    "this repository has local tags and tag names cannot be checked against the \
                     remote without contacting it; they will be verified when a removal is applied"
                }
                "all_branches_on_remote" => {
                    "the local branches could not be compared with the remote-tracking refs (git \
                     failed); run the scan again"
                }
                "stash_empty" => "the stash could not be read (git failed); run the scan again",
                "tree_clean" => {
                    "whether the working tree is clean could not be established (git failed, or a \
                     submodule could not be read); run the scan again"
                }
                "no_unique_ignored" => {
                    "the ignored files could not be listed (git failed); run the scan again"
                }
                "no_dependents" => {
                    "whether other repositories use this one's objects could not be established (an \
                     alternates file, symlink or git directory could not be resolved or lies \
                     outside the scanned folders); scan the folders that hold those repositories \
                     too"
                }
                "reflog_saved" => {
                    "commits kept only by the reflog could not be compared with the remote (git \
                     failed or timed out, or there is no commit yet); run the scan again"
                }
                "no_pending_state" => {
                    "an unfinished rebase, merge, cherry-pick, revert or bisect, or local LFS or \
                     annex content, is present or could not be checked; finish or abandon it"
                }
                _ => "a safety check could not be made; run the scan again",
            };
            text.to_string()
        })
        .collect();
    parts.join("; ")
}

/// Disclosure for a scan that covered only folders, not whole volumes.
fn scope_note(scan: &ScanResult) -> &'static str {
    if scan.scope == "volume" {
        ""
    } else {
        "; other clones outside the scanned folders were not checked for dependents"
    }
}

/// `strict` is set when the user required full coverage and the scan did not have it: what was
/// not read.
fn decide(
    scan: &ScanResult,
    idx: usize,
    state: ProjectState,
    why: &str,
    strict: Option<&str>,
) -> (Action, String, u64) {
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
                Some(r) => effective_facts(r.facts, strict.is_some()),
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
            if facts.plain_layout != Tri::Yes {
                // Only a repository with the plain layout is ever archived: anything else may
                // keep data in places this tool does not look.
                let issue = node
                    .repo
                    .as_ref()
                    .and_then(|r| r.layout_issue.clone())
                    .unwrap_or_else(|| "the layout could not be checked".to_string());
                return (
                    Action::Ask,
                    format!("unusual repository layout: {issue}"),
                    0,
                );
            }
            if facts.no_dependents == Tri::No {
                // Recovery saves this repository's own refs; it cannot make the repositories
                // that use its objects or refs safe, so it must not be removed first.
                return (Action::Ask, dependents_reason(node), 0);
            }
            if facts.any_unknown() {
                (
                    Action::Ask,
                    format!(
                        "idle ({why}) but cannot prove it is safe: {}",
                        explain_unknown(node, &facts, strict)
                    ),
                    0,
                )
            } else if !facts.all_yes() {
                (
                    Action::RecoverThenArchive,
                    format!(
                        "idle ({why}), but not fully saved elsewhere ({}); recover first, then \
                         archive{}",
                        join_names(&facts.failing()),
                        scope_note(scan)
                    ),
                    node.bytes_total,
                )
            } else {
                (
                    Action::Archive,
                    format!(
                        "idle ({why}); local remote-tracking refs say every commit (reflog \
                         entries included) is on a network remote, nothing is unsaved, no \
                         operation is half done and nothing depends on it{} \
                         (unverified until apply re-checks the remote with git ls-remote)",
                        scope_note(scan)
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
        Class::LooseCode => {
            let inside = scan
                .enclosing_repos
                .iter()
                .any(|e| Path::new(&node.path).starts_with(&e.root));
            let why = if inside {
                "source inside a repository this scan did not cover (see the header): assess \
                 that repository, or leave it"
            } else {
                "source outside any repo: connect an origin, back it up raw, remove it, or \
                 leave it"
            };
            (Action::Ask, why.to_string(), 0)
        }
        Class::Output | Class::Other => (Action::Keep, "analysis only".to_string(), 0),
    }
}
