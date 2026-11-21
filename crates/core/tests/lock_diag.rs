//! DIAGNOSTIC ONLY (branch diag/apply-lock-macos, never merged): is an flock that this
//! thread released still held for a moment while other threads spawn children? flock belongs
//! to the open file description, and a child holds a copy of every descriptor from its fork
//! until its exec closes the CLOEXEC ones. The experiment prints counts; it never fails on them.
#![cfg(unix)]

use rustix::fd::OwnedFd;
use rustix::fs::{flock, open, FlockOperation, Mode, OFlags};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn try_lock(p: &Path) -> Result<OwnedFd, rustix::io::Errno> {
    let fd = open(
        p,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    flock(&fd, FlockOperation::NonBlockingLockExclusive).map(|()| fd)
}

#[derive(Default)]
struct Tally {
    rounds: usize,
    wouldblock: usize,
    other: usize,
    max_clear_ms: f64,
    total_clear_ms: f64,
}

/// Take the lock, waiting at most 5 s and recording any wait.
fn take(p: &Path, t: &mut Tally) -> OwnedFd {
    let start = Instant::now();
    let mut waited = false;
    loop {
        match try_lock(p) {
            Ok(fd) => {
                if waited {
                    let ms = start.elapsed().as_secs_f64() * 1e3;
                    t.total_clear_ms += ms;
                    t.max_clear_ms = t.max_clear_ms.max(ms);
                }
                return fd;
            }
            Err(rustix::io::Errno::WOULDBLOCK) => {
                if !waited {
                    t.wouldblock += 1;
                    waited = true;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "lock never cleared"
                );
                std::thread::sleep(Duration::from_micros(200));
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                t.other += 1;
                panic!("flock: {e}");
            }
        }
    }
}

fn run(label: &str, spawners: usize, secs: u64, dir: &Path, script: &PathBuf) {
    let stop = Arc::new(AtomicBool::new(false));
    let spawned = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::new();
    for i in 0..spawners {
        let (stop, spawned, script, dir) = (
            stop.clone(),
            spawned.clone(),
            script.clone(),
            dir.to_path_buf(),
        );
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // The two shapes the apply suites use: the fake rclone script in its own
                // process group, and git with a working directory and extra environment.
                let mut c = if i % 2 == 0 {
                    let mut c = Command::new(&script);
                    c.process_group(0);
                    c
                } else {
                    let mut c = Command::new("git");
                    c.arg("--version")
                        .current_dir(&dir)
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .env("LC_ALL", "C");
                    c
                };
                let _ = c
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output();
                spawned.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    let p = dir.join(format!("{label}.lock"));
    let mut t = Tally::default();
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        let a = take(&p, &mut t);
        drop(a);
        t.rounds += 1;
    }
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().unwrap();
    }
    println!(
        "PD-LOCK-EXP os={} mode={label} spawners={spawners} secs={secs} spawned={} rounds={} wouldblock={} other={} max_clear_ms={:.2} mean_clear_ms={:.2}",
        std::env::consts::OS,
        spawned.load(Ordering::Relaxed),
        t.rounds,
        t.wouldblock,
        t.other,
        t.max_clear_ms,
        if t.wouldblock > 0 { t.total_clear_ms / t.wouldblock as f64 } else { 0.0 },
    );
}

#[test]
fn released_flock_while_other_threads_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake");
    std::fs::write(&script, "#!/bin/bash\necho ok\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    run("quiet", 0, 8, dir.path(), &script);
    run("spawn2", 2, 8, dir.path(), &script);
    run("spawn4", 4, 8, dir.path(), &script);
    run("quiet-after", 0, 4, dir.path(), &script);
}
