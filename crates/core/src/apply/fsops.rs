//! The only file-system mutations of the crate: member-wise unlink, removal of empty
//! directories, and no-replace publication for restore. Every path is walked from `/` one
//! component at a time with `O_NOFOLLOW | O_DIRECTORY`, so a directory swapped for a symlink
//! mid-run cannot redirect an unlink or a restore elsewhere (NEG-13, I-6).

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

/// Identity of an entry as stat saw it (lstat semantics: a symlink is itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ident {
    /// `f` regular, `l` symlink, `d` directory, `o` other.
    pub kind: char,
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_s: i64,
    pub mtime_ns: i64,
    pub ctime_s: i64,
    pub ctime_ns: i64,
    pub mode: u32,
}

impl Ident {
    /// Same entry and same content-defining stat fields, ctime included.
    pub fn same_strict(&self, o: &Ident) -> bool {
        self == o
    }

    /// Same entry and content fields, ctime ignored (resume after hard-link ctime drift, N5).
    pub fn same_loose(&self, o: &Ident) -> bool {
        self.kind == o.kind
            && self.dev == o.dev
            && self.ino == o.ino
            && self.len == o.len
            && self.mtime_s == o.mtime_s
            && self.mtime_ns == o.mtime_ns
    }
}

fn kind_of(mode: u32) -> char {
    match mode & 0o170000 {
        0o100000 => 'f',
        0o120000 => 'l',
        0o040000 => 'd',
        _ => 'o',
    }
}

pub fn ident_of(st: &fs::Stat) -> Ident {
    #[allow(clippy::unnecessary_cast)]
    Ident {
        kind: kind_of(st.st_mode as u32),
        dev: st.st_dev as u64,
        ino: st.st_ino as u64,
        len: st.st_size as u64,
        mtime_s: st.st_mtime as i64,
        mtime_ns: st.st_mtime_nsec as i64,
        ctime_s: st.st_ctime as i64,
        ctime_ns: st.st_ctime_nsec as i64,
        mode: (st.st_mode as u32) & 0o7777,
    }
}

fn e(err: Errno) -> io::Error {
    io::Error::from_raw_os_error(err.raw_os_error())
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Open an absolute directory path, refusing a symlink at every component.
pub fn open_dir_abs(path: &Path) -> io::Result<OwnedFd> {
    if !path.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not absolute"));
    }
    let mut fd = fs::open("/", dir_flags(), Mode::empty()).map_err(e)?;
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(name) => {
                fd = fs::openat(&fd, name, dir_flags(), Mode::empty()).map_err(e)?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path is not plain",
                ))
            }
        }
    }
    Ok(fd)
}

/// Open the directory `rel` (`/`-separated, plain) below `base`, refusing symlinks.
pub fn open_dir_rel(base: &OwnedFd, rel: &str) -> io::Result<OwnedFd> {
    let mut fd = base.try_clone()?;
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        fd = fs::openat(&fd, OsStr::new(c), dir_flags(), Mode::empty()).map_err(e)?;
    }
    Ok(fd)
}

fn split_rel(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

/// lstat `rel` below `base` without following any symlink on the way.
pub fn lstat_rel(base: &OwnedFd, rel: &str) -> io::Result<Ident> {
    let (dir, leaf) = split_rel(rel);
    let d = open_dir_rel(base, dir)?;
    let st = fs::statat(&d, OsStr::new(leaf), AtFlags::SYMLINK_NOFOLLOW).map_err(e)?;
    Ok(ident_of(&st))
}

/// Read a symlink's target bytes below `base`.
pub fn readlink_rel(base: &OwnedFd, rel: &str) -> io::Result<Vec<u8>> {
    let (dir, leaf) = split_rel(rel);
    let d = open_dir_rel(base, dir)?;
    let t = fs::readlinkat(&d, OsStr::new(leaf), Vec::new()).map_err(e)?;
    Ok(t.into_bytes())
}

/// Open a regular file below `base` for reading (no symlink anywhere, non-blocking so a FIFO
/// cannot hang us); returns the file and its identity from `fstat`.
pub fn open_file_rel(base: &OwnedFd, rel: &str) -> io::Result<(std::fs::File, Ident)> {
    let (dir, leaf) = split_rel(rel);
    let d = open_dir_rel(base, dir)?;
    let fd = fs::openat(
        &d,
        OsStr::new(leaf),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(e)?;
    let st = fs::fstat(&fd).map_err(e)?;
    let id = ident_of(&st);
    if id.kind != 'f' {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok((std::fs::File::from(fd), id))
}

/// Outcome of a guarded unlink.
#[derive(Debug, PartialEq, Eq)]
pub enum Unlinked {
    Done,
    /// Already gone (resume).
    Absent,
    /// The entry is not the one expected; left in place.
    Changed(Ident),
}

/// Unlink `rel` below `base` only if it still has identity `expect` (strict, or loose when
/// `loose`). Never follows a symlink: a symlink member is unlinked itself.
pub fn unlink_guarded(
    base: &OwnedFd,
    rel: &str,
    expect: &Ident,
    loose: bool,
) -> io::Result<Unlinked> {
    let (dir, leaf) = split_rel(rel);
    let d = match open_dir_rel(base, dir) {
        Ok(d) => d,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Unlinked::Absent),
        Err(err) => return Err(err),
    };
    let st = match fs::statat(&d, OsStr::new(leaf), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::NOENT) => return Ok(Unlinked::Absent),
        Err(err) => return Err(e(err)),
    };
    let now = ident_of(&st);
    let same = if loose {
        now.same_loose(expect)
    } else {
        now.same_strict(expect)
    };
    if !same || now.kind == 'd' {
        return Ok(Unlinked::Changed(now));
    }
    fs::unlinkat(&d, OsStr::new(leaf), AtFlags::empty()).map_err(e)?;
    Ok(Unlinked::Done)
}

/// Remove the directory `rel` below `base` if it is empty; any other outcome leaves it.
pub fn rmdir_if_empty(base: &OwnedFd, rel: &str) -> bool {
    let (dir, leaf) = split_rel(rel);
    if leaf.is_empty() {
        return false;
    }
    let Ok(d) = open_dir_rel(base, dir) else {
        return false;
    };
    fs::unlinkat(&d, OsStr::new(leaf), AtFlags::REMOVEDIR).is_ok()
}

/// Create the directories of `rel` below `base` (no symlink followed), returning the leaf dir.
pub fn mkdirs_rel(base: &OwnedFd, rel: &str, mode: u32) -> io::Result<OwnedFd> {
    let mut fd = base.try_clone()?;
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        match fs::mkdirat(&fd, OsStr::new(c), Mode::from_raw_mode(mode as _)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(err) => return Err(e(err)),
        }
        fd = fs::openat(&fd, OsStr::new(c), dir_flags(), Mode::empty()).map_err(e)?;
    }
    Ok(fd)
}

