//! Test-only fake git for tests/containment.rs: appends "<pid> <start time in ns since the
//! Unix epoch>" to the file named by `PRUNEDISK_FAKE_GIT_PIDS`, starts one more copy of itself
//! when `PRUNEDISK_FAKE_GIT_GRANDCHILD` is set (a grandchild of the worker), then stays
//! running. With `PRUNEDISK_FAKE_GIT_FIRST_EXITS` set, the first copy (the pid file was empty)
//! exits at once instead, so the worker is between two git commands.

// This test helper writes its pid file and starts a copy of itself.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() {
    let Some(pids) = std::env::var_os("PRUNEDISK_FAKE_GIT_PIDS") else {
        std::process::exit(2);
    };
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let first = std::fs::read_to_string(&pids)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&pids)
        .expect("pid file");
    writeln!(f, "{} {started}", std::process::id()).expect("write pid");
    drop(f);
    if first && std::env::var_os("PRUNEDISK_FAKE_GIT_FIRST_EXITS").is_some() {
        std::process::exit(0);
    }
    let sleeper = std::env::args().nth(1).as_deref() == Some("--sleeper");
    let mut child = None;
    if !sleeper && std::env::var_os("PRUNEDISK_FAKE_GIT_GRANDCHILD").is_some() {
        let exe = std::env::current_exe().expect("exe");
        child = Some(
            std::process::Command::new(exe)
                .arg("--sleeper")
                .spawn()
                .expect("spawn"),
        );
    }
    std::thread::sleep(Duration::from_secs(600));
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
}
