//! The developer view: the analysis (core `scan` and `plan`) grouped by class, with each item's
//! plan action, reason and state, and core's totals shown unchanged. Its numbers come only from
//! the analysis, never from the size walk.

use crate::sanitize::display_name;
use crate::table::MAX_PAGE;
use disk_cleaner_core::plan::PlanTotals;
use disk_cleaner_core::{Class, Config, Plan, PlanOptions, ProcessCheck, ScanResult};
use serde::Serialize;
use std::collections::BTreeMap;

/// Classes in display order, with their group labels.
pub const GROUPS: [(Class, &str); 8] = [
    (Class::Repo, "Repositories"),
    (Class::Worktree, "Worktrees"),
    (Class::BareRepo, "Bare repositories"),
    (Class::Regenerable, "Regenerable folders"),
    (Class::Cache, "Caches"),
    (Class::Output, "Outputs"),
    (Class::LooseCode, "Loose code"),
    (Class::Other, "Other"),
];

/// Seconds in a day.
const DAY: i64 = 86_400;

/// One plan item as the view shows it. Every string is display-safe.
#[derive(Debug, Clone, Serialize)]
pub struct DevItem {
    pub node: usize,
    pub path: String,
    pub class: &'static str,
    pub action: &'static str,
    pub removes: bool,
    pub reason: String,
    pub state: &'static str,
    pub state_reason: String,
    pub reclaimable_bytes: u64,
    pub bytes_total: u64,
    pub files_total: u64,
    pub newest_mtime: Option<i64>,
    pub days_since_activity: Option<u64>,
    /// Git facts that are `no`, and that are `unknown`.
    pub facts_no: Vec<&'static str>,
    pub facts_unknown: Vec<&'static str>,
}

/// One class's items.
#[derive(Debug, Clone, Serialize)]
pub struct DevGroup {
    pub class: &'static str,
    pub label: &'static str,
    pub count: usize,
    /// Core's `totals.bytes_by_class` for this class (0 when absent).
    pub class_bytes: u64,
}

/// Everything the developer view shows, except the item lists, which are paged.
#[derive(Debug, Clone, Serialize)]
pub struct DevSummary {
    /// Always "analysis": these numbers come from core scan and plan.
    pub source: &'static str,
    pub scan_generated_at: i64,
    pub plan_generated_at: i64,
    pub imported: bool,
    pub scan_age_days: i64,
    /// Set when an imported scan is older than the active window.
    pub stale_warning: Option<String>,
    /// Set for an imported scan: the process check describes this machine.
    pub imported_note: Option<String>,
    pub groups: Vec<DevGroup>,
    pub bytes_by_class: BTreeMap<String, u64>,
    pub plan_totals: PlanTotals,
    pub coverage_complete: bool,
    pub coverage_note: Option<String>,
    pub dry_run: bool,
    pub remote_facts_verified: bool,
    pub remote_facts_note: String,
    pub process_check: String,
    pub process_check_note: Option<String>,
    pub active_window_days: u64,
    pub window_source: String,
    pub roots: Vec<String>,
}

/// The developer view of one scan and its plan.
#[derive(Debug, Clone)]
pub struct DevView {
    pub summary: DevSummary,
    items: Vec<Vec<DevItem>>,
}

/// Plan inputs taken from the settings.
#[derive(Debug, Clone)]
pub struct PlanInputs {
    pub config: Config,
    /// Where the window came from (`default` or the config file path), shown in the plan.
    pub window_source: String,
}

/// Build the plan for a scan with the given settings, at time `now`.
pub fn make_plan(scan: &ScanResult, inputs: &PlanInputs, now: i64, process: ProcessCheck) -> Plan {
    let mut opts = PlanOptions::new(inputs.config.active_window_days, now);
    opts.window_source = inputs.window_source.clone();
    opts.process = process;
    opts.trust_partial_process_check = inputs.config.trust_partial_process_check;
    opts.accept_incomplete_coverage = inputs.config.accept_incomplete_coverage;
    opts.max_git_admin_bytes = inputs.config.max_git_admin_bytes;
    disk_cleaner_core::plan(scan, &opts)
}

fn group_index(class: Class) -> usize {
    GROUPS
        .iter()
        .position(|(c, _)| *c == class)
        .unwrap_or(GROUPS.len() - 1)
}

