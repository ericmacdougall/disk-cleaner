//! Human-readable summaries of scans and plans.

use disk_cleaner_core::plan::coverage_note;
use disk_cleaner_core::words::count;
use disk_cleaner_core::{Class, Plan, ScanResult, Tri};
use std::fmt::Write;

/// 1024-based size with one decimal.
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Make text that came from the file system safe to print: control characters (newlines, ANSI
/// escapes, ...) are shown as `\u{..}` so a directory name cannot forge or hide output lines.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn fact_word(t: Tri) -> &'static str {
    match t {
        Tri::Yes => "yes",
        Tri::No => "NO",
        Tri::Unknown => "UNKNOWN",
    }
}

/// Summary printed by `scan`.
pub fn scan_summary(s: &ScanResult) -> String {
    let mut o = String::new();
    let t = &s.totals;
    let _ = writeln!(o, "Scanned {}", esc(&s.roots.join(", ")));
    let _ = writeln!(
        o,
        "{}, {}, {} allocated ({} logical)",
        count(t.files, "file", "files"),
        count(t.dirs, "directory", "directories"),
        human_bytes(t.allocated_bytes),
        human_bytes(t.logical_bytes)
    );
    if t.hardlink_duplicates_skipped > 0 {
        let n = t.hardlink_duplicates_skipped;
        let _ = writeln!(
            o,
            "{} counted once",
            count(n, "hard-linked duplicate", "hard-linked duplicates")
        );
    }
    if t.unreadable > 0 {
        let n = t.unreadable;
        let _ = writeln!(
            o,
            "{} not be read (not fatal)",
            count(n, "entry could", "entries could")
        );
    }
    if t.mount_points_skipped > 0 {
        let n = t.mount_points_skipped;
        let _ = writeln!(
            o,
            "{} not crossed",
            count(n, "mount point was", "mount points were")
        );
    }
    let c = &s.coverage;
    if let Some(note) = coverage_note(c, c.complete) {
        let _ = writeln!(o, "{note}");
        for e in c
            .unreadable_dir_examples
            .iter()
            .chain(c.mount_point_examples.iter())
            .take(5)
        {
            let _ = writeln!(o, "  not read: {}", esc(e));
        }
    }
    for w in &s.warnings {
        let _ = writeln!(o, "warning: {}", esc(w));
    }
    for e in &s.enclosing_repos {
        let _ = writeln!(
            o,
            "note: {} is inside the git repository {}, which was not assessed; only the \
             artifact directories below the root were checked against it",
            esc(&e.root),
            esc(&e.repo)
        );
    }

    let _ = writeln!(o, "\nSpace by class (innermost classified ancestor):");
    let mut classes: Vec<(&String, &u64)> = t.bytes_by_class.iter().collect();
    classes.sort_by(|a, b| b.1.cmp(a.1));
    for (c, b) in classes {
        let _ = writeln!(o, "  {:<12} {:>10}", c, human_bytes(*b));
    }

    let mut repos: Vec<&disk_cleaner_core::Node> =
        s.nodes.iter().filter(|n| n.class.is_git()).collect();
    repos.sort_by_key(|n| std::cmp::Reverse(n.bytes_total));
    if !repos.is_empty() {
        let _ = writeln!(o, "\nRepos and worktrees ({}):", repos.len());
        for n in repos.iter().take(25) {
            let _ = writeln!(
                o,
                "  [{}] {}  {}",
                n.class.as_str(),
                esc(&n.path),
                human_bytes(n.bytes_total)
            );
            if let Some(r) = &n.repo {
                let facts: Vec<String> = r
                    .facts
                    .entries()
                    .iter()
                    .map(|(name, v)| format!("{name}={}", fact_word(*v)))
                    .collect();
                let _ = writeln!(o, "      {}", facts.join(" "));
                let _ = writeln!(
                    o,
                    "      .git {}, regenerable {}, outputs {}",
                    human_bytes(r.sizes.git),
                    human_bytes(r.sizes.regenerable),
                    human_bytes(r.sizes.output)
                );
            }
        }
        if repos.len() > 25 {
            let _ = writeln!(o, "  ... and {} more (see --json)", repos.len() - 25);
        }
    }

    let mut regen: Vec<&disk_cleaner_core::Node> = s
        .nodes
        .iter()
        .filter(|n| matches!(n.class, Class::Regenerable | Class::Cache))
        .collect();
    regen.sort_by_key(|n| std::cmp::Reverse(n.bytes_total));
    if !regen.is_empty() {
        let _ = writeln!(o, "\nRegenerable and cache directories ({}):", regen.len());
        for n in regen.iter().take(15) {
            let flag = if n.contains_outputs {
                "  CONTAINS OUTPUT FILES"
            } else {
                ""
            };
            let _ = writeln!(
                o,
                "  {:>10}  {}{}",
                human_bytes(n.bytes_total),
                esc(&n.path),
                flag
            );
        }
    }

    if !s.outputs.is_empty() {
        let _ = writeln!(o, "\nOutput files listed ({}):", s.outputs.len());
        for f in s.outputs.iter().take(15) {
            let _ = writeln!(o, "  {:>10}  {}", human_bytes(f.bytes), esc(&f.path));
        }
    }
    if !s.big_files.is_empty() {
        let _ = writeln!(o, "\nBiggest files:");
        for f in s.big_files.iter().take(10) {
            let _ = writeln!(o, "  {:>10}  {}", human_bytes(f.bytes), esc(&f.path));
        }
    }
    o
}

