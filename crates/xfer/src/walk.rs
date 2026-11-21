//! The input boundary: a metadata-only walk of the source tree minus `--exclude` patterns, or an
//! explicit `--files-from` list. Nothing outside the result is read, uploaded or receipted.
//!
//! - Paths are relative byte strings with `/` separators (unix bytes, so non-UTF-8 names work).
//! - Symlinks are never followed; the root itself must be a real directory.
//! - A hard-link group with any excluded member found in the walk is excluded entirely.
//! - A symlink whose target resolves lexically inside the tree to an excluded path is excluded.
//! - FIFOs, sockets and devices are skipped with a reason and never opened.
//! - Regular files are opened component by component from the root with `O_NOFOLLOW` (unix), so
//!   a symlink swapped into a middle component after the walk fails the open.

use crate::error::{Error, Result};
use crate::meta::{stat_of, Kind, Stat};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const PATH_MAX: usize = 4096;

#[derive(Clone, Debug)]
pub struct Node {
    pub rel: Vec<u8>,
    pub stat: Stat,
    /// Symlink target bytes.
    pub target: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct Walked {
    pub nodes: Vec<Node>,
    pub skipped: Vec<(Vec<u8>, String)>,
    pub excluded: u64,
}

/// `--exclude` patterns. A pattern with a `/` matches the whole relative path (or a prefix of
/// whole components); one without matches any single component. `*` and `?` do not cross `/`;
/// `**` does.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pats: Vec<Vec<u8>>,
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = &p[2..];
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        Some(b'*') => {
            let rest = &p[1..];
            for i in 0..=s.len() {
                if glob(rest, &s[i..]) {
                    return true;
                }
                if i < s.len() && s[i] == b'/' {
                    break;
                }
            }
            false
        }
        Some(b'?') => !s.is_empty() && s[0] != b'/' && glob(&p[1..], &s[1..]),
        Some(&c) => !s.is_empty() && s[0] == c && glob(&p[1..], &s[1..]),
    }
}

impl Filter {
    pub fn new(patterns: &[String]) -> Result<Filter> {
        let mut pats = Vec::new();
        for p in patterns {
            let t = p.trim_matches('/');
            if t.is_empty() {
                return Err(Error::Refused(format!("empty exclude pattern {p:?}")));
            }
            pats.push(t.as_bytes().to_vec());
        }
        Ok(Filter { pats })
    }

    /// True when `rel` or any of its ancestors is excluded.
    pub fn excluded(&self, rel: &[u8]) -> bool {
        if self.pats.is_empty() {
            return false;
        }
        let comps: Vec<&[u8]> = rel.split(|&b| b == b'/').collect();
        for p in &self.pats {
            if p.contains(&b'/') {
                for i in 1..=comps.len() {
                    let pre = comps[..i].join(&b'/');
                    if glob(p, &pre) {
                        return true;
                    }
                }
            } else if comps.iter().any(|c| glob(p, c)) {
                return true;
            }
        }
        false
    }
}

/// Validate a relative path from input or from the bucket: no empty, `.` or `..` components, not
/// absolute, no NUL, bounded length. On Windows also no `\` or `:`.
pub fn validate_rel(rel: &[u8]) -> Result<()> {
    let bad = rel.is_empty()
        || rel.len() > PATH_MAX
        || rel[0] == b'/'
        || rel.contains(&0)
        || rel
            .split(|&b| b == b'/')
            .any(|c| c.is_empty() || c == b"." || c == b"..")
        || (cfg!(windows) && (rel.contains(&b'\\') || rel.contains(&b':')));
    if bad {
        return Err(Error::Refused(format!(
            "unsafe relative path {:?}",
            String::from_utf8_lossy(rel)
        )));
    }
    Ok(())
}

#[cfg(unix)]
pub fn rel_path(rel: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(rel))
}

#[cfg(not(unix))]
pub fn rel_path(rel: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(rel).replace('/', "\\"))
}