impl DevView {
    /// Build the view. `imported` is true for a scan read from a file; `now` is unix seconds.
    pub fn new(scan: &ScanResult, plan: &Plan, imported: bool, now: i64) -> DevView {
        let mut items: Vec<Vec<DevItem>> = vec![Vec::new(); GROUPS.len()];
        for it in &plan.items {
            let node = scan.nodes.get(it.node);
            let (no, unknown) = match &it.facts {
                Some(f) => (f.failing(), f.unknown()),
                None => (Vec::new(), Vec::new()),
            };
            items[group_index(it.class)].push(DevItem {
                node: it.node,
                path: display_name(&it.path),
                class: it.class.as_str(),
                action: it.action.as_str(),
                removes: it.action.removes(),
                reason: display_name(&it.reason),
                state: match it.state {
                    disk_cleaner_core::ProjectState::Active => "active",
                    disk_cleaner_core::ProjectState::Idle => "idle",
                    disk_cleaner_core::ProjectState::Unknown => "unknown",
                },
                state_reason: display_name(&it.state_reason),
                reclaimable_bytes: it.reclaimable_bytes,
                bytes_total: it.bytes_total,
                files_total: node.map(|n| n.files_total).unwrap_or(0),
                newest_mtime: node.and_then(|n| n.newest_mtime),
                days_since_activity: it.days_since_activity,
                facts_no: no,
                facts_unknown: unknown,
            });
        }
        for list in &mut items {
            list.sort_by(|a, b| {
                b.reclaimable_bytes
                    .cmp(&a.reclaimable_bytes)
                    .then_with(|| b.bytes_total.cmp(&a.bytes_total))
                    .then_with(|| a.path.cmp(&b.path))
            });
        }
        let groups = GROUPS
            .iter()
            .zip(&items)
            .map(|((class, label), list)| DevGroup {
                class: class.as_str(),
                label: *label,
                count: list.len(),
                class_bytes: scan
                    .totals
                    .bytes_by_class
                    .get(class.as_str())
                    .copied()
                    .unwrap_or(0),
            })
            .collect();
        let age_secs = now.saturating_sub(scan.generated_at).max(0);
        let scan_age_days = age_secs / DAY;
        let window = plan.active_window_days;
        let stale_warning =
            (imported && age_secs > (window as i64).saturating_mul(DAY)).then(|| {
                format!(
                    "This scan is {scan_age_days} days old, older than the {window}-day active \
                 window, so projects can look idle when they are not. Scan again for a current \
                 plan."
                )
            });
        let imported_note = imported.then(|| {
            "This scan was read from a file. The running-process check describes this \
             computer, not the one that made the scan."
                .to_string()
        });
        DevView {
            summary: DevSummary {
                source: "analysis",
                scan_generated_at: scan.generated_at,
                plan_generated_at: plan.generated_at,
                imported,
                scan_age_days,
                stale_warning,
                imported_note,
                groups,
                bytes_by_class: scan.totals.bytes_by_class.clone(),
                plan_totals: plan.totals.clone(),
                coverage_complete: plan.coverage_complete,
                coverage_note: plan.coverage_note.as_deref().map(display_name),
                dry_run: plan.dry_run,
                remote_facts_verified: plan.remote_facts_verified,
                remote_facts_note: display_name(&plan.remote_facts_note),
                process_check: plan.process_check.clone(),
                process_check_note: plan.process_check_note.as_deref().map(display_name),
                active_window_days: plan.active_window_days,
                window_source: display_name(&plan.window_source),
                roots: scan.roots.iter().map(|r| display_name(r)).collect(),
            },
            items,
        }
    }

    /// Items of one class, `offset .. offset + limit` (limit capped at 1,000).
    pub fn page(&self, class: &str, offset: usize, limit: usize) -> Vec<DevItem> {
        let Some(g) = GROUPS.iter().position(|(c, _)| c.as_str() == class) else {
            return Vec::new();
        };
        let list = &self.items[g];
        let start = offset.min(list.len());
        let end = start.saturating_add(limit.min(MAX_PAGE)).min(list.len());
        list[start..end].to_vec()
    }

    /// Every item, in group order (for the TUI and exports).
    pub fn all(&self) -> impl Iterator<Item = &DevItem> {
        self.items.iter().flatten()
    }

    /// The items of group `g` (an index into [`GROUPS`]); empty when out of range.
    pub fn items(&self, g: usize) -> &[DevItem] {
        self.items.get(g).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{synthetic_scan, EPOCH, SEED};

    fn inputs(days: u64) -> PlanInputs {
        PlanInputs {
            config: Config {
                active_window_days: days,
                ..Config::default()
            },
            window_source: "default".into(),
        }
    }

    #[test]
    fn groups_follow_core_and_totals_are_unchanged() {
        let scan = synthetic_scan(300, SEED);
        let plan = make_plan(&scan, &inputs(7), EPOCH, ProcessCheck::none_running());
        let v = DevView::new(&scan, &plan, false, EPOCH);
        let n: usize = v.summary.groups.iter().map(|g| g.count).sum();
        assert_eq!(n, plan.items.len());
        assert_eq!(
            v.summary.plan_totals.reclaimable_bytes,
            plan.totals.reclaimable_bytes
        );
        assert_eq!(v.summary.bytes_by_class, scan.totals.bytes_by_class);
        assert!(v.summary.dry_run);
        assert!(!v.summary.remote_facts_verified);
        assert_eq!(v.summary.source, "analysis");
        // Repositories with unknown facts never get a removing action.
        for it in v.page("repo", 0, 1000) {
            assert!(!it.removes, "{}: {}", it.path, it.action);
        }
        assert!(v.page("no-such-class", 0, 10).is_empty());
        assert!(v.page("repo", 0, 10_000_000).len() <= MAX_PAGE);
    }

    #[test]
    fn an_old_imported_scan_is_flagged() {
        let scan = synthetic_scan(10, SEED);
        let now = EPOCH + 30 * DAY;
        let plan = make_plan(&scan, &inputs(7), now, ProcessCheck::none_running());
        let v = DevView::new(&scan, &plan, true, now);
        assert_eq!(v.summary.scan_age_days, 30);
        assert!(v
            .summary
            .stale_warning
            .as_deref()
            .unwrap()
            .contains("30 days old"));
        assert!(v.summary.imported_note.is_some());
        let fresh = DevView::new(&scan, &plan, false, now);
        assert!(fresh.summary.stale_warning.is_none());
        assert!(fresh.summary.imported_note.is_none());
    }
}
