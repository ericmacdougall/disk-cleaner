//! Tests of the apply engine, on disposable fixture homes. Nothing here touches a real home: the
//! state directory is always a fixture's, and every fixture is synthetic.

use super::fsops::{inventory, Dir, FileId};
use super::gates::{courtesy_refusal, plan_id, validate_sandbox};
use super::hooks::{self, Stage};
use super::journal::{self, fold, read_entries, Journal, Record, SkipReason, State};
use super::mounts::{parse_mountinfo, HostMounts, MountTable};
use super::*;
use crate::model::Class;
use crate::plan::Action;
use crate::procs::ProcessSnapshot;
use crate::{plan as make_plan, scan, Plan, PlanOptions, ScanOptions};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The plan's clock: far after every file the fixtures write, so every project is idle.
const T0: i64 = 4_000_000_000;

struct PassGuard;
impl GuardState for PassGuard {
    fn check(&self) -> std::result::Result<(), String> {
        Ok(())
    }
}

struct NoReach;
impl Reach for NoReach {
    fn reaches(&self, _q: &ReachQuery) -> Option<ReachFinding> {
        None
    }
}

struct FakeMounts(Vec<PathBuf>);
impl MountTable for FakeMounts {
    fn mount_points(&self) -> io::Result<Vec<PathBuf>> {
        Ok(self.0.clone())
    }
}

fn no_env(_k: &str) -> Option<String> {
    None
}

/// A fixture: a temporary directory with a home (and its state directory) and a work directory.
struct Fx {
    _tmp: Option<TempDir>,
    root: PathBuf,
    home: PathBuf,
    work: PathBuf,
    state: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut f = Fx::at(&root);
        f._tmp = Some(tmp);
        fs::create_dir_all(&f.home).unwrap();
        fs::create_dir_all(&f.work).unwrap();
        f
    }

    fn at(root: &Path) -> Fx {
        Fx {
            _tmp: None,
            root: root.to_path_buf(),
            home: root.join("home"),
            work: root.join("work"),
            state: root.join("home").join(".prunedisk"),
        }
    }

    fn ctx(&self) -> crate::ScanContext {
        crate::ScanContext {
            home_env: Some(self.home.to_string_lossy().into_owned()),
            declared_home: Some(self.home.clone()),
            ..Default::default()
        }
    }

    /// `work/NAME` with a manifest, a source file and a `node_modules` holding one package.
    fn project(&self, name: &str) -> PathBuf {
        let p = self.work.join(name);
        let nm = p.join("node_modules").join("pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::create_dir_all(p.join("src")).unwrap();
        fs::write(p.join("package.json"), "{}").unwrap();
        fs::write(p.join("src").join("main.js"), "console.log(1)\n").unwrap();
        fs::write(nm.join("package.json"), "{}").unwrap();
        fs::write(nm.join("index.js"), "CANARY-CONTENT-7f3a ".repeat(2048)).unwrap();
        p.join("node_modules")
    }

    fn scan_result(&self, ctx: crate::ScanContext) -> crate::ScanResult {
        let mut so = ScanOptions::new(vec![self.work.clone()]);
        so.context = ctx;
        scan(&so).unwrap()
    }

    fn plan_of(&self, sc: &crate::ScanResult) -> Plan {
        let mut po = PlanOptions::new(7, T0);
        po.window_source = "test".into();
        po.process = ProcessSnapshot::none_running();
        make_plan(sc, &po)
    }

    /// Scan and plan the work directory; write the plan file; return it with its id.
    fn make_plan(&self) -> (PathBuf, String, Plan) {
        let sc = self.scan_result(self.ctx());
        let p = self.plan_of(&sc);
        let file = self.root.join("plan.json");
        let text = serde_json::to_string_pretty(&p).unwrap();
        fs::write(&file, &text).unwrap();
        let id = plan_id(text.as_bytes());
        fs::write(self.root.join("plan.id"), &id).unwrap();
        (file, id, p)
    }

    fn env<'a>(
        &self,
        guard: &'a dyn GuardState,
        reach: &'a dyn Reach,
        mounts: &'a dyn MountTable,
    ) -> Env<'a> {
        Env {
            state_dir: self.state.clone(),
            home: self.home.clone(),
            now: T0,
            max_plan_age_days: 3,
            host: Default::default(),
            scan_context: self.ctx(),
            facts: Default::default(),
            guard,
            reach,
            mounts,
        }
    }

    fn journal_names(&self, id: &str) -> Vec<&'static str> {
        read_entries(&Journal::path_of(&self.state, id))
            .unwrap()
            .iter()
            .map(|e| hooks::rec_name(&e.rec))
            .collect()
    }

    fn journal_text(&self, id: &str) -> String {
        fs::read_to_string(Journal::path_of(&self.state, id)).unwrap_or_default()
    }
}

fn request<'a>(
    plan: &'a Path,
    id: &'a str,
    getenv: &'a dyn Fn(&str) -> Option<String>,
) -> Request<'a> {
    Request {
        plan_file: Some(plan),
        plan_id: Some(id),
        only: Some("regenerable"),
        resume: false,
        journal: None,
        max_items: None,
        sandbox: None,
        stdin_is_terminal: true,
        getenv,
    }
}

fn resume_request<'a>(
    plan: &'a Path,
    id: &'a str,
    getenv: &'a dyn Fn(&str) -> Option<String>,
) -> Request<'a> {
    Request {
        resume: true,
        only: None,
        ..request(plan, id, getenv)
    }
}

fn apply_ok(fx: &Fx, plan: &Path, id: &str) -> Report {
    let mounts = FakeMounts(vec![]);
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    match run(&request(plan, id, &no_env), &env).unwrap() {
        Outcome::Applied(r) => r,
        other => panic!("expected Applied, got {other:?}"),
    }
}

/// Index of the item for `path` in the plan file.
fn idx_of(p: &Plan, path: &Path) -> usize {
    p.items
        .iter()
        .position(|i| i.path == path.to_string_lossy())
        .expect("the plan has the item")
}

