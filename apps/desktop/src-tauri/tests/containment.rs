//! Spec AC-3, NEG-3, NEG-16: the analysis worker and every git process it started end on
//! Cancel and when the parent goes away; without "go" no git starts. The built app runs as the
//! worker, with a fake git that stays running (examples/fake_git.rs). Linux, macOS, Windows.
//!
//! "The parent goes away" is exercised by closing what the parent's death closes: the worker's
//! stdin (Unix, where the worker's watchdog sweeps its session) and the job object handle
//! (Windows, where kill-on-close ends the job). Killing a real parent harness closes the same.

// The test writes its fixture.
#![allow(clippy::disallowed_methods)]

use prunedisk_desktop_lib::worker::{
    spawn_worker, WorkerArgs, WorkerProcess, END_DEADLINE, GIT_TIMEOUT, GO,
};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, Signal, System};

/// How long a worker whose parent is gone may take to end. Its two sweeps take about 0.1 s
/// when nothing resists; the bound is well under the 10 s deadline, so the deadline cannot be
/// what passes these checks.
const END_BOUND: Duration = Duration::from_secs(5);

/// The tests share this process's environment (the worker and the fake git read it), so they
/// run one at a time; each starts with none of the test variables set.
static SERIAL: Mutex<()> = Mutex::new(());
const TEST_VARS: [&str; 5] = [
    "PRUNEDISK_FAKE_GIT_PIDS",
    "PRUNEDISK_FAKE_GIT_GRANDCHILD",
    "PRUNEDISK_FAKE_GIT_FIRST_EXITS",
    "PRUNEDISK_TEST_END_PAUSE_MS",
    "PRUNEDISK_TEST_STALL_END",
];

fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    for v in TEST_VARS {
        std::env::remove_var(v);
    }
    guard
}

fn app_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_prunedisk-desktop"))
}

/// A built example (`cargo build --examples`).
fn example(name: &str) -> PathBuf {
    let mut p = app_exe();
    p.pop();
    p.push("examples");
    p.push(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    });
    assert!(p.is_file(), "build the examples first: {}", p.display());
    p
}

fn fake_git() -> PathBuf {
    example("fake_git")
}

/// A folder core takes for a plain repository, so the scan runs git on it: core's layout check
/// wants HEAD, config, refs, objects, objects/info and objects/pack before it runs any git
/// (a fixture without config never starts git; CI run 44608860767).
fn repo_fixture(dir: &Path) -> PathBuf {
    repos_fixture(dir, "root", 1)
}

/// `n` plain repositories under `dir/name` (app0 ... app<n-1>).
fn repos_fixture(dir: &Path, name: &str, n: usize) -> PathBuf {
    let root = dir.join(name);
    for i in 0..n {
        let app = root.join(if n == 1 {
            "app".to_string()
        } else {
            format!("app{i}")
        });
        let git = app.join(".git");
        std::fs::create_dir_all(git.join("objects").join("info")).unwrap();
        std::fs::create_dir_all(git.join("objects").join("pack")).unwrap();
        std::fs::create_dir_all(git.join("refs").join("heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        std::fs::write(app.join("main.rs"), "fn main() {}\n").unwrap();
    }
    root
}

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Each fake git's pid and start time (ns since the Unix epoch).
fn starts(file: &Path) -> Vec<(u32, u128)> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// The time the worker says it stopped starting processes ("analysis worker: ending at <ns>"
/// on its stderr), if it said so.
fn ending_mark(stderr: &str) -> Option<u128> {
    stderr
        .lines()
        .find_map(|l| l.strip_prefix("analysis worker: ending at "))
        .and_then(|t| t.trim().parse().ok())
}

/// Why no git started: end the worker and return its exit and stderr.
fn worker_report(w: &mut WorkerProcess) -> String {
    w.handle.terminate();
    w.handle.wait();
    let mut err = String::new();
    if let Some(mut s) = w.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut s, &mut err);
    }
    format!("worker stderr: {err:?}")
}

fn pids(file: &Path) -> Vec<u32> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

fn alive(list: &[u32]) -> Vec<u32> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    list.iter()
        .copied()
        .filter(|p| {
            sys.process(Pid::from_u32(*p))
                .is_some_and(|x| x.status() != ProcessStatus::Zombie)
        })
        .collect()
}