#[cfg(unix)]
fn name_bytes(n: &std::ffi::OsStr) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Some(n.as_bytes().to_vec())
}

#[cfg(not(unix))]
fn name_bytes(n: &std::ffi::OsStr) -> Option<Vec<u8>> {
    n.to_str().map(|s| s.as_bytes().to_vec())
}

#[cfg(unix)]
fn target_bytes(t: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    t.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn target_bytes(t: &Path) -> Vec<u8> {
    t.to_string_lossy().replace('\\', "/").into_bytes()
}

fn join(a: &[u8], b: &[u8]) -> Vec<u8> {
    if a.is_empty() {
        return b.to_vec();
    }
    let mut v = a.to_vec();
    v.push(b'/');
    v.extend_from_slice(b);
    v
}

/// Resolve a symlink target lexically relative to the link's directory. None if it leaves the
/// tree (or is absolute outside `root_abs`).
fn resolve_lexical(link_rel: &[u8], target: &[u8], root_abs: &[u8]) -> Option<Vec<u8>> {
    let mut parts: Vec<&[u8]> = Vec::new();
    let tail: &[u8] = if target.first() == Some(&b'/') {
        let r = target.strip_prefix(root_abs)?;
        if !(r.is_empty() || r[0] == b'/') {
            return None;
        }
        r
    } else {
        if let Some(pos) = link_rel.iter().rposition(|&b| b == b'/') {
            parts.extend(link_rel[..pos].split(|&b| b == b'/'));
        }
        target
    };
    for c in tail.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join(&b'/'))
}

pub struct WalkOpts<'a> {
    pub filter: &'a Filter,
    pub files_from: Option<&'a [Vec<u8>]>,
    pub one_file_system: bool,
}

