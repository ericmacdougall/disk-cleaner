//! Pieces: the partition of every byte under the scan roots that plan lanes are built on
//! (docs/SCAN-FORMAT.md, "Pieces and lanes"). Read-only: the survey and [`recheck`] list
//! directories, `lstat` entries and read the content of the files they hash, and change nothing.
//!
//! The survey is a second walk after the main scan. It keeps its own stack (no recursion), so a
//! deep tree cannot overflow the call stack, and it visits each directory's entries in byte order
//! of their names, so the result (which name owns a hard-linked inode, the order of pieces) does
//! not depend on the order the file system returns entries.

use crate::model::{Class, Node};
use crate::rules::RuleSet;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default bound on a piece's members, summed at their logical length (1 GiB).
pub const DEFAULT_MAX_PIECE_BYTES: u64 = 1 << 30;
/// Default bound on a piece's member count.
pub const DEFAULT_MAX_PIECE_FILES: u64 = 10_000;
/// Default smallest file whose content is compared with others of the same length (64 KiB).
pub const DEFAULT_MIN_DUP_BYTES: u64 = 64 * 1024;

/// Why every piece is incomplete on a platform without file identity.
pub const NO_IDENTITY: &str = "this platform does not report file identity and change time, so a \
                               change after the scan cannot be detected";

/// Options of the survey, recorded in the scan file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PieceOptions {
    pub max_piece_bytes: u64,
    pub max_piece_files: u64,
    pub min_dup_bytes: u64,
    /// Stop hashing once this many bytes were read (absent: no limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hash_bytes: Option<u64>,
}

impl Default for PieceOptions {
    fn default() -> Self {
        PieceOptions {
            max_piece_bytes: DEFAULT_MAX_PIECE_BYTES,
            max_piece_files: DEFAULT_MAX_PIECE_FILES,
            min_dup_bytes: DEFAULT_MIN_DUP_BYTES,
            max_hash_bytes: None,
        }
    }
}

/// `tree`, `files` or `git-node`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PieceKind {
    Tree,
    Files,
    GitNode,
}

/// One member: `[path, kind, length, mtime_seconds, mtime_nanoseconds, device, inode, bytes]`.
/// `path` is relative to the piece's `path`, `/`-separated; `kind` is `f`, `l` or `o`; `bytes`
/// is the allocated bytes counted for this name (0 for a hard-link name counted elsewhere).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member(
    pub String,
    pub String,
    pub u64,
    pub i64,
    pub u32,
    pub u64,
    pub u64,
    pub u64,
);

impl Member {
    pub fn path(&self) -> &str {
        &self.0
    }
    pub fn kind(&self) -> &str {
        &self.1
    }
    pub fn length(&self) -> u64 {
        self.2
    }
    pub fn bytes(&self) -> u64 {
        self.7
    }
}

/// A bounded, enumerated set of entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Piece {
    pub id: usize,
    pub kind: PieceKind,
    pub path: String,
    pub members: Vec<Member>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<usize>,
    pub group: String,
    pub entries: u64,
    pub bytes: u64,
    pub logical_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes_linked_outside: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_mtime: Option<i64>,
    pub walked_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<String>,
}

/// One name of a blob's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobCopy {
    pub path: String,
    pub piece: usize,
    pub inode: u32,
    pub bytes: u64,
}

/// Content with more than one copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub sha256: String,
    pub size: u64,
    pub bytes: u64,
    pub copies: Vec<BlobCopy>,
}

/// Totals of the survey.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PieceTotals {
    pub pieces: u64,
    pub entries: u64,
    pub bytes: u64,
    pub logical_bytes: u64,
    pub dir_overhead_bytes: u64,
    pub hashed_files: u64,
    pub hashed_bytes: u64,
    pub unhashed_candidates: u64,
    pub duplicate_bytes: u64,
    pub hardlink_names: u64,
}

/// The scan field `pieces`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PieceSurvey {
    pub options: PieceOptions,
    pub pieces: Vec<Piece>,
    pub blobs: Vec<Blob>,
    pub totals: PieceTotals,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// SHA-256 (hex) of the compact JSON of a survey: binds a plan's lanes to the scan they came