/// Who is still running, for a failure message: role, name, status, parent and session.
fn describe(list: &[u32], worker: u32) -> String {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    alive(list)
        .iter()
        .map(|p| {
            let role = if *p == worker { "worker" } else { "fake git" };
            match sys.process(Pid::from_u32(*p)) {
                Some(x) => format!(
                    "{p} ({role}, {:?}, {:?}, parent {:?}, session {:?})",
                    x.name(),
                    x.status(),
                    x.parent(),
                    x.session_id()
                ),
                None => format!("{p} ({role}, gone)"),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn wait_for(cond: impl Fn() -> bool, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn args(root: &Path) -> WorkerArgs {
    WorkerArgs {
        roots: vec![root.to_path_buf()],
        one_file_system: false,
        collect_facts: true,
        top_files: 10,
        min_output_bytes: 1 << 20,
        git: fake_git().into_os_string(),
        git_timeout: GIT_TIMEOUT,
    }
}

fn start(root: &Path, pid_file: &Path, gits: usize) -> (WorkerProcess, Vec<u32>) {
    start_with(&args(root), pid_file, gits)
}

fn start_with(a: &WorkerArgs, pid_file: &Path, gits: usize) -> (WorkerProcess, Vec<u32>) {
    std::fs::write(pid_file, "").unwrap();
    let mut w = spawn_worker(&app_exe(), a).expect("spawn the worker");
    let stdin = w.stdin.as_mut().unwrap();
    stdin.write_all(&[GO]).unwrap();
    stdin.flush().unwrap();
    if !wait_for(|| pids(pid_file).len() >= gits, Duration::from_secs(60)) {
        let report = worker_report(&mut w);
        panic!(
            "the fake git never started ({} of {gits} pids); {report}",
            pids(pid_file).len()
        );
    }
    // Let a grandchild record itself too.
    std::thread::sleep(Duration::from_millis(200));
    (w, pids(pid_file))
}

#[test]
fn the_worker_and_every_git_it_started_end() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let root = repo_fixture(tmp.path());
    let pid_file = tmp.path().join("git-pids.txt");
    std::env::set_var("PRUNEDISK_FAKE_GIT_PIDS", &pid_file);
    std::fs::write(&pid_file, "").unwrap();

    // Handshake: without "go" the worker starts no git.
    {
        let mut w = spawn_worker(&app_exe(), &args(&root)).expect("spawn the worker");
        std::thread::sleep(Duration::from_secs(2));
        assert!(pids(&pid_file).is_empty(), "git started before go");
        let worker = w.handle.pid();
        w.handle.terminate();
        w.handle.wait();
        drop(w.stdin.take());
        assert!(alive(&[worker]).is_empty());
    }

    // Cancel while git runs: a git with no children, then one that started a grandchild.
    for grandchild in [false, true] {
        if grandchild {
            std::env::set_var("PRUNEDISK_FAKE_GIT_GRANDCHILD", "1");
        }
        let (mut w, gits) = start(&root, &pid_file, if grandchild { 2 } else { 1 });
        let worker = w.handle.pid();
        let found = w.handle.terminate();
        if cfg!(unix) {
            // The sweep saw the git processes by their session id (not vacuous).
            assert!(
                found > gits.len(),
                "the sweep found {found} processes; fake git pids {gits:?}"
            );
        }
        w.handle.wait();
        assert!(
            wait_for(
                || alive(&gits).is_empty() && alive(&[worker]).is_empty(),
                Duration::from_secs(5)
            ),
            "still running after cancel: {}",
            describe(&[&gits[..], &[worker]].concat(), worker)
        );
        drop(w.stdin.take());
        no_git_left(&pid_file, worker, "cancel");
    }
    std::env::remove_var("PRUNEDISK_FAKE_GIT_GRANDCHILD");

    // The parent goes away.
    {
        let (mut w, gits) = start(&root, &pid_file, 1);
        let worker = w.handle.pid();
        #[cfg(unix)]
        {
            drop(w.stdin.take());
            assert!(
                wait_for(
                    || alive(&gits).is_empty() && alive(&[worker]).is_empty(),
                    Duration::from_secs(5)
                ),
                "still running after stdin closed: {}",
                describe(&[&gits[..], &[worker]].concat(), worker)
            );
            w.handle.wait();
        }
        #[cfg(windows)]
        {
            drop(w.stdin.take());
            drop(w);
            assert!(
                wait_for(
                    || alive(&gits).is_empty() && alive(&[worker]).is_empty(),
                    Duration::from_secs(5)
                ),
                "still running after the job handle closed: {}",
                describe(&[&gits[..], &[worker]].concat(), worker)
            );
        }
        no_git_left(&pid_file, worker, "the parent going away");
    }

    // The parent goes away right after "go", and between two git commands (code inspection
    // F1): no git may start once the worker is ending, and none may outlive it. A test-build
    // pause after the worker's sweep holds open the window in which its analysis could start
    // one more git before it exits. Not vacuous: without the block on new processes core
    // starts its next git command as soon as one is killed, so gits keep starting while the
    // sweeps run and during the pause. Core runs at most 4 git jobs at a time and a pass every
    // 50 ms kills at most those 4, so within the 250 ms of slack below at most about 24 git
    // commands are ended; with 200 repositories, each needing several git commands, core still
    // has git work then, and a git starts after the slack whatever the timing. Red without the
    // block at 2286bf0: run 44644832396, jobs 135984870728 (Linux) and 135984870748 (macOS).
    std::env::set_var("PRUNEDISK_TEST_END_PAUSE_MS", "3000");
    let many = repos_fixture(tmp.path(), "many", 200);
    for between in [false, true] {
        let step = if between {
            "the parent going away between two git commands"
        } else {
            "the parent going away right after go"
        };
        let file = tmp.path().join(if between {
            "pids-between.txt"
        } else {
            "pids-after-go.txt"
        });
        std::fs::write(&file, "").unwrap();
        std::env::set_var("PRUNEDISK_FAKE_GIT_PIDS", &file);
        if between {
            std::env::set_var("PRUNEDISK_FAKE_GIT_FIRST_EXITS", "1");
        }
        let mut w = spawn_worker(&app_exe(), &args(&many)).expect("spawn the worker");
        let worker = w.handle.pid();
        {
            let stdin = w.stdin.as_mut().unwrap();
            stdin.write_all(&[GO]).unwrap();
            stdin.flush().unwrap();
        }
        if between {
            // The first git ran and ended: the worker is between two git commands.
            let first_done = || {
                let p = pids(&file);
                !p.is_empty() && alive(&p[..1]).is_empty()
            };
            if !wait_for(first_done, Duration::from_secs(60)) {
                let report = worker_report(&mut w);
                panic!("the fake git never started for {step}; {report}");
            }
        }
        let closed_at = now_ns();
        drop(w.stdin.take());
        #[cfg(windows)]
        let stderr = {
            // The parent's death closes the job handle too.
            drop(w);
            String::new()
        };
        #[cfg(unix)]
        let stderr = {
            assert!(
                wait_for(|| alive(&[worker]).is_empty(), Duration::from_secs(20)),
                "the worker is still running 20 s after {step}"
            );
            w.handle.wait();
            let mut err = String::new();
            if let Some(mut s) = w.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut s, &mut err);
            }
            err
        };
        assert!(
            wait_for(|| alive(&[worker]).is_empty(), Duration::from_secs(20)),
            "the worker is still running after {step}"
        );
        // A fake git records its start after exec, so one forked just before the mark may
        // record a later time: 250 ms of slack. Any git that starts later is a new start (and
        // `no_git_left` below catches every survivor, whenever it started).
        let mark = closed_at.max(ending_mark(&stderr).unwrap_or(0));
        let late: Vec<(u32, u128)> = starts(&file)
            .into_iter()
            .filter(|(_, t)| *t > mark + 250_000_000)
            .collect();
        assert!(
            late.is_empty(),
            "a git started after {step}: {} of {} fake gits started after the worker was \
             ending ({late:?}); worker stderr {stderr:?}",
            late.len(),
            starts(&file).len()
        );
        no_git_left(&file, worker, step);
    }
    std::env::remove_var("PRUNEDISK_FAKE_GIT_FIRST_EXITS");
    std::env::remove_var("PRUNEDISK_TEST_END_PAUSE_MS");
}

/// Every git the worker started, including one started after the pids were first read (core
/// runs its next git command when one is killed), has ended too.
/// A survivor is killed after it is described, so a failure leaves nothing running.
fn no_git_left(pid_file: &Path, worker: u32, step: &str) {
    std::thread::sleep(Duration::from_millis(300));
    let all = pids(pid_file);
    let none = wait_for(|| alive(&all).is_empty(), Duration::from_secs(2));
    let report = describe(&all, worker);
    if !none {
        alive(&all).into_iter().for_each(kill_only);
    }
    assert!(
        none,
        "a git started by the worker outlived {step}: {report} (all fake git pids {all:?})"
    );
}

/// Kill one process (SIGKILL; on Windows, terminate it) and nothing else.
fn kill_only(pid: u32) {
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[p]), true);
    if let Some(x) = sys.process(p) {
        x.kill_with(Signal::Kill);
    }
}

