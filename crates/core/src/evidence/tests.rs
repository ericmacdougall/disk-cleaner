//! Tests of the per-item evidence rules (goodqa#16711 AC-1 to AC-8). Fixture homes in temp
//! directories only; every path below is synthetic.

use super::*;
use crate::plan::{plan, Action, Plan, PlanItem, PlanOptions};
use crate::procs::{read_proc_entry, Probe};
use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAY: i64 = 86_400;

/// A fixture home with an agent-harness root, `.claude` and `.codex`. Tests date files
/// explicitly; `age_all` dates everything far before `now` so nothing is recent by accident.
struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    now: i64,
}

fn real_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

impl Fx {
    fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(tmp.path()).unwrap().join("user");
        for d in [".agent-harness/work", ".claude", ".codex"] {
            fs::create_dir_all(home.join(d)).unwrap();
        }
        Fx {
            _tmp: tmp,
            home,
            now: real_now() + 400 * DAY,
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.home.clone()
        } else {
            self.home.join(rel)
        }
    }

    fn s(&self, rel: &str) -> String {
        self.p(rel).to_string_lossy().into_owned()
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.p(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// A clean checkout's shape (the scan helper patches its facts to all-yes).
    fn repo(&self, rel: &str) -> PathBuf {
        let p = self.dir(rel);
        fs::create_dir_all(p.join(".git/objects")).unwrap();
        fs::create_dir_all(p.join(".git/refs")).unwrap();
        fs::write(p.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        self.write(&format!("{rel}/src/lib.rs"), "fn x() {}\n");
        p
    }

    /// Date `rel` (a file or directory) `ago` seconds before `now`.
    fn date(&self, rel: &str, ago: i64) {
        set_mtime(&self.p(rel), self.now - ago);
    }

    /// Date everything below the home far in the past (an old, untouched host).
    fn age_all(&self) {
        let mut stack = vec![self.home.clone()];
        let mut dirs = vec![self.home.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let ft = e.file_type().unwrap();
                if ft.is_dir() {
                    stack.push(e.path());
                    dirs.push(e.path());
                } else if ft.is_file() {
                    set_mtime(&e.path(), self.now - 400 * DAY);
                }
            }
        }
        for d in dirs.iter().rev() {
            set_mtime(d, self.now - 400 * DAY);
        }
    }

    fn scan(&self) -> ScanResult {
        let mut so = crate::scan::ScanOptions::new(vec![self.home.clone()]);
        so.collect_facts = false;
        so.context = crate::protect::ScanContext {
            passwd_home: Some(self.home.clone()),
            euid: Some(1000),
            home_env: Some(self.home.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let mut scan = crate::scan::scan(&so).unwrap();
        for n in scan.nodes.iter_mut() {
            // Without git facts no regenerable directory inside a repository is verified to be
            // ignored; a real scan asks git. Patch that one reason away like the facts.
            if n.ask_reason
                .as_deref()
                .is_some_and(|r| r.contains("git facts were not collected"))
            {
                n.ask_reason = None;
            }
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
        scan
    }

    fn opts(&self, snap: ProcessSnapshot) -> PlanOptions {
        let mut o = PlanOptions::new(7, self.now);
        o.process = snap;
        o
    }

    fn plan(&self, snap: ProcessSnapshot) -> Plan {
        plan(&self.scan(), &self.opts(snap))
    }
}

fn set_mtime(p: &Path, t: i64) {
    let f = fs::File::open(p).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(t as u64))
        .unwrap();
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
        reflog_saved: Tri::Yes,
        no_pending_state: Tri::Yes,
        plain_layout: Tri::Yes,
        no_local_admin_content: Tri::Yes,
    }
}

fn item<'a>(p: &'a Plan, path: &Path) -> &'a PlanItem {
    let want = path.to_string_lossy();
    p.items.iter().find(|i| i.path == want).unwrap_or_else(|| {
        panic!(
            "no item {want}: {:?}",
            p.items.iter().map(|i| &i.path).collect::<Vec<_>>()
        )
    })
}

fn action(p: &Plan, path: &Path) -> Action {
    item(p, path).action
}

fn why(p: &Plan, path: &Path) -> String {
    let it = item(p, path);
    format!("{:?}: {} / {}", it.action, it.state_reason, it.reason)
}

fn session<'a>(p: &'a Plan, name: &str) -> &'a SessionTotal {
    p.totals
        .sessions
        .iter()
        .find(|s| s.session == name)
        .unwrap_or_else(|| panic!("no session {name}: {:?}", p.totals.sessions))
}

/// A readable process.
fn proc(pid: u32, cwd: &str, args: &[&str]) -> Proc {
    Proc {
        pid,
        names: vec![Path::new(args.first().copied().unwrap_or("proc"))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()],
        cwd: Some(cwd.to_string()),
        cmdline: Some(args.iter().map(|a| a.to_string()).collect()),
        open_files: Some(Vec::new()),
        ..Proc::default()
    }
}

fn snap(procs: Vec<Proc>) -> ProcessSnapshot {
    ProcessSnapshot { procs, error: None }
}

// ------------------------------------------------------------------------------------- AC-1

/// AC-1, NEG-1, NEG-6: a process in another session does not hold a checkout; its own session's
/// checkouts are kept; a process inside a checkout keeps it; a protected node stays kept.
#[test]
fn sibling_process_does_not_hold() {
    let fx = Fx::new();
    let wt = fx.repo(".agent-harness/work/s1/wt");
    let x = fx.repo(".agent-harness/work/s2/x");
    let y = fx.repo(".agent-harness/work/s2/y");
    let p_out = fx.repo("projects/p");
    // A checkout inside an agent config dir: protected by spec A.
    let held = fx.repo(".claude/plugins/marketplaces/m");
    fx.age_all();

    // Control: nothing running, every clean idle checkout is offered, the held one is kept.
    let p = fx.plan(ProcessSnapshot::none_running());
    for c in [&wt, &x, &y, &p_out] {
        assert_eq!(action(&p, c), Action::BackupThenRemove, "{}", why(&p, c));
    }

    // A process working in s2/x.
    let cwd = fx.s(".agent-harness/work/s2/x/src");
    let p = fx.plan(snap(vec![proc(100, &cwd, &["bash"])]));
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
    assert_eq!(action(&p, &x), Action::Keep, "{}", why(&p, &x));
    assert_eq!(item(&p, &x).evidence.rule, Some(1), "{}", why(&p, &x));
    assert_eq!(action(&p, &y), Action::Keep, "{}", why(&p, &y));
    assert_eq!(item(&p, &y).evidence.rule, Some(4), "{}", why(&p, &y));
    assert!(item(&p, &y).state_reason.contains("session s2 is live"));
    assert!(session(&p, "s2").live && !session(&p, "s1").live);
    assert!(session(&p, "s1").bytes_offered > 0);
    assert_eq!(session(&p, "s2").bytes_offered, 0);
    assert_eq!(action(&p, &p_out), Action::BackupThenRemove);

    // NEG-1: a process inside the checkout itself keeps it (outside the harness too, where no
    // session is involved).
    let cwd = fx.s("projects/p/src");
    let p = fx.plan(snap(vec![proc(101, &cwd, &["bash"])]));
    assert_eq!(action(&p, &p_out), Action::Keep, "{}", why(&p, &p_out));
    assert_eq!(item(&p, &p_out).evidence.rule, Some(1));
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
    let cwd = fx.s(".agent-harness/work/s1/wt");
    let p = fx.plan(snap(vec![proc(102, &cwd, &["bash"])]));
    assert_eq!(action(&p, &wt), Action::Keep, "{}", why(&p, &wt));
    assert_eq!(item(&p, &wt).evidence.rule, Some(1));

    // NEG-6: the protected checkout is never offered, whatever the evidence says: idle, and
    // with a process working inside it.
    let cwd = fx.s(".claude/plugins/marketplaces/m/src");
    for snapshot in [
        ProcessSnapshot::none_running(),
        snap(vec![proc(103, &cwd, &["bash"])]),
    ] {
        let p = fx.plan(snapshot);
        let h = item(&p, &held);
        assert_eq!(h.action, Action::Keep, "{}", why(&p, &held));
        assert_eq!(
            h.hold.as_ref().map(|h| h.code),
            Some(HoldCode::Protected),
            "{}",
            why(&p, &held)
        );
    }
}

// ------------------------------------------------------------------------------------- AC-2

