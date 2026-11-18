//! Dry-run plan: AC-7, AC-6 (thresholds are inputs) and NEG-4 (plan side), NEG-8.

mod common;

use common::*;
use disk_cleaner_core::{
    plan, Action, Class, Facts, Node, Plan, PlanOptions, RepoInfo, ScanOptionsRecord, ScanResult,
    Totals, Tri, SCHEMA_VERSION,
};
use std::path::{Path, PathBuf};

const DAY: i64 = 86_400;

fn opts(active: Vec<PathBuf>, idle_days: u64, now: i64) -> PlanOptions {
    PlanOptions {
        active,
        idle_days,
        now,
    }
}

fn action_of(p: &Plan, path: &Path) -> Action {
    p.items
        .iter()
        .find(|i| i.path == path.to_string_lossy())
        .unwrap_or_else(|| panic!("no plan item for {}", path.display()))
        .action
}

fn synthetic(nodes: Vec<Node>) -> ScanResult {
    ScanResult {
        schema_version: SCHEMA_VERSION,
        tool_version: "test".into(),
        generated_at: 0,
        roots: vec![],
        options: ScanOptionsRecord::default(),
        totals: Totals::default(),
        nodes,
        outputs: vec![],
        big_files: vec![],
        issues: vec![],
        warnings: vec![],
    }
}

fn all_yes() -> Facts {
    Facts {
        has_network_remote: Tri::Yes,
        head_on_remote: Tri::Yes,
        all_branches_on_remote: Tri::Yes,
        stash_empty: Tri::Yes,
        tree_clean: Tri::Yes,
        no_unique_ignored: Tri::Yes,
        no_dependents: Tri::Yes,
    }
}

fn with_fact(index: usize, v: Tri) -> Facts {
    let mut f = all_yes();
    match index {
        0 => f.has_network_remote = v,
        1 => f.head_on_remote = v,
        2 => f.all_branches_on_remote = v,
        3 => f.stash_empty = v,
        4 => f.tree_clean = v,
        5 => f.no_unique_ignored = v,
        _ => f.no_dependents = v,
    }
    f
}

fn repo_node(path: &str, class: Class, facts: Facts) -> Node {
    let mut n = Node::new(path, class);
    n.bytes_self = 5_000;
    n.bytes_total = 5_000;
    n.newest_mtime = Some(0);
    n.repo = Some(RepoInfo {
        facts,
        ..RepoInfo::default()
    });
    n
}

#[test]
fn a_repo_with_any_fact_not_yes_never_gets_a_removing_action() {
    let now = 365 * DAY;
    // Control: all Yes and idle is archivable, so the matrix below is not vacuous.
    for class in [Class::Repo, Class::Worktree] {
        let s = synthetic(vec![repo_node("/r", class, all_yes())]);
        assert_eq!(
            plan(&s, &opts(vec![], 30, now)).items[0].action,
            Action::Archive
        );
    }
    for class in [Class::Repo, Class::Worktree] {
        for index in 0..7 {
            for (v, expected) in [
                (Tri::No, Action::RecoverThenArchive),
                (Tri::Unknown, Action::Ask),
            ] {
                let s = synthetic(vec![repo_node("/r", class, with_fact(index, v))]);
                let p = plan(&s, &opts(vec![], 30, now));
                let a = p.items[0].action;
                assert_eq!(a, expected, "fact {index} = {v:?} on {class:?}");
                assert!(!matches!(
                    a,
                    Action::Archive | Action::RemoveRegenerable | Action::BackupThenRemove
                ));
                assert!(!p.items[0].reason.is_empty());
            }
        }
    }
}

#[test]
fn unknown_beats_no_so_a_mixed_repo_asks() {
    let mut f = all_yes();
    f.stash_empty = Tri::No;
    f.tree_clean = Tri::Unknown;
    let s = synthetic(vec![repo_node("/r", Class::Repo, f)]);
    assert_eq!(
        plan(&s, &opts(vec![], 30, 365 * DAY)).items[0].action,
        Action::Ask
    );
}

#[test]
fn idle_threshold_is_an_input_not_a_constant() {
    let s = synthetic(vec![repo_node("/r", Class::Repo, all_yes())]);
    // Last activity at t=0; now is 100 days later.
    let now = 100 * DAY;
    assert_eq!(plan(&s, &opts(vec![], 99, now)).items[0].action, Action::Archive);
    assert_eq!(plan(&s, &opts(vec![], 100, now)).items[0].action, Action::Archive);
    assert_eq!(plan(&s, &opts(vec![], 101, now)).items[0].action, Action::Keep);
    assert_eq!(
        plan(&s, &opts(vec![], 365, now)).items[0].action,
        Action::Keep
    );
}

#[test]
fn no_activity_evidence_means_ask_never_remove() {
    let mut n = repo_node("/r", Class::Repo, all_yes());
    n.newest_mtime = None;
    let s = synthetic(vec![n]);
    assert_eq!(
        plan(&s, &opts(vec![], 1, 365 * DAY)).items[0].action,
        Action::Ask
    );
}

#[test]
fn loose_code_is_always_ask_unless_active() {
    let mut n = Node::new("/code/thing", Class::LooseCode);
    n.newest_mtime = Some(0);
    n.bytes_total = 10;
    let s = synthetic(vec![n]);
    assert_eq!(
        plan(&s, &opts(vec![], 1, 365 * DAY)).items[0].action,
        Action::Ask
    );
    assert_eq!(
        plan(&s, &opts(vec![], 1, 0)).items[0].action,
        Action::Ask,
        "recent loose code is still ask, never silently kept or removed"
    );
    let active = vec![PathBuf::from("/code")];
    assert_eq!(
        plan(&s, &opts(active, 1, 365 * DAY)).items[0].action,
        Action::Keep
    );
}

