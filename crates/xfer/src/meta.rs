//! File identity and metadata: capture at backup, apply at restore.
//!
//! - [`Identity`] is the tuple receipts and the metadata fast path compare.
//! - xattrs (unix, at most 64 KiB per file) and macOS st_flags are captured. macOS ACLs, oversized
//!   xattrs and xattr read errors set `incomplete`, which makes `Receipt::deletable` refuse.
//! - Restore applies mode (setid stripped unless asked), xattrs, mtime and flags, best effort,
//!   returning a warning per failure.

use serde::{Deserialize, Serialize};
use std::fs::Metadata;
use std::path::Path;

pub const XATTR_MAX: usize = 64 << 10;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub nlink: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Hardlink,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Symlink => "symlink",
            Kind::Hardlink => "hardlink",
        }
    }
}

/// What `lstat` says about a path, normalised across platforms.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub id: Identity,
    pub mode: u32,
    /// None for FIFOs, sockets and devices (with the reason).
    pub kind: Result<Kind, &'static str>,
    pub alloc: u64,
    pub flags: u32,
}

fn ns(secs: i64, nsec: i64) -> i64 {
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

#[cfg(unix)]
pub fn stat_of(md: &Metadata) -> Stat {
    use std::os::unix::fs::MetadataExt;
    let ft = md.file_type();
    let kind = if ft.is_file() {
        Ok(Kind::File)
    } else if ft.is_dir() {
        Ok(Kind::Dir)
    } else if ft.is_symlink() {
        Ok(Kind::Symlink)
    } else {
        use std::os::unix::fs::FileTypeExt;
        Err(if ft.is_fifo() {
            "fifo"
        } else if ft.is_socket() {
            "socket"
        } else if ft.is_block_device() || ft.is_char_device() {
            "device"
        } else {
            "unknown file type"
        })
    };
    #[cfg(target_os = "macos")]
    let flags = {
        use std::os::macos::fs::MetadataExt as M;
        md.st_flags()
    };
    #[cfg(not(target_os = "macos"))]
    let flags = 0u32;
    Stat {
        id: Identity {
            dev: md.dev(),
            ino: md.ino(),
            size: md.size(),
            mtime_ns: ns(md.mtime(), md.mtime_nsec()),
            ctime_ns: ns(md.ctime(), md.ctime_nsec()),
            nlink: md.nlink(),
        },
        mode: md.mode() & 0o7777,
        kind,
        alloc: md.blocks().saturating_mul(512),
        flags,
    }
}

#[cfg(not(unix))]
pub fn stat_of(md: &Metadata) -> Stat {
    let ft = md.file_type();
    let kind = if ft.is_file() {
        Ok(Kind::File)
    } else if ft.is_dir() {
        Ok(Kind::Dir)
    } else if ft.is_symlink() {
        Ok(Kind::Symlink)
    } else {
        Err("unknown file type")
    };
    let mt = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let mode = if ft.is_dir() {
        0o755
    } else if md.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    Stat {
        id: Identity {
            dev: 0,
            ino: 0,
            size: md.len(),
            mtime_ns: mt,
            ctime_ns: mt,
            nlink: 1,
        },
        mode,
        kind,
        alloc: md.len(),
        flags: 0,
    }
}

/// Captured extended metadata. Names and values are hex in JSON so any bytes survive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extra {
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub incomplete: Option<String>,
}

/// Capture xattrs (and the macOS ACL check) for a path, without following a final symlink.
/// `file` is the open handle for regular files, so the capture matches the bytes read.
#[cfg(unix)]
pub fn capture(path: &Path, file: Option<&std::fs::File>) -> Extra {
    use std::os::unix::ffi::OsStrExt;
    let mut ex = Extra::default();
    if cfg!(feature = "broken-meta") {
        // the broken variant captures nothing (failing-first evidence only)
        return ex;
    }
    let names: Vec<std::ffi::OsString> = match file {
        Some(f) => {
            use xattr::FileExt;
            match f.list_xattr() {
                Ok(it) => it.collect(),
                Err(e) => return incomplete_unless_unsupported(ex, e),
            }
        }
        None => match xattr::list(path) {
            Ok(it) => it.collect(),
            Err(e) => return incomplete_unless_unsupported(ex, e),
        },
    };
    let mut total = 0usize;
    for n in names {
        let v = match file {
            Some(f) => {
                use xattr::FileExt;
                f.get_xattr(&n)
            }
            None => xattr::get(path, &n),
        };
        match v {
            Ok(Some(v)) => {
                total += n.len() + v.len();
                if total > XATTR_MAX {
                    ex.incomplete = Some(format!("xattrs exceed {} bytes", XATTR_MAX));
                    ex.xattrs.clear();
                    break;
                }
                ex.xattrs.push((n.as_bytes().to_vec(), v));
            }
            Ok(None) => {}
            Err(e) => {
                ex.incomplete = Some(format!("xattr read failed: {e}"));
                break;
            }
        }
    }
    #[cfg(target_os = "macos")]
    if ex.incomplete.is_none() && macos_has_acl(path) {
        ex.incomplete = Some("an ACL is present and is not captured".into());
    }
    ex.xattrs.sort();
    ex
}

