//! Human-readable summaries of scans and plans.

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
    let _ = writeln!(o, "Scanned {}", s.roots.join(", "));
    let _ = writeln!(
        o,
        "{} files, {} directories, {} allocated ({} logical)",
        t.files,
        t.dirs,
        human_bytes(t.allocated_bytes),
        human_bytes(t.logical_bytes)
    );
    if t.hardlink_duplicates_skipped > 0 {
        let _ = writeln!(
            o,
            "{} hard-linked duplicate(s) counted once",
            t.hardlink_duplicates_skipped
        );
    }
    if t.unreadable > 0 {
        let _ = writeln!(o, "{} entries could not be read (not fatal)", t.unreadable);
    }
    if t.mount_points_skipped > 0 {
        let _ = writeln!(o, "{} mount point(s) not crossed", t.mount_points_skipped);
    }
    for w in &s.warnings {
        let _ = writeln!(o, "warning: {w}");
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
                n.path,
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
                n.path,
                flag
            );
        }
    }

    if !s.outputs.is_empty() {
        let _ = writeln!(o, "\nOutput files listed ({}):", s.outputs.len());
        for f in s.outputs.iter().take(15) {
            let _ = writeln!(o, "  {:>10}  {}", human_bytes(f.bytes), f.path);
        }
    }
    if !s.big_files.is_empty() {
        let _ = writeln!(o, "\nBiggest files:");
        for f in s.big_files.iter().take(10) {
            let _ = writeln!(o, "  {:>10}  {}", human_bytes(f.bytes), f.path);
        }
    }
    o
}

/// Summary printed by `plan`.
pub fn plan_summary(p: &Plan) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "Dry-run plan (nothing is changed). Active window: {} days (from {}).",
        p.active_window_days, p.window_source
    );
    let _ = write!(o, "Running-process check: {}", p.process_check);
    if let Some(n) = &p.process_check_note {
        let _ = write!(o, " ({n})");
    }
    let _ = writeln!(o);
    if !p.active.is_empty() {
        let _ = writeln!(o, "Active paths: {}", p.active.join(", "));
    }
    let _ = writeln!(
        o,
        "Reclaimable if every removing step completes: {} ({} with no recovery step)",
        human_bytes(p.totals.reclaimable_bytes),
        human_bytes(p.totals.ready_bytes)
    );
    let _ = writeln!(o, "\nBy action:");
    for (a, t) in &p.totals.by_action {
        let _ = writeln!(
            o,
            "  {:<22} {:>4} item(s)  {:>10}",
            a,
            t.items,
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
            "  {:<22} {:>10}  [{}] {}",
            it.action.as_str(),
            human_bytes(it.reclaimable_bytes),
            it.class.as_str(),
            it.path
        );
        let _ = writeln!(o, "      {:?}: {}", it.state, it.state_reason);
        let _ = writeln!(o, "      {}", it.reason);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