/// Send "go" to a worker.
fn go(w: &mut WorkerProcess) {
    let stdin = w.stdin.as_mut().unwrap();
    stdin.write_all(&[GO]).unwrap();
    stdin.flush().unwrap();
}

/// The app is force-killed (SIGKILL, Force Quit, a crash): the system closes every pipe end
/// it held, so the worker's stdout and stderr lose their reader as its stdin closes. A write
/// to stderr then fails, which must not stop the worker ending (security review S1: at
/// ed9e657 the watchdog's report panicked while it held the ending lock and the worker hung).
/// Once while a git with a child runs, once right after "go" among 200 repositories.
#[cfg(unix)]
#[test]
fn the_worker_ends_when_its_parent_dies_with_every_pipe() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("git-pids.txt");
    std::env::set_var("PRUNEDISK_FAKE_GIT_PIDS", &pid_file);
    std::env::set_var("PRUNEDISK_FAKE_GIT_GRANDCHILD", "1");
    let one = repo_fixture(tmp.path());
    let (w, _) = start(&one, &pid_file, 2);
    dies_with_every_pipe(w, &pid_file, "a force-killed parent while git runs");
    std::env::remove_var("PRUNEDISK_FAKE_GIT_GRANDCHILD");

    let many = repos_fixture(tmp.path(), "many", 200);
    std::fs::write(&pid_file, "").unwrap();
    let mut w = spawn_worker(&app_exe(), &args(&many)).expect("spawn the worker");
    go(&mut w);
    dies_with_every_pipe(w, &pid_file, "a force-killed parent right after go");
}