/// from (`lanes.scan_pieces_sha256`).
pub fn pieces_digest(survey: &PieceSurvey) -> String {
    let bytes = serde_json::to_vec(survey).unwrap_or_default();
    hex(&Sha256::digest(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ----------------------------------------------------------------------------------------------
// Metadata.

#[derive(Debug, Clone, Copy, Default)]
struct Meta {
    is_dir: bool,
    kind: u8,
    len: u64,
    alloc: u64,
    mtime: i64,
    nsec: u32,
    ctime: i64,
    dev: u64,
    ino: u64,
    nlink: u64,
}

#[cfg(unix)]
fn meta(md: &std::fs::Metadata) -> Meta {
    use std::os::unix::fs::MetadataExt;
    let ft = md.file_type();
    Meta {
        is_dir: ft.is_dir(),
        kind: if ft.is_file() {
            b'f'
        } else if ft.is_symlink() {
            b'l'
        } else {
            b'o'
        },
        len: md.len(),
        alloc: md.blocks().saturating_mul(512),
        mtime: md.mtime(),
        nsec: md.mtime_nsec().clamp(0, 999_999_999) as u32,
        ctime: md.ctime(),
        dev: md.dev(),
        ino: md.ino(),
        nlink: md.nlink(),
    }
}

#[cfg(not(unix))]
fn meta(md: &std::fs::Metadata) -> Meta {
    let ft = md.file_type();
    let (mtime, nsec) = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
        .unwrap_or((0, 0));
    Meta {
        is_dir: ft.is_dir(),
        kind: if ft.is_file() {
            b'f'
        } else if ft.is_symlink() {
            b'l'
        } else {
            b'o'
        },
        len: md.len(),
        alloc: md.len(),
        mtime,
        nsec,
        ctime: 0,
        dev: 0,
        ino: 0,
        nlink: 1,
    }
}

/// Does this platform report file identity and change time?
fn has_identity() -> bool {
    cfg!(unix)
}

// ----------------------------------------------------------------------------------------------
// The in-memory tree.

#[derive(Debug, Clone, Copy)]
enum Child {
    Dir(usize),
    Ent(usize),
}

#[derive(Debug)]
struct DirRec {
    path: PathBuf,
    name: OsString,
    parent: Option<usize>,
    root: usize,
    children: Vec<Child>,
    alloc: u64,
    unreadable: bool,
    mount_skipped: bool,
    other_root: bool,
    utf8: bool,
    /// Innermost scan node holding this directory.
    node: Option<usize>,
    /// This directory is a scan node's own path.
    is_node: bool,
    in_gitdir: bool,
    /// The top of a git directory: where a git directory starts, or the admin directory of a
    /// linked worktree or submodule (below `worktrees` or `modules`, holding a `HEAD` file).
    git_top: bool,
    /// Below `worktrees` or `modules` of a git top, outside an admin directory: these directory
    /// names are worktree names and submodule paths.
    git_area: bool,
    /// Below `refs` or `logs` of a git top: names here are ref names (branches, tags, remotes)
    /// that git chose from the user's words, never file contents, so name-based secret rules do
    /// not apply to them.
    git_named: bool,
    in_artifact: bool,
    /// The topmost secret directory at or above this one: its kind.
    secret_dir: Option<String>,
    secret_top: bool,
}

#[derive(Debug)]
struct EntRec {
    dir: usize,
    name: OsString,
    ok: bool,
    m: Meta,
    utf8: bool,
    in_gitdir: bool,
    /// A secret name inside a git directory: never cut out, it makes the git node incomplete.
    gitdir_secret: bool,
    secret: Option<String>,
    /// Allocated and logical bytes counted for this name (0 for a later hard-link name).
    counted: u64,
    counted_logical: u64,
    /// The inode has more links than names the survey saw (owner name only).
    linked_outside: bool,
    order: usize,
    piece: usize,
}

struct Tree {
    dirs: Vec<DirRec>,
    ents: Vec<EntRec>,
    roots: Vec<usize>,
}

fn utf8(name: &OsStr) -> bool {
    name.to_str().is_some()
}

/// Read every root into memory. Directories that are other scan roots are not entered here
/// (they are walked on their own); with `one_file_system`, directories on another device than
/// their root are not entered.
fn read_tree(roots: &[PathBuf], one_fs: bool, nodes: &[Node]) -> Tree {
    let node_at: HashMap<PathBuf, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (PathBuf::from(&n.path), i))
        .collect();
    let root_set: HashSet<&Path> = roots.iter().map(|r| r.as_path()).collect();
    let mut t = Tree {
        dirs: Vec::new(),
        ents: Vec::new(),
        roots: Vec::new(),
    };
    for (ri, root) in roots.iter().enumerate() {
        // A scan root may be spelled through a symlink to a directory, as the main scan allows;
        // only the root itself is followed, never an entry below it.
        let md = match std::fs::metadata(root) {
            Ok(md) if md.is_dir() => meta(&md),
            _ => continue,
        };
        let root_dev = md.dev;
        // The root's own blocks are counted as the main scan counts them: those of the name
        // given (a symlink's, not its target directory's).
        let root_alloc = std::fs::symlink_metadata(root)
            .map(|l| meta(&l).alloc)
            .unwrap_or(md.alloc);
        let idx = t.dirs.len();
        t.dirs.push(DirRec {
            path: root.clone(),
            name: root
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default(),
            parent: None,
            root: ri,
            children: Vec::new(),
            alloc: root_alloc,
            unreadable: false,
            mount_skipped: false,
            other_root: false,
            utf8: root.to_str().is_some(),
            node: node_at.get(root).copied(),
            is_node: node_at.contains_key(root),
            in_gitdir: false,
            git_top: false,
            git_area: false,
            git_named: false,
            in_artifact: false,
            secret_dir: None,
            secret_top: false,
        });
        t.roots.push(idx);
        let mut stack = vec![idx];
        while let Some(d) = stack.pop() {
            let path = t.dirs[d].path.clone();
            let mut names: Vec<OsString> = Vec::new();
            match std::fs::read_dir(&path) {
                Ok(rd) => {
                    for e in rd {
                        match e {
                            Ok(e) => names.push(e.file_name()),
                            Err(_) => t.dirs[d].unreadable = true,
                        }
                    }
                }
                Err(_) => {
                    t.dirs[d].unreadable = true;
                    continue;
                }
            }
            names.sort();
            for name in names {
                let p = path.join(&name);
                match std::fs::symlink_metadata(&p) {
                    Ok(md) if md.is_dir() => {
                        let m = meta(&md);
                        let ci = t.dirs.len();
                        let mount = one_fs && has_identity() && m.dev != root_dev;
                        let other = root_set.contains(p.as_path());
                        t.dirs.push(DirRec {
                            path: p.clone(),
                            name: name.clone(),
                            parent: Some(d),
                            root: ri,
                            children: Vec::new(),
                            alloc: if other { 0 } else { m.alloc },
                            unreadable: false,
                            mount_skipped: mount && !other,
                            other_root: other,
                            utf8: utf8(&name),
                            node: None,
                            is_node: node_at.contains_key(&p),
                            in_gitdir: false,
                            git_top: false,
                            git_area: false,
                            git_named: false,
                            in_artifact: false,
                            secret_dir: None,
                            secret_top: false,
                        });
                        t.dirs[d].children.push(Child::Dir(ci));
                        if !mount && !other {
                            stack.push(ci);
                        }
                        if let Some(&n) = node_at.get(&p) {
                            t.dirs[ci].node = Some(n);
                        }
                    }
                    Ok(md) => {
                        let ei = t.ents.len();
                        t.ents.push(EntRec {
                            dir: d,
                            name: name.clone(),
                            ok: true,
                            m: meta(&md),
                            utf8: utf8(&name),
                            in_gitdir: false,
                            gitdir_secret: false,
                            secret: None,
                            counted: 0,
                            counted_logical: 0,
                            linked_outside: false,
                            order: 0,
                            piece: usize::MAX,
                        });
                        t.dirs[d].children.push(Child::Ent(ei));
                    }
                    Err(_) => {
                        let ei = t.ents.len();
                        t.ents.push(EntRec {
                            dir: d,
                            name: name.clone(),
                            ok: false,
                            m: Meta {
                                kind: b'o',
                                ..Meta::default()
                            },
                            utf8: utf8(&name),
                            in_gitdir: false,
                            gitdir_secret: false,
                            secret: None,
                            counted: 0,
                            counted_logical: 0,
                            linked_outside: false,
                            order: 0,
                            piece: usize::MAX,
                        });
                        t.dirs[d].children.push(Child::Ent(ei));
                    }
                }
            }
        }
    }
    t
}

/// Directories in preorder (each root in turn), entries visited in name order.
fn preorder_dirs(t: &Tree) -> Vec<usize> {
    let mut out = Vec::with_capacity(t.dirs.len());
    for &r in &t.roots {
        let mut stack = vec![r];
        while let Some(d) = stack.pop() {
            out.push(d);
            for c in t.dirs[d].children.iter().rev() {
                if let Child::Dir(c) = c {
                    stack.push(*c);
                }
            }
        }
    }
    out
}

/// Entries in walk order: depth-first preorder with files and directories interleaved by name.
fn preorder_ents(t: &Tree) -> Vec<usize> {
    let mut out = Vec::with_capacity(t.ents.len());
    for &r in &t.roots {
        let mut stack: Vec<(usize, usize)> = vec![(r, 0)];
        while let Some((d, pos)) = stack.pop() {
            if pos >= t.dirs[d].children.len() {
                continue;
            }
            stack.push((d, pos + 1));
            match t.dirs[d].children[pos] {
                Child::Ent(e) => out.push(e),
                Child::Dir(c) => stack.push((c, 0)),
            }
        }
    }
    out
}

/// Directories of a git top whose entries are ref names.
const GIT_REF_DIRS: [&str; 2] = ["refs", "logs"];
/// Directories of a git top whose subdirectories are named after worktrees and submodules.
const GIT_ADMIN_DIRS: [&str; 2] = ["worktrees", "modules"];

/// The context a directory hands to its children.
#[derive(Clone, Debug, Default)]
struct Inherited {
    node: Option<usize>,
    is_node: bool,
    in_gitdir: bool,
    in_artifact: bool,
    secret_dir: Option<String>,
}

/// Physical evidence that `dir` is a git directory, whether or not the scan holds its repository
/// as a node: it is named `.git`, or it has the shape of one (a `HEAD` file and `objects` and
/// `refs` directories, as a bare repository has). Symlinks are not followed. This only ever adds
/// git-directory context, which keeps entries (the safe side).
fn looks_like_git_dir(dir: &Path) -> bool {
    if dir.file_name() == Some(OsStr::new(".git")) {
        return true;
    }
    let is = |name: &str, want_dir: bool| {
        std::fs::symlink_metadata(dir.join(name))
            .map(|m| if want_dir { m.is_dir() } else { m.is_file() })
            .unwrap_or(false)
    };
    is("HEAD", false) && is("objects", true) && is("refs", true)
}

/// The context a scan root inherits from the directories that physically enclose it, as if the
/// walk had reached it from above: whatever the order of the roots, and whether or not an
/// enclosing root was scanned too (a root walked on its own because it is another file system
/// under `--one-file-system`, or a root given alone). Node and artifact context follow the
/// root's real path through the scan's nodes. Git-directory context follows the nodes too, and
/// also the directories themselves (`looks_like_git_dir`), so a root inside the git directory of
/// a repository the scan did not cover is still inside a git directory. Secrecy follows any
/// secret-named directory above the root in its real path or its spelling, so everything inside
/// a secret directory stays local.
fn enclosing_context(
    root: &Path,
    real_nodes: &HashMap<PathBuf, usize>,
    nodes: &[Node],
    rules: &RuleSet,
) -> Inherited {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let class_of = |n: Option<usize>| n.and_then(|n| nodes.get(n)).map(|n| n.class);
    let secret_name = |a: &Path| {
        a.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| rules.secret_dir(n))
            .map(|k| k.to_string())
    };
    let mut c = Inherited::default();
    let above: Vec<&Path> = real.ancestors().skip(1).collect();
    for a in above.into_iter().rev() {
        let here = real_nodes.get(a).copied();
        let pclass = if c.is_node { class_of(c.node) } else { None };
        let name_is_git = a.file_name() == Some(OsStr::new(".git"));
        c.in_gitdir = c.in_gitdir
            || class_of(here) == Some(Class::BareRepo)
            || (name_is_git && matches!(pclass, Some(Class::Repo) | Some(Class::Worktree)))
            || looks_like_git_dir(a);
        if here.is_some() {
            c.node = here;
        }
        c.is_node = here.is_some();
        c.in_artifact = c.in_artifact || class_of(c.node).map(|k| k.is_artifact()).unwrap_or(false);
        if c.secret_dir.is_none() {
            c.secret_dir = secret_name(a);
        }
    }
    if c.secret_dir.is_none() {
        c.secret_dir = root.ancestors().skip(1).find_map(secret_name);
    }
    c
}

/// The real path of every node: its root's real path plus the rest, since the walk only
/// descends real directories.
fn real_node_paths(roots: &[PathBuf], nodes: &[Node]) -> HashMap<PathBuf, usize> {
    let canon: Vec<PathBuf> = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()))
        .collect();
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let p = Path::new(&n.path);
            let best = roots
                .iter()
                .zip(&canon)
                .filter(|(r, _)| p.starts_with(r))
                .max_by_key(|(r, _)| r.components().count());
            let real = match best.map(|(r, c)| (p.strip_prefix(r), c)) {
                Some((Ok(rel), c)) if rel.as_os_str().is_empty() => c.clone(),
                Some((Ok(rel), c)) => c.join(rel),
                _ => p.to_path_buf(),
            };
            (real, i)
        })
        .collect()
}

