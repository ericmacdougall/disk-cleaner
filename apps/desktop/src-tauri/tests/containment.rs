//! Spec AC-3, NEG-3, NEG-16: the analysis worker and every git process it started end on
//! Cancel and when the parent goes away; without "go" no git starts. The built app runs as the
//! worker, with a fake git that stays running (examples/fake_git.rs). Linux, macOS, Windows.
//!
//! "The parent goes away" is exercised by closing what the parent's death closes: the worker's
//! stdin (Unix, where the worker's watchdog sweeps its session) and the job object handle
//! (Windows, where kill-on-close ends the job). Killing a real parent harness closes the same.

// The test writes its fixture.
#![allow(clippy::disallowed_methods)]

use prunedisk_desktop_lib::worker::{spawn_worker, WorkerArgs, WorkerProcess, GO};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};

fn app_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_prunedisk-desktop"))
}

fn fake_git() -> PathBuf {
    let mut p = app_exe();
    p.pop();
    p.push("examples");
    p.push(if cfg!(windows) {
        "fake_git.exe"
    } else {
        "fake_git"
    });
    assert!(p.is_file(), "build the fake git first: {}", p.display());
    p
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
    }
}

fn start(root: &Path, pid_file: &Path, gits: usize) -> (WorkerProcess, Vec<u32>) {
    std::fs::write(pid_file, "").unwrap();
    let mut w = spawn_worker(&app_exe(), &args(root)).expect("spawn the worker");
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
    // one more git before it exits. Core runs 4 git jobs at a time and a sweep makes at most
    // 40 passes, so it kills at most about 160 gits: with 200 repositories core always has git
    // work left when a sweep stops, and without a block on new processes it starts another
    // git in that window whatever the timing (not vacuous).
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
fn no_git_left(pid_file: &Path, worker: u32, step: &str) {
    std::thread::sleep(Duration::from_millis(300));
    let all = pids(pid_file);
    assert!(
        wait_for(|| alive(&all).is_empty(), Duration::from_secs(2)),
        "a git started by the worker outlived {step}: {} (all fake git pids {all:?})",
        describe(&all, worker)
    );
}