/// AC-2, NEG-5: a working directory at the home, the harness root or `~/projects` holds nothing
/// below it except through a live session; an `--active` path still covers everything below.
#[test]
fn ancestor_cwd_does_not_hold() {
    let fx = Fx::new();
    // ~/projects is itself a repository holding nested checkouts (the "top-level folder").
    let top = fx.repo("projects");
    let a = fx.repo("projects/a");
    let wt = fx.repo(".agent-harness/work/s1/wt");
    let live_wt = fx.repo(".agent-harness/work/s3/wt");
    fx.dir(".agent-harness/work/s3/notes");
    fx.age_all();

    for cwd in [
        fx.s(""),
        fx.s(".agent-harness"),
        fx.s(".agent-harness/work"),
        fx.s("projects"),
        "/".to_string(),
    ] {
        let p = fx.plan(snap(vec![proc(200, &cwd, &["bash"])]));
        assert_eq!(
            action(&p, &a),
            Action::BackupThenRemove,
            "cwd {cwd}: {}",
            why(&p, &a)
        );
        assert_eq!(
            action(&p, &wt),
            Action::BackupThenRemove,
            "cwd {cwd}: {}",
            why(&p, &wt)
        );
        assert!(item(&p, &a).evidence.rule.is_none());
    }
    // A cwd at ~/projects is inside the ~/projects repository itself: that one is in use.
    let p = fx.plan(snap(vec![proc(201, &fx.s("projects"), &["bash"])]));
    assert_eq!(action(&p, &top), Action::Keep, "{}", why(&p, &top));

    // Except through a live session: a process working in s3 keeps s3's checkout, and an
    // ancestor cwd beside it still holds nothing of s1's.
    let p = fx.plan(snap(vec![
        proc(202, &fx.s(".agent-harness"), &["bash"]),
        proc(203, &fx.s(".agent-harness/work/s3/notes"), &["bash"]),
    ]));
    assert_eq!(action(&p, &live_wt), Action::Keep, "{}", why(&p, &live_wt));
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );

    // NEG-5: an --active path covers everything below it.
    let mut o = fx.opts(ProcessSnapshot::none_running());
    o.active = vec![fx.p("projects")];
    let p = plan(&fx.scan(), &o);
    for c in [&top, &a] {
        assert_eq!(action(&p, c), Action::Keep, "{}", why(&p, c));
    }
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
}

// ------------------------------------------------------------------------------------- AC-3

/// AC-3: the owning project's processes hold its regenerable directories; a sibling's do not; a
/// monorepo root holds its packages'; command-line paths (`--manifest-path`, relative scripts)
/// and open files count.
#[test]
fn owning_project() {
    let fx = Fx::new();
    for r in ["code/p", "code/wt", "code/x/foo", "code/v"] {
        fx.repo(r);
    }
    fx.write("code/p/package.json", "{}");
    fx.write("code/p/Cargo.toml", "[package]\n");
    fx.write("code/p/node_modules/dep/package.json", "{}");
    fx.write("code/p/target/debug/out.d", "x");
    fx.write("code/q/package.json", "{}");
    fx.repo("code/m");
    fx.write("code/m/packages/a/package.json", "{}");
    fx.write("code/m/packages/a/node_modules/dep/package.json", "{}");
    fx.write("code/wt/Cargo.toml", "[package]\n");
    fx.write("code/wt/src/main.rs", "fn main() {}\n");
    fx.write("code/wt/target/debug/out.d", "x");
    fx.write("code/x/foo/package.json", "{}");
    fx.write("code/x/foo/server.js", "x");
    fx.write("code/x/foo/node_modules/dep/package.json", "{}");
    fx.write("code/v/requirements.txt", "x\n");
    fx.write("code/v/.venv/pyvenv.cfg", "home = /usr/bin\n");
    fx.write("code/v/.venv/lib/site.py", "x\n");
    fx.age_all();
    let p_nm = fx.p("code/p/node_modules");
    let p_t = fx.p("code/p/target");
    let m_nm = fx.p("code/m/packages/a/node_modules");
    let wt_t = fx.p("code/wt/target");
    let x_nm = fx.p("code/x/foo/node_modules");
    let venv = fx.p("code/v/.venv");
    let all = [&p_nm, &p_t, &m_nm, &wt_t, &x_nm, &venv];

    // Control: nothing running, all removable; the scan recorded each one's project.
    let p = fx.plan(ProcessSnapshot::none_running());
    for d in all {
        assert_eq!(action(&p, d), Action::RemoveRegenerable, "{}", why(&p, d));
    }
    assert_eq!(item(&p, &p_nm).evidence.project, Some(fx.s("code/p")));

    let check = |procs: Vec<Proc>, held: &[&PathBuf], what: &str| {
        let p = fx.plan(snap(procs));
        for d in all {
            let want = if held.contains(&d) {
                Action::Keep
            } else {
                Action::RemoveRegenerable
            };
            assert_eq!(action(&p, d), want, "{what}: {}", why(&p, d));
        }
    };
    check(
        vec![proc(300, &fx.s("code/p"), &["npm", "run", "dev"])],
        &[&p_nm, &p_t],
        "cwd at the project root",
    );
    check(
        vec![proc(301, &fx.s("code/q"), &["npm", "run", "dev"])],
        &[],
        "cwd in a sibling",
    );
    check(
        vec![proc(302, &fx.s("code/m"), &["pnpm", "-r", "build"])],
        &[&m_nm],
        "cwd at the monorepo root",
    );
    let manifest = fx.s("code/wt/Cargo.toml");
    check(
        vec![proc(
            303,
            &fx.s(""),
            &["cargo", "test", "--manifest-path", &manifest],
        )],
        &[&wt_t],
        "--manifest-path from the home",
    );
    let flag = format!("--manifest-path={manifest}");
    check(
        vec![proc(304, &fx.s(""), &["cargo", "build", &flag])],
        &[&wt_t],
        "--flag=value",
    );
    let list = format!("/usr/lib:{}", fx.s("code/wt/src"));
    check(
        vec![proc(305, "/", &["tool", "--path", &list])],
        &[&wt_t],
        ":-separated list",
    );
    check(
        vec![proc(306, &fx.s("code/x"), &["node", "foo/server.js"])],
        &[&x_nm],
        "relative script from the parent",
    );
    let mut opener = proc(307, &fx.s(""), &["python3"]);
    opener.open_files = Some(vec![fx.s("code/v/.venv/lib/site.py")]);
    check(vec![opener], &[&venv], "an open file inside .venv");
}

// ------------------------------------------------------------------------------------- AC-4

/// AC-4: activity is the item's own (a regenerable directory: its owning project's), never an
/// enclosing top-level folder's.
#[test]
fn activity_per_item() {
    let fx = Fx::new();
    let top = fx.repo("top");
    let old = fx.repo("top/old");
    fx.write("top/app/package.json", "{}");
    fx.write("top/app/index.js", "x");
    fx.write("top/app/node_modules/dep/package.json", "{}");
    fx.write("top/notes.txt", "today");
    fx.age_all();
    // The checkout and the app untouched for 60 days; the top-level folder changed today.
    for rel in [
        "top/old/src/lib.rs",
        "top/app/package.json",
        "top/app/index.js",
        "top/app/node_modules/dep/package.json",
    ] {
        fx.date(rel, 60 * DAY);
    }
    fx.date("top/notes.txt", 3600);
    let nm = fx.p("top/app/node_modules");
    let p = fx.plan(ProcessSnapshot::none_running());
    assert_eq!(action(&p, &top), Action::Keep, "{}", why(&p, &top));
    assert_eq!(
        action(&p, &old),
        Action::BackupThenRemove,
        "{}",
        why(&p, &old)
    );
    assert_eq!(item(&p, &old).days_since_activity, Some(60));
    assert_eq!(
        action(&p, &nm),
        Action::RemoveRegenerable,
        "{}",
        why(&p, &nm)
    );
    assert_eq!(
        item(&p, &nm).days_since_activity,
        Some(60),
        "{}",
        why(&p, &nm)
    );
    assert_eq!(item(&p, &nm).evidence.project, Some(fx.s("top/app")));

    // Control: the app changed today, so its node_modules is active.
    fx.date("top/app/index.js", 3600);
    let p = fx.plan(ProcessSnapshot::none_running());
    assert_eq!(action(&p, &nm), Action::Keep, "{}", why(&p, &nm));
}

// ------------------------------------------------------------------------------------- AC-5

/// The fixture for one live-session case: sessions s1 (the checkout under test), abc and abcd
/// (prefix pair), and a checkout outside the harness.
struct Live {
    fx: Fx,
    wt: PathBuf,
    abcd: PathBuf,
    out: PathBuf,
}

fn live_fixture() -> Live {
    let fx = Fx::new();
    let wt = fx.repo(".agent-harness/work/s1/wt");
    fx.repo(".agent-harness/work/abc/wt");
    let abcd = fx.repo(".agent-harness/work/abcd/wt");
    let out = fx.repo("projects/p");
    fx.dir(".agent-harness/work/s1/task");
    fx.dir(".claude/projects/-proj");
    fx.dir(".claude/sessions");
    fx.dir(".codex/sessions/2026/01/01");
    fx.dir("projects/foo");
    fx.dir(".claude/spares/spare-1");
    // Untouched for 30 days (NEG-2), everything else far older.
    fx.age_all();
    fx.date(".agent-harness/work/s1/wt/src/lib.rs", 30 * DAY);
    Live { fx, wt, abcd, out }
}

fn agent(pid: u32, name: &str, cwd: &str) -> Proc {
    Proc {
        pid,
        names: vec![name.to_string()],
        cwd: Some(cwd.to_string()),
        cmdline: Some(vec![name.to_string()]),
        open_files: Some(Vec::new()),
        ..Proc::default()
    }
}