/// Fill node, git-directory, artifact and secret context, top down. A scan root starts from the
/// context of the directories that enclose it (`enclosing_context`).
fn annotate(t: &mut Tree, roots: &[PathBuf], nodes: &[Node], rules: &RuleSet, order: &[usize]) {
    let real_nodes = real_node_paths(roots, nodes);
    for &d in order {
        let parent = t.dirs[d].parent;
        let pctx = match parent {
            Some(p) => Inherited {
                node: t.dirs[p].node,
                is_node: t.dirs[p].is_node,
                in_gitdir: t.dirs[p].in_gitdir,
                in_artifact: t.dirs[p].in_artifact,
                secret_dir: t.dirs[p].secret_dir.clone(),
            },
            None => enclosing_context(&t.dirs[d].path, &real_nodes, nodes, rules),
        };
        let (pnode, pgit, part, psecret) =
            (pctx.node, pctx.in_gitdir, pctx.in_artifact, pctx.secret_dir);
        if t.dirs[d].node.is_none() {
            t.dirs[d].node = pnode;
        }
        let own = if t.dirs[d].is_node {
            t.dirs[d].node
        } else {
            None
        };
        let class = own.and_then(|n| nodes.get(n)).map(|n| n.class);
        let pclass = pnode.and_then(|n| nodes.get(n)).map(|n| n.class);
        let name_is_git = t.dirs[d].name == OsStr::new(".git");
        t.dirs[d].in_gitdir = pgit
            || class == Some(Class::BareRepo)
            || (name_is_git
                && pctx.is_node
                && matches!(pclass, Some(Class::Repo) | Some(Class::Worktree)))
            // A scan root that is itself a git directory the scan holds no node for.
            || (parent.is_none() && looks_like_git_dir(&t.dirs[d].path));
        let nclass = t.dirs[d].node.and_then(|n| nodes.get(n)).map(|n| n.class);
        t.dirs[d].in_artifact = part || nclass.map(|c| c.is_artifact()).unwrap_or(false);
        let (ptop, parea, pnamed) = parent
            .map(|p| (t.dirs[p].git_top, t.dirs[p].git_area, t.dirs[p].git_named))
            .unwrap_or((false, false, false));
        let name_in = |list: &[&str]| {
            t.dirs[d]
                .name
                .to_str()
                .map(|n| list.contains(&n))
                .unwrap_or(false)
        };
        let has_head = t.dirs[d].children.iter().any(|c| match *c {
            Child::Ent(e) => t.ents[e].name == OsStr::new("HEAD"),
            Child::Dir(_) => false,
        });
        let in_gitdir = t.dirs[d].in_gitdir;
        let top = in_gitdir && (!pgit || (parea && has_head));
        let area = in_gitdir && !top && ((ptop && name_in(&GIT_ADMIN_DIRS)) || parea);
        let named = in_gitdir && (pnamed || (ptop && name_in(&GIT_REF_DIRS)));
        t.dirs[d].git_top = top;
        t.dirs[d].git_area = area;
        t.dirs[d].git_named = named;
        if psecret.is_some() {
            t.dirs[d].secret_dir = psecret;
        } else if pnamed || parea {
            // A ref, worktree or submodule name: never a secret directory by name.
        } else if let Some(kind) = t.dirs[d].name.to_str().and_then(|n| rules.secret_dir(n)) {
            // A scan root with a secret directory name is a secret directory too. Inside a git
            // directory it is not cut out (the git piece stays whole), but its entries are
            // secret-in-git-dir, which makes the git piece incomplete.
            t.dirs[d].secret_dir = Some(kind.to_string());
            t.dirs[d].secret_top = !t.dirs[d].in_gitdir;
        }
        // A scan root inside a secret directory is the top of a secret piece of its own.
        if parent.is_none() && t.dirs[d].secret_dir.is_some() {
            t.dirs[d].secret_top = !t.dirs[d].in_gitdir;
        }
    }
    for e in 0..t.ents.len() {
        let d = t.ents[e].dir;
        let dir = &t.dirs[d];
        let parent_is_worktree_node = dir.is_node
            && dir
                .node
                .and_then(|n| nodes.get(n))
                .map(|n| n.class == Class::Worktree)
                .unwrap_or(false);
        let in_gitdir =
            dir.in_gitdir || (parent_is_worktree_node && t.ents[e].name == OsStr::new(".git"));
        let in_artifact = dir.in_artifact;
        // Git's own bookkeeping names (refs, reflogs, packed-refs) are branch names.
        let git_named = in_gitdir
            && (dir.git_named || (dir.git_top && t.ents[e].name == OsStr::new("packed-refs")));
        let secret = if in_gitdir {
            None
        } else if let Some(k) = &dir.secret_dir {
            Some(k.clone())
        } else {
            t.ents[e]
                .name
                .to_str()
                .and_then(|n| rules.secret_file(n, in_artifact))
                .map(|k| k.to_string())
        };
        t.ents[e].gitdir_secret = in_gitdir
            && (dir.secret_dir.is_some()
                || (!git_named
                    && t.ents[e]
                        .name
                        .to_str()
                        .and_then(|n| rules.secret_file(n, false))
                        .is_some()));
        t.ents[e].in_gitdir = in_gitdir;
        t.ents[e].secret = secret;
    }
}