#[cfg(unix)]
fn dies_with_every_pipe(mut w: WorkerProcess, pid_file: &Path, step: &str) {
    let worker = w.handle.pid();
    drop(w.stdout.take());
    drop(w.stderr.take());
    drop(w.stdin.take());
    let ended = wait_for(|| alive(&[worker]).is_empty(), END_BOUND);
    let report = describe(&[&pids(pid_file)[..], &[worker]].concat(), worker);
    if !ended {
        w.handle.terminate();
    }
    let code = w.handle.wait().and_then(|s| s.code());
    assert!(
        ended,
        "the worker is still running {END_BOUND:?} after {step}: {report}"
    );
    assert_eq!(code, Some(5), "the worker's exit after {step}");
    no_git_left(pid_file, worker, step);
}

/// A real parent process that holds the worker's pipes (and on Windows its job object) is
/// killed outright; the worker and every git it started still end.
#[test]
fn killing_the_parent_process_ends_the_worker_and_every_git() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let root = repo_fixture(tmp.path());
    let pid_file = tmp.path().join("git-pids.txt");
    std::fs::write(&pid_file, "").unwrap();
    std::env::set_var("PRUNEDISK_FAKE_GIT_PIDS", &pid_file);
    std::env::set_var("PRUNEDISK_FAKE_GIT_GRANDCHILD", "1");
    let mut parent = std::process::Command::new(example("worker_parent"))
        .arg(app_exe())
        .arg(fake_git())
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("start the parent");
    let mut line = String::new();
    let out = parent.stdout.take().unwrap();
    let _ = std::io::BufReader::new(out).read_line(&mut line);
    let Ok(worker) = line.trim().parse::<u32>() else {
        let _ = parent.kill();
        let _ = parent.wait();
        panic!("the parent printed no worker pid: {line:?}");
    };
    if !wait_for(|| pids(&pid_file).len() >= 2, Duration::from_secs(60)) {
        let _ = parent.kill();
        let _ = parent.wait();
        kill_only(worker);
        panic!("the fake git never started under the parent");
    }
    std::thread::sleep(Duration::from_millis(200));
    let gits = pids(&pid_file);
    // SIGKILL on Unix, TerminateProcess on Windows.
    parent.kill().expect("kill the parent");
    let _ = parent.wait();
    let ended = wait_for(|| alive(&[worker]).is_empty(), END_BOUND);
    let report = describe(&[&gits[..], &[worker]].concat(), worker);
    if !ended {
        kill_only(worker);
    }
    assert!(
        ended,
        "the worker is still running {END_BOUND:?} after its parent was killed: {report}"
    );
    no_git_left(&pid_file, worker, "the parent process being killed");
}

