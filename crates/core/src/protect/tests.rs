//! Tests of protected-root resolution and the per-node holds. Fixtures are synthetic: temporary
//! home directories and hand-built scans; the paths in comments use `/home/user`.

use super::*;
use crate::plan::{plan, Action, Plan, PlanOptions};
use std::fs;
use std::path::{Path, PathBuf};

const DAY: i64 = 86_400;

fn all_yes() -> Facts {
    Facts {
        has_network_remote: Tri::Yes,
        head_on_remote: Tri::Yes,
        all_branches_on_remote: Tri::Yes,
        stash_empty: Tri::Yes,
        tree_clean: Tri::Yes,
        no_unique_ignored: Tri::Yes,
        no_dependents: Tri::Yes,
        reflog_saved: Tri::Yes,
        no_pending_state: Tri::Yes,
        plain_layout: Tri::Yes,
        no_local_admin_content: Tri::Yes,
    }
}

fn node(path: impl AsRef<Path>, class: Class, facts: Facts) -> Node {
    let mut n = Node::new(path.as_ref().to_string_lossy().into_owned(), class);
    n.bytes_self = 5_000;
    n.bytes_total = 5_000;
    n.newest_mtime = Some(0);
    // The scan always records it; no agent file at the node's own top level.
    n.top_agent_files = Some(0);
    if class.is_git() {
        n.repo = Some(RepoInfo {
            facts,
            git_admin: Some(GitAdminSurvey {
                files: 12,
                bytes: 49_152,
                ..Default::default()
            }),
            object_store: Some(ObjectStoreSurvey {
                files: 3,
                bytes: 12_288,
            }),
            ..RepoInfo::default()
        });
    }
    if class.is_artifact() {
        n.rule = Some("node_modules".into());
    }
    n
}

/// A temporary home with every built-in protected root present.
fn fixture_home() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap().join("user");
    for (rel, _) in BUILTIN_PROTECTED {
        fs::create_dir_all(home.join(rel)).unwrap();
    }
    (tmp, home)
}

fn ctx_for(home: &Path) -> ScanContext {
    ScanContext {
        passwd_home: Some(home.to_path_buf()),
        euid: Some(1000),
        home_env: Some(home.to_string_lossy().into_owned()),
        ..ScanContext::default()
    }
}

fn scan_of(pc: ProtectContext, nodes: Vec<Node>) -> ScanResult {
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
        enclosing_repos: vec![],
        scope: "subtree".to_string(),
        coverage: Coverage {
            complete: true,
            ..Default::default()
        },
        protect: pc,
    }
}

fn opts(now: i64) -> PlanOptions {
    PlanOptions::new(7, now)
}

fn held_protected(p: &Plan, idx: usize) -> bool {
    let it = &p.items[idx];
    it.action == Action::Keep && it.hold.as_ref().map(|h| h.code) == Some(HoldCode::Protected)
}

// ---------------------------------------------------------------- AC-1, NEG-1, NEG-7

#[test]
fn every_class_in_every_root() {
    let (_t, home) = fixture_home();
    let pc = resolve(&ctx_for(&home)).unwrap();
    // The fixture list, written out: the built-in roots, home-relative.
    let roots = [
        ".claude",
        ".codex",
        ".agent-harness",
        ".prunedisk",
        ".disk-cleaner",
        ".kimi",
        ".kimi-code",
        ".gemini",
        ".cursor",
        ".config/claude",
        ".local/share/claude",
        "Library/Application Support/Claude",
    ];
    assert_eq!(roots.len(), BUILTIN_PROTECTED.len());
    let classes = [
        Class::Repo,
        Class::Worktree,
        Class::BareRepo,
        Class::Regenerable,
        Class::Cache,
        Class::LooseCode,
    ];
    // Idle for 300 days (NEG-1): the stale, fully pushed clone stays held.
    let now = 300 * DAY;
    for class in classes {
        // Control outside every root: the same node has no hold and the removable classes are
        // removing, so the matrix below is not vacuous.
        let ctl = node(home.join("elsewhere/x"), class, all_yes());
        let p = plan(&scan_of(pc.clone(), vec![ctl]), &opts(now));
        let it = &p.items[0];
        assert!(it.hold.is_none(), "{class:?} outside a root has a hold");
        if matches!(
            class,
            Class::Repo | Class::Worktree | Class::Regenerable | Class::Cache
        ) {
            assert!(it.action.removes(), "{class:?} control: {:?}", it.action);
        }
        for r in roots {
            // `projects/stale-clone` is not a carve-out position in any root.
            let n = node(home.join(r).join("projects/stale-clone"), class, all_yes());
            let p = plan(&scan_of(pc.clone(), vec![n]), &opts(now));
            assert!(held_protected(&p, 0), "{class:?} in {r}: {:?}", p.items);
            assert_eq!(p.items[0].reclaimable_bytes, 0);
            let h = p.items[0].hold.as_ref().unwrap();
            assert!(h.root.ends_with(r), "{} for {r}", h.root);
        }
    }
    // NEG-7: a dotfiles git repo at ~ contains the protected roots, so it is held and never
    // removing (its control, the same repo elsewhere, is removable above).
    let dot = node(&home, Class::Repo, all_yes());
    let p = plan(&scan_of(pc, vec![dot]), &opts(now));
    assert!(held_protected(&p, 0), "{:?}", p.items);
}

// ---------------------------------------------------------------- AC-3, NEG-2

fn dirty(mutate: fn(&mut Facts)) -> Facts {
    let mut f = all_yes();
    mutate(&mut f);
    f
}