/// Count each inode once, in its first name in walk order; spread secret status to every name
/// of an inode that has a secret name. Returns the names of every multi-name inode.
fn own_inodes(
    t: &mut Tree,
    ents_order: &[usize],
    totals: &mut PieceTotals,
) -> HashMap<(u64, u64), Vec<usize>> {
    let identity = has_identity();
    let mut names: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (i, &e) in ents_order.iter().enumerate() {
        t.ents[e].order = i;
        let m = t.ents[e].m;
        if identity && t.ents[e].ok && m.nlink > 1 {
            let v = names.entry((m.dev, m.ino)).or_default();
            if v.is_empty() {
                t.ents[e].counted = m.alloc;
                t.ents[e].counted_logical = m.len;
            } else {
                totals.hardlink_names += 1;
            }
            v.push(e);
        } else if t.ents[e].ok {
            t.ents[e].counted = m.alloc;
            t.ents[e].counted_logical = m.len;
        }
    }
    for list in names.values() {
        let owner = list[0];
        if (list.len() as u64) < t.ents[owner].m.nlink {
            t.ents[owner].linked_outside = true;
        }
        // Secrecy belongs to the inode, whichever side of a git-directory boundary its names
        // are on: a name in a git directory makes its git piece incomplete, any other name is a
        // secret piece of its own.
        if list
            .iter()
            .any(|&e| t.ents[e].secret.is_some() || t.ents[e].gitdir_secret)
        {
            for &e in list {
                if t.ents[e].in_gitdir {
                    t.ents[e].gitdir_secret = true;
                } else if t.ents[e].secret.is_none() {
                    t.ents[e].secret = Some("a hard link to a secret-named file".to_string());
                }
            }
        }
    }
    names
}

// ----------------------------------------------------------------------------------------------
// Cutting.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Normal,
    Git,
}

struct Cutter<'a> {
    t: &'a mut Tree,
    nodes: &'a [Node],
    roots: &'a [PathBuf],
    opts: &'a PieceOptions,
    walked_at: i64,
    pieces: Vec<Piece>,
    /// Walk-order key of each piece (for the final ordering).
    keys: Vec<usize>,
    dir_overhead: u64,
    /// Per directory: logical bytes and member count of its region (Normal context).
    agg: Vec<(u64, u64)>,
}

impl<'a> Cutter<'a> {
    fn dir_is_boundary(&self, d: usize, ctx: Ctx) -> bool {
        let dr = &self.t.dirs[d];
        if dr.other_root || dr.is_node || dr.secret_top {
            return true;
        }
        ctx == Ctx::Normal && (dr.unreadable || dr.mount_skipped || !dr.utf8)
    }

    fn ent_is_boundary(&self, e: usize, ctx: Ctx) -> bool {
        let er = &self.t.ents[e];
        if er.secret.is_some() {
            return true;
        }
        ctx == Ctx::Normal && (!er.ok || er.m.kind == b'o' || !er.utf8)
    }

    fn compute_agg(&mut self, dirs_order: &[usize]) {
        self.agg = vec![(0, 0); self.t.dirs.len()];
        for &d in dirs_order.iter().rev() {
            let mut sum = (0u64, 0u64);
            for c in &self.t.dirs[d].children {
                match *c {
                    Child::Ent(e) => {
                        if !self.ent_is_boundary(e, Ctx::Normal) {
                            sum.0 = sum.0.saturating_add(self.t.ents[e].m.len);
                            sum.1 += 1;
                        }
                    }
                    Child::Dir(c) => {
                        if !self.dir_is_boundary(c, Ctx::Normal) {
                            sum.0 = sum.0.saturating_add(self.agg[c].0);
                            sum.1 += self.agg[c].1;
                        }
                    }
                }
            }
            self.agg[d] = sum;
        }
    }

    fn fits(&self, bytes: u64, count: u64) -> bool {
        bytes <= self.opts.max_piece_bytes && count <= self.opts.max_piece_files
    }

    fn rel(&self, base: usize, d: usize, name: Option<&OsStr>) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(n) = name {
            parts.push(n.to_string_lossy().into_owned());
        }
        let mut cur = d;
        while cur != base {
            parts.push(self.t.dirs[cur].name.to_string_lossy().into_owned());
            match self.t.dirs[cur].parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        parts.reverse();
        parts.join("/")
    }

    fn group_of(&self, d: usize, node: Option<usize>) -> String {
        if let Some(n) = node.and_then(|n| self.nodes.get(n)) {
            return n.path.clone();
        }
        let root_dir = self
            .t
            .roots
            .iter()
            .copied()
            .find(|&r| self.t.dirs[r].root == self.t.dirs[d].root);
        let root_path = &self.roots[self.t.dirs[d].root];
        // The root's direct child holding d, or the root itself.
        let mut cur = d;
        let mut child = None;
        while let Some(p) = self.t.dirs[cur].parent {
            child = Some(cur);
            if Some(p) == root_dir {
                break;
            }
            cur = p;
        }
        match child {
            Some(c) if self.t.dirs[c].parent == root_dir => {
                self.t.dirs[c].path.to_string_lossy().into_owned()
            }
            _ => root_path.to_string_lossy().into_owned(),
        }
    }

