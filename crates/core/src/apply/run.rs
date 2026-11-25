//! The run order of class A, in one place.

use super::classify::Classifier;
use super::fsops::FileId;
use super::gates::{courtesy_refusal, plan_id, validate_sandbox};
use super::item::{self, ItemCtx, Outcome as ItemOutcome};
use super::journal::{self, Journal, SkipReason, Totals};
use super::resume;
use super::{ApplyError, Env, Outcome, Report, Request, Result};
use crate::model::{Class, ProtectContext};
use crate::plan::{plan_options_from, Action, Plan, PlanItem};
use crate::rules::RuleSet;
use crate::scan::{scan, ScanOptions};
use crate::{plan as make_plan, plan_from_json, protect, resolve_active};
use rustix::fs::FlockOperation;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

fn refused(msg: impl Into<String>) -> Result<Outcome> {
    Ok(Outcome::Refused(msg.into()))
}

/// A plan id as the journal's file name: 16 lowercase hex characters, nothing else.
fn is_plan_id(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The items `apply` would act on: `remove-regenerable` of the regenerable and cache classes.
fn candidates(plan: &Plan) -> Vec<(usize, &PlanItem)> {
    plan.items
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            i.action == Action::RemoveRegenerable
                && matches!(i.class, Class::Regenerable | Class::Cache)
        })
        .collect()
}

/// The reviewed plan's recorded roots joined to the fresh resolution (class E): a root the plan
/// knew and the environment no longer yields stays protected.
pub fn union_roots(fresh: &mut ProtectContext, recorded: &ProtectContext) {
    for r in &recorded.roots {
        if (r.exists || r.unreadable) && !fresh.roots.iter().any(|f| f.spelled == r.spelled) {
            fresh.roots.push(r.clone());
        }
    }
}

/// Identities of the protected roots: a walk never enters one.
fn protected_ids(ctx: &ProtectContext) -> HashSet<FileId> {
    let mut out = HashSet::new();
    for r in &ctx.roots {
        for p in std::iter::once(&r.spelled).chain(r.canonical.iter()) {
            if let Ok(m) = std::fs::metadata(p) {
                out.insert(FileId {
                    dev: m.dev(),
                    ino: m.ino(),
                });
            }
        }
    }
    out
}

/// What the plan says, read-only: the plan id, a dry run of this lane, and unfinished journals.
fn dry_run_text(id: &str, plan: Option<&Plan>, state_dir: &Path) -> String {
    let mut s = format!("plan id: {id}\n");
    match plan {
        Some(p) => {
            let c = candidates(p);
            let bytes: u64 = c.iter().map(|(_, i)| i.reclaimable_bytes).sum();
            s.push_str(&format!(
                "dry run, nothing changed: {} remove-regenerable item(s), {bytes} bytes\n",
                c.len()
            ));
            for (_, i) in c.iter().take(20) {
                s.push_str(&format!("  {} ({} bytes)\n", i.path, i.reclaimable_bytes));
            }
            if c.len() > 20 {
                s.push_str(&format!("  ... and {} more\n", c.len() - 20));
            }
        }
        None => s.push_str("the plan file could not be read as a plan of this schema\n"),
    }
    s.push_str(&format!(
        "to apply it, run again with --plan-id {id} --only regenerable\n"
    ));
    for (pid, items) in journal::all_unfinished(state_dir) {
        s.push_str(&format!(
            "pending journal {pid}: {} unfinished item(s); run `apply --resume --plan-id {pid}`\n",
            items.len()
        ));
    }
    s
}

/// A fresh scan and plan with the reviewed plan's recorded inputs and the current host.
fn fresh_plan(plan: &Plan, env: &Env) -> Result<(crate::ScanResult, Plan)> {
    let roots: Vec<PathBuf> = plan.scan_roots.iter().map(PathBuf::from).collect();
    let mut so = ScanOptions::new(roots);
    so.one_file_system = plan.scan_options.one_file_system;
    so.top_files = plan.scan_options.top_files;
    so.min_output_bytes = plan.scan_options.min_output_bytes;
    so.collect_facts = plan.scan_options.git_facts;
    so.facts = env.facts.clone();
    so.context = env.scan_context.clone();
    let mut fresh_scan = scan(&so)?;
    union_roots(&mut fresh_scan.protect, &plan.protect);
    let active: Vec<PathBuf> = plan.plan_options.active.iter().map(PathBuf::from).collect();
    let active = resolve_active(&fresh_scan, &active)?;
    let popts = plan_options_from(&plan.plan_options, active, env.now, &env.host);
    let fresh = make_plan(&fresh_scan, &popts);
    Ok((fresh_scan, fresh))
}