#[test]
fn carve_outs() {
    let (_t, home) = fixture_home();
    let pc = resolve(&ctx_for(&home)).unwrap();
    let positions = [
        ".agent-harness/work/s1/clone",
        ".agent-harness/work/s1/task/clone",
        ".claude/worktrees/wt",
        ".codex/worktrees/wt",
    ];
    for pos in positions {
        // Clean and fully pushed, with a target dir inside.
        let x = node(home.join(pos), Class::Repo, all_yes());
        let mut target = node(home.join(pos).join("target"), Class::Regenerable, all_yes());
        target.parent = Some(0);
        target.rule = Some("target".into());
        target.agent_files = Some(0);
        let p = plan(&scan_of(pc.clone(), vec![x, target]), &opts(300 * DAY));
        let xi = &p.items[0];
        assert_eq!(xi.action, Action::BackupThenRemove, "{pos}: {}", xi.reason);
        assert!(xi.hold.is_none(), "{pos}");
        assert_eq!(
            p.items[1].action,
            Action::RemoveRegenerable,
            "{pos}: {}",
            p.items[1].reason
        );
        // Unsaved work of three kinds (NEG-2): keep with the carve-out reason.
        let kinds: [(&str, Facts); 3] = [
            (
                "untracked or modified file",
                dirty(|f| f.tree_clean = Tri::No),
            ),
            ("unpushed commit", dirty(|f| f.head_on_remote = Tri::No)),
            (
                "ignored non-regenerable file",
                dirty(|f| f.no_unique_ignored = Tri::No),
            ),
        ];
        for (what, facts) in kinds {
            let x = node(home.join(pos), Class::Repo, facts);
            let p = plan(&scan_of(pc.clone(), vec![x]), &opts(300 * DAY));
            let it = &p.items[0];
            assert_eq!(it.action, Action::Keep, "{pos} with {what}: {}", it.reason);
            assert!(
                it.reason
                    .contains("protected: unsaved work inside agent dir"),
                "{pos} with {what}: {}",
                it.reason
            );
            assert_eq!(it.hold.as_ref().unwrap().code, HoldCode::Protected);
        }
    }
    // Not carve-out positions: a checkout inside a task dir that is itself a checkout (with agent
    // files at its top level), two levels below a task dir, and the session dir.
    let mut task_checkout = node(
        home.join(".agent-harness/work/s2/task"),
        Class::Repo,
        all_yes(),
    );
    task_checkout.top_agent_files = Some(2);
    let mut inside_task_checkout = node(
        home.join(".agent-harness/work/s2/task/clone"),
        Class::Repo,
        all_yes(),
    );
    inside_task_checkout.parent = Some(0);
    let deep = node(
        home.join(".agent-harness/work/s1/task/a/clone"),
        Class::Repo,
        all_yes(),
    );
    let session_dir = node(home.join(".agent-harness/work/s3"), Class::Repo, all_yes());
    let session_loose = node(
        home.join(".agent-harness/work/s4"),
        Class::LooseCode,
        all_yes(),
    );
    let s = scan_of(
        pc,
        vec![
            task_checkout,
            inside_task_checkout,
            deep,
            session_dir,
            session_loose,
        ],
    );
    let p = plan(&s, &opts(300 * DAY));
    // The task dir itself is a direct child of the session dir: a carve-out position.
    // Its child checkout vetoes it though (the child is held), so it asks.
    assert!(!p.items[0].action.removes(), "{:?}", p.items[0]);
    for (k, it) in p.items.iter().enumerate().skip(1) {
        assert!(
            !it.action.removes() && it.hold.is_some(),
            "item {k} {}: {:?}",
            it.path,
            it.action
        );
    }
    // On its own the task checkout is a carve-out position.
    let alone = node(
        home.join(".agent-harness/work/s2/task"),
        Class::Repo,
        all_yes(),
    );
    let p = plan(
        &scan_of(resolve(&ctx_for(&home)).unwrap(), vec![alone]),
        &opts(300 * DAY),
    );
    assert_eq!(p.items[0].action, Action::BackupThenRemove);

    // A checkout directly below a session with no agent files at its top level is a carve-out;
    // with agent files at its top level it is an agent-data checkout: held, never a candidate.
    // A missing count is treated as nonzero.
    for top in [Some(1), None] {
        let mut agent_data = node(
            home.join(".agent-harness/work/s2/task"),
            Class::Repo,
            all_yes(),
        );
        agent_data.top_agent_files = top;
        let p = plan(
            &scan_of(resolve(&ctx_for(&home)).unwrap(), vec![agent_data]),
            &opts(300 * DAY),
        );
        assert!(held_protected(&p, 0), "{top:?}: {:?}", p.items[0]);
    }
    // The count matters only in that position: a checkout under a task dir, or in a worktrees
    // dir, with agent files at its own top level is judged by its facts as before.
    for pos in [".agent-harness/work/s7/t/clone", ".claude/worktrees/wt"] {
        let mut x = node(home.join(pos), Class::Repo, all_yes());
        x.top_agent_files = Some(3);
        let p = plan(
            &scan_of(resolve(&ctx_for(&home)).unwrap(), vec![x]),
            &opts(300 * DAY),
        );
        assert_eq!(p.items[0].action, Action::BackupThenRemove, "{pos}");
    }

    // A task dir holding only notes.md and results.tsv (so not a checkout, and no node of its
    // own) with a clean, pushed `wt/`: `wt/` is removable after a backup, nothing else is.
    let pc = resolve(&ctx_for(&home)).unwrap();
    let wt = node(
        home.join(".agent-harness/work/s6/notes-task/wt"),
        Class::Repo,
        all_yes(),
    );
    let mut target = node(
        home.join(".agent-harness/work/s6/notes-task/wt/target"),
        Class::Regenerable,
        all_yes(),
    );
    target.parent = Some(0);
    target.rule = Some("target".into());
    target.agent_files = Some(0);
    let p = plan(&scan_of(pc, vec![wt, target]), &opts(300 * DAY));
    assert_eq!(
        p.items[0].action,
        Action::BackupThenRemove,
        "{}",
        p.items[0].reason
    );
    assert_eq!(p.items[1].action, Action::RemoveRegenerable);
}

// ---------------------------------------------------------------- AC-4, NEG-4

