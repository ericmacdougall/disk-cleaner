#![allow(dead_code)]
//! Shared helpers for the integration tests.

/// The rclone binary for tests that need one. `XFER_TEST_RCLONE` names it. When
/// `XFER_REQUIRE_RCLONE=1` (set by the t2-dev runner) a missing binary fails the test instead of
/// skipping it, so a mis-set environment can never pass silently.
pub fn rclone() -> Option<String> {
    match std::env::var("XFER_TEST_RCLONE") {
        Ok(b) if !b.is_empty() && std::path::Path::new(&b).exists() => Some(b),
        _ => {
            if std::env::var("XFER_REQUIRE_RCLONE").as_deref() == Ok("1") {
                panic!("XFER_REQUIRE_RCLONE=1 but XFER_TEST_RCLONE does not name an rclone binary");
            }
            eprintln!("skipping: XFER_TEST_RCLONE is not set");
            None
        }
    }
}

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
