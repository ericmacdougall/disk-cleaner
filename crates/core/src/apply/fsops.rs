//! Directory-fd file operations: the only place apply touches the file system.
//!
//! Walks and deletions open every component from a parent fd with `O_NOFOLLOW | O_DIRECTORY`,
//! `fstat` it and compare (dev, ino) with what the path-level `statat` said, so a component
//! swapped for a symlink or another directory between the two is never entered. The mutating
//! functions here ([`rename_noreplace`], [`unlink_child`], [`delete_tree`]) each take a
//! [`Proof`](super::journal::Proof) from the journal: no fsynced intent, no syscall. This file is
//! also the only one of the apply module that may name a mutating call (a lint in `tests` fails
//! on any other).

use super::journal::{Proof, Quarantine, Remove, Restore};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// Deepest directory nesting a walk or deletion enters.
const MAX_DEPTH: usize = 4096;

/// A file's identity on its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// What an entry is, from `lstat` (never followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

/// The `lstat` facts apply uses.
#[derive(Debug, Clone, Copy)]
pub struct Meta {
    pub id: FileId,
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub nlink: u64,
    pub size: u64,
    /// Allocated bytes (`st_blocks` * 512).
    pub alloc: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

#[allow(clippy::unnecessary_cast)]
fn meta_of(st: &rustix::fs::Stat) -> Meta {
    let kind = match FileType::from_raw_mode(st.st_mode as _) {
        FileType::Directory => Kind::Dir,
        FileType::RegularFile => Kind::File,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    Meta {
        id: FileId {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        },
        kind,
        mode: st.st_mode as u32,
        uid: st.st_uid as u32,
        nlink: st.st_nlink as u64,
        size: st.st_size as u64,
        alloc: (st.st_blocks as u64).saturating_mul(512),
        mtime: (st.st_mtime as i64, st.st_mtime_nsec as i64),
        ctime: (st.st_ctime as i64, st.st_ctime_nsec as i64),
    }
}

fn io_err(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

/// An open directory, with its identity read from the fd.
pub struct Dir {
    fd: OwnedFd,
    id: FileId,
}

impl Dir {
    /// Open a directory by path (intermediate components may be links: the caller checks the
    /// identity against the journal or the plan).
    pub fn open_path(path: &Path) -> io::Result<Dir> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_err)?;
        Dir::from_fd(fd)
    }

    fn from_fd(fd: OwnedFd) -> io::Result<Dir> {
        let st = rustix::fs::fstat(&fd).map_err(io_err)?;
        Ok(Dir {
            id: meta_of(&st).id,
            fd,
        })
    }

    /// The directory's (dev, ino), read from the open fd.
    pub fn id(&self) -> FileId {
        self.id
    }

    /// Open a child directory, never through a link.
    pub fn open_child(&self, name: &OsStr) -> io::Result<Dir> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_err)?;
        Dir::from_fd(fd)
    }

    /// `lstat` of a child.
    pub fn stat_child(&self, name: &OsStr) -> io::Result<Meta> {
        rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|st| meta_of(&st))
            .map_err(io_err)
    }

    /// The names inside, sorted by bytes (no `.` or `..`).
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        let dir = rustix::fs::Dir::read_from(&self.fd).map_err(io_err)?;
        let mut out = Vec::new();
        for entry in dir {
            let entry = entry.map_err(io_err)?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                out.push(OsString::from_vec(name.to_vec()));
            }
        }
        out.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        Ok(out)
    }

    /// fsync the directory (after a rename into or out of it).
    pub fn sync(&self) -> io::Result<()> {
        rustix::fs::fsync(&self.fd).map_err(io_err)
    }
}