/// Summary printed by `plan`.
pub fn plan_summary(p: &Plan) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "Dry-run plan (nothing is changed). Active window: {} (from {}).",
        count(p.active_window_days, "day", "days"),
        p.window_source
    );
    let _ = write!(o, "Running-process check: {}", p.process_check);
    if let Some(n) = &p.process_check_note {
        let _ = write!(o, " ({n})");
    }
    let _ = writeln!(o);
    if p.scope == "volume" {
        let _ = writeln!(
            o,
            "Scope: volume (every scanned root is a mount point). Dependents were looked for \
             within the scanned roots only."
        );
    } else {
        let _ = writeln!(
            o,
            "Scope: subtree. Dependents (linked worktrees, shared object stores) were looked \
             for within the scanned folders only; other clones elsewhere were not checked, and \
             apply re-verifies."
        );
    }
    for e in &p.enclosing_repos {
        let _ = writeln!(
            o,
            "Scan root {} is inside the git repository {}, which the scan did not assess: it \
             is never offered for removal, and artifacts below the root were checked against \
             its git only",
            esc(&e.root),
            esc(&e.repo)
        );
    }
    match &p.coverage_note {
        Some(note) => {
            let _ = writeln!(o, "{note}");
        }
        None => {
            let _ = writeln!(o, "Coverage: complete (every folder was read).");
        }
    }
    if p.accepted_incomplete_coverage {
        let _ = writeln!(
            o,
            "Incomplete coverage was accepted by the user (accept_incomplete_coverage or \
             --accept-incomplete-coverage): repositories may be backed up then removed although \
             links or clones in places the scan could not read were not checked"
        );
    } else if !p.coverage_complete {
        let _ = writeln!(
            o,
            "Because coverage is incomplete, no repository is offered for removal: each one \
             asks (--accept-incomplete-coverage offers them anyway, saying what was not read)"
        );
    }
    if p.trusted_partial_process_check {
        let _ = writeln!(
            o,
            "Partial process check accepted by the user: if some processes could not be \
             inspected, removable items may still be in use by them"
        );
    }
    if !p.active.is_empty() {
        let _ = writeln!(o, "Active paths: {}", esc(&p.active.join(", ")));
    }
    let _ = writeln!(o, "Remote facts verified: no ({})", p.remote_facts_note);
    if p.items.iter().any(|i| i.preserve.is_some()) {
        let _ = writeln!(
            o,
            "Before a repository is removed, its own git files are saved with the manifest (the \
             manifest is the restore record a later apply step writes), and its object store is \
             backed up too."
        );
    }
    let _ = writeln!(
        o,
        "Reclaimable if every removing step completes: {} ({} from regenerable directories, \
         needing no recovery or remote check)",
        human_bytes(p.totals.reclaimable_bytes),
        human_bytes(p.totals.ready_bytes)
    );
    let _ = writeln!(o, "\nBy action:");
    for (a, t) in &p.totals.by_action {
        let _ = writeln!(
            o,
            "  {:<26} {:>10}  {:>10}",
            a,
            count(t.items, "item", "items"),
            human_bytes(t.reclaimable_bytes)
        );
    }
    let mut items: Vec<&disk_cleaner_core::PlanItem> = p.items.iter().collect();
    items.sort_by(|a, b| {
        b.reclaimable_bytes
            .cmp(&a.reclaimable_bytes)
            .then_with(|| b.bytes_total.cmp(&a.bytes_total))
    });
    let _ = writeln!(o, "\nItems:");
    for it in items {
        let _ = writeln!(
            o,
            "  {:<26} {:>10}  [{}] {}",
            it.action.as_str(),
            human_bytes(it.reclaimable_bytes),
            it.class.as_str(),
            esc(&it.path)
        );
        let _ = writeln!(o, "      {:?}: {}", it.state, esc(&it.state_reason));
        let _ = writeln!(o, "      {}", esc(&it.reason));
        if let Some(pre) = &it.preserve {
            let g = &pre.git_admin;
            let _ = writeln!(
                o,
                "      back up first: {} ({}) from {}",
                count(g.files, "file", "files"),
                human_bytes(g.bytes),
                esc(&g.git_dirs.join(", "))
            );
            if !g.examples.is_empty() {
                let _ = writeln!(o, "      including: {}", esc(&g.examples.join(", ")));
            }
            let s = &pre.object_store;
            let _ = writeln!(
                o,
                "      object store: {} ({}) of object data, to be backed up before removal",
                count(s.files, "file", "files"),
                human_bytes(s.bytes)
            );
        }
    }
    if let Some(l) = &p.lanes {
        lane_summary(&mut o, l);
    }
    o
}

