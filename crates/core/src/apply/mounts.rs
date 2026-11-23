//! The mount table (class C): read once per run and again at each item's last check. An item
//! with any mount point at or below its path is skipped, which covers same-filesystem bind
//! mounts (they share `st_dev`, so a device comparison cannot see them).

use std::io;
use std::path::{Path, PathBuf};

/// The mount points of the host, as canonical paths.
pub trait MountTable {
    fn mount_points(&self) -> io::Result<Vec<PathBuf>>;
}

/// The running host's mount table: `/proc/self/mountinfo` on Linux, `getmntinfo` on macOS.
pub struct HostMounts;

impl MountTable for HostMounts {
    fn mount_points(&self) -> io::Result<Vec<PathBuf>> {
        host_mount_points()
    }
}

/// Decode the octal escapes (`\040` for a space, `\011`, `\012`, `\134`) of a mountinfo field.
pub fn decode_octal(field: &str) -> Vec<u8> {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = u32::from(b[i + 1] - b'0') * 64
                + u32::from(b[i + 2] - b'0') * 8
                + u32::from(b[i + 3] - b'0');
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Mount points from the text of `/proc/self/mountinfo` (field 5 of each line, decoded).
pub fn parse_mountinfo(text: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    text.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|f| PathBuf::from(std::ffi::OsString::from_vec(decode_octal(f))))
        .collect()
}

/// The first mount point at or below `path` (the path itself counts).
pub fn at_or_below<'a>(mounts: &'a [PathBuf], path: &Path) -> Option<&'a PathBuf> {
    mounts.iter().find(|m| m.starts_with(path))
}

#[cfg(target_os = "linux")]
fn host_mount_points() -> io::Result<Vec<PathBuf>> {
    let bytes = std::fs::read("/proc/self/mountinfo")?;
    Ok(parse_mountinfo(&String::from_utf8_lossy(&bytes)))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn host_mount_points() -> io::Result<Vec<PathBuf>> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo stores a pointer to a libc-owned array and returns its length; the
    // array is read before any other call that could reuse it and is never freed by us.
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as isize {
        // SAFETY: `buf` points to `n` initialised `statfs` records.
        let fs = unsafe { &*buf.offset(i) };
        // SAFETY: `f_mntonname` is a NUL-terminated C string inside the record.
        let name = unsafe { CStr::from_ptr(fs.f_mntonname.as_ptr()) };
        out.push(PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes())));
    }
    Ok(out)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn host_mount_points() -> io::Result<Vec<PathBuf>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no mount table reader for this platform",
    ))
}
