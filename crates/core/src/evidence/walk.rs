//! The one agent-evidence walker (goodqa#16711 gate F1, failure class U). Every directory
//! listing, entry type and metadata read that feeds agent evidence goes through this file;
//! `evidence.rs` touches the filesystem for a walk nowhere else, and a source lint in the tests
//! (`walk_choke_point_lint`) fails if it does. Nothing here drops an error: a listing that
//! fails, an entry that cannot be read, a type or metadata read that fails and a walk that
//! stops at its bound each come back as a typed [`WalkError`]. A path that does not exist is
//! not an error (callers probe absent paths all the time); anything else that stops a read is.
//!
//! [`Walker`] records every error it meets itself, so a walk through it cannot lose one; the
//! plan turns each into an `agent-dir-unreadable` gap of unknown age, counted as recent. The
//! raw functions ([`list`], [`stat`]) hand the error to a caller that has a better class for it
//! (the Claude project-name walk calls a listing error `transcript-slug-incomplete`, aged by its
//! transcript; git admin files that cannot be read count as active now).
//!
//! Symbolic links are followed (an entry's kind is its target's), so a transcript or a
//! directory reached through a link is seen; every walk is bounded in depth and entries, so a
//! link loop ends at the bound (and a bound reached is an error, never a silent stop).

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// What an entry is, after following a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Dir,
    File,
    /// A device, socket, FIFO, or a link to nothing.
    Other,
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: Kind,
}

impl Entry {
    pub fn name_lossy(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }
}

/// What a metadata read found.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Meta {
    pub kind: Kind,
    pub len: u64,
    /// Seconds since the epoch.
    pub mtime: i64,
    /// Seconds since the epoch (Unix; the mtime elsewhere).
    pub ctime: i64,
}

/// Why a walk could not see something. Each one may hide agent data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WalkError {
    /// The directory could not be listed.
    List(PathBuf),
    /// An entry of this directory could not be read while listing it.
    Entry(PathBuf),
    /// The type of this entry could not be read.
    Type(PathBuf),
    /// The metadata of this path (or its modification time) could not be read.
    Meta(PathBuf),
    /// The walk stopped at its bound here; what lies beyond was not looked at.
    Limit(PathBuf),
}

impl WalkError {
    pub fn path(&self) -> &Path {
        match self {
            WalkError::List(p)
            | WalkError::Entry(p)
            | WalkError::Type(p)
            | WalkError::Meta(p)
            | WalkError::Limit(p) => p,
        }
    }

    /// In words, for the plan's notes (path only, never content).
    pub fn describe(&self) -> String {
        let what = match self {
            WalkError::List(_) => "could not be listed",
            WalkError::Entry(_) => "has an entry that could not be read",
            WalkError::Type(_) => "type could not be read",
            WalkError::Meta(_) => "metadata could not be read",
            WalkError::Limit(_) => "walk limit reached",
        };
        format!("{}: {what}", self.path().display())
    }
}

fn secs(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    let t = t.ok()?;
    Some(match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        // Before the epoch: still a time, an old one.
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    })
}

fn kind_of(ft: std::fs::FileType) -> Kind {
    if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

/// The metadata of `path`, following a link. `Ok(None)`: it does not exist (or is a link to
/// nothing). `Err`: it exists, or may, but could not be read (or has no modification time).
pub(crate) fn stat(path: &Path) -> Result<Option<Meta>, WalkError> {
    let md = match std::fs::metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(WalkError::Meta(path.to_path_buf())),
    };
    let mtime = secs(md.modified()).ok_or_else(|| WalkError::Meta(path.to_path_buf()))?;
    #[cfg(unix)]
    let ctime = {
        use std::os::unix::fs::MetadataExt;
        md.ctime()
    };
    #[cfg(not(unix))]
    let ctime = mtime;
    Ok(Some(Meta {
        kind: kind_of(md.file_type()),
        len: md.len(),
        mtime,
        ctime,
    }))
}

/// Like [`stat`], without following a final link.
pub(crate) fn lstat(path: &Path) -> Result<Option<Meta>, WalkError> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(WalkError::Meta(path.to_path_buf())),
    };
    if md.file_type().is_symlink() {
        // Its own kind is neither a directory nor a file.
        let mtime = secs(md.modified()).ok_or_else(|| WalkError::Meta(path.to_path_buf()))?;
        return Ok(Some(Meta {
            kind: Kind::Other,
            len: md.len(),
            mtime,
            ctime: mtime,
        }));
    }
    stat(path)
}

/// List one directory: at most `cap` entries, each an [`Entry`] or a [`WalkError`]. A
/// directory that does not exist lists nothing; one that cannot be listed is one
/// `Err(List)`; an entry that cannot be read, or whose type cannot be read, is an error in its
/// place; more than `cap` entries end with `Err(Limit)`.
pub(crate) fn list(dir: &Path, cap: usize) -> Vec<Result<Entry, WalkError>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(_) => return vec![Err(WalkError::List(dir.to_path_buf()))],
    };
    let mut out = Vec::new();
    for (k, e) in rd.enumerate() {
        if k >= cap {
            out.push(Err(WalkError::Limit(dir.to_path_buf())));
            break;
        }
        let e = match e {
            Ok(e) => e,
            Err(_) => {
                out.push(Err(WalkError::Entry(dir.to_path_buf())));
                continue;
            }
        };
        let path = e.path();
        let kind = match e.file_type() {
            Ok(ft) if ft.is_symlink() => match stat(&path) {
                Ok(Some(m)) => m.kind,
                Ok(None) => Kind::Other,
                Err(err) => {
                    out.push(Err(err));
                    continue;
                }
            },
            Ok(ft) => kind_of(ft),
            Err(_) => {
                out.push(Err(WalkError::Type(path)));
                continue;
            }
        };
        out.push(Ok(Entry {
            path,
            name: e.file_name(),
            kind,
        }));
    }
    out
}

