//! Test-only fake git for tests/containment.rs: appends its pid to the file named by
//! `PRUNEDISK_FAKE_GIT_PIDS`, starts one more copy of itself when
//! `PRUNEDISK_FAKE_GIT_GRANDCHILD` is set (a grandchild of the worker), then stays running.

// This test helper writes its pid file and starts a copy of itself.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::time::Duration;

fn main() {
    let Some(pids) = std::env::var_os("PRUNEDISK_FAKE_GIT_PIDS") else {
        std::process::exit(2);
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&pids)
        .expect("pid file");
    writeln!(f, "{}", std::process::id()).expect("write pid");
    drop(f);
    let sleeper = std::env::args().nth(1).as_deref() == Some("--sleeper");
    if !sleeper && std::env::var_os("PRUNEDISK_FAKE_GIT_GRANDCHILD").is_some() {
        let exe = std::env::current_exe().expect("exe");
        let _child = std::process::Command::new(exe)
            .arg("--sleeper")
            .spawn()
            .expect("spawn");
    }
    std::thread::sleep(Duration::from_secs(600));
}