#[test]
fn bare_repo_is_kept_with_dependents_otherwise_ask() {
    let mut with_deps = all_yes();
    with_deps.no_dependents = Tri::No;
    let s = synthetic(vec![
        repo_node("/a.git", Class::BareRepo, with_deps),
        repo_node("/b.git", Class::BareRepo, all_yes()),
    ]);
    let p = plan(&s, &opts(vec![], 1, 365 * DAY));
    assert_eq!(p.items[0].action, Action::Keep);
    assert_eq!(p.items[1].action, Action::Ask);
}

#[test]
fn neg8_repo_under_an_active_path_is_kept_with_its_huge_node_modules() {
    let (_tmp, root, remotes) = workspace();
    let work = root.join("work");
    let repo = work.join("app");
    init_repo(&repo);
    write(&repo.join("package.json"), "{}");
    write(&repo.join(".gitignore"), "node_modules/\n");
    git(&repo, &["add", ".gitignore", "package.json"]);
    git(&repo, &["commit", "-q", "-m", "project files"]);
    write_bytes(&repo.join("node_modules").join("big").join("x.js"), 2_000_000);
    let remote = remotes.join("app.git");
    init_bare(&remote);
    push_to_remote(&repo, &remote);
    let res = scan_with(&root, network_everywhere());
    let now = far_future();

    // Control: not active, so the idle project's node_modules would be removed.
    let p = plan(&res, &opts(vec![], 30, now));
    assert_eq!(
        action_of(&p, &repo.join("node_modules")),
        Action::RemoveRegenerable
    );
    assert_eq!(action_of(&p, &repo), Action::Archive);

    // Active: everything under it is kept, however idle.
    let p = plan(&res, &opts(vec![work.clone()], 30, now));
    assert_eq!(action_of(&p, &repo), Action::Keep);
    assert_eq!(action_of(&p, &repo.join("node_modules")), Action::Keep);
    assert_eq!(p.totals.reclaimable_bytes, 0);
    for it in &p.items {
        assert_eq!(it.reclaimable_bytes, 0, "{}", it.path);
    }
}

#[test]
fn an_active_path_inside_a_project_keeps_the_project_and_its_regenerable_dirs() {
    let (_tmp, root, _) = workspace();
    write(&root.join("proj").join("package.json"), "{}");
    write(&root.join("proj").join("src").join("a.js"), "1");
    write_bytes(&root.join("proj").join("node_modules").join("x.js"), 5000);
    let res = scan_plain(&root);
    let active = vec![root.join("proj").join("src")];
    let p = plan(&res, &opts(active, 30, far_future()));
    assert_eq!(action_of(&p, &root.join("proj")), Action::Keep);
    assert_eq!(
        action_of(&p, &root.join("proj").join("node_modules")),
        Action::Keep
    );
}

#[test]
fn neg4_build_dir_with_an_output_file_is_never_offered_for_removal() {
    let (_tmp, root, _) = workspace();
    let with_video = root.join("a");
    let without = root.join("b");
    for proj in [&with_video, &without] {
        write(&proj.join("package.json"), "{}");
        write_bytes(&proj.join("out").join("bundle.js"), 5000);
    }
    write_bytes(&with_video.join("out").join("render.mp4"), 1024);
    let res = scan_plain(&root);
    let p = plan(&res, &opts(vec![], 30, far_future()));
    // Control: the same directory without the video is removable.
    assert_eq!(
        action_of(&p, &without.join("out")),
        Action::RemoveRegenerable
    );
    let flagged = action_of(&p, &with_video.join("out"));
    assert_eq!(flagged, Action::Ask);
    let item = p
        .items
        .iter()
        .find(|i| i.path == with_video.join("out").to_string_lossy())
        .unwrap();
    assert_eq!(item.reclaimable_bytes, 0);
    assert!(item.reason.contains("output"));
}

#[test]
fn recent_projects_keep_their_regenerable_directories() {
    let (_tmp, root, _) = workspace();
    write(&root.join("proj").join("package.json"), "{}");
    write_bytes(&root.join("proj").join("node_modules").join("x.js"), 5000);
    let res = scan_plain(&root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let p = plan(&res, &opts(vec![], 30, now + 2 * DAY));
    assert_eq!(
        action_of(&p, &root.join("proj").join("node_modules")),
        Action::Keep
    );
}

#[test]
fn totals_do_not_double_count_nested_removals() {
    let mut repo = repo_node("/r", Class::Repo, all_yes());
    repo.bytes_total = 10_000;
    let mut nm = Node::new("/r/node_modules", Class::Regenerable);
    nm.parent = Some(0);
    nm.bytes_total = 6_000;
    nm.bytes_self = 6_000;
    let s = synthetic(vec![repo, nm]);
    let p = plan(&s, &opts(vec![], 30, 365 * DAY));
    assert_eq!(action_of(&p, Path::new("/r")), Action::Archive);
    assert_eq!(
        action_of(&p, Path::new("/r/node_modules")),
        Action::RemoveRegenerable
    );
    assert_eq!(p.totals.reclaimable_bytes, 10_000);
}

#[test]
fn plan_is_a_dry_run_and_serializes_with_a_schema_version() {
    let s = synthetic(vec![repo_node("/r", Class::Repo, all_yes())]);
    let p = plan(&s, &opts(vec![], 30, 365 * DAY));
    assert!(p.dry_run);
    let v: serde_json::Value = serde_json::to_value(&p).unwrap();
    assert_eq!(v["schema_version"], SCHEMA_VERSION);
    assert_eq!(v["items"][0]["action"], "archive");
}