fn tree_digest(root: &Path) -> String {
    let d = Dir::open_path(root).unwrap();
    inventory(&d, &HashSet::new()).unwrap().digest
}

fn quarantines(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".prunedisk-q-")
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ AC-2, NEG-2, NEG-8

#[test]
fn regenerable_happy_path() {
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, p) = fx.make_plan();
    assert_eq!(
        p.items
            .iter()
            .filter(|i| i.action == Action::RemoveRegenerable)
            .count(),
        1,
        "control: the fixture offers exactly one item"
    );
    hooks::take_trace();
    let report = apply_ok(&fx, &plan, &id);
    assert!(!nm.exists(), "the node_modules directory is gone");
    assert!(
        fx.work.join("a/src/main.js").exists(),
        "the rest of the project stays"
    );
    assert_eq!(report.removed, 1);
    assert!(report.bytes >= 40_960, "bytes freed {}", report.bytes);
    assert!(quarantines(&fx.work.join("a")).is_empty());
    assert_eq!(
        fx.journal_names(&id),
        [
            "start",
            "quarantining",
            "quarantined",
            "removing",
            "removed",
            "end"
        ]
    );
    // Every mutating syscall is directly preceded by the fsynced record that announces it.
    let trace = hooks::take_trace();
    let mut announced: Option<&str> = None;
    let mut syscalls = 0;
    for ev in &trace {
        match ev.as_str() {
            "J:quarantining" => announced = Some("rename"),
            "J:removing" => announced = Some("unlink"),
            "J:restoring" => announced = Some("rename"),
            e if e.starts_with("S:") => {
                assert_eq!(
                    announced,
                    Some(&e[2..]),
                    "syscall {e} without its intent: {trace:?}"
                );
                syscalls += 1;
            }
            _ => {}
        }
    }
    assert!(syscalls > 3, "the trace saw the syscalls: {trace:?}");
    // File contents never reach the journal or the report.
    assert!(!fx.journal_text(&id).contains("CANARY-CONTENT"));
    assert!(!report.to_string().contains("CANARY-CONTENT"));
}

#[test]
fn rename_target_exists_is_skipped_untouched() {
    // NEG-2
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, p) = fx.make_plan();
    let squatter = fx
        .work
        .join("a")
        .join(format!(".prunedisk-q-{id}-{}", idx_of(&p, &nm)));
    fs::create_dir(&squatter).unwrap();
    fs::write(squatter.join("keep.txt"), "mine").unwrap();
    let report = apply_ok(&fx, &plan, &id);
    assert_eq!(report.removed, 0);
    assert!(nm.join("pkg/index.js").exists(), "the item is untouched");
    assert_eq!(
        fs::read_to_string(squatter.join("keep.txt")).unwrap(),
        "mine"
    );
    assert!(matches!(report.skipped[0].2, SkipReason::Rename));
}

#[test]
fn filesystem_without_noreplace_rename_is_skipped_and_counted() {
    // NEG-8
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    hooks::force_rename_error(Some(rustix::io::Errno::INVAL));
    let report = apply_ok(&fx, &plan, &id);
    hooks::force_rename_error(None);
    assert!(nm.join("pkg/index.js").exists());
    assert_eq!(report.removed, 0);
    let (_, _, reason, detail) = &report.skipped[0];
    assert_eq!(*reason, SkipReason::Rename);
    assert!(detail.contains("counted"), "{detail}");
    assert!(quarantines(&fx.work.join("a")).is_empty());
}

// ------------------------------------------------------------------ AC-3, NEG-1

/// Plan, change the fixture, apply: the item must stay.
fn assert_stays(change: impl FnOnce(&Fx, &Path), what: &str) {
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    change(&fx, &nm);
    let before = fs::symlink_metadata(&nm).is_ok();
    let report = apply_ok(&fx, &plan, &id);
    assert_eq!(report.removed, 0, "{what}: nothing is removed");
    if before {
        assert!(
            nm.join("pkg/index.js").exists() || fs::symlink_metadata(&nm).is_ok(),
            "{what}: the item is still there"
        );
    }
    assert!(
        quarantines(&fx.work.join("a")).is_empty(),
        "{what}: no quarantine left"
    );
    assert!(
        fx.journal_names(&id).iter().all(|n| *n != "removing"),
        "{what}: no deletion started"
    );
}

#[test]
fn changed_after_plan() {
    // The control: unchanged, the same fixture is removed.
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    assert_eq!(apply_ok(&fx, &plan, &id).removed, 1);
    assert!(!nm.exists());

    assert_stays(
        |_, nm| {
            let d = nm.join("loose");
            fs::create_dir_all(d.join("deep/er")).unwrap();
            fs::write(d.join("deep/er/clip.mp4"), vec![1u8; 4096]).unwrap();
        },
        "an output file deep inside",
    );
    assert_stays(
        |_, nm| fs::write(nm.join("pkg/.env"), "SECRET=1").unwrap(),
        "a state file",
    );
    assert_stays(
        |fx, _| {
            // The owning project becomes active: a source file written inside the window.
            let f = fs::File::options()
                .write(true)
                .open(fx.work.join("a/src/main.js"))
                .unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(T0 as u64))
                .unwrap();
        },
        "the project becomes active",
    );
    assert_stays(
        |_, nm| {
            fs::remove_dir_all(nm).unwrap();
            fs::create_dir_all(nm.parent().unwrap().join("elsewhere")).unwrap();
            std::os::unix::fs::symlink(nm.parent().unwrap().join("elsewhere"), nm).unwrap();
        },
        "replaced by a symlink",
    );
    assert_stays(
        |fx, _| fs::remove_file(fx.work.join("a/package.json")).unwrap(),
        "its rule's marker is gone",
    );
}

