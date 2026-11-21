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

/// Outcome of a guarded removal.
#[derive(Debug, PartialEq, Eq)]
pub enum Unlinked {
    Done,
    /// Already gone (resume).
    Absent,
    /// The entry is not the one expected; left in place.
    Changed(Ident),
    /// Another entry was swapped in while the member was being removed. It was not removed and
    /// could not be put back (its name was taken), so it is left under this relative path.
    Displaced(String),
}

/// Stages of a removal a test can act at (feature `test-hooks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveStage {
    /// After the identity check, before the entry is moved to its quarantine name.
    BeforeQuarantine,
    /// After the move, before the moved entry is checked and unlinked.
    AfterQuarantine,
}

#[cfg(feature = "test-hooks")]
type RemoveHook = Box<dyn FnMut(RemoveStage, &str) -> io::Result<()>>;

#[cfg(feature = "test-hooks")]
thread_local! {
    /// Called with the member's relative path at each stage; an error aborts the removal.
    pub static REMOVE_HOOK: std::cell::RefCell<Option<RemoveHook>> =
        const { std::cell::RefCell::new(None) };
}

/// The error message a hook returns to simulate a crash at a removal stage.
pub const INJECTED_CRASH: &str = "pd-injected-crash";

#[cfg(feature = "test-hooks")]
fn hook(stage: RemoveStage, rel: &str) -> io::Result<()> {
    REMOVE_HOOK.with(|h| match h.borrow_mut().as_mut() {
        Some(f) => f(stage, rel),
        None => Ok(()),
    })
}

#[cfg(not(feature = "test-hooks"))]
#[inline]
fn hook(_stage: RemoveStage, _rel: &str) -> io::Result<()> {
    Ok(())
}

/// The quarantine name of `leaf`: fixed for a given leaf, so a resumed run finds an entry an
/// interrupted run moved and did not unlink.
pub fn quarantine_name(leaf: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(leaf.as_bytes());
    let hex: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!(".pd-rm-{hex}")
}

/// Rename within `d` without replacing an existing target. On a file system without no-replace
/// support (`EINVAL`, `ENOSYS`, `ENOTSUP`) it checks the target is absent, then renames.
fn rename_noreplace(d: &OwnedFd, from: &str, to: &str) -> io::Result<()> {
    match fs::renameat_with(
        d,
        OsStr::new(from),
        d,
        OsStr::new(to),
        fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(Errno::INVAL) | Err(Errno::NOSYS) | Err(Errno::NOTSUP) => {
            match fs::statat(d, OsStr::new(to), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => Err(e(Errno::EXIST)),
                Err(Errno::NOENT) => {
                    fs::renameat(d, OsStr::new(from), d, OsStr::new(to)).map_err(e)
                }
                Err(err) => Err(e(err)),
            }
        }
        Err(err) => Err(e(err)),
    }
}

fn rel_with_leaf(dir: &str, leaf: &str) -> String {
    if dir.is_empty() {
        leaf.to_string()
    } else {
        format!("{dir}/{leaf}")
    }
}

/// The only way the crate removes a member (I-1, r6b F1). It removes `rel` below `base` only
/// if it is still the entry with identity `expect` (strict, or loose when `loose`), and never
/// removes an entry swapped in at the last moment:
///
/// 1. lstat the leaf and compare it with `expect`;
/// 2. rename it, replacing nothing, to its quarantine name in the same directory;
/// 3. lstat the quarantined entry: it must be the inode of step 1 with the same kind, size and
///    mtime (a rename moves ctime, so ctime is not compared here);
/// 4. unlink it. On a mismatch the entry is renamed back; if its name was taken meanwhile, it
///    stays under the quarantine name and is reported, never removed.
///
/// A run interrupted between 2 and 4 leaves the quarantine name. A resumed run finds it when
/// the leaf is absent and finishes (same identity) or leaves it (anything else). Never follows
/// a symlink: a symlink member is removed itself.
pub fn remove_member(
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
    let q = quarantine_name(leaf);
    let st = match fs::statat(&d, OsStr::new(leaf), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::NOENT) => {
            // Resume: an interrupted run may have moved it to the quarantine name.
            return match fs::statat(&d, OsStr::new(&q), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(qst) => {
                    let qid = ident_of(&qst);
                    if qid.same_loose(expect) && qid.kind != 'd' {
                        fs::unlinkat(&d, OsStr::new(&q), AtFlags::empty()).map_err(e)?;
                        Ok(Unlinked::Done)
                    } else {
                        Ok(Unlinked::Displaced(rel_with_leaf(dir, &q)))
                    }
                }
                Err(Errno::NOENT) => Ok(Unlinked::Absent),
                Err(err) => Err(e(err)),
            };
        }
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
    hook(RemoveStage::BeforeQuarantine, rel)?;
    match rename_noreplace(&d, leaf, &q) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Unlinked::Absent),
        Err(err) => return Err(err),
    }
    hook(RemoveStage::AfterQuarantine, rel)?;
    let moved = ident_of(&fs::statat(&d, OsStr::new(&q), AtFlags::SYMLINK_NOFOLLOW).map_err(e)?);
    if moved.same_loose(&now) && moved.kind != 'd' {
        fs::unlinkat(&d, OsStr::new(&q), AtFlags::empty()).map_err(e)?;
        return Ok(Unlinked::Done);
    }
    // Not the entry that was checked: put it back, never over something else.
    match rename_noreplace(&d, &q, leaf) {
        Ok(()) => Ok(Unlinked::Changed(moved)),
        Err(_) => Ok(Unlinked::Displaced(rel_with_leaf(dir, &q))),
    }
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