pub fn walk(root: &Path, o: &WalkOpts) -> Result<Walked> {
    let rmd = fs::symlink_metadata(root)?;
    if !rmd.is_dir() {
        return Err(Error::Refused(format!(
            "source {} is not a directory (symlinks are not followed)",
            root.display()
        )));
    }
    let root_dev = stat_of(&rmd).id.dev;
    let root_abs = target_bytes(&fs::canonicalize(root)?);
    let mut w = Walked::default();
    // inodes of excluded hard-linked files seen in the walk
    let mut excl_inodes: HashSet<(u64, u64)> = HashSet::new();

    let push = |w: &mut Walked, rel: Vec<u8>, md: &fs::Metadata, full: &Path| {
        let st = stat_of(md);
        match st.kind {
            Err(why) => w.skipped.push((rel, why.to_string())),
            Ok(Kind::Symlink) => match fs::read_link(full) {
                Ok(t) => w.nodes.push(Node {
                    rel,
                    stat: st,
                    target: Some(target_bytes(&t)),
                }),
                Err(e) => w.skipped.push((rel, format!("readlink: {e}"))),
            },
            Ok(_) => w.nodes.push(Node {
                rel,
                stat: st,
                target: None,
            }),
        }
    };

    if let Some(list) = o.files_from {
        // Explicit list: each entry and its ancestor directories, nothing else.
        let mut dirs: HashSet<Vec<u8>> = HashSet::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        for rel in list {
            validate_rel(rel)?;
            let comps: Vec<&[u8]> = rel.split(|&b| b == b'/').collect();
            for i in 1..comps.len() {
                let d = comps[..i].join(&b'/');
                if dirs.insert(d.clone()) {
                    let full = root.join(rel_path(&d));
                    match fs::symlink_metadata(&full) {
                        Ok(md) if md.is_dir() => push(&mut w, d, &md, &full),
                        Ok(_) => {
                            w.skipped.push((d, "ancestor is not a directory".into()));
                        }
                        Err(e) => w.skipped.push((d, e.to_string())),
                    }
                }
            }
            if !seen.insert(rel.clone()) || dirs.contains(rel) {
                continue;
            }
            if o.filter.excluded(rel) {
                w.excluded += 1;
                continue;
            }
            let full = root.join(rel_path(rel));
            match fs::symlink_metadata(&full) {
                Ok(md) if md.is_dir() => {
                    dirs.insert(rel.clone());
                    push(&mut w, rel.clone(), &md, &full)
                }
                Ok(md) => push(&mut w, rel.clone(), &md, &full),
                Err(e) => w.skipped.push((rel.clone(), e.to_string())),
            }
        }
        // a node listed both as an ancestor and later is kept once
        let mut uniq = HashSet::new();
        w.nodes.retain(|n| uniq.insert(n.rel.clone()));
    } else {
        let mut stack: Vec<Vec<u8>> = vec![Vec::new()];
        while let Some(dir) = stack.pop() {
            let full_dir = if dir.is_empty() {
                root.to_path_buf()
            } else {
                root.join(rel_path(&dir))
            };
            let rd = match fs::read_dir(&full_dir) {
                Ok(r) => r,
                Err(e) => {
                    w.skipped.push((dir.clone(), format!("read_dir: {e}")));
                    continue;
                }
            };
            let mut entries: Vec<(Vec<u8>, PathBuf)> = Vec::new();
            for e in rd {
                let e = match e {
                    Ok(e) => e,
                    Err(err) => {
                        w.skipped
                            .push((dir.clone(), format!("read_dir entry: {err}")));
                        continue;
                    }
                };
                match name_bytes(&e.file_name()) {
                    Some(n) => entries.push((join(&dir, &n), e.path())),
                    None => w.skipped.push((
                        join(&dir, e.file_name().to_string_lossy().as_bytes()),
                        "name is not valid Unicode on this platform".into(),
                    )),
                }
            }
            entries.sort();
            let mut subdirs = Vec::new();
            for (rel, full) in entries {
                if rel.len() > PATH_MAX {
                    w.skipped.push((rel, "path longer than 4096 bytes".into()));
                    continue;
                }
                let md = match fs::symlink_metadata(&full) {
                    Ok(m) => m,
                    Err(e) => {
                        w.skipped.push((rel, format!("lstat: {e}")));
                        continue;
                    }
                };
                if o.filter.excluded(&rel) {
                    w.excluded += 1;
                    let st = stat_of(&md);
                    if st.kind == Ok(Kind::File) && st.id.nlink > 1 {
                        excl_inodes.insert((st.id.dev, st.id.ino));
                    }
                    continue;
                }
                if md.is_dir() {
                    let st = stat_of(&md);
                    push(&mut w, rel.clone(), &md, &full);
                    if o.one_file_system && st.id.dev != root_dev {
                        continue;
                    }
                    subdirs.push(rel);
                } else {
                    push(&mut w, rel, &md, &full);
                }
            }
            // depth-first, in name order
            for d in subdirs.into_iter().rev() {
                stack.push(d);
            }
        }
    }

    // Hard-link groups with an excluded member are excluded entirely.
    // (the broken-exclusion variant skips this and the symlink rule: failing-first evidence only)
    let rules = !cfg!(feature = "broken-exclusion");
    if rules && !excl_inodes.is_empty() {
        let before = w.nodes.len();
        w.nodes.retain(|n| {
            !(n.stat.kind == Ok(Kind::File)
                && n.stat.id.nlink > 1
                && excl_inodes.contains(&(n.stat.id.dev, n.stat.id.ino)))
        });
        w.excluded += (before - w.nodes.len()) as u64;
    }
    // Symlinks pointing at an excluded path are excluded.
    if rules && !o.filter.pats.is_empty() {
        let before = w.nodes.len();
        w.nodes.retain(|n| match (&n.stat.kind, &n.target) {
            (Ok(Kind::Symlink), Some(t)) => match resolve_lexical(&n.rel, t, &root_abs) {
                Some(r) if !r.is_empty() => !o.filter.excluded(&r),
                _ => true,
            },
            _ => true,
        });
        w.excluded += (before - w.nodes.len()) as u64;
    }
    Ok(w)
}