/// Do the reviewed and the fresh plan agree on this item (path, class, rule and action)?
fn agreement(reviewed: &PlanItem, fresh: &Plan) -> std::result::Result<(), String> {
    let Some(now) = fresh.items.iter().find(|i| i.path == reviewed.path) else {
        return Err("the fresh scan finds no such item".to_string());
    };
    if now.class != reviewed.class {
        return Err(format!("its class is now {}", now.class.as_str()));
    }
    if now.rule_id != reviewed.rule_id {
        return Err("its rule is not the reviewed one".to_string());
    }
    if now.action != Action::RemoveRegenerable {
        return Err(format!(
            "the fresh plan now says {}: {}",
            now.action.as_str(),
            now.reason
        ));
    }
    Ok(())
}

/// Does `inner` lie at or inside `outer` (both canonicalised as far as they exist)?
fn inside(inner: &Path, outer: &Path) -> bool {
    let c = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    c(inner).starts_with(c(outer))
}

/// Run apply as the command line asked.
pub fn run(req: &Request, env: &Env) -> Result<Outcome> {
    if let Some(j) = req.journal {
        if !is_plan_id(j) {
            return refused("--journal takes a plan id: 16 lowercase hex characters");
        }
    }
    let bytes = match req.plan_file {
        Some(p) => match std::fs::read(p) {
            Ok(b) => Some(b),
            Err(e) => return refused(format!("cannot read the plan {}: {e}", p.display())),
        },
        None => None,
    };
    if bytes.is_none() && !(req.resume && req.journal.is_some()) {
        return refused("give --plan FILE (or, to finish a run, --resume --journal PLANID)");
    }
    let id = bytes.as_deref().map(plan_id);
    let parsed = bytes
        .as_deref()
        .map(|b| plan_from_json(&String::from_utf8_lossy(b)));
    let plan: Option<&Plan> = parsed.as_ref().and_then(|r| r.as_ref().ok());
    let Some(target) = id.clone().or_else(|| req.journal.map(str::to_string)) else {
        return refused("no plan id");
    };

    // 1. Courtesy check, unless the sandbox is valid.
    let journal_path = Journal::path_of(&env.state_dir, &target);
    let cand_paths: Vec<PathBuf> = match plan {
        Some(p) => candidates(p)
            .iter()
            .map(|(_, i)| PathBuf::from(&i.path))
            .collect(),
        None => journal::unfinished(&journal_path)
            .unwrap_or_default()
            .iter()
            .map(|l| PathBuf::from(&l.from))
            .collect(),
    };
    if let Some(dir) = req.sandbox {
        let roots = match protect::resolve(&env.scan_context) {
            Ok(c) => c
                .roots
                .iter()
                .flat_map(|r| {
                    std::iter::once(PathBuf::from(&r.spelled))
                        .chain(r.canonical.iter().map(PathBuf::from))
                })
                .collect::<Vec<_>>(),
            Err(e) => return refused(format!("protection: {e}")),
        };
        if let Err(e) = validate_sandbox(dir, &env.home, &roots, &cand_paths, req.getenv) {
            return refused(format!("--sandbox is not valid: {e}"));
        }
    } else if let Some(why) = courtesy_refusal(req.stdin_is_terminal, req.getenv) {
        return refused(format!(
            "refusing to run: {why}. apply is for a person at their own terminal"
        ));
    }

    // 2. Plan id.
    if let Some(id) = &id {
        if req.plan_id != Some(id.as_str()) {
            return Ok(Outcome::NothingChanged(dry_run_text(
                id,
                plan,
                &env.state_dir,
            )));
        }
    }

    if !req.resume && req.only != Some("regenerable") {
        return refused(
            "apply needs --only regenerable: removing regenerable directories is the only lane \
             this build has",
        );
    }

    // 3. Host lock.
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&env.state_dir)
            .map_err(|e| ApplyError::io(&env.state_dir, e))?;
    }
    let lock_path = env.state_dir.join("apply.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| ApplyError::io(&lock_path, e))?;
    match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(Outcome::Busy),
        Err(e) => {
            return Err(ApplyError::io(
                &lock_path,
                std::io::Error::from_raw_os_error(e.raw_os_error()),
            ))
        }
    }

    // 4. Resume, or refuse new work while this plan id has unfinished items.
    if req.resume {
        if !journal_path.exists() {
            return refused(format!("there is no journal for plan {target}"));
        }
        let mut report = Report::default();
        if let Err(drift) = env.guard.check() {
            report.notes.push(format!(
                "guard state: {drift} (finishing an interrupted run goes on regardless)"
            ));
        }
        let mut j = Journal::open(&env.state_dir, &target, env.now)?;
        let pending = journal::unfinished(&journal_path)?.len() as u64;
        j.start("resume", pending)?;
        let parents: Vec<PathBuf> = plan
            .map(|p| {
                candidates(p)
                    .iter()
                    .filter_map(|(_, i)| Path::new(&i.path).parent().map(Path::to_path_buf))
                    .collect()
            })
            .unwrap_or_default();
        resume::reconcile(&env.state_dir, &mut j, env.mounts, &parents, &mut report)?;
        j.end(
            "resume",
            &Totals {
                removed: report.removed,
                bytes: report.bytes,
                skipped: 0,
                restored: report.restored.len() as u64,
                kept: report.kept.len() as u64,
            },
        )?;
        return Ok(Outcome::Resumed(report));
    }
    let unfinished = journal::unfinished(&journal_path)?;
    if !unfinished.is_empty() {
        return refused(format!(
            "plan {target} has {} unfinished item(s) in its journal: run `apply --resume \
             --plan-id {target}` first",
            unfinished.len()
        ));
    }

    // 5. New work.
    if let Err(why) = env.guard.check() {
        return refused(format!("guard state: {why}"));
    }
    let plan = match parsed {
        Some(Ok(p)) => p,
        Some(Err(e)) => return refused(format!("the plan is not accepted: {e}")),
        None => return refused("new work needs the plan file"),
    };
    let age = env.now.saturating_sub(plan.generated_at);
    if age > (env.max_plan_age_days.min(36_500) as i64) * 86_400 {
        return refused(format!(
            "the plan is {} day(s) old; plans older than {} day(s) are refused: make a fresh scan \
             and plan",
            age / 86_400,
            env.max_plan_age_days
        ));
    }
    if plan.generated_at > env.now.saturating_add(3_600) {
        return refused("the plan is dated in the future");
    }
    let cands = candidates(&plan);
    if let Some((_, hit)) = cands
        .iter()
        .find(|(_, i)| inside(&env.state_dir, Path::new(&i.path)))
    {
        return refused(format!(
            "the journal directory {} lies inside the candidate {}",
            env.state_dir.display(),
            hit.path
        ));
    }
    let (fresh_scan, fresh) = fresh_plan(&plan, env)?;
    let forbidden = protected_ids(&fresh_scan.protect);
    let rules = RuleSet::builtin_shared();
    let classifier = Classifier {
        rules,
        options: &plan.scan_options,
        context: &env.scan_context,
    };
    let mut report = Report::default();
    let mut totals = Totals::default();
    let mut j = Journal::open(&env.state_dir, &target, env.now)?;
    j.start("new", cands.len() as u64)?;
    let limit = req.max_items.unwrap_or(usize::MAX);
    for (n, (idx, it)) in cands.iter().enumerate() {
        if n >= limit {
            break;
        }
        if let Err(why) = agreement(it, &fresh) {
            j.skip(*idx, &it.path, SkipReason::PlanChanged, &why)?;
            report.not_agreed.push((*idx, it.path.clone(), why));
            totals.skipped += 1;
            continue;
        }
        let mut cx = ItemCtx {
            journal: &mut j,
            mounts: env.mounts,
            reach: env.reach,
            classifier: &classifier,
            forbidden: &forbidden,
            plan_id: &target,
        };
        match item::process(&mut cx, *idx, &it.path, it.rule_id.as_deref())? {
            ItemOutcome::Removed(b) => {
                totals.removed += 1;
                totals.bytes += b;
                report.removed += 1;
                report.bytes += b;
            }
            ItemOutcome::Skipped(reason, detail) => {
                totals.skipped += 1;
                report.skipped.push((*idx, it.path.clone(), reason, detail));
            }
            ItemOutcome::Restored(why) => {
                totals.restored += 1;
                report.restored.push((*idx, it.path.clone(), why));
            }
            ItemOutcome::Kept(why) => {
                totals.kept += 1;
                report.kept.push((*idx, it.path.clone(), why));
            }
        }
    }
    j.end("new", &totals)?;
    report.other_pending = journal::all_unfinished(&env.state_dir)
        .into_iter()
        .filter(|(p, _)| *p != target)
        .map(|(p, v)| (p, v.len()))
        .collect();
    Ok(Outcome::Applied(report))
}