#[test]
fn changed_after_plan_a_protected_root_inside() {
    // The apply-time environment names a protected root inside the item: the fresh plan holds it.
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    let inner = nm.join("pkg");
    let mut ctx = fx.ctx();
    ctx.extra_paths = vec![inner.to_string_lossy().into_owned()];
    let mounts = FakeMounts(vec![]);
    let mut env = fx.env(&PassGuard, &NoReach, &mounts);
    env.scan_context = ctx;
    match run(&request(&plan, &id, &no_env), &env).unwrap() {
        Outcome::Applied(r) => assert_eq!(r.removed, 0, "{r}"),
        o => panic!("{o:?}"),
    }
    assert!(inner.join("index.js").exists());
}

// ------------------------------------------------------------------ AC-4

#[test]
fn change_before_delete() {
    // A change in the quarantine after the digest is recorded restores the item in place.
    type Change = Box<dyn Fn(&Path)>;
    let changes: Vec<(&str, Change)> = vec![
        (
            "a file is added",
            Box::new(|q: &Path| fs::write(q.join("pkg/new.js"), "x").unwrap()),
        ),
        (
            "a file is rewritten with the same size and mtime but a new ctime",
            Box::new(|q: &Path| {
                let f = q.join("pkg/index.js");
                let md = fs::metadata(&f).unwrap();
                let mtime = md.modified().unwrap();
                let bytes = fs::read(&f).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                fs::write(&f, bytes).unwrap();
                fs::File::options()
                    .write(true)
                    .open(&f)
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
            }),
        ),
    ];
    for (what, change) in changes {
        let fx = Fx::new();
        let nm = fx.project("a");
        let (plan, id, _) = fx.make_plan();
        hooks::set_stage(Some(Box::new(move |st, q| {
            if st == Stage::AfterQuarantined {
                change(q);
            }
        })));
        let report = apply_ok(&fx, &plan, &id);
        hooks::set_stage(None);
        assert_eq!(report.removed, 0, "{what}");
        assert!(
            nm.join("pkg/index.js").exists(),
            "{what}: restored in place"
        );
        assert!(quarantines(&fx.work.join("a")).is_empty(), "{what}");
        let names = fx.journal_names(&id);
        assert!(
            names.contains(&"restoring") && names.contains(&"restored-in-place"),
            "{what}: {names:?}"
        );
        assert!(!names.contains(&"removing"), "{what}");
    }
}

#[test]
fn change_after_the_copy_checks_restores_via_last_check() {
    // The change lands after step 3, so only the last check (step 4) can see it.
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    hooks::set_stage(Some(Box::new(|st, q| {
        if st == Stage::BeforeLastCheck {
            fs::write(q.join("pkg/late.js"), "x").unwrap();
        }
    })));
    let report = apply_ok(&fx, &plan, &id);
    hooks::set_stage(None);
    assert_eq!(report.removed, 0);
    assert!(nm.join("pkg/late.js").exists());
}

#[test]
fn a_reach_found_once_quarantined_restores_in_place() {
    // A process that reaches the item once it sits in quarantine (the Class D rules plug in
    // through this seam) restores it; before the move the same process is not seen.
    struct QuarantineReach;
    impl Reach for QuarantineReach {
        fn reaches(&self, q: &ReachQuery) -> Option<ReachFinding> {
            q.quarantine.map(|_| ReachFinding {
                rule: "M",
                detail: "pid 1 (stub)".into(),
            })
        }
    }
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    let mounts = FakeMounts(vec![]);
    let env = fx.env(&PassGuard, &QuarantineReach, &mounts);
    let Outcome::Applied(r) = run(&request(&plan, &id, &no_env), &env).unwrap() else {
        panic!()
    };
    assert_eq!(r.removed, 0);
    assert!(nm.join("pkg/index.js").exists());
    let names = fx.journal_names(&id);
    assert!(
        names.contains(&"quarantined") && names.contains(&"restored-in-place"),
        "{names:?}"
    );
}

// ------------------------------------------------------------------ AC-5 (table logic) and NEG-5

#[test]
fn mount_boundary() {
    // A mount point at or below the item skips it, nothing under it is removed.
    for mount in ["node_modules", "node_modules/pkg"] {
        let fx = Fx::new();
        let nm = fx.project("a");
        let (plan, id, _) = fx.make_plan();
        let mounts = FakeMounts(vec![fx.work.join("a").join(mount)]);
        let env = fx.env(&PassGuard, &NoReach, &mounts);
        let Outcome::Applied(r) = run(&request(&plan, &id, &no_env), &env).unwrap() else {
            panic!()
        };
        assert_eq!(r.removed, 0, "{mount}");
        assert!(matches!(r.skipped[0].2, SkipReason::Mountpoint), "{mount}");
        assert!(nm.join("pkg/index.js").exists());
        assert!(fx.journal_names(&id).iter().all(|n| *n != "quarantining"));
    }
    // A mount point elsewhere (a sibling) does not.
    let fx = Fx::new();
    fx.project("a");
    let (plan, id, _) = fx.make_plan();
    let mounts = FakeMounts(vec![fx.work.join("a/src")]);
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    let Outcome::Applied(r) = run(&request(&plan, &id, &no_env), &env).unwrap() else {
        panic!()
    };
    assert_eq!(r.removed, 1);
    // An unreadable table fails closed.
    struct Broken;
    impl MountTable for Broken {
        fn mount_points(&self) -> io::Result<Vec<PathBuf>> {
            Err(io::Error::other("no table"))
        }
    }
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    let env = fx.env(&PassGuard, &NoReach, &Broken);
    let Outcome::Applied(r) = run(&request(&plan, &id, &no_env), &env).unwrap() else {
        panic!()
    };
    assert_eq!(r.removed, 0);
    assert!(nm.exists());
}

#[test]
fn mountinfo_is_decoded() {
    let text = "36 35 98:0 /mnt1 /mnt/with\\040space rw - ext3 /dev/sda rw\n\
                37 35 98:0 /x /a\\134b rw - ext3 /dev/sda rw\n";
    let m = parse_mountinfo(text);
    assert_eq!(
        m,
        [PathBuf::from("/mnt/with space"), PathBuf::from("/a\\b")]
    );
    // The real table of this host parses and holds the root.
    let real = HostMounts.mount_points().unwrap();
    assert!(real.iter().any(|p| p == Path::new("/")), "{real:?}");
}