/// The walker every agent-evidence walk goes through: it records each error it meets, so no
/// walk through it can drop one.
#[derive(Debug, Default)]
pub(crate) struct Walker {
    pub errors: Vec<WalkError>,
}

impl Walker {
    fn keep<T>(&mut self, r: Result<T, WalkError>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.errors.push(e);
                None
            }
        }
    }

    /// The readable entries of `dir` (at most `cap`); every error is recorded.
    pub fn list(&mut self, dir: &Path, cap: usize) -> Vec<Entry> {
        list(dir, cap)
            .into_iter()
            .filter_map(|r| self.keep(r))
            .collect()
    }

    /// The metadata of `path` (following a link); `None` when it does not exist or could not
    /// be read, the latter recorded. A caller must treat an unreadable time as recent.
    pub fn stat(&mut self, path: &Path) -> Option<Meta> {
        self.keep(stat(path)).flatten()
    }

    /// Is `path` a directory (following a link)? An unreadable one is recorded and counts as a
    /// directory, so the listing that follows records why it cannot be read.
    pub fn is_dir(&mut self, path: &Path) -> bool {
        match stat(path) {
            Ok(m) => m.is_some_and(|m| m.kind == Kind::Dir),
            Err(e) => {
                self.errors.push(e);
                true
            }
        }
    }

    /// Record that a walk stopped at its bound at `at`.
    pub fn limit(&mut self, at: &Path) {
        self.errors.push(WalkError::Limit(at.to_path_buf()));
    }

    /// Directories named `name` at `base` or up to `depth` levels below it, each level's
    /// listing at most `cap` entries.
    pub fn named_dirs(&mut self, base: &Path, name: &str, depth: usize, cap: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut level = vec![base.to_path_buf()];
        for d in 0..=depth {
            let mut next = Vec::new();
            for dir in &level {
                let cand = dir.join(name);
                if self.is_dir(&cand) {
                    out.push(cand);
                }
                if d < depth {
                    for e in self.list(dir, cap) {
                        if e.kind == Kind::Dir && e.name != name {
                            next.push(e.path);
                        }
                    }
                }
            }
            level = next;
        }
        out
    }

    /// Files below `dir` up to `depth` levels (1 = direct children) whose name `want` accepts,
    /// with their modification times (`None`: unreadable, recorded; treat as recent). At most
    /// `cap` entries are looked at in all; beyond that the walk stops with a recorded limit.
    pub fn files_below(
        &mut self,
        dir: &Path,
        depth: usize,
        cap: usize,
        want: &dyn Fn(&str) -> bool,
    ) -> Vec<(PathBuf, Option<i64>)> {
        let mut out = Vec::new();
        let mut seen = 0usize;
        let mut stack = vec![(dir.to_path_buf(), 1usize)];
        while let Some((d, lvl)) = stack.pop() {
            let left = cap.saturating_sub(seen);
            if left == 0 {
                self.limit(&d);
                return out;
            }
            let entries = self.list(&d, left);
            seen += entries.len();
            for e in entries {
                match e.kind {
                    Kind::Dir if lvl < depth => stack.push((e.path, lvl + 1)),
                    Kind::File if want(&e.name_lossy()) => {
                        let m = self.stat(&e.path).map(|m| m.mtime);
                        out.push((e.path, m));
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn errors_are_typed_and_absent_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        assert!(list(&base.join("absent"), 10).is_empty());
        assert!(matches!(stat(&base.join("absent")), Ok(None)));
        for i in 0..3 {
            fs::write(base.join(format!("f{i}")), "x").unwrap();
        }
        let got = list(base, 2);
        assert_eq!(got.len(), 3);
        assert!(matches!(got.last(), Some(Err(WalkError::Limit(_)))));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            symlink(base.join("nowhere"), base.join("dangling")).unwrap();
            let kinds: Vec<Kind> = list(base, 10)
                .into_iter()
                .map(|r| r.unwrap().kind)
                .collect();
            assert!(kinds.contains(&Kind::Other));
            let locked = base.join("locked");
            fs::create_dir(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            let root = fs::read_dir(&locked).is_ok();
            if root {
                eprintln!("SKIPPED the unreadable-directory case: running as root");
            } else {
                assert_eq!(list(&locked, 10), vec![Err(WalkError::List(locked.clone()))]);
                let mut w = Walker::default();
                assert!(w.list(&locked, 10).is_empty());
                assert_eq!(w.errors, vec![WalkError::List(locked.clone())]);
                // A path below a directory that cannot be searched: its metadata is an error,
                // and as a directory probe it counts as one (so its listing records why).
                let mut w = Walker::default();
                assert!(w.is_dir(&locked.join("x")));
                assert_eq!(w.errors, vec![WalkError::Meta(locked.join("x"))]);
            }
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// A link loop ends at the walk's bound, recorded.
    #[cfg(unix)]
    #[test]
    fn link_loop_ends_at_the_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::create_dir(&a).unwrap();
        std::os::unix::fs::symlink(&a, a.join("loop")).unwrap();
        fs::write(a.join("t.jsonl"), "{}").unwrap();
        let mut w = Walker::default();
        let got = w.files_below(&a, 50, 20, &|n| n.ends_with(".jsonl"));
        assert!(!got.is_empty());
        assert!(
            w.errors.iter().any(|e| matches!(e, WalkError::Limit(_))),
            "{:?}",
            w.errors
        );
    }
}