#[test]
fn spellings() {
    let (_t, home) = fixture_home();
    let pc = resolve(&ctx_for(&home)).unwrap();
    let held = |n: Node| {
        let p = plan(&scan_of(pc.clone(), vec![n]), &opts(300 * DAY));
        held_protected(&p, 0)
    };
    let mk = |path: PathBuf, real: Option<PathBuf>| {
        let mut n = node(path, Class::Regenerable, all_yes());
        n.real_path = real.map(|r| r.to_string_lossy().into_owned());
        n
    };
    let inside = home.join(".codex/sessions/node_modules");
    // Control: neither identity is inside a root.
    assert!(!held(mk(
        home.join("proj/node_modules"),
        Some(home.join("proj/node_modules"))
    )));
    // Lexical only: the canonical path points elsewhere (or is unknown).
    assert!(held(mk(
        inside.clone(),
        Some(PathBuf::from("/data/elsewhere"))
    )));
    assert!(held(mk(inside.clone(), None)));
    // Canonical only: spelled outside, resolves into the root (a scan root that is a symlink).
    assert!(held(mk(
        home.join("link/sessions/node_modules"),
        Some(inside.clone())
    )));
    // A path spelled with `..`.
    assert!(held(mk(
        home.join("proj/../.codex/sessions/node_modules"),
        None
    )));
    // A `..` that climbs out of the root again is not inside.
    assert!(!held(mk(
        home.join(".codex/../proj/node_modules"),
        Some(home.join("proj/node_modules"))
    )));
    // A sibling whose name only starts like a root is not inside it.
    assert!(!held(mk(home.join(".codex-other/node_modules"), None)));
    #[cfg(target_os = "macos")]
    {
        // A different-case spelling (macOS volumes are case-insensitive).
        assert!(held(mk(home.join(".CODEX/sessions/node_modules"), None)));
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Case-sensitive file systems keep case distinct.
        assert!(!held(mk(home.join(".CODEX/sessions/node_modules"), None)));
    }
    // A node whose canonical path could not be resolved because of a permission error.
    let mut unreadable = mk(home.join("proj/node_modules"), None);
    unreadable.real_unreadable = true;
    let p = plan(&scan_of(pc.clone(), vec![unreadable]), &opts(300 * DAY));
    assert!(held_protected(&p, 0));
    assert!(
        p.items[0].reason.contains("unreadable"),
        "{}",
        p.items[0].reason
    );

    // A root absent on this host is recorded absent, without error, and ignored.
    fs::remove_dir_all(home.join(".cursor")).unwrap();
    let pc2 = resolve(&ctx_for(&home)).unwrap();
    let cursor = pc2
        .roots
        .iter()
        .find(|r| r.spelled.ends_with("/.cursor"))
        .unwrap();
    assert!(!cursor.exists && cursor.canonical.is_none() && !cursor.unreadable);
    let n = node(
        home.join(".cursor/x/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    let p = plan(&scan_of(pc2, vec![n]), &opts(300 * DAY));
    assert!(p.items[0].hold.is_none());

    // A root whose canonical path fails with a permission error.
    #[cfg(unix)]
    permission_denied_root_case();

    // `.agent-harness` a symlink, scanned at its real target: an agent-data checkout is held
    // although the walk never spells `.agent-harness` (class P).
    #[cfg(unix)]
    symlinked_harness_scanned_at_real_target();

    // A symlink loop (ELOOP): a root behind it is present and unresolved, so it is compared by
    // its spelled path, and a node behind it cannot be shown to be outside protected data.
    #[cfg(unix)]
    {
        let (_t3, home3) = fixture_home();
        std::os::unix::fs::symlink(home3.join("loop-b"), home3.join("loop-a")).unwrap();
        std::os::unix::fs::symlink(home3.join("loop-a"), home3.join("loop-b")).unwrap();
        let mut ctx = ctx_for(&home3);
        ctx.extra_paths = vec!["~/loop-a/inner".into()];
        let pc3 = resolve(&ctx).unwrap();
        let r = pc3
            .roots
            .iter()
            .find(|r| r.spelled.ends_with("/loop-a/inner"))
            .unwrap();
        assert!(r.exists && r.unreadable && r.canonical.is_none(), "{r:?}");
        let n = node(
            home3.join("loop-a/inner/x/node_modules"),
            Class::Regenerable,
            all_yes(),
        );
        let p = plan(&scan_of(pc3, vec![n]), &opts(300 * DAY));
        assert!(held_protected(&p, 0));
        // A node path that loops resolves to nothing: held as unreadable by the scan.
        let (real, unreadable) = node_real_path(&home3.join("loop-a/x"));
        assert!(real.is_none() && unreadable);
        // A path that simply does not exist is not a failure.
        let (real, unreadable) = node_real_path(&home3.join("missing"));
        assert!(real.is_none() && !unreadable);
    }

    // A real scan whose root is a symlink to `.codex`: every node is held through its canonical
    // path alone (the spelled path never mentions `.codex`).
    #[cfg(unix)]
    {
        let (_t2, home2) = fixture_home();
        let proj = home2.join(".codex/sessions/p");
        fs::create_dir_all(proj.join("node_modules")).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();
        fs::write(proj.join("node_modules/a.js"), vec![b'x'; 9_000]).unwrap();
        let link = home2.join("shortcut");
        std::os::unix::fs::symlink(home2.join(".codex"), &link).unwrap();
        let mut o = crate::scan::ScanOptions::new(vec![link]);
        o.collect_facts = false;
        o.context = ctx_for(&home2);
        let s = crate::scan::scan(&o).unwrap();
        assert!(s.nodes.iter().any(|n| n.path.ends_with("node_modules")));
        let p = plan(&s, &opts(i64::MAX / 4));
        for it in &p.items {
            assert!(!it.path.contains(".codex"), "spelled path: {}", it.path);
            assert_eq!(it.action, Action::Keep, "{}: {}", it.path, it.reason);
            assert_eq!(it.hold.as_ref().unwrap().code, HoldCode::Protected);
        }
    }
}

/// NEG-4: a root whose canonical path fails with a permission error holds every node that could
/// be inside it, or contain it, by lexical path.
#[cfg(unix)]
fn permission_denied_root_case() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, home) = fixture_home();
    let locked = home.join("locked");
    fs::create_dir_all(locked.join("secret")).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let unrestricted = fs::read_dir(&locked).is_ok(); // running as root: cannot test
    let mut ctx = ctx_for(&home);
    ctx.extra_paths = vec!["~/locked/secret".into()];
    let res = resolve(&ctx);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if unrestricted {
        return;
    }
    let pc = res.unwrap();
    let r = pc
        .roots
        .iter()
        .find(|r| r.spelled.ends_with("/locked/secret"))
        .unwrap();
    assert!(r.exists && r.unreadable && r.canonical.is_none(), "{r:?}");
    // Every node that could be inside it by lexical path is held.
    let inside = node(
        home.join("locked/secret/proj/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    let contains = node(home.join("locked"), Class::LooseCode, all_yes());
    let unrelated = node(
        home.join("other/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    let p = plan(
        &scan_of(pc, vec![inside, contains, unrelated]),
        &opts(300 * DAY),
    );
    assert!(held_protected(&p, 0), "inside: {:?}", p.items[0]);
    assert!(held_protected(&p, 1), "contains: {:?}", p.items[1]);
    assert!(p.items[2].hold.is_none(), "unrelated: {:?}", p.items[2]);
}

// ---------------------------------------------------------------- AC-5, NEG-5, NEG-6

#[test]
fn context_and_overrides() {
    let (tmp, home) = fixture_home();
    let outside = fs::canonicalize(tmp.path())
        .unwrap()
        .join("other-agent-home");
    fs::create_dir_all(outside.join("sessions")).unwrap();
    // The context is recorded: home, uid, the overrides as seen, the resolved roots.
    let mut ctx = ctx_for(&home);
    ctx.claude_config_dir = Some(outside.to_string_lossy().into_owned());
    ctx.codex_home = Some(home.join(".codex").to_string_lossy().into_owned());
    ctx.xdg_config_home = Some(home.join(".config").to_string_lossy().into_owned());
    let pc = resolve(&ctx).unwrap();
    assert_eq!(pc.home, home.to_string_lossy());
    assert_eq!(pc.passwd_home.as_deref(), Some(&*home.to_string_lossy()));
    assert_eq!(pc.uid, Some(1000));
    assert_eq!(pc.home_env.as_deref(), Some(&*home.to_string_lossy()));
    assert_eq!(pc.claude_config_dir, ctx.claude_config_dir);
    assert_eq!(pc.codex_home, ctx.codex_home);
    assert_eq!(pc.xdg_config_home, ctx.xdg_config_home);
    let root = pc
        .roots
        .iter()
        .find(|r| r.source == "CLAUDE_CONFIG_DIR")
        .expect("CLAUDE_CONFIG_DIR root recorded");
    assert!(root.exists && root.canonical.is_some());
    // Nodes inside an override outside the home are held.
    let n = node(
        outside.join("sessions/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    let p = plan(&scan_of(pc, vec![n]), &opts(300 * DAY));
    assert!(held_protected(&p, 0), "{:?}", p.items);

    // NEG-5: an empty, relative, `~`, `/` or home-equal override stops the scan, naming it.
    let home_s = home.to_string_lossy().into_owned();
    let with_slash = format!("{home_s}/");
    let bad_values = ["", "relative/dir", "~", "~/x", "/", &home_s, &with_slash];
    for var in ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "XDG_CONFIG_HOME"] {
        for v in bad_values {
            let mut c = ctx_for(&home);
            match var {
                "CLAUDE_CONFIG_DIR" => c.claude_config_dir = Some(v.to_string()),
                "CODEX_HOME" => c.codex_home = Some(v.to_string()),
                _ => c.xdg_config_home = Some(v.to_string()),
            }
            let e = resolve(&c).expect_err(&format!("{var}={v:?} must be refused"));
            assert!(e.to_string().contains(var), "{var}={v:?}: {e}");
        }
    }
    // A good value next to them is accepted (the refusals are not blanket).
    let mut c = ctx_for(&home);
    c.codex_home = Some(outside.to_string_lossy().into_owned());
    assert!(resolve(&c).is_ok());

    // NEG-6: euid 0 and a HOME that is not the passwd home are refused.
    let mut c = ctx_for(&home);
    c.euid = Some(0);
    let e = resolve(&c).expect_err("euid 0");
    assert!(e.to_string().contains("euid 0"), "{e}");
    let mut c = ctx_for(&home);
    c.home_env = Some(outside.to_string_lossy().into_owned());
    let e = resolve(&c).expect_err("HOME differs");
    assert!(e.to_string().contains("HOME"), "{e}");
    // A declared fixture home needs HOME to be the same directory, and the passwd home stays
    // protected as well.
    let mut c = ctx_for(&home);
    c.declared_home = Some(outside.clone());
    assert!(resolve(&c).is_err());
    c.home_env = Some(outside.to_string_lossy().into_owned());
    let pc = resolve(&c).unwrap();
    assert_eq!(pc.home, outside.to_string_lossy());
    assert!(pc.roots.iter().any(|r| r.spelled.starts_with(&*home_s)));
    assert!(pc
        .roots
        .iter()
        .any(|r| r.spelled.starts_with(&*outside.to_string_lossy())));

    // The plan refuses a scan without roots (older schema) and a uid mismatch.
    let good = scan_of(resolve(&ctx_for(&home)).unwrap(), vec![]);
    assert!(check_plan_scan(&good, Some(1000)).is_ok());
    let e = check_plan_scan(&good, Some(1001)).expect_err("uid mismatch");
    assert!(e.to_string().contains("uid"), "{e}");
    let old = scan_of(ProtectContext::default(), vec![]);
    let e = check_plan_scan(&old, Some(1000)).expect_err("no roots");
    assert!(e.to_string().contains("protected_roots"), "{e}");
}

#[test]
fn scan_records_context_and_attributes_root_bytes() {
    let (_t, home) = fixture_home();
    let proj = home.join(".claude/projects/p");
    fs::create_dir_all(proj.join("node_modules")).unwrap();
    fs::write(proj.join("package.json"), "{}").unwrap();
    fs::write(proj.join("node_modules/a.js"), vec![b'x'; 20_000]).unwrap();
    fs::write(proj.join("rollout-1.jsonl"), vec![b'y'; 50_000]).unwrap();
    let mut o = crate::scan::ScanOptions::new(vec![home.clone()]);
    o.collect_facts = false;
    o.context = ctx_for(&home);
    let s = crate::scan::scan(&o).unwrap();
    assert_eq!(s.protect.uid, Some(1000));
    assert_eq!(s.protect.home, home.to_string_lossy());
    let claude = s
        .protect
        .roots
        .iter()
        .find(|r| r.spelled.ends_with("/.claude"))
        .unwrap();
    assert!(claude.exists && claude.canonical.is_some());
    assert!(claude.bytes >= 70_000, "bytes attributed: {}", claude.bytes);
    let nm = s
        .nodes
        .iter()
        .find(|n| n.path.ends_with("node_modules"))
        .unwrap();
    assert_eq!(nm.real_path.as_deref(), Some(&*nm.path));
    // And a scan root that is a symlink to the agent home is held through its canonical path.
    #[cfg(unix)]
    {
        let link = home.join("link");
        std::os::unix::fs::symlink(home.join(".claude"), &link).unwrap();
        let mut o = crate::scan::ScanOptions::new(vec![link.clone()]);
        o.collect_facts = false;
        o.context = ctx_for(&home);
        let s = crate::scan::scan(&o).unwrap();
        let p = plan(&s, &opts(i64::MAX / 4));
        let nm = p
            .items
            .iter()
            .find(|i| i.path.ends_with("node_modules"))
            .expect("node_modules via the link");
        assert_eq!(nm.action, Action::Keep, "{}", nm.reason);
        assert_eq!(nm.hold.as_ref().unwrap().code, HoldCode::Protected);
    }
}

// ---------------------------------------------------------------- AC-6, NEG-3, NEG-9

#[test]
fn synced_and_packages() {
    let (_t, home) = fixture_home();
    fs::create_dir_all(home.join("Dropbox (Personal)/proj")).unwrap();
    fs::create_dir_all(home.join("Library/CloudStorage/GoogleDrive-x")).unwrap();
    let pc = resolve(&ctx_for(&home)).unwrap();
    assert!(pc
        .roots
        .iter()
        .any(|r| r.code == HoldCode::Synced && r.spelled.ends_with("Dropbox (Personal)")));
    let now = 300 * DAY;
    let plan_of = |nodes: Vec<Node>| plan(&scan_of(pc.clone(), nodes), &opts(now));
    // NEG-3: a node_modules inside a Dropbox folder is held synced.
    let nm = node(
        home.join("Dropbox (Personal)/proj/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    let p = plan_of(vec![nm]);
    assert_eq!(p.items[0].action, Action::Keep);
    assert_eq!(p.items[0].hold.as_ref().unwrap().code, HoldCode::Synced);
    // Library/CloudStorage too.
    let nm = node(
        home.join("Library/CloudStorage/GoogleDrive-x/p/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    assert_eq!(
        plan_of(vec![nm]).items[0].hold.as_ref().map(|h| h.code),
        Some(HoldCode::Synced)
    );
    // ... and its removing parent becomes ask, naming the held child.
    let parent = node(home.join("work/proj"), Class::Repo, all_yes());
    let mut child = node(
        home.join("work/proj/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    child.parent = Some(0);
    child.real_path = Some(
        home.join("Dropbox (Personal)/proj/node_modules")
            .to_string_lossy()
            .into_owned(),
    );
    let p = plan_of(vec![parent, child]);
    assert_eq!(p.items[1].hold.as_ref().unwrap().code, HoldCode::Synced);
    assert_eq!(p.items[0].action, Action::Ask, "{}", p.items[0].reason);
    assert!(
        p.items[0].reason.contains("work/proj/node_modules"),
        "{}",
        p.items[0].reason
    );
    // A node that contains the sync folder is held too.
    let over = node(&home, Class::LooseCode, all_yes());
    let p = plan_of(vec![over]);
    assert!(p.items[0].hold.is_some());

    // Packages: held `package`, and a removing parent asks.
    for dir in [
        "Foo.app/Contents/Resources",
        "lib/Bar.framework/Versions",
        "x.photoslibrary",
    ] {
        let n = node(
            home.join(format!("Applications/{dir}/node_modules")),
            Class::Regenerable,
            all_yes(),
        );
        let p = plan_of(vec![n]);
        let h = p.items[0].hold.as_ref().unwrap_or_else(|| panic!("{dir}"));
        assert_eq!(
            (p.items[0].action, h.code),
            (Action::Keep, HoldCode::Package)
        );
    }
    let parent = node(home.join("work/app"), Class::Repo, all_yes());
    let mut child = node(
        home.join("work/app/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    child.parent = Some(0);
    child.real_path = Some(
        home.join("Applications/Foo.app/Contents/node_modules")
            .to_string_lossy()
            .into_owned(),
    );
    let p = plan_of(vec![parent, child]);
    assert_eq!(p.items[0].action, Action::Ask);

    // NEG-9: `vendor/bundle` (no `.bundle` suffix rule) and look-alike names are not packages.
    for path in [
        "work/ruby/vendor/bundle",
        "work/foo.apple/node_modules",
        "work/application/node_modules",
        "work/a.bundle-x/node_modules",
    ] {
        let n = node(home.join(path), Class::Regenerable, all_yes());
        let p = plan_of(vec![n]);
        assert!(p.items[0].hold.is_none(), "{path}: {:?}", p.items[0].hold);
        assert!(p.items[0].action.removes(), "{path}");
    }
}

// ---------------------------------------------------------------- misc units

#[test]
fn normalize_resolves_dots_without_leaving_the_root() {
    assert_eq!(
        normalize(Path::new("/a/./b/../c//d")),
        PathBuf::from("/a/c/d")
    );
    assert_eq!(normalize(Path::new("/../..")), PathBuf::from("/"));
}

fn put(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'q'; bytes]).unwrap();
}

fn scan_home(home: &Path) -> ScanResult {
    let mut o = crate::scan::ScanOptions::new(vec![home.to_path_buf()]);
    o.collect_facts = false;
    o.context = ctx_for(home);
    crate::scan::scan(&o).unwrap()
}

fn action_at(p: &Plan, suffix: &str) -> Action {
    p.items
        .iter()
        .find(|i| i.path.ends_with(suffix))
        .unwrap_or_else(|| {
            panic!(
                "no item ending {suffix}: {:?}",
                p.items.iter().map(|i| &i.path).collect::<Vec<_>>()
            )
        })
        .action
}

#[test]
fn task_dirs_and_prefixes() {
    let (_t, home) = fixture_home();
    let work = home.join(".agent-harness/work/s");
    // A git-initialised task dir holding a log and receipts.
    let tg = work.join("tg");
    fs::create_dir_all(tg.join(".git/objects")).unwrap();
    fs::create_dir_all(tg.join(".git/refs")).unwrap();
    fs::write(tg.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    put(&tg.join("session.jsonl"), 500);
    put(&tg.join("run-receipt.json"), 500);
    put(&tg.join("src/main.rs"), 500);
    // A task dir (it has a log at its top level) with a realistic installed tree: READMEs in
    // packages, an estree-walker package, walk.js and a fixture log inside a package.
    let u = work.join("u");
    put(&u.join("session.jsonl"), 500);
    put(&u.join("package.json"), 2);
    put(&u.join("node_modules/estree-walker/package.json"), 2);
    put(&u.join("node_modules/estree-walker/README.md"), 500);
    put(&u.join("node_modules/estree-walker/src/walk.js"), 500);
    put(
        &u.join("node_modules/estree-walker/test/fixtures/a.jsonl"),
        500,
    );
    put(&u.join("node_modules/left-pad/package.json"), 2);
    put(&u.join("node_modules/left-pad/README.md"), 500);
    // Agent files outside package dirs hold the directory: x.JSONL, run-receipt.json, db.wal.
    for (dir, file) in [("v", "x.JSONL"), ("w", "run-receipt.json"), ("y", "db.wal")] {
        let d = work.join(dir);
        put(&d.join("session.jsonl"), 500);
        put(&d.join("package.json"), 2);
        put(&d.join("node_modules").join(file), 500);
    }
    // Prefix roots: a file and directories whose names start like an agent home.
    put(&home.join(".claude.json"), 800);
    put(&home.join(".kimi-work/proj/package.json"), 2);
    put(&home.join(".kimi-work/proj/node_modules/a.js"), 800);
    put(&home.join(".disk-cleaner/manifests/m1.json"), 800);

    let s = scan_home(&home);
    for want in [".claude.json", ".kimi-work", ".disk-cleaner"] {
        let r = s
            .protect
            .roots
            .iter()
            .find(|r| r.spelled.ends_with(&format!("/{want}")))
            .unwrap_or_else(|| panic!("{want} is not a protected root"));
        assert!(r.exists && r.code == HoldCode::Protected, "{r:?}");
    }
    // The checkout `tg` records its top-level log and receipt (2); its `src` has none.
    let tg_node = s
        .nodes
        .iter()
        .find(|n| n.path.ends_with("/s/tg"))
        .expect("tg is a node");
    assert_eq!(tg_node.top_agent_files, Some(2), "{tg_node:?}");
    let nm = |d: &str| {
        s.nodes
            .iter()
            .find(|n| n.path.ends_with(&format!("/s/{d}/node_modules")))
            .unwrap()
    };
    // Counts: packages are not looked into; files straight in node_modules are counted.
    assert_eq!(nm("u").agent_files, Some(0));
    for d in ["v", "w", "y"] {
        assert_eq!(nm(d).agent_files, Some(1), "{d}");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 400 * DAY;
    let p = plan(&s, &opts(now));
    // The git-initialised task dir and its contents are never removing.
    for it in &p.items {
        if it.path.contains("/s/tg") {
            assert!(!it.action.removes(), "{}: {:?}", it.path, it.action);
        }
    }
    assert!(p.items.iter().any(|i| i.path.ends_with("/s/tg")));
    // The realistic installed tree is not held and gets its normal regenerable action.
    let u_nm = p
        .items
        .iter()
        .find(|i| i.path.ends_with("/s/u/node_modules"))
        .unwrap();
    assert_eq!(u_nm.action, Action::RemoveRegenerable, "{}", u_nm.reason);
    assert!(u_nm.hold.is_none());
    // With an agent file outside package dirs it is held.
    for d in ["v", "w", "y"] {
        let it = p
            .items
            .iter()
            .find(|i| i.path.ends_with(&format!("/s/{d}/node_modules")))
            .unwrap();
        assert_eq!(it.action, Action::Keep, "{d}: {}", it.reason);
        assert_eq!(it.hold.as_ref().unwrap().code, HoldCode::Protected);
    }
    // Inside a prefix root.
    assert_eq!(action_at(&p, "/.kimi-work/proj/node_modules"), Action::Keep);
}

#[test]
fn work_dir_allowlist() {
    // Real scan: a task dir `clicktest` with package.json and out/results.tsv; node_modules
    // beside them is removable, out/ is held as build output.
    let (_t, home) = fixture_home();
    let ct = home.join(".agent-harness/work/s/clicktest");
    put(&ct.join("package.json"), 2);
    put(&ct.join("session.jsonl"), 100);
    put(&ct.join("out/results.tsv"), 500);
    put(&ct.join("node_modules/a.js"), 500);
    let s = scan_home(&home);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 400 * DAY;
    let p = plan(&s, &opts(now));
    assert_eq!(
        action_at(&p, "/clicktest/node_modules"),
        Action::RemoveRegenerable
    );
    let out = p
        .items
        .iter()
        .find(|i| i.path.ends_with("/clicktest/out"))
        .unwrap();
    assert_eq!(out.action, Action::Keep, "{}", out.reason);
    assert!(
        out.reason
            .contains("protected: build output inside agent dir"),
        "{}",
        out.reason
    );
    assert!(!action_at(&p, "/clicktest").removes());

    // Synthetic: inside a carve-out checkout, out/ and dist/ are held and node_modules is not.
    let pc = resolve(&ctx_for(&home)).unwrap();
    let base = home.join(".agent-harness/work/s9/clone");
    let x = node(&base, Class::Repo, all_yes());
    let mut kids = Vec::new();
    for (name, rule) in [
        ("out", "out"),
        ("dist", "dist"),
        ("node_modules", "node_modules"),
        ("target", "target"),
        ("coverage", "coverage"),
        ("htmlcov", "htmlcov"),
    ] {
        let mut k = node(base.join(name), Class::Regenerable, all_yes());
        k.parent = Some(0);
        k.rule = Some(rule.into());
        k.agent_files = Some(0);
        kids.push(k);
    }
    // One allowlisted dir with an agent file below it stays held; one with no count is held.
    let mut with_file = node(base.join(".venv"), Class::Regenerable, all_yes());
    with_file.parent = Some(0);
    with_file.rule = Some(".venv".into());
    with_file.agent_files = Some(2);
    let mut no_count = node(base.join("venv"), Class::Regenerable, all_yes());
    no_count.parent = Some(0);
    no_count.rule = Some("venv".into());
    let mut nodes = vec![x];
    nodes.extend(kids);
    nodes.push(with_file);
    nodes.push(no_count);
    let p = plan(&scan_of(pc, nodes), &opts(300 * DAY));
    for (name, removes) in [
        ("out", false),
        ("dist", false),
        ("node_modules", true),
        ("target", true),
        ("coverage", false),
        ("htmlcov", false),
        (".venv", false),
        ("venv", false),
    ] {
        let it = p
            .items
            .iter()
            .find(|i| i.path.ends_with(&format!("/clone/{name}")))
            .unwrap();
        assert_eq!(it.action.removes(), removes, "{name}: {}", it.reason);
    }
    let out = p
        .items
        .iter()
        .find(|i| i.path.ends_with("/clone/out"))
        .unwrap();
    assert!(out.reason.contains("build output inside agent dir"));
}

#[test]
fn config_paths_are_validated() {
    assert!(validate_config_path("/data/agents").is_ok());
    assert!(validate_config_path("~/agents").is_ok());
    for bad in ["", "rel/x", "~", "~/", "/", "~user/x"] {
        assert!(validate_config_path(bad).is_err(), "{bad:?}");
    }
}

// ---------------------------------------------------------------- class P: identity

/// A checkout made by hand (no git process): enough for the scan to classify it as a repo.
fn fake_checkout(dir: &Path) {
    fs::create_dir_all(dir.join(".git/objects")).unwrap();
    fs::create_dir_all(dir.join(".git/refs")).unwrap();
    fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    put(&dir.join("src/main.rs"), 300);
}

/// The scan ran without git: give every checkout the facts of a clean, fully pushed one, so a
/// carve-out position really reaches `backup-then-remove` and "not removed" means something.
fn patch_clean(s: &mut ScanResult) {
    for n in s.nodes.iter_mut() {
        if let Some(info) = n.repo.as_mut() {
            info.facts = all_yes();
            info.git_admin = Some(GitAdminSurvey {
                files: 1,
                bytes: 4096,
                ..Default::default()
            });
            info.object_store = Some(ObjectStoreSurvey {
                files: 1,
                bytes: 4096,
            });
        }
    }
}

/// Every agent-named file below `dir`, found by a plain walk (symlinks not followed).
fn agent_named_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let ft = e.file_type().unwrap();
            if ft.is_dir() && e.file_name() != ".git" {
                stack.push(e.path());
            } else if ft.is_file() && is_agent_file_name(&e.file_name().to_string_lossy()) {
                out.push(e.path());
            }
        }
    }
    out
}

/// The work-session tree every spelling below is scanned through: one entry per path-keyed
/// protection fact the scan records (agent-data checkouts by a top-level WAL or log, a
/// git-initialised task dir, a task dir's child checkout, an allowlisted dependency dir directly
/// in the session), plus the clean controls that must stay removable.
fn build_session(ah: &Path) {
    let s = ah.join("work/s");
    fake_checkout(&s.join("agentdata"));
    put(&s.join("agentdata/run.wal"), 200);
    fake_checkout(&s.join("agentlog"));
    put(&s.join("agentlog/x.jsonl"), 200);
    fake_checkout(&s.join("tg"));
    put(&s.join("tg/run-receipt.json"), 200);
    fake_checkout(&s.join("clean"));
    put(&s.join("task/notes.md"), 100);
    put(&s.join("task/results.tsv"), 100);
    fake_checkout(&s.join("task/wt"));
    put(&s.join("plain/package.json"), 2);
    put(&s.join("plain/node_modules/a.js"), 400);
}

/// What each spelling must produce: the agent-data checkouts and the git-initialised task dir
/// are never removing; the clean checkouts in carve-out positions are `backup-then-remove`; the
/// allowlisted `node_modules` keeps its regenerable action; and no removing item is, contains or
/// lies inside an agent-named file (found with a plain walk of the real tree).
fn assert_session_plan(label: &str, s: &ScanResult, real_ah: &Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 400 * DAY;
    let p = plan(s, &opts(now));
    let find = |suffix: &str| {
        p.items
            .iter()
            .find(|i| i.path.ends_with(suffix))
            .unwrap_or_else(|| {
                panic!(
                    "{label}: no item ending {suffix}: {:?}",
                    p.items.iter().map(|i| &i.path).collect::<Vec<_>>()
                )
            })
    };
    for held in ["/s/agentdata", "/s/agentlog", "/s/tg"] {
        let it = find(held);
        assert!(
            !it.action.removes() && it.hold.is_some(),
            "{label}: {held} is {:?}: {}",
            it.action,
            it.reason
        );
    }
    for clean in ["/s/clean", "/s/task/wt"] {
        let it = find(clean);
        assert_eq!(
            it.action,
            Action::BackupThenRemove,
            "{label}: {clean}: {}",
            it.reason
        );
    }
    let nm = find("/s/plain/node_modules");
    assert_eq!(
        nm.action,
        Action::RemoveRegenerable,
        "{label}: {}",
        nm.reason
    );
    let files: Vec<PathBuf> = agent_named_files(real_ah)
        .into_iter()
        .map(|f| fs::canonicalize(f).unwrap())
        .collect();
    assert!(files.len() >= 3, "{label}: fixture has agent files");
    for it in p.items.iter().filter(|i| i.action.removes()) {
        let r = fs::canonicalize(&it.path).unwrap();
        for f in &files {
            assert!(
                !(f.starts_with(&r) || r.starts_with(f)),
                "{label}: removing {} touches {}",
                it.path,
                f.display()
            );
        }
    }
}

fn scan_roots(home: &Path, roots: Vec<PathBuf>) -> ScanResult {
    let mut o = crate::scan::ScanOptions::new(roots);
    o.collect_facts = false;
    o.context = ctx_for(home);
    let mut s = crate::scan::scan(&o).unwrap();
    patch_clean(&mut s);
    s
}

/// `.agent-harness` is a symlink to a directory elsewhere and the scan root is that real
/// target: the walk never spells `.agent-harness`, yet an agent-data checkout must be held.
#[cfg(unix)]
fn symlinked_harness_scanned_at_real_target() {
    let (tmp, home) = fixture_home();
    let real = fs::canonicalize(tmp.path()).unwrap().join("ah-data");
    fs::remove_dir_all(home.join(".agent-harness")).unwrap();
    fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, home.join(".agent-harness")).unwrap();
    build_session(&real);
    let s = scan_roots(&home, vec![real.clone()]);
    assert_session_plan("scan root at the real target", &s, &real);
}

/// Guard over every path-keyed protection fact (class P): each one must be judged the same
/// whichever spelling the walk used. Spellings: the plain home, a scan root that is another
/// symlink to `.agent-harness`, `.agent-harness` itself a symlink scanned through its link, and
/// scanned at its real target.
#[cfg(unix)]
#[test]
fn path_keyed_facts_hold_under_every_spelling() {
    // `.agent-harness` a real directory.
    {
        let (tmp, home) = fixture_home();
        let ah = home.join(".agent-harness");
        build_session(&ah);
        let s = scan_roots(&home, vec![home.clone()]);
        assert_session_plan("plain home", &s, &ah);
        let alias = fs::canonicalize(tmp.path()).unwrap().join("alias");
        std::os::unix::fs::symlink(&ah, &alias).unwrap();
        let s = scan_roots(&home, vec![alias]);
        assert_session_plan("scan root a symlink to .agent-harness", &s, &ah);
    }
    // `.agent-harness` a symlink to a directory outside the home.
    {
        let (tmp, home) = fixture_home();
        let real = fs::canonicalize(tmp.path()).unwrap().join("ah-data");
        fs::remove_dir_all(home.join(".agent-harness")).unwrap();
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, home.join(".agent-harness")).unwrap();
        build_session(&real);
        let s = scan_roots(&home, vec![home.join(".agent-harness")]);
        assert_session_plan("scan root the .agent-harness link", &s, &real);
        let s = scan_roots(&home, vec![real.clone()]);
        assert_session_plan("scan root the real target", &s, &real);
    }
}

// ---------------------------------------------------------------- class X: exemption scope

/// The work-dir allowlist exempts only the hold whose cause it exempts: the agent harness root's
/// hold on `work/<session>/`, or the roots that put the enclosing carve-out checkout in its
/// carve-out position. Every other root kind keeps its hold on an allowlisted directory.
#[test]
fn allowlist_exempts_only_the_harness_hold() {
    let (_t, home) = fixture_home();
    let s = home.join(".agent-harness/work/s");
    fs::create_dir_all(home.join("Dropbox/p")).unwrap();
    for d in ["ccd", "cx", "xdg/claude", "cfg", "clone2/node_modules"] {
        fs::create_dir_all(s.join(d)).unwrap();
    }
    fs::create_dir_all(home.join(".claude/worktrees/wt/target")).unwrap();
    let mut ctx = ctx_for(&home);
    ctx.claude_config_dir = Some(s.join("ccd").to_string_lossy().into_owned());
    ctx.codex_home = Some(s.join("cx").to_string_lossy().into_owned());
    ctx.xdg_config_home = Some(s.join("xdg").to_string_lossy().into_owned());
    ctx.extra_paths = vec![
        "~/.agent-harness/work/s/cfg".into(),
        "~/.agent-harness/work/s/clone2/node_modules".into(),
        "~/.claude/worktrees/wt/target".into(),
    ];
    let pc = resolve(&ctx).unwrap();
    // Every root kind is in the resolved context (the matrix below is built from it).
    for src in [
        "built-in",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "XDG_CONFIG_HOME",
        "config",
    ] {
        assert!(pc.roots.iter().any(|r| r.source == src), "{src}");
    }
    assert!(pc.roots.iter().any(|r| r.code == HoldCode::Synced));

    let allow = |path: PathBuf, real: Option<PathBuf>, rule: &str| {
        let mut n = node(path, Class::Regenerable, all_yes());
        n.rule = Some(rule.into());
        n.agent_files = Some(0);
        n.real_path = real.map(|r| r.to_string_lossy().into_owned());
        n
    };
    // (label, node, removable?) for a node with no checkout above it.
    let alone: Vec<(&str, Node, bool)> = vec![
        (
            "harness only",
            allow(s.join("plain/node_modules"), None, "node_modules"),
            true,
        ),
        (
            "CLAUDE_CONFIG_DIR",
            allow(s.join("ccd/node_modules"), None, "node_modules"),
            false,
        ),
        (
            "CODEX_HOME",
            allow(s.join("cx/p/.venv"), None, ".venv"),
            false,
        ),
        (
            "XDG_CONFIG_HOME/claude",
            allow(s.join("xdg/claude/node_modules"), None, "node_modules"),
            false,
        ),
        (
            "protected_paths",
            allow(s.join("cfg/node_modules"), None, "node_modules"),
            false,
        ),
        (
            "built-in .claude by real path",
            allow(
                s.join("p/node_modules"),
                Some(home.join(".claude/p/node_modules")),
                "node_modules",
            ),
            false,
        ),
        (
            "prefix root by real path",
            allow(
                s.join("q/node_modules"),
                Some(home.join(".claude.json.d/node_modules")),
                "node_modules",
            ),
            false,
        ),
        (
            "synced by real path",
            allow(
                s.join("d/node_modules"),
                Some(home.join("Dropbox/p/node_modules")),
                "node_modules",
            ),
            false,
        ),
        (
            "package",
            allow(
                s.join("Foo.app/Contents/node_modules"),
                None,
                "node_modules",
            ),
            false,
        ),
        (
            // Spelled under work/<s>/ through a symlink, really elsewhere in the harness root:
            // not on `work/<s>/` by every identity, so not exempt.
            "harness alias from outside work",
            allow(
                s.join("alias/node_modules"),
                Some(home.join(".agent-harness/other/proj/node_modules")),
                "node_modules",
            ),
            false,
        ),
    ];
    fs::create_dir_all(home.join(".claude.json.d")).unwrap();
    let pc = resolve(&ctx).unwrap();
    for (label, n, removable) in alone {
        let p = plan(&scan_of(pc.clone(), vec![n]), &opts(300 * DAY));
        let it = &p.items[0];
        assert_eq!(
            it.action.removes(),
            removable,
            "{label}: {:?} {:?}: {}",
            it.action,
            it.hold,
            it.reason
        );
        if !removable {
            assert!(it.hold.is_some(), "{label}: no hold");
        }
    }
    // Inside a checkout in a carve-out position: the roots that put it there are exempt; a root
    // that covers only the allowlisted directory is not (and the checkout, containing that root,
    // is held itself), nor is another root the directory reaches by its canonical path.
    type Row<'a> = (&'a str, PathBuf, PathBuf, Option<PathBuf>, &'a str, bool);
    let inside: Vec<Row> = vec![
        (
            "work clone, harness only",
            s.join("clone"),
            s.join("clone/node_modules"),
            None,
            "node_modules",
            true,
        ),
        (
            "work clone, config root on it",
            s.join("clone2"),
            s.join("clone2/node_modules"),
            None,
            "node_modules",
            false,
        ),
        (
            "worktree, built-in only",
            home.join(".claude/worktrees/wt2"),
            home.join(".claude/worktrees/wt2/target"),
            None,
            "target",
            true,
        ),
        (
            "worktree, config root on it",
            home.join(".claude/worktrees/wt"),
            home.join(".claude/worktrees/wt/target"),
            None,
            "target",
            false,
        ),
        (
            "worktree, dir really in .codex",
            home.join(".claude/worktrees/wt3"),
            home.join(".claude/worktrees/wt3/target"),
            Some(home.join(".codex/x/target")),
            "target",
            false,
        ),
        (
            // Inside the checkout's own root, but by its canonical path outside the checkout.
            "worktree, dir really elsewhere in .claude",
            home.join(".claude/worktrees/wt4"),
            home.join(".claude/worktrees/wt4/target"),
            Some(home.join(".claude/projects/x/target")),
            "target",
            false,
        ),
    ];
    for (label, co, dep, real, rule, removable) in inside {
        let x = node(&co, Class::Repo, all_yes());
        let mut k = allow(dep, real, rule);
        k.parent = Some(0);
        let p = plan(&scan_of(pc.clone(), vec![x, k]), &opts(300 * DAY));
        // The checkout is removable after a backup only when nothing below it is held.
        assert_eq!(
            p.items[0].action == Action::BackupThenRemove,
            removable,
            "{label}: checkout {:?}: {}",
            p.items[0].action,
            p.items[0].reason
        );
        let it = &p.items[1];
        assert_eq!(
            it.action.removes(),
            removable,
            "{label}: {:?} {:?}: {}",
            it.action,
            it.hold,
            it.reason
        );
    }
}