    fn member(&self, base: usize, e: usize) -> Member {
        let er = &self.t.ents[e];
        Member(
            self.rel(base, er.dir, Some(&er.name)),
            (er.m.kind as char).to_string(),
            er.m.len,
            er.m.mtime,
            er.m.nsec,
            er.m.dev,
            er.m.ino,
            er.counted,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push_piece(
        &mut self,
        kind: PieceKind,
        anchor: usize,
        ents: &[usize],
        dirs_alloc: u64,
        excludes: Vec<String>,
        secret: Option<String>,
        mut incomplete: Option<String>,
        key: usize,
    ) {
        let id = self.pieces.len();
        let mut members: Vec<Member> = ents.iter().map(|&e| self.member(anchor, e)).collect();
        members.sort_by(|a, b| a.0.cmp(&b.0));
        let mut bytes = dirs_alloc;
        let mut logical = 0u64;
        let mut linked = 0u64;
        let mut newest: Option<i64> = None;
        for &e in ents {
            let er = &self.t.ents[e];
            bytes = bytes.saturating_add(er.counted);
            logical = logical.saturating_add(er.counted_logical);
            if er.linked_outside {
                linked = linked.saturating_add(er.counted);
            }
            if er.ok && !(kind == PieceKind::GitNode && er.in_gitdir) {
                newest = Some(newest.map_or(er.m.mtime, |n: i64| n.max(er.m.mtime)));
            }
        }
        for &e in ents {
            self.t.ents[e].piece = id;
        }
        // The choke point for every piece kind other than `git-node`: a git directory is handled
        // only whole, as part of its repository's `git-node` piece under the preserve obligation
        // (where a secret-named file makes that piece incomplete, in `collect`). So any other
        // piece that lies in a git directory or holds a file of one (the piece of a scan root
        // inside a git directory, secret-named or not) is incomplete, and kept.
        if incomplete.is_none()
            && kind != PieceKind::GitNode
            && (self.t.dirs[anchor].in_gitdir || ents.iter().any(|&e| self.t.ents[e].in_gitdir))
        {
            incomplete = Some(
                "it lies inside a git directory, which is handled only whole, as part of its \
                 repository; kept"
                    .to_string(),
            );
        }
        if incomplete.is_none() && !has_identity() {
            incomplete = Some(NO_IDENTITY.to_string());
        }
        let node = self.t.dirs[anchor].node;
        let mut excludes = excludes;
        excludes.sort();
        self.pieces.push(Piece {
            id,
            kind,
            path: self.t.dirs[anchor].path.to_string_lossy().into_owned(),
            members,
            excludes,
            node,
            group: self.group_of(anchor, node),
            entries: ents.len() as u64,
            bytes,
            logical_bytes: logical,
            bytes_linked_outside: linked,
            newest_mtime: newest,
            walked_at: self.walked_at,
            secret,
            incomplete,
        });
        self.keys.push(key);
    }

    /// Everything below `d` that belongs to its region: members, directory blocks, the
    /// boundaries met (with their relative paths) and, in the git context, what makes the piece
    /// incomplete. `take_all` takes every entry (a secret or unreadable directory).
    fn collect(&self, d: usize, ctx: Ctx, take_all: bool) -> Region {
        let mut r = Region {
            dirs_alloc: self.t.dirs[d].alloc,
            ..Region::default()
        };
        let mut stack = vec![d];
        while let Some(cur) = stack.pop() {
            let dr = &self.t.dirs[cur];
            if ctx == Ctx::Git && cur != d && (dr.unreadable || dr.mount_skipped || !dr.utf8) {
                r.incomplete.get_or_insert_with(|| {
                    format!("{} could not be read whole", self.rel(d, cur, None))
                });
            }
            if cur == d && ctx == Ctx::Git && (dr.unreadable || !dr.utf8) {
                r.incomplete
                    .get_or_insert_with(|| "its directory could not be read whole".to_string());
            }
            for c in &dr.children {
                match *c {
                    Child::Ent(e) => {
                        let er = &self.t.ents[e];
                        if !take_all && self.ent_is_boundary(e, ctx) {
                            r.excludes.push(self.rel(d, er.dir, Some(&er.name)));
                            r.boundary_ents.push(e);
                            continue;
                        }
                        if ctx == Ctx::Git {
                            if !er.ok || er.m.kind == b'o' || !er.utf8 {
                                r.incomplete.get_or_insert_with(|| {
                                    format!(
                                        "{} is unreadable, a FIFO, socket or device file, or not a valid UTF-8 name",
                                        self.rel(d, er.dir, Some(&er.name))
                                    )
                                });
                            }
                            if er.gitdir_secret {
                                r.incomplete.get_or_insert_with(|| {
                                    format!(
                                        "its git directory holds the secret-named file {}, which the \
                                         preserve obligation would back up",
                                        self.rel(d, er.dir, Some(&er.name))
                                    )
                                });
                            }
                        }
                        r.ents.push(e);
                    }
                    Child::Dir(c) => {
                        let cd = &self.t.dirs[c];
                        if cd.other_root {
                            // Another scan root inside the git directory: the git directory is
                            // not whole in this piece, exactly as when that directory is a
                            // mount the walk does not enter, whichever roots were given.
                            if ctx == Ctx::Git && cd.in_gitdir {
                                r.incomplete.get_or_insert_with(|| {
                                    format!(
                                        "{} is another scan root inside its git directory, so \
                                         the git directory is not whole in this piece",
                                        self.rel(d, c, None)
                                    )
                                });
                            }
                            r.excludes.push(self.rel(d, c, None));
                            continue;
                        }
                        if !take_all && self.dir_is_boundary(c, ctx) {
                            r.excludes.push(self.rel(d, c, None));
                            r.boundary_dirs.push(c);
                            continue;
                        }
                        r.dirs_alloc = r.dirs_alloc.saturating_add(cd.alloc);
                        stack.push(c);
                    }
                }
            }
        }
        r
    }

    fn cut_all(&mut self, dirs_order: &[usize]) {
        let dir_key: HashMap<usize, usize> = dirs_order
            .iter()
            .enumerate()
            .map(|(i, &d)| (d, i))
            .collect();
        let mut tasks: Vec<Task> = self
            .t
            .roots
            .iter()
            .rev()
            .map(|&r| {
                if self.t.dirs[r].is_node || self.t.dirs[r].secret_top {
                    Task::Boundary(r)
                } else {
                    Task::Cut(r)
                }
            })
            .collect();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Cut(d) => self.cut(d, &dir_key, &mut tasks),
                Task::Boundary(d) => self.boundary(d, &dir_key, &mut tasks),
                Task::Ent(e) => {
                    let er = &self.t.ents[e];
                    let secret = er.secret.clone();
                    let incomplete = if secret.is_some() {
                        None
                    } else if !er.ok {
                        Some("its metadata could not be read".to_string())
                    } else if !er.utf8 {
                        Some(
                            "its name is not valid UTF-8, so it cannot be written back exactly"
                                .to_string(),
                        )
                    } else if er.m.kind == b'o' {
                        Some("a FIFO, socket or device file".to_string())
                    } else {
                        None
                    };
                    let d = er.dir;
                    let key = dir_key.get(&d).copied().unwrap_or(0);
                    self.push_piece(
                        PieceKind::Files,
                        d,
                        &[e],
                        0,
                        Vec::new(),
                        secret,
                        incomplete,
                        key,
                    );
                }
            }
        }
    }