/// A stuck ending (a test build's watchdog stops for good before its sweep) is cut short by
/// the deadline, which sweeps and exits with 6. Core's git timeout (3 s here) fires while the
/// worker is ending: its kill of the git's process group needs a new `kill` process, which the
/// zero process limit refuses, so only the git itself ends and its child is left to a sweep
/// (security review item 5).
#[cfg(unix)]
#[test]
fn a_stuck_ending_is_cut_short_by_the_deadline() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let root = repo_fixture(tmp.path());
    let pid_file = tmp.path().join("git-pids.txt");
    std::env::set_var("PRUNEDISK_FAKE_GIT_PIDS", &pid_file);
    std::env::set_var("PRUNEDISK_FAKE_GIT_GRANDCHILD", "1");
    std::env::set_var("PRUNEDISK_TEST_STALL_END", "1");
    let mut a = args(&root);
    a.git_timeout = Duration::from_secs(3);
    let (mut w, gits) = start_with(&a, &pid_file, 2);
    let worker = w.handle.pid();
    let began = Instant::now();
    drop(w.stdin.take());
    let git_ended = wait_for(|| alive(&gits[..1]).is_empty(), Duration::from_secs(6));
    let child_left = !alive(&gits[1..]).is_empty();
    let ended = wait_for(|| alive(&[worker]).is_empty(), END_DEADLINE + END_BOUND);
    let took = began.elapsed();
    let report = describe(&[&pids(&pid_file)[..], &[worker]].concat(), worker);
    if !ended {
        w.handle.terminate();
    }
    let code = w.handle.wait().and_then(|s| s.code());
    std::env::remove_var("PRUNEDISK_TEST_STALL_END");
    assert!(
        ended,
        "the worker is still running {took:?} after a stuck ending began: {report}"
    );
    assert_eq!(code, Some(6), "the deadline's exit");
    assert!(
        took + Duration::from_secs(1) >= END_DEADLINE,
        "ended {took:?} after the ending began, before the deadline"
    );
    assert!(git_ended, "core's timeout did not end the git: {report}");
    assert!(
        child_left,
        "the git's child ended before any sweep: {report}"
    );
    no_git_left(&pid_file, worker, "a stuck ending");
}

/// The worker dies on its own (a crash, the out-of-memory killer) while a git with a child
/// runs: the app reads the end of its output and reaps it (`WorkerHandle::reap`, as
/// state.rs's drive_worker does), and nothing it started may outlive it (security review M3).
#[test]
fn a_worker_that_dies_on_its_own_leaves_no_git() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let root = repo_fixture(tmp.path());
    let pid_file = tmp.path().join("git-pids.txt");
    std::env::set_var("PRUNEDISK_FAKE_GIT_PIDS", &pid_file);
    std::env::set_var("PRUNEDISK_FAKE_GIT_GRANDCHILD", "1");
    let (mut w, _) = start(&root, &pid_file, 2);
    let worker = w.handle.pid();
    kill_only(worker);
    // Bounded here, so a reap that never returns fails the test instead of hanging it.
    let handle = std::sync::Arc::clone(&w.handle);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.reap());
    });
    let reaped = rx.recv_timeout(Duration::from_secs(20));
    if reaped.is_err() {
        w.handle.terminate();
        alive(&pids(&pid_file)).into_iter().for_each(kill_only);
    }
    drop(w.stdin.take());
    let status = reaped.expect("the worker was not reaped within 20 s");
    assert!(
        status.is_some_and(|s| !s.success()),
        "the killed worker's exit: {status:?}"
    );
    no_git_left(&pid_file, worker, "a worker that died on its own");
}