fn child_of(pid: u32, parent: u32) -> Proc {
    Proc {
        ppid: Some(parent),
        ..proc(pid, "/", &["mcp-server"])
    }
}

const CANARY: &str = "CANARY-7f3a9c-do-not-print";

fn kept(l: &Live, p: &Plan, what: &str) {
    let it = item(p, &l.wt);
    assert_eq!(it.action, Action::Keep, "{what}: {}", why(p, &l.wt));
    assert!(
        it.state_reason.contains("session s1 is live"),
        "{what}: {}",
        why(p, &l.wt)
    );
    assert_eq!(it.days_since_activity, Some(30), "NEG-2 {what}");
    assert!(session(p, "s1").live, "{what}");
    assert_eq!(action(p, &l.abcd), Action::BackupThenRemove, "{what}: abcd");
}

/// AC-5, NEG-2, NEG-7, NEG-8: each live-session source alone keeps the session's checkouts;
/// agents with no locus fail closed unless idle; transcripts are read for `cwd` only.
#[test]
fn live_session() {
    // Baseline: with none of the sources the 30-day-old checkout is offered.
    let l = live_fixture();
    let p = l.fx.plan(ProcessSnapshot::none_running());
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &l.wt)
    );
    assert!(!session(&p, "s1").live);

    // A process's working directory inside the session directory.
    let l = live_fixture();
    let cwd = l.fx.s(".agent-harness/work/s1/task");
    kept(
        &l,
        &l.fx.plan(snap(vec![proc(500, &cwd, &["bash"])])),
        "cwd",
    );

    // The session id on an agent's command line.
    let l = live_fixture();
    let mut a = agent(501, "claude", &l.fx.s("projects/foo"));
    a.cmdline = Some(vec!["claude".into(), "--resume".into(), "s1".into()]);
    kept(
        &l,
        &l.fx.plan(snap(vec![a])),
        "session id on a command line",
    );
    // ...also as a `--flag=value` and inside a `:` list, where no such path exists on disk.
    for arg in ["--resume=s1", "--sessions=zz:s1"] {
        let l = live_fixture();
        let mut a = agent(501, "claude", &l.fx.s("projects/foo"));
        a.cmdline = Some(vec!["claude".into(), arg.into()]);
        kept(&l, &l.fx.plan(snap(vec![a])), arg);
    }

    // Recent files below the session directory.
    let l = live_fixture();
    l.fx.write(".agent-harness/work/s1/notes.md", "x");
    l.fx.date(".agent-harness/work/s1/notes.md", 3600);
    l.fx.date(".agent-harness/work/s1", 400 * DAY);
    kept(
        &l,
        &l.fx.plan(ProcessSnapshot::none_running()),
        "recent session files",
    );

    // A recent same-named directory in a protected root.
    let l = live_fixture();
    l.fx.dir(".claude/wals/s1");
    l.fx.date(".claude/wals/s1", 3600);
    l.fx.date(".claude/wals", 400 * DAY);
    kept(
        &l,
        &l.fx.plan(ProcessSnapshot::none_running()),
        "same-named entry",
    );

    // A live Claude Code registry entry whose cwd is in the session (the process itself is
    // not an agent and works at /, so only the registry says so). Beside it, a `.key` FIFO
    // that would block if opened (it must never be).
    let l = live_fixture();
    let cwd = l.fx.s(".agent-harness/work/s1/wt");
    l.fx.write(
        ".claude/sessions/4242.json",
        &format!(r#"{{"pid":4242,"sessionId":"x-1","cwd":"{cwd}","name":"{CANARY}"}}"#),
    );
    l.fx.date(".claude/sessions/4242.json", DAY);
    l.fx.date(".claude/sessions", 400 * DAY);
    let mut reg = proc(4242, "/", &["other"]);
    reg.start_time = Some(l.fx.now - 2 * DAY);
    let scan = l.fx.scan();
    let opts = l.fx.opts(snap(vec![reg.clone()]));
    #[cfg(unix)]
    let p = {
        let key = l.fx.p(".claude/sessions/4242.key");
        crate::test_fifo::mkfifo(&key);
        let p = crate::test_fifo::must_not_block(&key, move || plan(&scan, &opts));
        fs::remove_file(&key).unwrap();
        p
    };
    #[cfg(not(unix))]
    let p = plan(&scan, &opts);
    kept(&l, &p, "registry");
    assert!(!serde_json::to_string(&p).unwrap().contains(CANARY));
    // NEG-8: the same entry with its pid dead, or reused by a process started after the entry
    // was written, makes nothing live.
    let p = l.fx.plan(snap(vec![proc(4343, "/", &["other"])]));
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "dead pid: {}",
        why(&p, &l.wt)
    );
    let mut reused = reg.clone();
    reused.start_time = Some(l.fx.now - 3600);
    let p = l.fx.plan(snap(vec![reused]));
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "reused pid: {}",
        why(&p, &l.wt)
    );

    // An agent with an open transcript whose recorded cwd is in the session; the transcript is
    // old (so only the open file says so) and holds a canary beside `cwd`.
    let l = live_fixture();
    let cwd = l.fx.s(".agent-harness/work/s1");
    let t = l.fx.write(
        ".codex/sessions/2026/01/01/rollout-1.jsonl",
        &format!("{{\"cwd\":\"{cwd}\",\"note\":\"{CANARY}\"}}\n{{\"text\":\"{CANARY}\"}}\n"),
    );
    l.fx.date(".codex/sessions/2026/01/01/rollout-1.jsonl", 30 * DAY);
    l.fx.date(".codex/sessions/2026/01/01", 400 * DAY);
    let mut a = agent(502, "codex", &l.fx.s("projects/foo"));
    a.open_files = Some(vec![t.to_string_lossy().into_owned()]);
    let p = l.fx.plan(snap(vec![a]));
    kept(&l, &p, "open transcript");
    let json = serde_json::to_string(&p).unwrap();
    assert!(
        json.contains(&cwd),
        "control: the plan names the session dir"
    );
    assert!(
        !json.contains(CANARY),
        "the canary leaked into the plan JSON"
    );

    // An agent working in s1/task with a child: s1 is live and nothing fails closed.
    let l = live_fixture();
    let a = agent(503, "claude", &l.fx.s(".agent-harness/work/s1/task"));
    let p = l.fx.plan(snap(vec![a, child_of(504, 503)]));
    kept(&l, &p, "agent in a work dir with a child");
    assert!(p.process_counts.unmapped_agents.is_empty());

    // A transcript modified an hour ago records its cwd in s1; no process runs at all.
    let l = live_fixture();
    let cwd = l.fx.s(".agent-harness/work/s1/wt");
    l.fx.write(
        ".claude/projects/-proj/abc.jsonl",
        &format!("{{\"type\":\"x\",\"cwd\":\"{cwd}\",\"secret\":\"{CANARY}\"}}\n"),
    );
    l.fx.date(".claude/projects/-proj/abc.jsonl", 3600);
    l.fx.date(".claude/projects/-proj", 400 * DAY);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    kept(&l, &p, "recent transcript");
    assert!(!serde_json::to_string(&p).unwrap().contains(CANARY));

    // A spare session working under the Claude config dir, with MCP children: blocks nothing.
    let l = live_fixture();
    let a = agent(505, "claude", &l.fx.s(".claude/spares/spare-1"));
    let p =
        l.fx.plan(snap(vec![a, child_of(506, 505), child_of(507, 505)]));
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "spare: {}",
        why(&p, &l.wt)
    );
    assert!(p.process_counts.unmapped_agents.is_empty());

    // An agent whose only locus is the home, with a child: every removing item under a work
    // dir asks, naming the pid; the checkout outside the harness is unaffected.
    let l = live_fixture();
    let a = agent(508, "claude", &l.fx.s(""));
    let p = l.fx.plan(snap(vec![a.clone(), child_of(509, 508)]));
    let it = item(&p, &l.wt);
    assert_eq!(it.action, Action::Ask, "home locus: {}", why(&p, &l.wt));
    assert!(
        it.reason.contains("508") && it.reason.contains("fail closed"),
        "{}",
        it.reason
    );
    assert_eq!(it.ask_code.as_deref(), Some("agent-check"));
    assert_eq!(
        action(&p, &l.out),
        Action::BackupThenRemove,
        "{}",
        why(&p, &l.out)
    );
    assert_eq!(p.process_counts.unmapped_agents, vec![508]);
    // The same agent idle (no child, no open transcript or file): holds nothing.
    let p = l.fx.plan(snap(vec![a]));
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "idle: {}",
        why(&p, &l.wt)
    );
    assert_eq!(p.process_counts.idle_agents, 1);
    assert!(p.process_counts.unmapped_agents.is_empty());

    // A live agent whose only locus is ~/projects/foo does not block an idle session.
    let l = live_fixture();
    let a = agent(510, "claude", &l.fx.s("projects/foo"));
    let p = l.fx.plan(snap(vec![a, child_of(511, 510)]));
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "elsewhere: {}",
        why(&p, &l.wt)
    );

    // NEG-7: evidence for `abc` does not make `abcd` live (whole path segments only).
    let l = live_fixture();
    let cwd = l.fx.s(".agent-harness/work/abc");
    let mut a = agent(512, "claude", &l.fx.s("projects/foo"));
    a.cmdline = Some(vec!["claude".into(), l.fx.s("projects/foo/abc/log")]);
    let p = l.fx.plan(snap(vec![proc(513, &cwd, &["bash"]), a]));
    assert!(session(&p, "abc").live);
    assert!(!session(&p, "abcd").live, "{:?}", session(&p, "abcd"));
    assert_eq!(
        action(&p, &l.abcd),
        Action::BackupThenRemove,
        "{}",
        why(&p, &l.abcd)
    );
}