    fn queue_boundaries(&self, r: &Region, tasks: &mut Vec<Task>) {
        for &c in r.boundary_dirs.iter().rev() {
            tasks.push(Task::Boundary(c));
        }
        for &e in r.boundary_ents.iter().rev() {
            tasks.push(Task::Ent(e));
        }
    }

    fn cut(&mut self, d: usize, dir_key: &HashMap<usize, usize>, tasks: &mut Vec<Task>) {
        let key = dir_key.get(&d).copied().unwrap_or(0);
        let (b, n) = self.agg[d];
        // A scan root outside every node is always split: its direct children are separate
        // activity groups, and one piece must never span two of them.
        let ungrouped_root = self.t.dirs[d].parent.is_none() && self.t.dirs[d].node.is_none();
        if !ungrouped_root && self.fits(b, n) {
            let r = self.collect(d, Ctx::Normal, false);
            self.push_piece(
                PieceKind::Tree,
                d,
                &r.ents,
                r.dirs_alloc,
                r.excludes.clone(),
                None,
                None,
                key,
            );
            self.queue_boundaries(&r, tasks);
            return;
        }
        // Split: the directory's own blocks are overhead; subdirectories are cut on their own and
        // the direct files packed in name order.
        self.dir_overhead = self.dir_overhead.saturating_add(self.t.dirs[d].alloc);
        let children = self.t.dirs[d].children.clone();
        let mut batch: Vec<usize> = Vec::new();
        let mut batch_bytes = 0u64;
        let mut later: Vec<Task> = Vec::new();
        for c in children {
            match c {
                Child::Ent(e) => {
                    if self.ent_is_boundary(e, Ctx::Normal) {
                        later.push(Task::Ent(e));
                        continue;
                    }
                    let len = self.t.ents[e].m.len;
                    if !batch.is_empty()
                        && !self.fits(batch_bytes.saturating_add(len), batch.len() as u64 + 1)
                    {
                        let ents = std::mem::take(&mut batch);
                        self.push_piece(PieceKind::Files, d, &ents, 0, Vec::new(), None, None, key);
                        batch_bytes = 0;
                    }
                    batch.push(e);
                    batch_bytes = batch_bytes.saturating_add(len);
                }
                Child::Dir(c) => {
                    if self.t.dirs[c].other_root {
                        continue;
                    }
                    if self.dir_is_boundary(c, Ctx::Normal) {
                        later.push(Task::Boundary(c));
                    } else {
                        later.push(Task::Cut(c));
                    }
                }
            }
        }
        if !batch.is_empty() {
            self.push_piece(PieceKind::Files, d, &batch, 0, Vec::new(), None, None, key);
        }
        for t in later.into_iter().rev() {
            tasks.push(t);
        }
    }

    fn boundary(&mut self, d: usize, dir_key: &HashMap<usize, usize>, tasks: &mut Vec<Task>) {
        let key = dir_key.get(&d).copied().unwrap_or(0);
        let dr = &self.t.dirs[d];
        if dr.other_root {
            return;
        }
        if dr.secret_top {
            let kind = dr.secret_dir.clone();
            let r = self.collect(d, Ctx::Normal, true);
            self.push_piece(
                PieceKind::Tree,
                d,
                &r.ents,
                r.dirs_alloc,
                r.excludes.clone(),
                kind,
                None,
                key,
            );
            return;
        }
        if dr.is_node {
            let class = dr.node.and_then(|n| self.nodes.get(n)).map(|n| n.class);
            if class.map(|c| c.is_git()).unwrap_or(false) {
                let r = self.collect(d, Ctx::Git, false);
                self.push_piece(
                    PieceKind::GitNode,
                    d,
                    &r.ents,
                    r.dirs_alloc,
                    r.excludes.clone(),
                    None,
                    r.incomplete.clone(),
                    key,
                );
                self.queue_boundaries(&r, tasks);
            } else {
                self.cut(d, dir_key, tasks);
            }
            return;
        }
        // Unreadable, not entered, or not a valid UTF-8 name: one incomplete piece of whatever
        // it holds.
        let why = if dr.mount_skipped {
            "a mount point that was not entered"
        } else if dr.unreadable {
            "a directory whose contents could not be read whole"
        } else {
            "a directory whose name is not valid UTF-8, so it cannot be written back exactly"
        };
        let r = self.collect(d, Ctx::Normal, true);
        self.push_piece(
            PieceKind::Tree,
            d,
            &r.ents,
            r.dirs_alloc,
            r.excludes.clone(),
            None,
            Some(why.to_string()),
            key,
        );
    }
}

#[derive(Default)]
struct Region {
    ents: Vec<usize>,
    dirs_alloc: u64,
    excludes: Vec<String>,
    boundary_dirs: Vec<usize>,
    boundary_ents: Vec<usize>,
    incomplete: Option<String>,
}

enum Task {
    Cut(usize),
    Boundary(usize),
    Ent(usize),
}

// ----------------------------------------------------------------------------------------------
// Hashing.

fn same_file(a: &Meta, b: &Meta) -> bool {
    a.len == b.len && a.mtime == b.mtime && a.nsec == b.nsec && a.dev == b.dev && a.ino == b.ino
}

fn hash_file(path: &Path, want: &Meta) -> Option<String> {
    let mut f = crate::regular::open_regular(path).ok()?;
    let before = meta(&f.metadata().ok()?);
    if !same_file(&before, want) {
        return None;
    }
    let digest = hash_bounded(&mut f, want.len)?;
    let after = meta(&f.metadata().ok()?);
    if !same_file(&after, want) {
        return None;
    }
    Some(digest)
}