#[test]
fn symlink_inside_the_item_is_unlinked_not_followed() {
    // NEG-5: a link inside the item pointing at a protected directory is removed as a link.
    let fx = Fx::new();
    let nm = fx.project("a");
    let target = fx.root.join("precious");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("data.txt"), "keep me").unwrap();
    std::os::unix::fs::symlink(&target, nm.join("pkg/link-out")).unwrap();
    let (plan, id, _) = fx.make_plan();
    let report = apply_ok(&fx, &plan, &id);
    assert_eq!(report.removed, 1, "{report}");
    assert!(!nm.exists());
    assert_eq!(
        fs::read_to_string(target.join("data.txt")).unwrap(),
        "keep me"
    );
}

// ------------------------------------------------------------------ AC-8

#[test]
fn lock_and_journal_home() {
    use rustix::fs::{flock, FlockOperation};
    // A second apply while one runs exits as Busy with no change.
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    fs::create_dir_all(&fx.state).unwrap();
    let held = fs::File::create(fx.state.join("apply.lock")).unwrap();
    flock(&held, FlockOperation::NonBlockingLockExclusive).unwrap();
    let mounts = FakeMounts(vec![]);
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    assert!(matches!(
        run(&request(&plan, &id, &no_env), &env).unwrap(),
        Outcome::Busy
    ));
    assert!(nm.join("pkg/index.js").exists());
    assert!(!Journal::path_of(&fx.state, &id).exists());
    drop(held);
    assert!(matches!(
        run(&request(&plan, &id, &no_env), &env).unwrap(),
        Outcome::Applied(_)
    ));

    // The journal directory inside a candidate: refuse to start.
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    let mounts = FakeMounts(vec![]);
    let mut env = fx.env(&PassGuard, &NoReach, &mounts);
    env.state_dir = nm.join(".prunedisk");
    match run(&request(&plan, &id, &no_env), &env).unwrap() {
        Outcome::Refused(m) => assert!(m.contains("lies inside the candidate"), "{m}"),
        o => panic!("{o:?}"),
    }
    assert!(nm.join("pkg/index.js").exists());

    // An unfinished journal of this plan id, without --resume, refuses new work.
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    {
        let mut j = Journal::open(&fx.state, &id, T0).unwrap();
        j.start("new", 1).unwrap();
        let parent = Dir::open_path(&fx.work.join("a")).unwrap();
        let from = nm.to_string_lossy().into_owned();
        j.intent_quarantine(
            0,
            &from,
            "/nowhere/.prunedisk-q-x-0",
            &parent,
            Some("node_modules"),
        )
        .unwrap();
    }
    let mounts = FakeMounts(vec![]);
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    match run(&request(&plan, &id, &no_env), &env).unwrap() {
        Outcome::Refused(m) => assert!(m.contains("--resume"), "{m}"),
        o => panic!("{o:?}"),
    }
    assert!(nm.join("pkg/index.js").exists());
}

// ------------------------------------------------------------------ AC-9

#[test]
fn hardlink_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let outside = root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let mk = |name: &str| -> PathBuf {
        let t = root.join(name);
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("big.bin"), vec![7u8; 100_000]).unwrap();
        t
    };
    let bytes = |t: &Path| {
        inventory(&Dir::open_path(t).unwrap(), &HashSet::new())
            .unwrap()
            .bytes
    };
    let plain = mk("plain");
    let base = bytes(&plain);
    assert!(base >= 100_000, "control: the file is counted ({base})");
    // A second link inside the tree: counted once.
    let twice = mk("twice");
    fs::hard_link(twice.join("big.bin"), twice.join("copy.bin")).unwrap();
    assert_eq!(bytes(&twice), base, "a link inside adds nothing");
    // A link outside the tree: not counted (freeing the tree frees nothing).
    let linked = mk("linked");
    fs::hard_link(linked.join("big.bin"), outside.join("other.bin")).unwrap();
    let empty = root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    assert_eq!(
        bytes(&linked),
        bytes(&empty),
        "a file with a link outside is not counted"
    );
}

// ------------------------------------------------------------------ AC-10 and gates

#[test]
fn protection_parity() {
    // A plan that names a protected root which this environment no longer yields still holds it.
    let build = |strip_recorded: bool| {
        let fx = Fx::new();
        let codex = fx.work.join("x").join("codexhome");
        let nm = codex.join("proj").join("node_modules");
        fs::create_dir_all(nm.join("pkg")).unwrap();
        fs::write(codex.join("proj/package.json"), "{}").unwrap();
        fs::write(nm.join("pkg/package.json"), "{}").unwrap();
        fs::write(nm.join("pkg/index.js"), "x".repeat(8192)).unwrap();
        // The plan is made with CODEX_HOME at `codex`.
        let mut ctx = fx.ctx();
        ctx.codex_home = Some(codex.to_string_lossy().into_owned());
        let sc = fx.scan_result(ctx);
        let p = fx.plan_of(&sc);
        let held = p
            .items
            .iter()
            .find(|i| i.path == nm.to_string_lossy())
            .unwrap();
        assert_eq!(
            held.action,
            Action::Keep,
            "control: the root holds it at plan time"
        );
        // A reviewed plan that nevertheless offers it (hand-edited), recorded roots kept or not.
        let mut v = serde_json::to_value(&p).unwrap();
        for it in v["items"].as_array_mut().unwrap() {
            if it["path"].as_str() == nm.to_str() {
                it["action"] = "remove-regenerable".into();
            }
        }
        if strip_recorded {
            v["protect"]["roots"] = serde_json::json!([]);
        }
        let text = serde_json::to_string(&v).unwrap();
        let file = fx.root.join("plan.json");
        fs::write(&file, &text).unwrap();
        (fx, nm, file, plan_id(text.as_bytes()))
    };
    let (fx, nm, file, id) = build(false);
    let mounts = FakeMounts(vec![]);
    // Apply's environment has no CODEX_HOME.
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    let Outcome::Applied(r) = run(&request(&file, &id, &no_env), &env).unwrap() else {
        panic!()
    };
    assert_eq!(r.removed, 0, "{r}");
    assert!(
        nm.join("pkg/index.js").exists(),
        "the recorded root still holds it"
    );
    // Control: without the recorded root the same offer is acted on, so the test is not vacuous.
    let (fx, nm, file, id) = build(true);
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    let Outcome::Applied(r) = run(&request(&file, &id, &no_env), &env).unwrap() else {
        panic!()
    };
    assert_eq!(r.removed, 1, "{r}");
    assert!(!nm.exists());

    // CODEX_HOME is configuration, not an agent marker; the markers refuse.
    let codex = |k: &str| (k == "CODEX_HOME").then(|| "/x".to_string());
    assert!(courtesy_refusal(true, &codex).is_none());
    for m in super::gates::AGENT_MARKERS {
        let env = move |k: &str| (k == m).then(|| "1".to_string());
        assert!(courtesy_refusal(true, &env).is_some(), "{m}");
    }
    assert!(courtesy_refusal(false, &no_env).is_some(), "no terminal");
}