/// A Codex rollout: its first line is a `session_meta` record whose `payload.cwd` is the
/// session's working directory (no top-level `cwd`). Every value is synthetic.
fn codex_rollout(cwd: &str, secret: &str) -> String {
    format!(
        "{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"session_meta\",\
         \"payload\":{{\"id\":\"0000\",\"cwd\":\"{cwd}\",\"instructions\":\"{secret}\"}}}}\n\
         {{\"timestamp\":\"2026-01-01T00:00:01Z\",\"type\":\"response_item\",\
         \"payload\":{{\"type\":\"message\",\"text\":\"{secret}\"}}}}\n"
    )
}

/// goodqa#16711 repair: a recently modified Codex rollout whose `session_meta` line records
/// `payload.cwd` in the session makes it live, with no process running, in `sessions/` and in
/// `archived_sessions/`; an agent holding an old rollout open gets that cwd as its locus. The
/// canary beside it reaches no output. (Run against 53b9bc0 too, where it fails: the
/// top-level-only read mapped nothing.)
#[test]
fn codex_session_meta_transcript() {
    for dir in [".codex/sessions/2026/01/01", ".codex/archived_sessions"] {
        let l = live_fixture();
        let rel = format!("{dir}/rollout-2026-01-01T00-00-00-0000.jsonl");
        let cwd = l.fx.s(".agent-harness/work/s1/wt");
        l.fx.write(&rel, &codex_rollout(&cwd, CANARY));
        l.fx.date(&rel, 3600);
        let p = l.fx.plan(ProcessSnapshot::none_running());
        kept(&l, &p, dir);
        assert!(!serde_json::to_string(&p).unwrap().contains(CANARY));
    }
    let l = live_fixture();
    let rel = ".codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-0001.jsonl";
    let t = l.fx.write(
        rel,
        &codex_rollout(&l.fx.s(".agent-harness/work/s1"), CANARY),
    );
    l.fx.date(rel, 30 * DAY);
    let mut a = agent(520, "codex", &l.fx.s(""));
    a.open_files = Some(vec![t.to_string_lossy().into_owned()]);
    let p = l.fx.plan(snap(vec![a, child_of(521, 520)]));
    kept(&l, &p, "open Codex rollout");
    assert!(!serde_json::to_string(&p).unwrap().contains(CANARY));
}

/// A Claude Code subagent transcript (`projects/<p>/<id>/subagents/<agent>.jsonl`) is read like
/// any other transcript.
#[test]
fn subagent_transcript_layout() {
    let l = live_fixture();
    let rel = ".claude/projects/-proj/0000-aaaa/subagents/agent-1.jsonl";
    let cwd = l.fx.s(".agent-harness/work/s1/task");
    l.fx.write(rel, &format!("{{\"type\":\"user\",\"cwd\":\"{cwd}\"}}\n"));
    l.fx.date(rel, 3600);
    kept(
        &l,
        &l.fx.plan(ProcessSnapshot::none_running()),
        "subagent transcript",
    );
}

/// The five cases of the transcript-cwd contract as files in `dir`, each with its expected
/// cwd (`None`: a coverage gap). `with_mapped` false writes only the three gap shapes.
fn contract_fixtures(dir: &Path, with_mapped: bool) -> Vec<(PathBuf, Option<String>)> {
    let cases: Vec<(&str, String, Option<&str>)> = vec![
        (
            "claude.jsonl",
            "{\"type\":\"summary\",\"summary\":\"x\"}\n\
             {\"type\":\"user\",\"cwd\":\"/w/claude\",\"sessionId\":\"0000\"}\n"
                .to_string(),
            Some("/w/claude"),
        ),
        (
            "codex-meta.jsonl",
            codex_rollout("/w/codex", "x"),
            Some("/w/codex"),
        ),
        (
            "codex-other.jsonl",
            "{\"timestamp\":\"t\",\"type\":\"response_item\",\"payload\":{\"cwd\":\"/w/not-meta\"}}\n"
                .to_string(),
            None,
        ),
        (
            "oversized.jsonl",
            format!(
                "{{\"cwd\":\"/w/too-late\",\"pad\":\"{}\"}}",
                "x".repeat(TRANSCRIPT_HEAD_BYTES + 10)
            ),
            None,
        ),
        (
            "garbage.jsonl",
            "not json at all\n\u{0}\u{1}\n".to_string(),
            None,
        ),
    ];
    fs::create_dir_all(dir).unwrap();
    cases
        .into_iter()
        .filter(|(_, _, want)| with_mapped || want.is_none())
        .map(|(name, text, want)| {
            let p = dir.join(name);
            fs::write(&p, text).unwrap();
            (p, want.map(str::to_string))
        })
        .collect()
}

/// The transcript-cwd contract (goodqa#16711 repair, wal:w1f09cb4cf9a): one extractor; the cwd
/// field only (top-level for Claude, `payload.cwd` of the `session_meta` line for Codex); any
/// other shape is a coverage gap. The tool's extractor and the independent oracle
/// (`scripts/check-live-sessions.py --transcript-cwd`) agree on every fixture.
#[test]
fn transcript_cwd_contract() {
    let fx = Fx::new();
    let cases = contract_fixtures(&fx.p("contract"), true);
    let rust: Vec<Option<String>> = cases
        .iter()
        .map(|(p, want)| {
            let got = transcript_cwd(p);
            match want {
                Some(w) => assert_eq!(got.as_deref(), Ok(w.as_str()), "{}", p.display()),
                None => assert_eq!(got, Err(TRANSCRIPT_CWD_UNKNOWN), "{}", p.display()),
            }
            got.ok()
        })
        .collect();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-live-sessions.py");
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(&script)
        .arg("--transcript-cwd")
        .args(cases.iter().map(|(p, _)| p))
        .output()
        .expect("python3 runs the oracle");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let oracle: Vec<Option<String>> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let (_, v) = l.split_once('\t').expect("path<TAB>result");
            (v != "gap").then(|| v.to_string())
        })
        .collect();
    assert_eq!(oracle, rust, "the oracle and the tool disagree");

    // Through the plan: recent transcripts in the three gap shapes are counted as coverage
    // gaps by class, never mapped and never a crash; so is a registry file without a pid.
    let l = live_fixture();
    for (p, _) in contract_fixtures(&l.fx.p(".claude/projects/-proj"), false) {
        set_mtime(&p, l.fx.now - 3600);
    }
    l.fx.write(".claude/sessions/77.json", "{\"sessionId\":\"x\"}");
    let p = l.fx.plan(ProcessSnapshot::none_running());
    assert_eq!(
        p.coverage_gaps.agent_data.get(TRANSCRIPT_CWD_UNKNOWN),
        Some(&3),
        "{:?}",
        p.coverage_gaps
    );
    assert_eq!(
        p.coverage_gaps.agent_data.get("registry-entry-unknown"),
        Some(&1),
        "{:?}",
        p.coverage_gaps
    );
    assert!(p
        .evidence_notes
        .iter()
        .any(|n| n.starts_with("transcript-cwd-unknown: ") && n.ends_with("oversized.jsonl")));
    // They are recent, so the work-dir checkout asks (see recent_agent_data_gap_fails_closed).
    assert_eq!(action(&p, &l.wt), Action::Ask, "{}", why(&p, &l.wt));
}

