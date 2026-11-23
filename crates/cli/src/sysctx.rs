//! The one place the command line asks the operating system who is running it (the effective
//! user id and the passwd entry's home directory) and whether a pid still exists (a signal-0
//! probe). The core crate forbids unsafe code, so these libc calls live here, behind safe
//! functions, and their results are passed to the core as plain values (see
//! `disk_cleaner_core::ScanContext` and `disk_cleaner_core::procs`).

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

/// Signal-0 re-probe of a pid (it sends nothing): `ESRCH` means it is gone, success or `EPERM`
/// that it exists.
#[cfg(unix)]
pub fn probe(pid: u32) -> disk_cleaner_core::Probe {
    use disk_cleaner_core::Probe;
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return Probe::Unknown;
    };
    if pid <= 0 {
        return Probe::Unknown;
    }
    // SAFETY: signal 0 sends no signal; the call only checks that the pid exists and may be
    // signalled. It has no memory effects.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return Probe::Exists;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Probe::Gone,
        Some(libc::EPERM) => Probe::Exists,
        _ => Probe::Unknown,
    }
}

#[cfg(not(unix))]
pub fn probe(_pid: u32) -> disk_cleaner_core::Probe {
    disk_cleaner_core::Probe::Unknown
}