#[test]
fn refusals_change_nothing() {
    // AC-1 at the library level: every refusal leaves the tree and the journal as they were.
    let fx = Fx::new();
    let nm = fx.project("a");
    let (plan, id, _) = fx.make_plan();
    // Yesterday's unfinished journal of another plan id must stay byte for byte.
    let other = "0123456789abcdef";
    {
        let mut j = Journal::open(&fx.state, other, T0).unwrap();
        j.start("new", 1).unwrap();
        let parent = Dir::open_path(&fx.work).unwrap();
        j.intent_quarantine(0, "/x/y", "/x/.prunedisk-q-z-0", &parent, None)
            .unwrap();
    }
    let journal_bytes = || fs::read(Journal::path_of(&fx.state, other)).unwrap();
    let (before_tree, before_journal) = (tree_digest(&fx.work), journal_bytes());
    let mounts = FakeMounts(vec![]);
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    let agent = |k: &str| (k == "CLAUDECODE").then(|| "1".to_string());

    let mut cases: Vec<(&str, Outcome)> = Vec::new();
    // no --plan-id
    let mut r = request(&plan, &id, &no_env);
    r.plan_id = None;
    cases.push(("no plan id", run(&r, &env).unwrap()));
    // wrong --plan-id
    let r = request(&plan, "ffffffffffffffff", &no_env);
    cases.push(("wrong plan id", run(&r, &env).unwrap()));
    // no terminal
    let mut r = request(&plan, &id, &no_env);
    r.stdin_is_terminal = false;
    cases.push(("no terminal", run(&r, &env).unwrap()));
    // agent marker
    cases.push((
        "agent marker",
        run(&request(&plan, &id, &agent), &env).unwrap(),
    ));
    // missing --only
    let mut r = request(&plan, &id, &no_env);
    r.only = None;
    cases.push(("no --only", run(&r, &env).unwrap()));
    // an invalid sandbox
    let mut r = request(&plan, &id, &no_env);
    let bad = fx.root.join("not-a-sandbox");
    fs::create_dir_all(&bad).unwrap();
    r.sandbox = Some(&bad);
    cases.push(("invalid sandbox", run(&r, &env).unwrap()));
    // the guard refuses
    let env2 = fx.env(&Unverified, &NoReach, &mounts);
    cases.push((
        "guard unverified",
        run(&request(&plan, &id, &no_env), &env2).unwrap(),
    ));
    // an over-age plan
    let mut env3 = fx.env(&PassGuard, &NoReach, &mounts);
    env3.now = T0 + 10 * 86_400;
    cases.push((
        "over-age plan",
        run(&request(&plan, &id, &no_env), &env3).unwrap(),
    ));
    for (what, out) in &cases {
        match (*what, out) {
            ("no plan id" | "wrong plan id", Outcome::NothingChanged(m)) => {
                assert!(m.contains(&id), "{what}: prints the plan id");
                assert!(
                    m.contains(other),
                    "{what}: lists the pending journal of another plan"
                );
            }
            (_, Outcome::Refused(_)) => {}
            (w, o) => panic!("{w}: unexpected {o:?}"),
        }
        assert_eq!(
            tree_digest(&fx.work),
            before_tree,
            "{what}: the tree is unchanged"
        );
        assert_eq!(
            journal_bytes(),
            before_journal,
            "{what}: the journal is unchanged"
        );
        assert!(
            !Journal::path_of(&fx.state, &id).exists(),
            "{what}: no journal was made"
        );
        assert!(nm.join("pkg/index.js").exists());
    }
    // A plan of another schema is refused as new work.
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
    v["schema_version"] = 17.into();
    let old = fx.root.join("old-plan.json");
    let text = v.to_string();
    fs::write(&old, &text).unwrap();
    let old_id = plan_id(text.as_bytes());
    match run(&request(&old, &old_id, &no_env), &env).unwrap() {
        Outcome::Refused(m) => assert!(m.contains("not accepted"), "{m}"),
        o => panic!("{o:?}"),
    }
    assert_eq!(tree_digest(&fx.work), before_tree);
}

#[test]
fn plan_id_is_16_hex_of_sha256() {
    // SHA-256("abc") = ba7816bf8f01cfea...
    assert_eq!(plan_id(b"abc"), "ba7816bf8f01cfea");
}

