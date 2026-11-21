//! The apply lock (r7 F4, macOS CI): an flock belongs to the open file description, and a
//! child spawned by any thread holds a copy of every descriptor until its exec closes the
//! close-on-exec ones. A lock that was just released can therefore stay held for a moment
//! while other threads spawn children. `take_lock` waits that out; a lock held for longer is
//! another apply and is refused.
#![cfg(unix)]

use disk_cleaner_core::apply::{hold_lock, ApplyError};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn a_lock_released_while_other_threads_spawn_children_is_taken_again() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let script = dir.path().join("fake-rclone");
    std::fs::write(&script, "#!/bin/bash\necho ok\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let spawned = Arc::new(AtomicUsize::new(0));
    let mut spawners = Vec::new();
    for i in 0..4 {
        let (stop, spawned, script, cwd) = (
            stop.clone(),
            spawned.clone(),
            script.clone(),
            dir.path().to_path_buf(),
        );
        spawners.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // The two shapes apply spawns: rclone in its own process group, and git with a
                // working directory and its own environment.
                let mut c = if i % 2 == 0 {
                    let mut c = Command::new(&script);
                    c.process_group(0);
                    c
                } else {
                    let mut c = Command::new("git");
                    c.arg("--version")
                        .current_dir(&cwd)
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
    // Release and take the lock again, as a second apply in the same process does, for long
    // enough that children are often mid-spawn when it is released.
    let mut refused = Vec::new();
    let (start, mut rounds) = (Instant::now(), 0usize);
    while rounds < 20_000 || start.elapsed() < Duration::from_secs(4) {
        if start.elapsed() > Duration::from_secs(60) {
            break;
        }
        // Each take follows the release of the one before; no other apply exists.
        match hold_lock(&state) {
            Ok(lock) => drop(lock),
            Err(e) => refused.push(format!("{e:?}")),
        }
        rounds += 1;
    }
    stop.store(true, Ordering::Relaxed);
    for h in spawners {
        h.join().unwrap();
    }
    assert!(
        spawned.load(Ordering::Relaxed) > 100,
        "children were spawned"
    );
    assert!(
        refused.is_empty(),
        "{} of {rounds} takes after a release were refused: {:?}",
        refused.len(),
        refused.first()
    );
}

#[test]
fn a_lock_held_by_another_apply_is_refused_after_a_short_wait() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let held = hold_lock(&state).unwrap();
    let t = Instant::now();
    let r = hold_lock(&state).map(|_| ());
    let waited = t.elapsed();
    assert!(matches!(r, Err(ApplyError::Locked(_))), "{r:?}");
    assert!(waited >= Duration::from_millis(400), "waited {waited:?}");
    assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    drop(held);
    assert!(hold_lock(&state).is_ok(), "released on drop");
}