/// goodqa#16711 recent-gap rule (wal:w4178d932654): agent data modified within the active
/// window that could not be read for a working directory makes every removing item under the
/// harness work dirs and the agent worktree dirs ask ("agent data unreadable"); the same gap
/// just outside the window is only counted; items outside the agent dirs are unaffected.
#[test]
fn recent_agent_data_gap_fails_closed() {
    let setup = |ago: i64| {
        let l = live_fixture();
        let wt2 = l.fx.repo(".claude/worktrees/w1");
        l.fx.age_all();
        l.fx.date(".agent-harness/work/s1/wt/src/lib.rs", 30 * DAY);
        let rel = ".claude/projects/-proj/garbage.jsonl";
        l.fx.write(rel, "not json\n");
        l.fx.date(rel, ago);
        (l, wt2)
    };
    // Control: no gap at all, both checkouts offered.
    let l = live_fixture();
    let wt2 = l.fx.repo(".claude/worktrees/w1");
    l.fx.age_all();
    let p = l.fx.plan(ProcessSnapshot::none_running());
    assert_eq!(
        action(&p, &wt2),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt2)
    );
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &l.wt)
    );

    // Just inside the window: every removing item under an agent dir asks.
    let (l, wt2) = setup(7 * DAY - 60);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    for c in [&l.wt, &wt2] {
        let it = item(&p, c);
        assert_eq!(it.action, Action::Ask, "{}", why(&p, c));
        assert!(
            it.reason.contains("agent data unreadable: 1 entry"),
            "{}",
            it.reason
        );
        assert_eq!(it.ask_code.as_deref(), Some("agent-data"));
    }
    assert_eq!(
        action(&p, &l.out),
        Action::BackupThenRemove,
        "{}",
        why(&p, &l.out)
    );
    assert_eq!(
        p.coverage_gaps
            .agent_data_recent
            .get(TRANSCRIPT_CWD_UNKNOWN),
        Some(&1)
    );

    // Just outside the window: the transcript is not recent and is not read; the normal
    // actions stand.
    let (l, wt2) = setup(7 * DAY + 60);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    for c in [&l.wt, &wt2] {
        assert_eq!(action(&p, c), Action::BackupThenRemove, "{}", why(&p, c));
    }
    assert!(p.coverage_gaps.agent_data_recent.is_empty());

    // An old unreadable transcript an agent holds open is read: counted, not recent.
    let (l, _) = setup(30 * DAY);
    let mut a = agent(530, "claude", &l.fx.s(".agent-harness/work/s1/task"));
    a.open_files = Some(vec![l.fx.s(".claude/projects/-proj/garbage.jsonl")]);
    let p = l.fx.plan(snap(vec![a]));
    assert_eq!(
        p.coverage_gaps.agent_data.get(TRANSCRIPT_CWD_UNKNOWN),
        Some(&1),
        "{:?}",
        p.coverage_gaps
    );
    assert!(p.coverage_gaps.agent_data_recent.is_empty());

    // A recent registry file without a pid is a recent gap too.
    let l = live_fixture();
    l.fx.write(".claude/sessions/88.json", "{\"cwd\":\"/x\"}");
    l.fx.date(".claude/sessions/88.json", 3600);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    assert_eq!(action(&p, &l.wt), Action::Ask, "{}", why(&p, &l.wt));
}

/// Past the transcript read limit the rest cannot be judged: counted as recent, so removals
/// under the agent dirs ask (the limit is 8 in tests).
#[test]
fn transcript_limit_fails_closed() {
    let l = live_fixture();
    for i in 0..MAX_TRANSCRIPTS + 1 {
        let rel = format!(".claude/projects/-proj/t{i}.jsonl");
        l.fx.write(&rel, "{\"cwd\":\"/elsewhere\"}\n");
        l.fx.date(&rel, 3600);
    }
    let p = l.fx.plan(ProcessSnapshot::none_running());
    assert_eq!(
        p.coverage_gaps
            .agent_data_recent
            .get("transcripts-not-read"),
        Some(&1),
        "{:?}",
        p.coverage_gaps
    );
    assert_eq!(action(&p, &l.wt), Action::Ask, "{}", why(&p, &l.wt));
    assert_eq!(
        action(&p, &l.out),
        Action::BackupThenRemove,
        "{}",
        why(&p, &l.out)
    );
}

/// The bounded transcript read (AC-5): only `cwd` is kept, nothing past 20 lines or 64 KiB is
/// read, and a file with no usable head is a gap class, never its content.
#[test]
fn transcript_cwd_is_bounded() {
    let fx = Fx::new();
    let t = fx.write(
        "t/a.jsonl",
        &format!("{{\"x\":1}}\n{{\"cwd\":\"/w/s\",\"k\":\"{CANARY}\"}}\n"),
    );
    assert_eq!(transcript_cwd(&t).as_deref(), Ok("/w/s"));
    let mut late = String::new();
    for _ in 0..TRANSCRIPT_HEAD_LINES {
        late.push_str("{\"x\":1}\n");
    }
    late.push_str("{\"cwd\":\"/w/late\"}\n");
    let t = fx.write("t/b.jsonl", &late);
    assert_eq!(
        transcript_cwd(&t),
        Err(TRANSCRIPT_CWD_UNKNOWN),
        "line 21 is not read"
    );
    let big = format!(
        "{{\"pad\":\"{}\"}}\n{{\"cwd\":\"/w/far\"}}\n",
        "x".repeat(TRANSCRIPT_HEAD_BYTES)
    );
    let t = fx.write("t/c.jsonl", &big);
    assert_eq!(
        transcript_cwd(&t),
        Err(TRANSCRIPT_CWD_UNKNOWN),
        "nothing past 64 KiB is read"
    );
    assert_eq!(
        transcript_cwd(&fx.p("t/missing.jsonl")),
        Err(TRANSCRIPT_UNREADABLE)
    );
}

// ------------------------------------------------------------------------------------- AC-6

/// AC-6 on the real `/proc` (Linux): a zombie child, an exited (reaped) child and a kernel
/// thread found by its PF_KTHREAD flag are classified; this process is live and readable.
#[cfg(target_os = "linux")]
#[test]
fn real_proc_classifier() {
    let probe = |pid: u32| -> Probe {
        let out = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output();
        match out {
            Ok(o) if o.status.success() => Probe::Exists,
            Ok(o) if String::from_utf8_lossy(&o.stderr).contains("No such process") => Probe::Gone,
            Ok(_) => Probe::Exists,
            Err(_) => Probe::Unknown,
        }
    };
    let root = Path::new("/proc");
    let boot = crate::procs::boot_time(root);
    // This process.
    let me = read_proc_entry(root, std::process::id(), boot, &probe);
    assert_eq!(me.kind, ProcKind::Live);
    assert!(me.cwd.is_some() && me.cmdline.is_some() && me.open_files.is_some());
    assert!(me.start_time.is_some());
    // A zombie: a child that exited and was not yet reaped.
    let mut z = std::process::Command::new("true").spawn().unwrap();
    let zpid = z.id();
    let mut state = ' ';
    for _ in 0..500 {
        let st = fs::read_to_string(format!("/proc/{zpid}/stat")).unwrap_or_default();
        state = crate::procs::parse_stat(&st).map(|s| s.0).unwrap_or(' ');
        if state == 'Z' {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(state, 'Z', "the child did not become a zombie");
    assert_eq!(
        read_proc_entry(root, zpid, boot, &probe).kind,
        ProcKind::Gone
    );
    z.wait().unwrap();
    // The same pid after it was reaped: its /proc entry is gone and the re-probe says ESRCH.
    let gone = read_proc_entry(root, zpid, boot, &probe);
    assert_eq!(gone.kind, ProcKind::Gone, "{gone:?}");
    // A kernel thread, found by its flag.
    let kthread = fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .find(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| crate::procs::parse_stat(&s))
                .is_some_and(|s| s.2 & crate::procs::PF_KTHREAD != 0)
        });
    match kthread {
        Some(pid) => assert_eq!(
            read_proc_entry(root, pid, boot, &probe).kind,
            ProcKind::KernelThread
        ),
        None => eprintln!("no kernel thread is visible in this /proc (a container): skipped"),
    }
}