#[test]
fn sandbox_validity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sb = root.join("sb");
    let home = root.join("home");
    fs::create_dir_all(sb.join("proj/node_modules")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let cand = vec![sb.join("proj/node_modules")];
    let ok = |dir: &Path, prot: &[PathBuf], c: &[PathBuf]| {
        validate_sandbox(dir, &home, prot, c, &no_env)
    };
    assert!(ok(&sb, &[], &cand).is_err(), "no .prunedisk-sandbox file");
    fs::write(sb.join(".prunedisk-sandbox"), "").unwrap();
    assert!(ok(&sb, &[], &cand).is_ok(), "control: valid");
    assert!(
        ok(&sb, &[], &[home.join("proj")]).is_err(),
        "a candidate outside it"
    );
    assert!(
        ok(&sb, &[sb.join("proj")], &cand).is_err(),
        "a protected root inside it"
    );
    assert!(
        ok(&sb, std::slice::from_ref(&root), &cand).is_err(),
        "inside a protected root"
    );
    // Under the home directory: refused.
    let in_home = home.join("sb");
    fs::create_dir_all(&in_home).unwrap();
    fs::write(in_home.join(".prunedisk-sandbox"), "").unwrap();
    assert!(ok(&in_home, &[], &[]).is_err());
    // `/tmp/../x` style spellings and links.
    let link = root.join("link-to-sb");
    std::os::unix::fs::symlink(&sb, &link).unwrap();
    assert!(ok(&link, &[], &cand).is_err(), "a symlink component");
    assert!(
        ok(&sb.join("..").join("sb"), &[], &cand).is_err(),
        "a .. component"
    );
    // Not under a temporary root.
    assert!(ok(Path::new("/usr"), &[], &[]).is_err());
}

// ------------------------------------------------------------------ the choke point

/// The only file of this module that may name a mutating call. Returns the violations in `src`
/// (a file called `name`): lines that name one outside `fsops.rs` and outside test code.
fn mutation_violations(name: &str, src: &str) -> Vec<String> {
    const CALLS: [&str; 12] = [
        "unlinkat(",
        "renameat(",
        "renameat_with(",
        "remove_file(",
        "remove_dir(",
        "remove_dir_all(",
        "fs::rename(",
        "fs::write(",
        "create_dir(",
        "create_dir_all(",
        "symlink(",
        "set_permissions(",
    ];
    if name == "fsops.rs" || name == "tests.rs" || name == "hooks.rs" {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let named = |c: &&str| {
            code.match_indices(*c).any(|(i, _)| {
                !code[..i]
                    .chars()
                    .next_back()
                    .is_some_and(|p| p.is_alphanumeric() || p == '_')
            })
        };
        if CALLS.iter().any(named) {
            out.push(format!("{name}:{}: {}", i + 1, line.trim()));
        }
    }
    out
}

#[test]
fn no_mutation_outside_the_choke_point() {
    // The journal, the lock and the state directory are created by the engine itself; those
    // lines are the reviewed exceptions, named exactly.
    let allowed = ["create_dir_all(", ".create(dir)", ".create(&env.state_dir)"];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/apply");
    let mut bad = Vec::new();
    for e in fs::read_dir(&dir).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".rs") {
            continue;
        }
        let src = fs::read_to_string(e.path()).unwrap();
        for v in mutation_violations(&name, &src) {
            if !allowed.iter().any(|a| v.contains(a)) {
                bad.push(v);
            }
        }
    }
    assert!(bad.is_empty(), "mutating calls outside fsops.rs: {bad:#?}");
    // The lint can fail: a planted bypass is found.
    let planted = "fn bad(d: &Dir) { rustix::fs::unlinkat(&d.fd, \"x\", AtFlags::empty()); }";
    assert_eq!(mutation_violations("item.rs", planted).len(), 1);
    let planted = "fn bad(p: &Path) { std::fs::remove_dir_all(p); }";
    assert_eq!(mutation_violations("run.rs", planted).len(), 1);
}

// ------------------------------------------------------------------ resume (AC-7)

/// A child process of this test binary runs the scenario and parks after a chosen journal
/// record (or inside the deletion); the parent SIGKILLs it there.
#[test]
fn child_scenario() {
    let Ok(dir) = std::env::var("PD_TEST_CHILD_DIR") else {
        return;
    };
    let fx = Fx::at(Path::new(&dir));
    let id = fs::read_to_string(fx.root.join("plan.id")).unwrap();
    let mounts = FakeMounts(vec![]);
    let env = fx.env(&PassGuard, &NoReach, &mounts);
    if std::env::var("PD_TEST_SCENARIO").as_deref() == Ok("restore") {
        hooks::set_stage(Some(Box::new(|st, q| {
            if st == Stage::AfterQuarantined {
                fs::write(q.join("pkg/new.js"), "x").unwrap();
            }
        })));
    }
    let plan = fx.root.join("plan.json");
    let _ = run(&request(&plan, &id, &no_env), &env);
}

/// Run the scenario in a child, kill it with SIGKILL once it has parked, return the fixture.
fn killed_run(park: &[(&str, &str)], scenario: &str) -> (Fx, PathBuf, String) {
    let fx = Fx::new();
    let nm = fx.project("a");
    // A second file tree level so the deletion has something below the root to park in.
    fs::create_dir_all(nm.join("pkg/sub/deeper")).unwrap();
    fs::write(nm.join("pkg/sub/deeper/f.txt"), "f").unwrap();
    let (_, id, _) = fx.make_plan();
    let marker = fx.root.join("parked");
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "apply::tests::child_scenario",
        "--nocapture",
        "--test-threads=1",
    ])
    .env("PD_TEST_CHILD_DIR", &fx.root)
    .env("PD_TEST_SCENARIO", scenario)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    for (k, v) in park {
        cmd.env(k, v.replace("MARKER", &marker.to_string_lossy()));
    }
    let mut child = cmd.spawn().unwrap();
    let start = std::time::Instant::now();
    while !marker.exists() {
        assert!(start.elapsed().as_secs() < 120, "the child never parked");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the child exited before parking"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.kill().unwrap(); // SIGKILL on Unix
    child.wait().unwrap();
    (fx, nm, id)
}

