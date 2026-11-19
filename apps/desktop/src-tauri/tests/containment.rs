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
    let root = dir.join("root");
    let git = root.join("app").join(".git");
    std::fs::create_dir_all(git.join("objects").join("info")).unwrap();
    std::fs::create_dir_all(git.join("objects").join("pack")).unwrap();
    std::fs::create_dir_all(git.join("refs").join("heads")).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .unwrap();
    std::fs::write(root.join("app").join("main.rs"), "fn main() {}\n").unwrap();
    root
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
        .filter_map(|l| l.trim().parse().ok())
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
            "still running after cancel: {:?}",
            alive(&[&gits[..], &[worker]].concat())
        );
        drop(w.stdin.take());
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
                "still running after stdin closed: {:?}",
                alive(&[&gits[..], &[worker]].concat())
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
                "still running after the job handle closed: {:?}",
                alive(&[&gits[..], &[worker]].concat())
            );
        }
    }
}