/// AC-6, NEG-3 on a snapshot fixture and a fake `/proc`.
#[test]
fn uninspectable_processes() {
    let fx = Fx::new();
    let wt = fx.repo("code/wt");
    let other = fx.repo("code/other");
    fx.age_all();
    let file = fx.s("code/wt/data/db.sqlite");
    let hidden = |pid: u32, uid: u32, args: Option<Vec<String>>| Proc {
        pid,
        uid: Some(uid),
        names: vec!["x".into()],
        cmdline: args,
        ..Proc::default()
    };
    let gone = |pid: u32| Proc {
        pid,
        kind: ProcKind::Gone,
        ..Proc::default()
    };
    let kthread = Proc {
        pid: 602,
        kind: ProcKind::KernelThread,
        ..Proc::default()
    };
    // Gone processes and a kernel thread are ignored: nothing asks.
    let p = fx.plan(snap(vec![gone(600), gone(601), kthread]));
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
    assert_eq!(p.process_counts.gone, 2);
    assert_eq!(p.process_counts.kernel_threads, 1);
    assert_eq!(p.process_counts.fully_unknown, 0);
    assert_eq!(p.process_check, "per-item");
    // Another user's process (cwd unreadable) naming a path inside the item holds it.
    let p = fx.plan(snap(vec![hidden(
        603,
        0,
        Some(vec!["/usr/bin/sqlite3".into(), file.clone()]),
    )]));
    assert_eq!(action(&p, &wt), Action::Keep, "{}", why(&p, &wt));
    assert_eq!(item(&p, &wt).evidence.rule, Some(2));
    assert_eq!(action(&p, &other), Action::BackupThenRemove);
    assert_eq!(p.process_counts.by_command_line, 1);
    // One naming nothing there leaves it offered (argv-rewriting daemons name nothing).
    let p = fx.plan(snap(vec![hidden(
        604,
        0,
        Some(vec!["postgres: checkpointer".into()]),
    )]));
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
    // A same-uid process with an unreadable cwd (not dumpable) is judged by its command line.
    let p = fx.plan(snap(vec![hidden(
        605,
        1000,
        Some(vec![
            "ssh-agent".into(),
            "-a".into(),
            fx.s("code/wt/agent.sock"),
        ]),
    )]));
    assert_eq!(action(&p, &wt), Action::Keep, "{}", why(&p, &wt));
    // NEG-3: cwd and command line both hidden (hidepid): every removing item asks...
    let p = fx.plan(snap(vec![hidden(606, 0, None)]));
    for c in [&wt, &other] {
        let it = item(&p, c);
        assert_eq!(it.action, Action::Ask, "{}", why(&p, c));
        assert_eq!(it.ask_code.as_deref(), Some("process-check"));
    }
    assert_eq!(p.process_counts.fully_unknown, 1);
    // ...unless the user accepts that risk, the flag's one remaining meaning.
    let mut o = fx.opts(snap(vec![hidden(606, 0, None)]));
    o.trust_partial_process_check = true;
    let p = plan(&fx.scan(), &o);
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
    // Process information unreadable at all: everything asks.
    let p = fx.plan(ProcessSnapshot::unknown("denied"));
    assert_eq!(action(&p, &wt), Action::Ask);
    assert_eq!(p.process_check, "unknown");

    // A fake /proc: a zombie, a kernel thread, a readable process, a pid that vanished
    // mid-read (no entry, ESRCH on the re-probe) and one with no entry that the re-probe says
    // exists (EPERM: it is there, so it is fully unknown).
    let fake = fx.dir("fakeproc");
    let stat = |pid: u32, state: char, flags: u64| {
        let d = fake.join(pid.to_string());
        fs::create_dir_all(&d).unwrap();
        let mut fields = vec!["0".to_string(); 22];
        fields[0] = state.to_string();
        fields[1] = "1".into();
        fields[6] = flags.to_string();
        fs::write(
            d.join("stat"),
            format!("{pid} (a (b) c) {}\n", fields.join(" ")),
        )
        .unwrap();
    };
    stat(700, 'Z', 0);
    stat(701, 'S', crate::procs::PF_KTHREAD);
    stat(702, 'S', 0);
    fs::write(fake.join("702/cmdline"), b"/bin/app\0--x\0").unwrap();
    let probe = |pid: u32| {
        if pid == 703 {
            Probe::Gone
        } else {
            Probe::Exists
        }
    };
    assert_eq!(
        read_proc_entry(&fake, 700, None, &probe).kind,
        ProcKind::Gone
    );
    assert_eq!(
        read_proc_entry(&fake, 701, None, &probe).kind,
        ProcKind::KernelThread
    );
    let live = read_proc_entry(&fake, 702, None, &probe);
    assert_eq!(live.kind, ProcKind::Live);
    assert_eq!(live.cmdline, Some(vec!["/bin/app".into(), "--x".into()]));
    assert_eq!(
        read_proc_entry(&fake, 703, None, &probe).kind,
        ProcKind::Gone
    );
    let eperm = read_proc_entry(&fake, 704, None, &probe);
    assert!(eperm.fully_unknown(), "{eperm:?}");
}

// ------------------------------------------------------------------------------------- AC-7

/// AC-7: a running container's bind source holds what it contains or lies in; an unqueryable
/// runtime, or a shim with no returned container, makes every removing item ask.
#[test]
fn containers() {
    let fx = Fx::new();
    let wt = fx.repo("code/wt");
    let other = fx.repo("code/other");
    fx.age_all();
    let id = "4f2a".repeat(16);
    let shim = Proc {
        pid: 800,
        uid: Some(0),
        names: vec!["containerd-shim-runc-v2".into()],
        cmdline: Some(vec![
            "/usr/bin/containerd-shim-runc-v2".into(),
            "-namespace".into(),
            "moby".into(),
            "-id".into(),
            id.clone(),
        ]),
        ..Proc::default()
    };
    let run = |binds: Vec<String>, state: Option<ContainerState>| {
        let mut o = fx.opts(snap(vec![shim.clone()]));
        o.containers = state.unwrap_or(ContainerState::Queried {
            containers: vec![Container {
                id: id.clone(),
                binds,
            }],
        });
        plan(&fx.scan(), &o)
    };
    // A bind inside the item, and a bind containing it.
    let p = run(vec![fx.s("code/wt/data")], None);
    assert_eq!(action(&p, &wt), Action::Keep, "{}", why(&p, &wt));
    assert_eq!(item(&p, &wt).evidence.rule, Some(3));
    assert_eq!(
        action(&p, &other),
        Action::BackupThenRemove,
        "{}",
        why(&p, &other)
    );
    let p = run(vec![fx.s("code")], None);
    assert_eq!(action(&p, &wt), Action::Keep, "{}", why(&p, &wt));
    assert_eq!(action(&p, &other), Action::Keep);
    // A bind elsewhere holds nothing.
    let p = run(vec!["/srv/data".into()], None);
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
    assert_eq!(p.process_counts.container_processes, 1);
    // The runtime could not be queried.
    let p = run(
        vec![],
        Some(ContainerState::Unqueryable {
            reason: "permission denied on the socket".into(),
        }),
    );
    assert_eq!(action(&p, &wt), Action::Ask, "{}", why(&p, &wt));
    assert_eq!(item(&p, &wt).ask_code.as_deref(), Some("containers"));
    // Not queried although a runtime child runs.
    let p = run(vec![], Some(ContainerState::NotQueried));
    assert_eq!(action(&p, &wt), Action::Ask, "{}", why(&p, &wt));
    // A shim whose container the runtime did not return.
    let p = run(
        vec![],
        Some(ContainerState::Queried {
            containers: vec![Container {
                id: "9".repeat(64),
                binds: vec![],
            }],
        }),
    );
    assert_eq!(action(&p, &wt), Action::Ask, "{}", why(&p, &wt));
    // No runtime process at all: nothing asks.
    let mut o = fx.opts(ProcessSnapshot::none_running());
    o.containers = ContainerState::NotQueried;
    let p = plan(&fx.scan(), &o);
    assert_eq!(
        action(&p, &wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &wt)
    );
    // `inspect` output.
    let json = format!(
        r#"[{{"Id":"{id}","Mounts":[{{"Type":"bind","Source":"/a/b"}},{{"Type":"volume","Source":"/var/lib/v"}}]}}]"#
    );
    let got = parse_inspect(&json).unwrap();
    assert_eq!(got[0].id, id);
    assert!(got[0].binds.contains(&"/a/b".to_string()));
    assert!(parse_inspect("not json").is_err());
    assert_eq!(
        shim_container_id(&["conmon".into(), "-c".into(), "abc".into()]).as_deref(),
        Some("abc")
    );
}

// ------------------------------------------------------------------------------------- AC-8

/// AC-8, NEG-4: a gap inside an item makes it ask and vetoes its parent; a gap elsewhere leaves
/// a clean pushed repository offered with the count in its reason; an overflowed record
/// restores the old every-repository ask.
#[test]
fn coverage_per_item() {
    let fx = Fx::new();
    let proj = fx.repo("code/proj");
    fx.write("code/proj/package.json", "{}");
    fx.write("code/proj/node_modules/dep/package.json", "{}");
    let r = fx.repo("code/r");
    let m = fx.repo("code/m");
    fx.age_all();
    let nm = fx.p("code/proj/node_modules");
    let gap = |path: String, class: GapClass| CoverageGap {
        path,
        class,
        owner_uid: Some(0),
    };
    let with_gaps = |gaps: Vec<CoverageGap>, overflowed: bool| {
        let mut s = fx.scan();
        s.coverage.complete = false;
        s.coverage.unreadable_dirs = gaps.len() as u64;
        s.coverage.gaps = gaps;
        s.coverage.gaps_overflowed = overflowed;
        s
    };
    let opts = fx.opts(ProcessSnapshot::none_running());

    // Control: complete coverage, all offered.
    let p = plan(&fx.scan(), &opts);
    assert_eq!(
        action(&p, &nm),
        Action::RemoveRegenerable,
        "{}",
        why(&p, &nm)
    );
    assert_eq!(action(&p, &r), Action::BackupThenRemove, "{}", why(&p, &r));

    // NEG-4: an unreadable folder inside node_modules makes it ask (never a partial removal),
    // and its parent repository is not offered; a gap elsewhere leaves `r` offered with the
    // count.
    let s = with_gaps(
        vec![
            gap(
                fx.s("code/proj/node_modules/dep/private"),
                GapClass::Permission,
            ),
            gap(fx.s("vault"), GapClass::Privacy),
        ],
        false,
    );
    let p = plan(&s, &opts);
    let it = item(&p, &nm);
    assert_eq!(it.action, Action::Ask, "{}", why(&p, &nm));
    assert!(it.reason.contains("could not be read"), "{}", it.reason);
    assert_eq!(it.evidence.gaps_inside, 1);
    assert_ne!(
        action(&p, &proj),
        Action::BackupThenRemove,
        "{}",
        why(&p, &proj)
    );
    assert_eq!(item(&p, &proj).evidence.gaps_inside, 1);
    let ri = item(&p, &r);
    assert_eq!(ri.action, Action::BackupThenRemove, "{}", why(&p, &r));
    assert!(ri.reason.contains("2 place"), "{}", ri.reason);
    assert!(ri.reason.contains("alternates clones"), "{}", ri.reason);
    assert_eq!(p.coverage_gaps.permission, 1);
    assert_eq!(p.coverage_gaps.privacy, 1);
    // A mount point not entered inside a repository: a gap inside it.
    let s = with_gaps(vec![gap(fx.s("code/m/mnt"), GapClass::Mount)], false);
    let p = plan(&s, &opts);
    assert_eq!(action(&p, &m), Action::Ask, "{}", why(&p, &m));
    assert_eq!(action(&p, &r), Action::BackupThenRemove, "{}", why(&p, &r));
    // An overflowed record: every repository asks, naming the opt-in; with it, offered again.
    let s = with_gaps(vec![gap(fx.s("vault"), GapClass::Permission)], true);
    let p = plan(&s, &opts);
    let ri = item(&p, &r);
    assert_eq!(ri.action, Action::Ask, "{}", why(&p, &r));
    assert!(
        ri.reason.contains("--accept-incomplete-coverage"),
        "{}",
        ri.reason
    );
    assert!(p.coverage_gaps.overflowed);
    let mut accept = fx.opts(ProcessSnapshot::none_running());
    accept.accept_incomplete_coverage = true;
    let p = plan(&s, &accept);
    assert_eq!(action(&p, &r), Action::BackupThenRemove, "{}", why(&p, &r));
}