fn resume(fx: &Fx, id: &str, now: i64) -> Report {
    let mounts = FakeMounts(vec![]);
    let mut env = fx.env(&Unverified, &NoReach, &mounts);
    env.now = now;
    let plan = fx.root.join("plan.json");
    let out = run(&resume_request(&plan, id, &no_env), &env).unwrap();
    match out {
        Outcome::Resumed(r) => r,
        o => panic!("{o:?}"),
    }
}

#[test]
fn resume_matrix() {
    let q = |rec: &str| vec![("PD_TEST_PARK_AFTER", format!("{rec}:1:MARKER"))];
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Vec<(&str, String)>, &str, bool)> = vec![
        // (killed after, park spec, scenario, ends removed)
        ("quarantining", q("quarantining"), "remove", false),
        ("quarantined", q("quarantined"), "remove", false),
        ("removing", q("removing"), "remove", true),
        ("removed", q("removed"), "remove", true),
        ("restoring", q("restoring"), "restore", false),
        (
            "restored-in-place",
            q("restored-in-place"),
            "restore",
            false,
        ),
        (
            "mid-delete",
            vec![("PD_TEST_PARK_PURGE", "MARKER".into())],
            "remove",
            true,
        ),
    ];
    for (what, park, scenario, removed) in cases {
        let park: Vec<(&str, &str)> = park.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (fx, nm, id) = killed_run(&park, scenario);
        // Resume works with an over-age plan and without the guard.
        let report = resume(&fx, &id, T0 + 30 * 86_400);
        let names = fx.journal_names(&id);
        assert!(
            quarantines(&fx.work.join("a")).is_empty(),
            "{what}: no quarantine remains: {report}"
        );
        if removed {
            assert!(!nm.exists(), "{what}: removed: {report}");
            let n = names.iter().filter(|n| **n == "removed").count();
            assert_eq!(n, 1, "{what}: removed exactly once: {names:?}");
        } else {
            assert!(
                nm.join("pkg/index.js").exists(),
                "{what}: restored in place: {report}"
            );
            assert!(!names.contains(&"removed"), "{what}: {names:?}");
        }
        // Every item ends in a final state; a second resume finds nothing to do.
        let logs = fold(&read_entries(&Journal::path_of(&fx.state, &id)).unwrap());
        assert!(logs.iter().all(|l| l.state.is_final()), "{what}: {logs:?}");
        let again = resume(&fx, &id, T0);
        assert!(again.lines.is_empty(), "{what}: nothing left: {again}");
    }
}

#[test]
fn resume_parent_swapped_stops_the_item() {
    let q = vec![("PD_TEST_PARK_AFTER", "quarantined:1:MARKER")];
    let (fx, _nm, id) = killed_run(&q, "remove");
    let parent = fx.work.join("a");
    let moved = fx.work.join("a-moved");
    fs::rename(&parent, &moved).unwrap();
    fs::create_dir(&parent).unwrap(); // a different directory at the same path
    let report = resume(&fx, &id, T0);
    assert_eq!(report.kept.len(), 1, "{report}");
    assert!(report.kept[0].2.contains("swapped"), "{report}");
    // Nothing was deleted or renamed in either directory.
    assert!(
        quarantines(&moved).len() == 1,
        "the quarantine stays in the moved directory"
    );
    assert!(fs::read_dir(&parent).unwrap().next().is_none());
}

#[test]
fn resume_original_recreated_is_never_overwritten() {
    // NEG-4
    let q = vec![("PD_TEST_PARK_AFTER", "quarantined:1:MARKER")];
    let (fx, nm, id) = killed_run(&q, "remove");
    fs::create_dir(&nm).unwrap();
    fs::write(nm.join("mine.txt"), "new").unwrap();
    let report = resume(&fx, &id, T0);
    assert_eq!(report.kept.len(), 1, "{report}");
    assert_eq!(fs::read_to_string(nm.join("mine.txt")).unwrap(), "new");
    let qs = quarantines(&fx.work.join("a"));
    assert_eq!(qs.len(), 1);
    assert!(
        qs[0].join("pkg/index.js").exists(),
        "the quarantined tree is intact"
    );
    let names = fx.journal_names(&id);
    assert!(names.contains(&"kept-in-quarantine"), "{names:?}");
}

#[test]
fn resume_reports_but_never_touches_an_unjournaled_quarantine() {
    // NEG-3
    let q = vec![("PD_TEST_PARK_AFTER", "quarantined:1:MARKER")];
    let (fx, _nm, id) = killed_run(&q, "remove");
    let stray = fx.work.join("a").join(".prunedisk-q-deadbeefdeadbeef-9");
    fs::create_dir(&stray).unwrap();
    fs::write(stray.join("x.txt"), "x").unwrap();
    let report = resume(&fx, &id, T0);
    assert_eq!(report.unjournaled, vec![stray.clone()]);
    assert!(stray.join("x.txt").exists());
    assert!(report.to_string().contains("no journal entry"));
}

#[test]
fn enotempty_mid_delete_keeps_the_item() {
    // NEG-7: a file appears in a directory after it was listed: never forced.
    let fx = Fx::new();
    let nm = fx.project("a");
    fs::create_dir_all(nm.join("pkg/sub")).unwrap();
    fs::write(nm.join("pkg/sub/f.txt"), "f").unwrap();
    let (plan, id, p) = fx.make_plan();
    let root = fx
        .work
        .join("a")
        .join(format!(".prunedisk-q-{id}-{}", idx_of(&p, &nm)));
    let late = root.join("pkg/sub/late.txt");
    hooks::set_purge(Some(Box::new(move |rel| {
        if rel == "pkg/sub" {
            fs::write(&late, "late").unwrap();
        }
    })));
    let report = apply_ok(&fx, &plan, &id);
    hooks::set_purge(None);
    assert_eq!(report.removed, 0, "{report}");
    assert_eq!(report.kept.len(), 1, "{report}");
    assert_eq!(
        fs::read_to_string(root.join("pkg/sub/late.txt")).unwrap(),
        "late"
    );
    let names = fx.journal_names(&id);
    assert!(
        names.contains(&"removing") && names.contains(&"kept-in-quarantine"),
        "{names:?}"
    );
    assert!(!names.contains(&"removed"));
    // The state is final: nothing for resume to do.
    let logs = fold(&read_entries(&Journal::path_of(&fx.state, &id)).unwrap());
    assert_eq!(logs[0].state, State::KeptInQuarantine);
}