/// Why a no-replace rename did not happen.
#[derive(Debug)]
pub enum RenameError {
    /// The target name exists.
    Exists,
    /// Source and target are on different file systems.
    CrossDevice,
    /// The file system or kernel has no no-replace rename (EINVAL, ENOSYS, ENOTSUP).
    Unsupported,
    Other(io::Error),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameError::Exists => write!(f, "target exists"),
            RenameError::CrossDevice => write!(f, "another file system"),
            RenameError::Unsupported => write!(f, "no-replace rename unsupported here"),
            RenameError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Kinds of intent that authorise a rename.
pub trait RenameKind: super::journal::Kind {}
impl RenameKind for Quarantine {}
impl RenameKind for Restore {}

/// Rename `from` in `from_dir` to `to` in `to_dir`, never replacing an existing target
/// (`renameat2(RENAME_NOREPLACE)`, `renameatx_np(RENAME_EXCL)`). The proof is the fsynced intent.
pub fn rename_noreplace<K: RenameKind>(
    _proof: &Proof<K>,
    from_dir: &Dir,
    from: &OsStr,
    to_dir: &Dir,
    to: &OsStr,
) -> Result<(), RenameError> {
    #[cfg(test)]
    {
        super::hooks::syscall("rename");
        if let Some(e) = super::hooks::forced_rename_error() {
            return Err(map_rename_errno(e));
        }
    }
    rustix::fs::renameat_with(&from_dir.fd, from, &to_dir.fd, to, RenameFlags::NOREPLACE)
        .map_err(map_rename_errno)
}

/// What a failed no-replace rename means for the item.
pub(crate) fn map_rename_errno(e: Errno) -> RenameError {
    match e {
        Errno::EXIST | Errno::NOTEMPTY => RenameError::Exists,
        Errno::XDEV => RenameError::CrossDevice,
        Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP => RenameError::Unsupported,
        other => RenameError::Other(io_err(other)),
    }
}

/// Remove one entry of `dir` (`unlinkat`, with `AT_REMOVEDIR` for a directory).
pub fn unlink_child(
    _proof: &Proof<Remove>,
    dir: &Dir,
    name: &OsStr,
    is_dir: bool,
) -> Result<(), Errno> {
    #[cfg(test)]
    super::hooks::syscall("unlink");
    let flags = if is_dir {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    rustix::fs::unlinkat(&dir.fd, name, flags)
}

/// Why a walk did not complete.
#[derive(Debug)]
pub enum WalkStop {
    /// An entry on another device (a mount point) at this path below the root.
    CrossDevice(String),
    /// A directory whose identity is one that must never be entered (a protected root's).
    Forbidden(String),
    /// A component changed between `lstat` and open (swapped for a link or another directory).
    Changed(String),
    /// Nesting beyond the walk's bound.
    TooDeep,
    Io(String, io::Error),
}

impl std::fmt::Display for WalkStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkStop::CrossDevice(p) => write!(f, "another device at {p}"),
            WalkStop::Forbidden(p) => write!(f, "a protected directory at {p}"),
            WalkStop::Changed(p) => write!(f, "{p} changed while it was opened"),
            WalkStop::TooDeep => write!(f, "nested too deeply"),
            WalkStop::Io(p, e) => write!(f, "{p}: {e}"),
        }
    }
}

/// What the fd walk found below a directory.
#[derive(Debug, Clone)]
pub struct Inventory {
    /// SHA-256 (hex) over every entry: relative path, type, size, mtime, ctime, dev, ino, mode,
    /// owner, link count and allocated bytes.
    pub digest: String,
    pub entries: u64,
    /// Identity of every entry (the root included).
    pub ids: HashSet<FileId>,
    /// Allocated bytes, once per inode, leaving out files that have links outside the tree.
    pub bytes: u64,
}

fn lossy(rel: &[u8]) -> String {
    String::from_utf8_lossy(rel).into_owned()
}

struct Walk<'a> {
    dev: u64,
    forbidden: &'a HashSet<FileId>,
    hasher: Sha256,
    entries: u64,
    ids: HashSet<FileId>,
    dir_bytes: u64,
    /// inode -> (allocated bytes, link count, links seen in the tree)
    files: HashMap<FileId, (u64, u64, u64)>,
}