/// Claude Code's project directory name for a path (every non-alphanumeric character to `-`),
/// written out here so the test also compiles where the tool has no encoder.
fn slug_of(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The AC-10 shape: a `session-owner` first line (no cwd) and a second line past the 64 KiB
/// bound, so the cwd is never reached. Every value is synthetic.
fn session_owner_transcript(cwd: &Path) -> String {
    format!(
        "{{\"type\":\"session-owner\",\"accountUuid\":\"0\",\"organizationUuid\":\"0\",\
         \"sessionId\":\"0000\"}}\n{{\"type\":\"user\",\"pad\":\"{}\",\"cwd\":\"{}\"}}\n",
        "x".repeat(70 * 1024),
        cwd.display()
    )
}

/// goodqa#16711 slug fallback (wal:w6043f204c8a): a recent Claude transcript with no cwd
/// within the bound (the session-owner shape, or empty) is placed by its project directory
/// name; its candidate in a session holds that session, and no gap is left. (Fails at 78d584a,
/// where it was a recent gap and the checkout asked.)
#[test]
fn claude_slug_fallback_holds() {
    for body in [Some(()), None] {
        let l = live_fixture();
        let rel = format!(".claude/projects/{}/0000.jsonl", slug_of(&l.wt));
        let text = body.map_or(String::new(), |_| session_owner_transcript(&l.wt));
        l.fx.write(&rel, &text);
        l.fx.date(&rel, 3600);
        let p = l.fx.plan(ProcessSnapshot::none_running());
        let what = if body.is_some() {
            "session-owner"
        } else {
            "empty"
        };
        kept(&l, &p, what);
        assert!(
            p.coverage_gaps.agent_data_recent.is_empty(),
            "{what}: {:?}",
            p.coverage_gaps
        );
    }
}

fn oracle_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-live-sessions.py")
}

/// The slug contract (wal:w6043f204c8a): the tool and the oracle decode project directory
/// names to the same candidates. Ambiguous names (every candidate kept), each lossy character
/// (`.`, `_`, space, `-`, `/`), no match, and the walk bound (incomplete).
#[test]
fn claude_slug_contract() {
    let fx = Fx::new();
    let b = fx.p("slugs");
    let ways = ["a.b", "a_b", "a b", "a-b"];
    for d in ways.iter().chain(&["x/y", "x-y", "p.q/r"]) {
        fs::create_dir_all(b.join(d)).unwrap();
    }
    // Four ways at each of three levels: far more listings than MAX_SLUG_DIRS.
    let mut lvl = vec![b.join("deep")];
    for _ in 0..3 {
        lvl = lvl
            .iter()
            .flat_map(|d| ways.iter().map(move |n| d.join(n)))
            .collect();
    }
    for d in &lvl {
        fs::create_dir_all(d.join("end")).unwrap();
    }
    let base = slug_of(&b);
    let mut ab: Vec<PathBuf> = ways.iter().map(|n| b.join(n)).collect();
    ab.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    let mut xy = vec![b.join("x-y"), b.join("x/y")];
    xy.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    let cases: Vec<(String, Vec<PathBuf>, bool)> = vec![
        (format!("{base}-a-b"), ab, false),
        (format!("{base}-x-y"), xy, false),
        (format!("{base}-p-q-r"), vec![b.join("p.q/r")], false),
        (format!("{base}-nothing-here"), vec![], false),
        (format!("{base}-deep-a-b-a-b-a-b-end"), vec![], true),
    ];
    for (s, want, inc) in &cases {
        let got = slug_candidates(s, MAX_SLUG_DIRS);
        assert_eq!(got.incomplete, *inc, "{s}");
        if !inc {
            assert_eq!(&got.candidates, want, "{s}");
        }
    }
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(oracle_script())
        .arg("--slug-max-dirs")
        .arg(MAX_SLUG_DIRS.to_string())
        .arg("--slug-candidates")
        .arg("--")
        .args(cases.iter().map(|c| &c.0))
        .output()
        .expect("python3 runs the oracle");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), cases.len(), "{text}");
    for (line, (s, want, inc)) in lines.iter().zip(&cases) {
        let mut f = line.split('\t');
        assert_eq!(f.next(), Some(s.as_str()), "{line}");
        let rest: Vec<&str> = f.collect();
        if *inc {
            assert_eq!(rest, vec!["incomplete"], "{s}");
        } else if want.is_empty() {
            assert_eq!(rest, vec!["none"], "{s}");
        } else {
            let w: Vec<String> = want.iter().map(|p| p.display().to_string()).collect();
            assert_eq!(rest, w, "{s}");
        }
    }
}

/// The oracle, run on the fixture home with no real process read, agrees with the plan: exit
/// 0, the same agent-data gap counts and the same live sessions.
fn oracle_agrees(l: &Live, p: &Plan, what: &str) {
    let plan_file = l.fx.home.parent().unwrap().join("plan.json");
    fs::write(&plan_file, serde_json::to_string(p).unwrap()).unwrap();
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(oracle_script())
        .args(["--plan", &plan_file.to_string_lossy()])
        .args(["--home", &l.fx.s("")])
        .args(["--now", &l.fx.now.to_string()])
        .args(["--slug-max-dirs", &MAX_SLUG_DIRS.to_string()])
        .arg("--no-processes")
        .output()
        .expect("python3 runs the oracle");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{what}: {text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut gaps: BTreeMap<String, u64> = BTreeMap::new();
    let mut live: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(r) = line.strip_prefix("  gap   ") {
            let mut w = r.split_whitespace();
            let n: u64 = w.next().unwrap().parse().unwrap();
            gaps.insert(w.next().unwrap().to_string(), n);
        } else if let Some(r) = line.strip_prefix("  live  ") {
            live.push(r.split("  (").next().unwrap().to_string());
        }
    }
    let tool_gaps: BTreeMap<String, u64> = p
        .coverage_gaps
        .agent_data
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    assert_eq!(gaps, tool_gaps, "{what}: gap counts differ\n{text}");
    let mut tool_live: Vec<String> = p
        .totals
        .sessions
        .iter()
        .filter(|s| s.live)
        .map(|s| s.session.clone())
        .collect();
    tool_live.sort();
    live.sort();
    assert_eq!(live, tool_live, "{what}: live sessions differ\n{text}");
}