// ------------------------------------------------------------------ journal

#[test]
fn a_torn_last_line_is_dropped_and_a_bad_middle_line_is_an_error() {
    let fx = Fx::new();
    let id = "0123456789abcdef";
    {
        let mut j = Journal::open(&fx.state, id, T0).unwrap();
        j.start("new", 1).unwrap();
        j.skip(0, "/x", SkipReason::Protected, "d").unwrap();
    }
    let path = Journal::path_of(&fx.state, id);
    let good = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{good}{{\"seq\":2,\"t\":0,\"rec\":\"remov")).unwrap();
    assert_eq!(
        read_entries(&path).unwrap().len(),
        2,
        "the torn line is dropped"
    );
    fs::write(&path, format!("garbage\n{good}")).unwrap();
    assert!(
        read_entries(&path).is_err(),
        "a bad line elsewhere is corrupt"
    );
    // The journal holds no file contents: its fields are the typed ones.
    assert!(good.contains("\"rec\":\"skip\""));
}

#[test]
fn fold_keeps_the_latest_state_of_each_item() {
    let fx = Fx::new();
    let id = "0123456789abcdef";
    let mut j = Journal::open(&fx.state, id, T0).unwrap();
    let parent = Dir::open_path(&fx.root).unwrap();
    j.intent_quarantine(3, "/a/b", "/a/.prunedisk-q-x-3", &parent, Some("r"))
        .unwrap();
    j.quarantined(3, FileId { dev: 1, ino: 2 }, "dig", 5, 10)
        .unwrap();
    j.intent_remove(3).unwrap();
    let logs = fold(&read_entries(j.path()).unwrap());
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].state, State::Removing);
    assert_eq!(logs[0].root, Some(FileId { dev: 1, ino: 2 }));
    j.removed(3, 10).unwrap();
    let logs = fold(&read_entries(j.path()).unwrap());
    assert!(logs[0].state.is_final());
    assert!(journal::unfinished(j.path()).unwrap().is_empty());
    let _ = Record::End {
        mode: "x".into(),
        removed: 0,
        bytes: 0,
        skipped: 0,
        restored: 0,
        kept: 0,
    };
}

// ------------------------------------------------------------------ the classifier

#[test]
fn classifier_agrees_with_the_scan() {
    // Plan time and removal time use the scan's own classifier: the same fixtures get the same
    // verdict from the plan (offered for removal or not) and from the check of the quarantined copy.
    type Setup = Box<dyn Fn(&Path)>;
    let variants: Vec<(&str, Setup)> = vec![
        ("clean", Box::new(|_| {})),
        (
            "state file",
            Box::new(|nm| fs::write(nm.join("pkg/.env"), "S=1").unwrap()),
        ),
        (
            "output file",
            Box::new(|nm| {
                fs::create_dir_all(nm.join("loose")).unwrap();
                fs::write(nm.join("loose/clip.mp4"), vec![1u8; 2048]).unwrap();
            }),
        ),
        (
            "nested repository",
            Box::new(|nm| fs::create_dir_all(nm.join("pkg/vendored/.git")).unwrap()),
        ),
        (
            "agent-named file outside an agent home",
            Box::new(|nm| fs::write(nm.join("pkg/log.jsonl"), "{}").unwrap()),
        ),
    ];
    let mut verdicts = Vec::new();
    for (what, setup) in variants {
        let fx = Fx::new();
        let nm = fx.project("a");
        setup(&nm);
        let sc = fx.scan_result(fx.ctx());
        let plan = fx.plan_of(&sc);
        let offered = plan
            .items
            .iter()
            .find(|i| i.path == nm.to_string_lossy())
            .is_some_and(|i| {
                i.action == Action::RemoveRegenerable && i.class == Class::Regenerable
            });
        let qname = ".prunedisk-q-contract-0";
        let qpath = fx.work.join("a").join(qname);
        fs::rename(&nm, &qpath).unwrap();
        let parent = Dir::open_path(&fx.work.join("a")).unwrap();
        let rules = crate::RuleSet::builtin_shared();
        let cls = classify::Classifier {
            rules,
            options: &plan.scan_options,
            context: &fx.ctx(),
        };
        let copy = cls.verify_copy("node_modules", &parent, qname, &qpath);
        assert_eq!(
            offered,
            copy.is_ok(),
            "{what}: plan says {offered}, copy check says {copy:?}"
        );
        verdicts.push(offered);
    }
    assert!(
        verdicts.iter().any(|v| *v) && verdicts.iter().any(|v| !*v),
        "{verdicts:?}"
    );
}

#[test]
fn a_broken_classifier_variant_would_be_caught() {
    // The contract above can fail: a check that ignores the marker accepts a copy whose parent has
    // lost package.json, which the real one refuses.
    let fx = Fx::new();
    let nm = fx.project("a");
    let sc = fx.scan_result(fx.ctx());
    let plan = fx.plan_of(&sc);
    let qname = ".prunedisk-q-contract-1";
    let qpath = fx.work.join("a").join(qname);
    fs::rename(&nm, &qpath).unwrap();
    fs::remove_file(fx.work.join("a/package.json")).unwrap();
    let parent = Dir::open_path(&fx.work.join("a")).unwrap();
    let cls = classify::Classifier {
        rules: crate::RuleSet::builtin_shared(),
        options: &plan.scan_options,
        context: &fx.ctx(),
    };
    let err = cls
        .verify_copy("node_modules", &parent, qname, &qpath)
        .unwrap_err();
    assert!(err.contains("marker"), "{err}");
}