impl Walk<'_> {
    fn note(&mut self, rel: &[u8], m: &Meta) {
        self.entries += 1;
        self.ids.insert(m.id);
        let kind = match m.kind {
            Kind::Dir => b'd',
            Kind::File => b'f',
            Kind::Symlink => b'l',
            Kind::Other => b'o',
        };
        self.hasher.update(rel);
        self.hasher.update([0, kind, 0]);
        let line = format!(
            "{} {}.{} {}.{} {} {} {:o} {} {} {}\n",
            m.size,
            m.mtime.0,
            m.mtime.1,
            m.ctime.0,
            m.ctime.1,
            m.id.dev,
            m.id.ino,
            m.mode,
            m.uid,
            m.nlink,
            m.alloc
        );
        self.hasher.update(line.as_bytes());
        if m.kind == Kind::Dir {
            self.dir_bytes = self.dir_bytes.saturating_add(m.alloc);
        } else {
            let e = self.files.entry(m.id).or_insert((m.alloc, m.nlink, 0));
            e.2 += 1;
        }
    }

    fn visit(&mut self, dir: &Dir, rel: &mut Vec<u8>, depth: usize) -> Result<(), WalkStop> {
        if depth > MAX_DEPTH {
            return Err(WalkStop::TooDeep);
        }
        let names = dir.names().map_err(|e| WalkStop::Io(lossy(rel), e))?;
        for name in names {
            let mark = rel.len();
            if mark > 0 {
                rel.push(b'/');
            }
            rel.extend_from_slice(name.as_bytes());
            let m = dir
                .stat_child(&name)
                .map_err(|e| WalkStop::Io(lossy(rel), e))?;
            if m.kind == Kind::Dir {
                if m.id.dev != self.dev {
                    return Err(WalkStop::CrossDevice(lossy(rel)));
                }
                if self.forbidden.contains(&m.id) {
                    return Err(WalkStop::Forbidden(lossy(rel)));
                }
                self.note(rel, &m);
                let child = dir
                    .open_child(&name)
                    .map_err(|e| WalkStop::Io(lossy(rel), e))?;
                if child.id() != m.id {
                    return Err(WalkStop::Changed(lossy(rel)));
                }
                self.visit(&child, rel, depth + 1)?;
            } else {
                self.note(rel, &m);
            }
            rel.truncate(mark);
        }
        Ok(())
    }
}

/// Walk everything below `root` with directory fds and describe it. `root_meta` is the `lstat` of
/// the root as seen from its parent: the fd's own identity must equal it. Fails on another
/// device or on a directory whose identity is in `forbidden`.
pub fn inventory(root: &Dir, forbidden: &HashSet<FileId>) -> Result<Inventory, WalkStop> {
    let mut w = Walk {
        dev: root.id().dev,
        forbidden,
        hasher: Sha256::new(),
        entries: 0,
        ids: HashSet::new(),
        dir_bytes: 0,
        files: HashMap::new(),
    };
    let st = rustix::fs::fstat(&root.fd).map_err(|e| WalkStop::Io(".".into(), io_err(e)))?;
    w.note(b".", &meta_of(&st));
    w.visit(root, &mut Vec::new(), 0)?;
    let mut bytes = w.dir_bytes;
    for (alloc, nlink, seen) in w.files.values() {
        if seen >= nlink {
            bytes = bytes.saturating_add(*alloc);
        }
    }
    Ok(Inventory {
        digest: hex(&w.hasher.finalize()),
        entries: w.entries,
        ids: w.ids,
        bytes,
    })
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Why a deletion stopped.
#[derive(Debug)]
pub enum DeleteStop {
    /// An entry that is not in the recorded inventory (something new, or replaced).
    New(String),
    /// A component changed identity while it was opened, or sits on another device.
    Changed(String),
    /// A directory was not empty when it was removed (a file appeared): never forced.
    NotEmpty(String),
    TooDeep,
    Io(String, io::Error),
}

impl std::fmt::Display for DeleteStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeleteStop::New(p) => write!(f, "{p} is not in the recorded inventory"),
            DeleteStop::Changed(p) => write!(f, "{p} changed identity"),
            DeleteStop::NotEmpty(p) => write!(f, "{p} was not empty"),
            DeleteStop::TooDeep => write!(f, "nested too deeply"),
            DeleteStop::Io(p, e) => write!(f, "{p}: {e}"),
        }
    }
}