/// Hard-link groups among walked regular files: (dev, ino) -> node indices, for nlink > 1.
pub fn link_groups(nodes: &[Node]) -> HashMap<(u64, u64), Vec<usize>> {
    let mut g: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if n.stat.kind == Ok(Kind::File) && n.stat.id.nlink > 1 {
            g.entry((n.stat.id.dev, n.stat.id.ino)).or_default().push(i);
        }
    }
    g.retain(|_, v| v.len() > 1);
    g
}

/// Open a regular file under `root` without following any symlink on the way (unix: `openat`
/// with `O_NOFOLLOW` per component; FIFOs are not blocked on, and a non-regular file is refused).
#[cfg(unix)]
pub fn open_beneath(root: &Path, rel: &[u8]) -> std::io::Result<fs::File> {
    use std::os::fd::{FromRawFd, OwnedFd};
    if cfg!(feature = "broken-read-guard") {
        // the broken variant follows symlinks on the way (failing-first evidence only)
        return fs::File::open(root.join(rel_path(rel)));
    }
    use std::os::unix::ffi::OsStrExt;
    let croot = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("root contains NUL"))?;
    // SAFETY: plain libc calls on valid C strings; every fd is wrapped in OwnedFd at once.
    unsafe {
        let fd = libc::open(
            croot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut dir = OwnedFd::from_raw_fd(fd);
        let comps: Vec<&[u8]> = rel.split(|&b| b == b'/').collect();
        for (i, c) in comps.iter().enumerate() {
            let cc = std::ffi::CString::new(*c)
                .map_err(|_| std::io::Error::other("component contains NUL"))?;
            let last = i + 1 == comps.len();
            let flags = if last {
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
            } else {
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC
            };
            use std::os::fd::AsRawFd;
            let nfd = libc::openat(dir.as_raw_fd(), cc.as_ptr(), flags);
            if nfd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let owned = OwnedFd::from_raw_fd(nfd);
            if last {
                let f = fs::File::from(owned);
                if !f.metadata()?.is_file() {
                    return Err(std::io::Error::other("not a regular file"));
                }
                return Ok(f);
            }
            dir = owned;
        }
        Err(std::io::Error::other("empty path"))
    }
}

#[cfg(not(unix))]
pub fn open_beneath(root: &Path, rel: &[u8]) -> std::io::Result<fs::File> {
    let p = root.join(rel_path(rel));
    if fs::symlink_metadata(&p)?.file_type().is_symlink() {
        return Err(std::io::Error::other("path is a symlink"));
    }
    fs::File::open(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patterns() {
        let f = Filter::new(&[
            ".env".into(),
            "secrets/".into(),
            "a/*.key".into(),
            "**/tmp".into(),
        ])
        .expect("filter");
        assert!(f.excluded(b".env"));
        assert!(f.excluded(b"x/y/.env"));
        assert!(f.excluded(b"secrets"));
        assert!(f.excluded(b"secrets/inner/file"));
        assert!(f.excluded(b"a/b.key"));
        assert!(!f.excluded(b"a/b/c.key"));
        assert!(f.excluded(b"q/r/tmp"));
        assert!(f.excluded(b"q/r/tmp/z"));
        assert!(!f.excluded(b"envoy"));
        assert!(!f.excluded(b"b/secrets.txt"));
    }
    #[test]
    fn rel_validation_and_resolution() {
        assert!(validate_rel(b"a/b").is_ok());
        for bad in [&b""[..], b"/a", b"a/../b", b"a//b", b"./a", b"a/\0"] {
            assert!(validate_rel(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            resolve_lexical(b"l/x", b"../s/k", b"/r"),
            Some(b"s/k".to_vec())
        );
        assert_eq!(resolve_lexical(b"x", b"../../e", b"/r"), None);
        assert_eq!(
            resolve_lexical(b"x", b"/r/s/k", b"/r"),
            Some(b"s/k".to_vec())
        );
        assert_eq!(resolve_lexical(b"x", b"/rr/k", b"/r"), None);
    }
}