#[cfg(unix)]
fn incomplete_unless_unsupported(mut ex: Extra, e: std::io::Error) -> Extra {
    if e.raw_os_error() == Some(libc::ENOTSUP) {
        return ex;
    }
    #[cfg(target_os = "linux")]
    if e.raw_os_error() == Some(libc::EOPNOTSUPP) {
        return ex;
    }
    ex.incomplete = Some(format!("xattr list failed: {e}"));
    ex
}

#[cfg(not(unix))]
pub fn capture(_path: &Path, _file: Option<&std::fs::File>) -> Extra {
    Extra::default()
}

#[cfg(target_os = "macos")]
mod acl {
    use std::os::raw::{c_char, c_int, c_void};
    pub const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    pub const ACL_FIRST_ENTRY: c_int = 0;
    extern "C" {
        pub fn acl_get_link_np(path: *const c_char, ty: c_int) -> *mut c_void;
        pub fn acl_get_entry(acl: *mut c_void, id: c_int, entry: *mut *mut c_void) -> c_int;
        pub fn acl_free(p: *mut c_void) -> c_int;
    }
}

/// True when the path carries an extended ACL with at least one entry.
#[cfg(target_os = "macos")]
pub fn macos_has_acl(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: c is a valid NUL-terminated path; the returned ACL is freed below.
    unsafe {
        let a = acl::acl_get_link_np(c.as_ptr(), acl::ACL_TYPE_EXTENDED);
        if a.is_null() {
            return false;
        }
        let mut e: *mut std::os::raw::c_void = std::ptr::null_mut();
        let has = acl::acl_get_entry(a, acl::ACL_FIRST_ENTRY, &mut e) == 0;
        acl::acl_free(a);
        has
    }
}

/// Set a path's mtime (and atime to the same value) without following a final symlink.
#[cfg(unix)]
pub fn set_mtime(path: &Path, mtime_ns: i64) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("path contains NUL"))?;
    let ts = libc::timespec {
        tv_sec: mtime_ns.div_euclid(1_000_000_000) as libc::time_t,
        tv_nsec: mtime_ns.rem_euclid(1_000_000_000) as _,
    };
    let times = [ts, ts];
    // SAFETY: valid C string and a two-element timespec array, as utimensat requires.
    let r = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
pub fn set_mtime(path: &Path, mtime_ns: i64) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if !md.is_file() {
        return Ok(());
    }
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns.max(0) as u64);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_modified(t)
}

/// Apply mode, xattrs, mtime and flags. Returns one warning per failure; never fails the restore.
pub fn apply(
    path: &Path,
    kind: Kind,
    mode: u32,
    mtime_ns: i64,
    xattrs: &[(Vec<u8>, Vec<u8>)],
    flags: u32,
    keep_setid: bool,
) -> Vec<String> {
    let mut w = Vec::new();
    let shown = path.display();
    if kind != Kind::Symlink {
        let mut m = mode & 0o7777;
        let strip = !keep_setid && !cfg!(feature = "broken-restore-guards");
        if strip && m & 0o6000 != 0 {
            w.push(format!("{shown}: setuid/setgid bits stripped"));
            m &= !0o6000;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)) {
                w.push(format!("{shown}: chmod: {e}"));
            }
        }
        #[cfg(not(unix))]
        {
            if m & 0o222 == 0 {
                if let Ok(md) = std::fs::metadata(path) {
                    let mut p = md.permissions();
                    p.set_readonly(true);
                    let _ = std::fs::set_permissions(path, p);
                }
            }
        }
    }
    #[cfg(unix)]
    for (n, v) in xattrs {
        use std::os::unix::ffi::OsStrExt;
        if let Err(e) = xattr::set(path, std::ffi::OsStr::from_bytes(n), v) {
            w.push(format!(
                "{shown}: xattr {}: {e}",
                String::from_utf8_lossy(n)
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = xattrs;
    if let Err(e) = set_mtime(path, mtime_ns) {
        w.push(format!("{shown}: mtime: {e}"));
    }
    #[cfg(target_os = "macos")]
    if flags != 0 {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
            // SAFETY: valid C string; lchflags does not follow a final symlink.
            if unsafe { libc::lchflags(c.as_ptr(), flags as _) } != 0 {
                w.push(format!(
                    "{shown}: flags: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = flags;
    w
}
