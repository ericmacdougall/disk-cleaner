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
}

#[test]
fn dotfiles_repo_at_home_contains_the_roots_and_is_held() {
    // NEG-7: a repo at ~ contains protected roots, so it is held and never removing.
    let (_t, home) = fixture_home();
    let pc = resolve(&ctx_for(&home)).unwrap();
    let dot = node(&home, Class::Repo, all_yes());
    let p = plan(&scan_of(pc, vec![dot]), &opts(300 * DAY));
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
        target.rule = Some("cargo-target".into());
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
    // Not carve-out positions: a checkout inside a task dir that is itself a checkout, two
    // levels below a task dir, and the session dir.
    let task_checkout = node(
        home.join(".agent-harness/work/s2/task"),
        Class::Repo,
        all_yes(),
    );
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
}

#[cfg(unix)]
#[test]
fn a_root_whose_real_path_fails_with_permission_is_compared_by_spelling() {
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
    let n = node(
        home.join("locked/secret/proj/node_modules"),
        Class::Regenerable,
        all_yes(),
    );
    let p = plan(&scan_of(pc, vec![n]), &opts(300 * DAY));
    assert!(held_protected(&p, 0));
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

#[test]
fn config_paths_are_validated() {
    assert!(validate_config_path("/data/agents").is_ok());
    assert!(validate_config_path("~/agents").is_ok());
    for bad in ["", "rel/x", "~", "~/", "/", "~user/x"] {
        assert!(validate_config_path(bad).is_err(), "{bad:?}");
    }
}