/// SHA-256 of exactly `len` bytes of `r`, reading at most `len + 1` bytes: a reader that ends
/// early or has more (a file that grew while it was read) gives `None`, so a hash budget bounds
/// the bytes actually read.
fn hash_bounded<R: Read>(r: &mut R, len: u64) -> Option<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = r.read(&mut buf[..want]).ok()?;
        if n == 0 {
            return None; // shorter than recorded
        }
        h.update(&buf[..n]);
        left -= n as u64;
    }
    let mut one = [0u8; 1];
    loop {
        match r.read(&mut one) {
            Ok(0) => break,
            Ok(_) => return None, // longer than recorded
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    Some(hex(&h.finalize()))
}

fn ent_path(t: &Tree, e: usize) -> PathBuf {
    t.dirs[t.ents[e].dir].path.join(&t.ents[e].name)
}

fn blobs(
    t: &Tree,
    names: &HashMap<(u64, u64), Vec<usize>>,
    opts: &PieceOptions,
    totals: &mut PieceTotals,
) -> Vec<Blob> {
    // One candidate per inode: its owner name.
    let eligible = |e: usize| -> bool {
        let er = &t.ents[e];
        er.ok
            && er.m.kind == b'f'
            && er.secret.is_none()
            && !er.gitdir_secret
            && er.counted_logical > 0
            && !er.linked_outside
            && !(has_identity() && er.m.alloc.saturating_mul(2) < er.m.len)
    };
    let mut by_len: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    let mut multi: HashSet<usize> = HashSet::new();
    for (e, er) in t.ents.iter().enumerate() {
        if er.counted_logical == 0
            || !er.ok
            || er.m.kind != b'f'
            || er.secret.is_some()
            || er.gitdir_secret
        {
            continue; // not an owner name, or not a regular non-secret file
        }
        if er.m.len >= opts.min_dup_bytes {
            by_len.entry(er.m.len).or_default().push(e);
        }
        if has_identity() && er.m.nlink > 1 {
            if let Some(list) = names.get(&(er.m.dev, er.m.ino)) {
                if list.len() > 1 {
                    multi.insert(e);
                }
            }
        }
    }
    let mut cand: Vec<usize> = Vec::new();
    for list in by_len.values() {
        if list.len() > 1 {
            cand.extend(list.iter().copied());
        }
    }
    for &e in &multi {
        cand.push(e);
    }
    cand.sort_by_key(|&e| t.ents[e].order);
    cand.dedup();
    let mut chosen: Vec<usize> = Vec::new();
    let mut budget = 0u64;
    for e in cand {
        if !eligible(e) {
            totals.unhashed_candidates += 1;
            continue;
        }
        let len = t.ents[e].m.len;
        if let Some(max) = opts.max_hash_bytes {
            if budget.saturating_add(len) > max {
                totals.unhashed_candidates += 1;
                continue;
            }
        }
        budget = budget.saturating_add(len);
        chosen.push(e);
    }
    let hashed: Vec<(usize, Option<String>)> = chosen
        .par_iter()
        .map(|&e| (e, hash_file(&ent_path(t, e), &t.ents[e].m)))
        .collect();
    let mut by_sha: HashMap<String, Vec<usize>> = HashMap::new();
    for (e, h) in hashed {
        match h {
            Some(h) => {
                totals.hashed_files += 1;
                totals.hashed_bytes = totals.hashed_bytes.saturating_add(t.ents[e].m.len);
                by_sha.entry(h).or_default().push(e);
            }
            None => totals.unhashed_candidates += 1,
        }
    }
    let mut out: Vec<(usize, Blob)> = Vec::new();
    for (sha, owners) in by_sha {
        let mut copies: Vec<usize> = Vec::new();
        for &o in &owners {
            let m = t.ents[o].m;
            match names.get(&(m.dev, m.ino)) {
                Some(list) if has_identity() && m.nlink > 1 => copies.extend(list.iter().copied()),
                _ => copies.push(o),
            }
        }
        if copies.len() < 2 {
            continue;
        }
        copies.sort_by_key(|&e| t.ents[e].order);
        let mut inode_no: HashMap<(u64, u64, usize), u32> = HashMap::new();
        let mut out_copies = Vec::new();
        for &e in &copies {
            let m = t.ents[e].m;
            // Without identity every name is its own inode.
            let k = if has_identity() {
                (m.dev, m.ino, 0)
            } else {
                (0, 0, e)
            };
            let next = inode_no.len() as u32;
            let n = *inode_no.entry(k).or_insert(next);
            out_copies.push(BlobCopy {
                path: ent_path(t, e).to_string_lossy().into_owned(),
                piece: t.ents[e].piece,
                inode: n,
                bytes: t.ents[e].counted,
            });
        }
        let first = copies[0];
        let bytes = t.ents[first].counted;
        let distinct = inode_no.len() as u64;
        totals.duplicate_bytes = totals
            .duplicate_bytes
            .saturating_add(bytes.saturating_mul(distinct.saturating_sub(1)));
        out.push((
            t.ents[first].order,
            Blob {
                sha256: sha,
                size: t.ents[first].m.len,
                bytes,
                copies: out_copies,
            },
        ));
    }
    out.sort_by_key(|(k, _)| *k);
    out.into_iter().map(|(_, b)| b).collect()
}

// ----------------------------------------------------------------------------------------------
// The survey.

/// Survey `roots` (the scan's walked roots, already normalised) into pieces. `nodes` are the
/// scan's classified directories.
pub fn survey(
    roots: &[PathBuf],
    nodes: &[Node],
    one_file_system: bool,
    rules: &RuleSet,
    opts: &PieceOptions,
) -> PieceSurvey {
    let walked_at = unix_now();
    let mut totals = PieceTotals::default();
    let mut t = read_tree(roots, one_file_system, nodes);
    let dirs_order = preorder_dirs(&t);
    let ents_order = preorder_ents(&t);
    annotate(&mut t, roots, nodes, rules, &dirs_order);
    let names = own_inodes(&mut t, &ents_order, &mut totals);

    let mut cutter = Cutter {
        t: &mut t,
        nodes,
        roots,
        opts,
        walked_at,
        pieces: Vec::new(),
        keys: Vec::new(),
        dir_overhead: 0,
        agg: Vec::new(),
    };
    cutter.compute_agg(&dirs_order);
    cutter.cut_all(&dirs_order);
    let dir_overhead = cutter.dir_overhead;
    let mut pieces = std::mem::take(&mut cutter.pieces);
    let keys = std::mem::take(&mut cutter.keys);
    drop(cutter);

    // Walk order: by the anchor directory's preorder position, then by the first member's.
    let mut idx: Vec<usize> = (0..pieces.len()).collect();
    let piece_first: Vec<usize> = {
        let mut v = vec![usize::MAX; pieces.len()];
        for er in &t.ents {
            if er.piece != usize::MAX && er.order < v[er.piece] {
                v[er.piece] = er.order;
            }
        }
        v
    };
    idx.sort_by_key(|&i| (keys[i], piece_first[i], i));
    let mut remap = vec![0usize; pieces.len()];
    for (new, &old) in idx.iter().enumerate() {
        remap[old] = new;
    }
    for er in t.ents.iter_mut() {
        if er.piece != usize::MAX {
            er.piece = remap[er.piece];
        }
    }
    let mut ordered: Vec<Option<Piece>> = pieces.drain(..).map(Some).collect();
    let mut pieces: Vec<Piece> = idx
        .iter()
        .map(|&old| ordered[old].take().expect("each piece once"))
        .collect();
    for (i, p) in pieces.iter_mut().enumerate() {
        p.id = i;
    }

    let blobs = blobs(&t, &names, opts, &mut totals);

    totals.pieces = pieces.len() as u64;
    for p in &pieces {
        totals.entries += p.entries;
        totals.bytes = totals.bytes.saturating_add(p.bytes);
        totals.logical_bytes = totals.logical_bytes.saturating_add(p.logical_bytes);
    }
    totals.dir_overhead_bytes = dir_overhead;
    PieceSurvey {
        options: opts.clone(),
        pieces,
        blobs,
        totals,
    }
}

// ----------------------------------------------------------------------------------------------
// Re-check.

/// What a re-check covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecheckScope {
    /// Every member.
    #[default]
    All,
    /// A `git-node` piece's work tree only: leaves out its git directory (used only after apply's
    /// own recovery step wrote into it; the preserve obligation then covers it).
    WorkTree,
}