/// The slug fallback through the plan, each case checked against the oracle: ambiguous
/// candidates in two sessions (both held); a candidate under an agent dir in no session
/// (the gap stays, fail closed); a recent name with no match (ask); an old one (counted only);
/// an incomplete walk (a recent gap, ask).
#[test]
fn claude_slug_fallback_plan() {
    // Held: the session-owner shape.
    let l = live_fixture();
    let rel = format!(".claude/projects/{}/0000.jsonl", slug_of(&l.wt));
    l.fx.write(&rel, &session_owner_transcript(&l.wt));
    l.fx.date(&rel, 3600);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    kept(&l, &p, "held");
    assert!(p
        .evidence_notes
        .iter()
        .any(|n| n.starts_with("transcript-cwd-from-project-name (1 candidate): ")));
    oracle_agrees(&l, &p, "held");

    // Ambiguous: two sessions whose checkouts encode alike are both held.
    let l = live_fixture();
    let q1 = l.fx.repo(".agent-harness/work/q.1/wt");
    let q2 = l.fx.repo(".agent-harness/work/q_1/wt");
    l.fx.age_all();
    let rel = format!(".claude/projects/{}/0000.jsonl", slug_of(&q1));
    l.fx.write(&rel, "");
    l.fx.date(&rel, 3600);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    for (c, s) in [(&q1, "q.1"), (&q2, "q_1")] {
        assert_eq!(action(&p, c), Action::Keep, "{}", why(&p, c));
        assert!(session(&p, s).live, "{s}");
    }
    assert!(p.coverage_gaps.agent_data_recent.is_empty());
    assert_eq!(
        action(&p, &l.wt),
        Action::BackupThenRemove,
        "{}",
        why(&p, &l.wt)
    );
    oracle_agrees(&l, &p, "ambiguous");

    // A candidate under an agent dir but in no session: no session holds it, so the gap stays
    // (the ruled fail-closed case) and the other agent-dir items ask; rule 5 keeps the
    // candidate itself.
    let l = live_fixture();
    let w1 = l.fx.repo(".claude/worktrees/w1");
    l.fx.age_all();
    let rel = format!(".claude/projects/{}/0000.jsonl", slug_of(&w1));
    l.fx.write(&rel, "");
    l.fx.date(&rel, 3600);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    assert_eq!(action(&p, &w1), Action::Keep, "{}", why(&p, &w1));
    assert_eq!(item(&p, &w1).evidence.rule, Some(5));
    assert_eq!(action(&p, &l.wt), Action::Ask, "{}", why(&p, &l.wt));
    assert_eq!(
        p.coverage_gaps
            .agent_data_recent
            .get(TRANSCRIPT_CWD_UNKNOWN),
        Some(&1)
    );
    oracle_agrees(&l, &p, "unheld candidate");

    // No match: recent asks, old is counted only.
    for (ago, want) in [(3600, Action::Ask), (30 * DAY, Action::BackupThenRemove)] {
        let l = live_fixture();
        let rel = ".claude/projects/-nowhere-0b7d-none/0000.jsonl";
        l.fx.write(rel, "");
        l.fx.date(rel, ago);
        // Held open by an agent working in session abc, so that the old one is read too.
        let mut a = agent(540, "claude", &l.fx.s(".agent-harness/work/abc/wt"));
        a.open_files = Some(vec![l.fx.s(rel)]);
        let p = l.fx.plan(snap(vec![a]));
        assert_eq!(action(&p, &l.wt), want, "{ago}: {}", why(&p, &l.wt));
        assert_eq!(
            p.coverage_gaps.agent_data.get(TRANSCRIPT_CWD_UNKNOWN),
            // A recent one is read twice: held open, and as a recent transcript.
            Some(&if ago == 3600 { 2 } else { 1 }),
            "{ago}"
        );
        assert_eq!(
            p.coverage_gaps
                .agent_data_recent
                .contains_key(TRANSCRIPT_CWD_UNKNOWN),
            ago == 3600
        );
    }

    // Incomplete walk: a recent gap of its own class.
    let l = live_fixture();
    let ways = ["a.b", "a_b", "a b", "a-b"];
    let mut lvl = vec![l.fx.p("deep")];
    for _ in 0..3 {
        lvl = lvl
            .iter()
            .flat_map(|d| ways.iter().map(move |n| d.join(n)))
            .collect();
    }
    for d in &lvl {
        fs::create_dir_all(d.join("end")).unwrap();
    }
    l.fx.age_all();
    let rel = format!(
        ".claude/projects/{}/0000.jsonl",
        slug_of(&l.fx.p("deep/a-b/a-b/a-b/end"))
    );
    l.fx.write(&rel, "");
    l.fx.date(&rel, 3600);
    let p = l.fx.plan(ProcessSnapshot::none_running());
    assert_eq!(
        p.coverage_gaps
            .agent_data_recent
            .get(TRANSCRIPT_SLUG_INCOMPLETE),
        Some(&1),
        "{:?}",
        p.coverage_gaps
    );
    assert_eq!(action(&p, &l.wt), Action::Ask, "{}", why(&p, &l.wt));
    oracle_agrees(&l, &p, "incomplete");
}

/// The fixture for rule 5: one item of each removing kind outside every session, each with a
/// subdirectory, plus a session checkout. `code/r` has unsaved work (recover first).
fn rule5_fixture() -> (Fx, Vec<(&'static str, Action)>) {
    let fx = Fx::new();
    fx.repo("code/p");
    fx.write("code/p/package.json", "{}");
    fx.write("code/p/node_modules/dep/package.json", "{}");
    fx.repo("code/r");
    fx.repo("code/b");
    fx.repo(".agent-harness/work/s1/wt");
    fx.age_all();
    let items = vec![
        ("code/p/node_modules", Action::RemoveRegenerable),
        ("code/r", Action::RecoverBackupThenRemove),
        ("code/b", Action::BackupThenRemove),
        (".agent-harness/work/s1/wt", Action::BackupThenRemove),
    ];
    (fx, items)
}

/// Plan `fx` with `code/r` dirty and `procs` running.
fn rule5_plan(fx: &Fx, procs: Vec<Proc>) -> Plan {
    let mut scan = fx.scan();
    let r = fx.s("code/r");
    for n in scan.nodes.iter_mut().filter(|n| n.path == r) {
        if let Some(info) = n.repo.as_mut() {
            info.facts.tree_clean = Tri::No;
        }
    }
    plan(&scan, &fx.opts(snap(procs)))
}

/// A recent transcript recording `cwd`.
fn recent_transcript(fx: &Fx, name: &str, cwd: &Path) {
    let rel = format!(".claude/projects/-r5-none/{name}.jsonl");
    fx.write(
        &rel,
        &format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
    );
    fx.date(&rel, 3600);
}

/// Run the oracle over `plan` on `fx`'s home (no real process read): (exit 0, stdout).
fn oracle_on(fx: &Fx, plan_json: &str) -> (bool, String) {
    let plan_file = fx.home.parent().unwrap().join("plan5.json");
    fs::write(&plan_file, plan_json).unwrap();
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-live-sessions.py"))
        .args(["--plan", &plan_file.to_string_lossy()])
        .args(["--home", &fx.s("")])
        .args(["--now", &fx.now.to_string()])
        .arg("--no-processes")
        .output()
        .expect("python3 runs the oracle");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// goodqa#16711 rule 5 (main ruling wal:w34c987e0ea8): a recent transcript's working directory
/// holds every item it equals, contains or lies inside, whatever the removing action, at one
/// choke point in the plan; one at /, the home, the harness root or work/ holds nothing, and the
/// same list governs process working directories. The oracle flags each case the tool would
/// offer. (Fails at 78d584a: the item inside a recent cwd was offered.)
#[test]
fn recent_cwd_holds_every_removing_kind() {
    // Control: each removing kind is offered.
    let (fx, items) = rule5_fixture();
    let p = rule5_plan(&fx, vec![]);
    for (rel, kind) in &items {
        let path = fx.p(rel);
        assert_eq!(action(&p, &path), *kind, "control: {}", why(&p, &path));
    }

    for (rel, kind) in &items {
        let below = if *kind == Action::RemoveRegenerable {
            format!("{rel}/dep")
        } else {
            format!("{rel}/src")
        };
        let above = rel.rsplit_once('/').unwrap().0.to_string();
        for (pos, cwd_rel) in [("at", rel.to_string()), ("above", above), ("below", below)] {
            let (fx, _) = rule5_fixture();
            recent_transcript(&fx, "t", &fx.p(&cwd_rel));
            let p = rule5_plan(&fx, vec![]);
            let target = fx.p(rel);
            let it = item(&p, &target);
            assert_eq!(
                it.action,
                Action::Keep,
                "{kind:?} cwd {pos}: {}",
                why(&p, &target)
            );
            assert!(
                it.reason.starts_with("recent agent session: ") || it.evidence.rule == Some(4),
                "{kind:?} cwd {pos}: {}",
                it.reason
            );
            let (ok, out) = oracle_on(&fx, &serde_json::to_string(&p).unwrap());
            assert!(
                ok,
                "{kind:?} cwd {pos}: the oracle rejects the tool's plan\n{out}"
            );
            // Offer the item anyway: the oracle flags it.
            let mut v: serde_json::Value = serde_json::to_value(&p).unwrap();
            for i in v["items"].as_array_mut().unwrap() {
                if i["path"] == target.to_string_lossy().as_ref() {
                    i["action"] = serde_json::Value::from(kind.as_str());
                }
            }
            let (ok, out) = oracle_on(&fx, &v.to_string());
            assert!(
                !ok,
                "{kind:?} cwd {pos}: the oracle missed the offered item\n{out}"
            );
            assert!(out.contains(&format!(": {}", target.display())), "{out}");
        }
    }

    // The ancestor-exception list holds nothing, for transcripts and processes alike.
    let (fx, items) = rule5_fixture();
    for cwd in [
        "/".to_string(),
        fx.s(""),
        fx.s(".agent-harness"),
        fx.s(".agent-harness/work"),
    ] {
        recent_transcript(&fx, "anc", Path::new(&cwd));
        let p = rule5_plan(&fx, vec![proc(600, &cwd, &["bash"])]);
        for (rel, kind) in &items {
            let path = fx.p(rel);
            assert_eq!(action(&p, &path), *kind, "cwd {cwd}: {}", why(&p, &path));
        }
        let (ok, out) = oracle_on(&fx, &serde_json::to_string(&p).unwrap());
        assert!(ok, "cwd {cwd}: {out}");
    }
}