/// Outcome of a no-replace publication.
#[derive(Debug, PartialEq, Eq)]
pub enum Published {
    Created,
    Exists,
}

/// Write `data_writer` output to a fresh temp file in `dir`, fsync it, then link it to `leaf`
/// without replacing anything (`linkat` fails with EEXIST), and remove the temp name. The
/// temp file is removed on every failure path.
pub fn publish_file<F>(dir: &OwnedFd, leaf: &str, mode: u32, write: F) -> io::Result<Published>
where
    F: FnOnce(&mut std::fs::File) -> io::Result<()>,
{
    use std::io::Write;
    let tmp = format!(".{leaf}.pd-restore-{}", std::process::id());
    let fd = fs::openat(
        dir,
        OsStr::new(&tmp),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(e)?;
    let mut f = std::fs::File::from(fd);
    let res = (|| {
        write(&mut f)?;
        f.flush()?;
        f.sync_all()?;
        fs::fchmod(f.as_fd(), Mode::from_raw_mode((mode & 0o7777) as _)).map_err(e)?;
        match fs::linkat(
            dir,
            OsStr::new(&tmp),
            dir,
            OsStr::new(leaf),
            AtFlags::empty(),
        ) {
            Ok(()) => Ok(Published::Created),
            Err(Errno::EXIST) => Ok(Published::Exists),
            Err(err) => Err(e(err)),
        }
    })();
    let _ = fs::unlinkat(dir, OsStr::new(&tmp), AtFlags::empty());
    res
}

/// Create a symlink without replacing anything.
pub fn publish_symlink(dir: &OwnedFd, leaf: &str, target: &[u8]) -> io::Result<Published> {
    match fs::symlinkat(OsStr::from_bytes(target), dir, OsStr::new(leaf)) {
        Ok(()) => Ok(Published::Created),
        Err(Errno::EXIST) => Ok(Published::Exists),
        Err(err) => Err(e(err)),
    }
}

/// Hard-link an existing restored file to a new name (no replace).
pub fn publish_link(
    dir_from: &OwnedFd,
    from: &str,
    dir_to: &OwnedFd,
    to: &str,
) -> io::Result<Published> {
    match fs::linkat(
        dir_from,
        OsStr::new(from),
        dir_to,
        OsStr::new(to),
        AtFlags::empty(),
    ) {
        Ok(()) => Ok(Published::Created),
        Err(Errno::EXIST) => Ok(Published::Exists),
        Err(err) => Err(e(err)),
    }
}

/// Set a restored entry's mtime (nanoseconds), not following a symlink.
pub fn set_mtime(dir: &OwnedFd, leaf: &str, s: i64, ns: i64) -> io::Result<()> {
    let ts = fs::Timestamps {
        last_access: fs::Timespec {
            tv_sec: s as _,
            tv_nsec: ns as _,
        },
        last_modification: fs::Timespec {
            tv_sec: s as _,
            tv_nsec: ns as _,
        },
    };
    fs::utimensat(dir, OsStr::new(leaf), &ts, AtFlags::SYMLINK_NOFOLLOW).map_err(e)
}

/// The leaf name and parent of a `/`-separated relative path.
pub fn parent_leaf(rel: &str) -> (&str, &str) {
    split_rel(rel)
}