fn purge(
    proof: &Proof<Remove>,
    dir: &Dir,
    ids: Option<&HashSet<FileId>>,
    dev: u64,
    rel: &mut Vec<u8>,
    depth: usize,
) -> Result<(), DeleteStop> {
    if depth > MAX_DEPTH {
        return Err(DeleteStop::TooDeep);
    }
    let names = dir.names().map_err(|e| DeleteStop::Io(lossy(rel), e))?;
    #[cfg(test)]
    super::hooks::before_purge(rel);
    for name in names {
        let mark = rel.len();
        if mark > 0 {
            rel.push(b'/');
        }
        rel.extend_from_slice(name.as_bytes());
        let m = match dir.stat_child(&name) {
            Ok(m) => m,
            // Gone already (an earlier, interrupted pass removed it).
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                rel.truncate(mark);
                continue;
            }
            Err(e) => return Err(DeleteStop::Io(lossy(rel), e)),
        };
        if let Some(ids) = ids {
            if !ids.contains(&m.id) {
                return Err(DeleteStop::New(lossy(rel)));
            }
        }
        if m.kind == Kind::Dir {
            if m.id.dev != dev {
                return Err(DeleteStop::Changed(lossy(rel)));
            }
            let child = dir
                .open_child(&name)
                .map_err(|e| DeleteStop::Io(lossy(rel), e))?;
            if child.id() != m.id {
                return Err(DeleteStop::Changed(lossy(rel)));
            }
            purge(proof, &child, ids, dev, rel, depth + 1)?;
            unlink_child(proof, dir, &name, true).map_err(|e| match e {
                Errno::NOTEMPTY => DeleteStop::NotEmpty(lossy(rel)),
                other => DeleteStop::Io(lossy(rel), io_err(other)),
            })?;
        } else {
            unlink_child(proof, dir, &name, false)
                .map_err(|e| DeleteStop::Io(lossy(rel), io_err(e)))?;
        }
        rel.truncate(mark);
    }
    Ok(())
}

/// Delete the tree `name` in `parent`, bottom-up, relative to verified directory fds. The root's
/// identity must be `root_id`. With `ids` (the recorded inventory) an entry not in it stops the
/// deletion; without it (resume after a crash) only identity, device and link checks apply.
/// Never forced: a non-empty directory stops it.
pub fn delete_tree(
    proof: &Proof<Remove>,
    parent: &Dir,
    name: &OsStr,
    root_id: FileId,
    ids: Option<&HashSet<FileId>>,
) -> Result<(), DeleteStop> {
    let root = parent
        .open_child(name)
        .map_err(|e| DeleteStop::Io(lossy(name.as_bytes()), e))?;
    if root.id() != root_id {
        return Err(DeleteStop::Changed(lossy(name.as_bytes())));
    }
    purge(proof, &root, ids, root.id().dev, &mut Vec::new(), 0)?;
    drop(root);
    unlink_child(proof, parent, name, true).map_err(|e| match e {
        Errno::NOTEMPTY => DeleteStop::NotEmpty(lossy(name.as_bytes())),
        other => DeleteStop::Io(lossy(name.as_bytes()), io_err(other)),
    })?;
    parent
        .sync()
        .map_err(|e| DeleteStop::Io(lossy(name.as_bytes()), e))
}
