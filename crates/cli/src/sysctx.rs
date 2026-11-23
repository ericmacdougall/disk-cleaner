//! The one place the command line asks the operating system who is running it: the effective
//! user id and the passwd entry's home directory. The core crate forbids unsafe code, so these
//! two libc calls live here, behind safe functions, and their results are passed to the core as
//! plain values (see `disk_cleaner_core::ScanContext`).

use std::path::PathBuf;

/// Effective user id of this process (`None` where the platform has none).
#[cfg(unix)]
pub fn euid() -> Option<u32> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    Some(unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
pub fn euid() -> Option<u32> {
    None
}

/// The home directory the passwd database gives for `uid`.
#[cfg(unix)]
pub fn passwd_home(uid: u32) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: an all-zero `passwd` is a valid value for the out parameter (null pointers).
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: the buffer, its length, the out struct and the result pointer are valid for the
    // call; getpwuid_r writes only inside them.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: pw_dir points at a NUL-terminated string inside `buf`, which is still alive.
    let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
    if dir.to_bytes().is_empty() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}

#[cfg(not(unix))]
pub fn passwd_home(_uid: u32) -> Option<PathBuf> {
    None
}