/// A piece no longer matches the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// The first path that differs (relative to the piece's `path`), empty when the piece as a
    /// whole could not be checked.
    pub path: String,
    /// What differs, in words.
    pub reason: String,
}

impl std::fmt::Display for Drift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.path, self.reason)
        }
    }
}

fn drift(path: &str, reason: impl Into<String>) -> Drift {
    Drift {
        path: path.to_string(),
        reason: reason.into(),
    }
}

fn under(rel: &str, prefix: &str) -> bool {
    rel == prefix
        || (rel.len() > prefix.len()
            && rel.starts_with(prefix)
            && rel.as_bytes()[prefix.len()] == b'/')
}

fn compare(rel: &str, want: &Member, md: &std::fs::Metadata, walked_at: i64) -> Result<(), Drift> {
    let m = meta(md);
    if m.is_dir {
        return Err(drift(rel, "is now a directory"));
    }
    if (m.kind as char).to_string() != want.1 {
        return Err(drift(rel, "changed kind"));
    }
    if m.len != want.2 {
        return Err(drift(rel, "changed length"));
    }
    if m.mtime != want.3 || m.nsec != want.4 {
        return Err(drift(rel, "changed modification time"));
    }
    if m.dev != want.5 || m.ino != want.6 {
        return Err(drift(rel, "is a different file now (its inode changed)"));
    }
    if m.ctime >= walked_at {
        return Err(drift(
            rel,
            "changed after the scan read it (status-change time)",
        ));
    }
    Ok(())
}

/// Enumerate a scan piece again, read-only, and compare it with its `members`
/// (docs/SCAN-FORMAT.md, "Re-check"). An apply step calls this before acting on a piece and
/// skips the piece on `Err`.
pub fn recheck(piece: &Piece, scope: RecheckScope) -> Result<(), Drift> {
    if !has_identity() {
        return Err(drift("", NO_IDENTITY));
    }
    let base = Path::new(&piece.path);
    let skip_git = scope == RecheckScope::WorkTree && piece.kind == PieceKind::GitNode;
    // A git-node piece with no `.git` member is a bare repository: all of it is the git
    // directory, so the work-tree scope leaves all of it out (the preserve obligation covers it).
    let bare =
        piece.kind == PieceKind::GitNode && !piece.members.iter().any(|m| under(&m.0, ".git"));
    let in_scope = |rel: &str| !(skip_git && (bare || under(rel, ".git")));
    let mut want: BTreeMap<&str, &Member> = BTreeMap::new();
    for m in &piece.members {
        if in_scope(&m.0) {
            want.insert(m.0.as_str(), m);
        }
    }
    if piece.kind == PieceKind::Files {
        for (rel, m) in &want {
            let md =
                std::fs::symlink_metadata(base.join(rel)).map_err(|_| drift(rel, "is missing"))?;
            compare(rel, m, &md, piece.walked_at)?;
        }
        return Ok(());
    }
    // The piece's own directory is followed if it is a symlink (a scan root spelled through
    // one); its members are compared by device and inode, so a different target is drift.
    let md = std::fs::metadata(base).map_err(|_| drift("", "the piece's directory is missing"))?;
    if !md.is_dir() {
        return Err(drift("", "the piece's directory is no longer a directory"));
    }
    // A directory's status-change time moves when an entry is added to, removed from or renamed
    // in it, so a new (even empty) directory or entry below the piece shows up on its parent.
    // Reported after any entry-level difference, which names the path more usefully.
    let dir_changed = |md: &std::fs::Metadata| meta(md).ctime >= piece.walked_at;
    let mut dir_drift: Option<Drift> = None;
    if !(skip_git && bare) && dir_changed(&md) {
        dir_drift = Some(drift(
            "",
            "the piece's directory changed after the scan read it (status-change time)",
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(base.to_path_buf(), String::new())];
    while let Some((dir, rel_dir)) = stack.pop() {
        let rd = std::fs::read_dir(&dir)
            .map_err(|e| drift(&rel_dir, format!("cannot be listed: {e}")))?;
        let mut names: Vec<OsString> = Vec::new();
        for e in rd {
            let e = e.map_err(|e| drift(&rel_dir, format!("cannot be listed: {e}")))?;
            names.push(e.file_name());
        }
        names.sort();
        for name in names {
            let n = name.to_string_lossy();
            let rel = if rel_dir.is_empty() {
                n.into_owned()
            } else {
                format!("{rel_dir}/{n}")
            };
            if piece.excludes.iter().any(|x| under(&rel, x)) || !in_scope(&rel) {
                continue;
            }
            let p = dir.join(&name);
            let md = std::fs::symlink_metadata(&p).map_err(|_| drift(&rel, "cannot be read"))?;
            if md.is_dir() {
                if dir_drift.is_none() && dir_changed(&md) {
                    dir_drift = Some(drift(
                        &rel,
                        "directory changed after the scan read it (status-change time)",
                    ));
                }
                stack.push((p, rel));
                continue;
            }
            match want.get(rel.as_str()) {
                Some(m) => compare(&rel, m, &md, piece.walked_at)?,
                None => return Err(drift(&rel, "appeared after the scan")),
            }
            seen.insert(rel);
        }
    }
    if let Some((rel, _)) = want.iter().find(|(r, _)| !seen.contains(**r)) {
        return Err(drift(rel, "is missing"));
    }
    match dir_drift {
        Some(d) => Err(d),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::hash_bounded;
    use std::io::Read;

    #[test]
    fn hashing_reads_at_most_one_byte_past_the_recorded_length() {
        // Exact length: hashed.
        let mut exact: &[u8] = b"abcdef";
        assert!(hash_bounded(&mut exact, 6).is_some());
        // Shorter than recorded: refused.
        let mut short: &[u8] = b"abc";
        assert!(hash_bounded(&mut short, 6).is_none());
        // A source that never ends (a file that keeps growing): refused after len + 1 bytes.
        let mut endless = std::io::repeat(7).take(u64::MAX);
        assert!(hash_bounded(&mut endless, 1000).is_none());
        let read = u64::MAX - endless.limit();
        assert_eq!(read, 1001, "read {read} bytes for a 1000-byte budget");
    }
}