/// Human summary of an apply run.
pub fn apply_summary(r: &disk_cleaner_core::apply::ApplyReport) -> String {
    let mut o = String::new();
    if r.dry_run {
        let _ = writeln!(
            o,
            "Dry run (nothing is changed; pass --apply to act). Run {}.",
            r.run
        );
    } else {
        let _ = writeln!(o, "Apply run {}.", r.run);
    }
    for p in &r.pieces {
        let _ = writeln!(
            o,
            "  [{}] {} {}: {}",
            p.lane.as_str(),
            p.outcome,
            p.path.display(),
            p.reason
        );
        if !p.restore.is_empty() && p.outcome != "kept" {
            let _ = writeln!(o, "      restore: {}", p.restore);
        }
    }
    if r.dry_run {
        let _ = writeln!(
            o,
            "Would upload at most {} bytes (deduplicated; verified at apply).",
            r.uploaded_bytes
        );
    } else {
        let _ = writeln!(
            o,
            "Uploaded {} bytes in {} shards; {} failed; removed {} entries.",
            r.uploaded_bytes,
            r.shards_uploaded,
            r.shards_failed.len(),
            r.removed_entries
        );
    }
    o
}

/// Human summary of a restore.
pub fn restore_summary(r: &disk_cleaner_core::apply::restore::RestoreReport) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "Restored {}, already identical {}, conflicts {}, failed {}.",
        r.restored.len(),
        r.identical.len(),
        r.conflicts.len(),
        r.failed.len()
    );
    for p in &r.conflicts {
        let _ = writeln!(o, "  conflict (left as is): {}", p.display());
    }
    for (p, e) in &r.failed {
        let _ = writeln!(o, "  failed: {}: {e}", p.display());
    }
    for (p, rule) in &r.regenerate {
        let _ = writeln!(o, "  regenerate: {} ({rule})", p.display());
    }
    o
}

/// The lanes of a plan: per lane its bytes, pieces and reclaimable bytes, the dedup saving, the
/// secret pieces kept, and the ten largest pieces of each lane with their reasons.
fn lane_summary(o: &mut String, l: &disk_cleaner_core::Lanes) {
    let t = &l.totals;
    let _ = writeln!(
        o,
        "\nLanes ({} scanned, {} in directories that were split):",
        human_bytes(t.scanned_bytes),
        human_bytes(t.dir_overhead_bytes)
    );
    for lane in disk_cleaner_core::Lane::ALL {
        let lt = t.by_lane.get(&lane).cloned().unwrap_or_default();
        let _ = writeln!(
            o,
            "  {:<7} {:>10} in {}, {} reclaimable, {} to upload",
            lane.as_str(),
            human_bytes(lt.bytes),
            count(lt.pieces, "piece", "pieces"),
            human_bytes(lt.reclaimable_bytes),
            human_bytes(lt.upload_bytes)
        );
    }
    let _ = writeln!(
        o,
        "  duplicates take {} beyond one copy each; uploading each content once saves {}",
        human_bytes(t.duplicate_bytes),
        human_bytes(t.dedup_saved_bytes)
    );
    let _ = writeln!(
        o,
        "  {} kept on this disk, never uploaded",
        count(t.secret_pieces, "secret piece", "secret pieces")
    );
    for lane in disk_cleaner_core::Lane::ALL {
        let mut ps: Vec<&disk_cleaner_core::LanePiece> =
            l.pieces.iter().filter(|p| p.lane == lane).collect();
        if ps.is_empty() {
            continue;
        }
        ps.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.id.cmp(&b.id)));
        let _ = writeln!(o, "\nLargest {} pieces:", lane.as_str());
        for p in ps.iter().take(10) {
            let what = match &p.member {
                Some(m) => format!("{}/{m}", p.path),
                None => p.path.clone(),
            };
            let _ = writeln!(o, "  {:>10}  {}", human_bytes(p.bytes), esc(&what));
            let _ = writeln!(o, "      {}", esc(&p.reason));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{esc, human_bytes};

    #[test]
    fn escapes_control_characters() {
        assert_eq!(esc("a\nb\u{1b}[0m"), "a\\u{a}b\\u{1b}[0m");
        assert_eq!(esc("plain é 日本"), "plain é 日本");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
