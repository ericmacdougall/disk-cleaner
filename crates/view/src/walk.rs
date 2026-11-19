//! The size walk: a read-only parallel walk that records every folder and file in a [`Tree`],
//! with throttled progress and cancellation.
//!
//! It keeps core's safety posture: symlinks are never followed (a link counts as the link
//! object itself), `one_file_system` does not enter other file systems, and no file is opened
//! or read: only metadata is looked at. Allocated bytes are blocks times 512 on Unix and the
//! file length elsewhere; on Unix a hard-linked inode is counted once, as core does.

use crate::sanitize::display_name;
use crate::tree::{flags, EntryKind, NewEntry, NodeId, TopFile, Tree, TreeBuilder, WalkStats};
use jwalk::WalkDirGeneric;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Called for every entry from the walk's worker threads before its metadata is read. Tests use
/// it to hold a walk inside a metadata call.
pub type EntryHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Walk configuration.
#[derive(Clone)]
pub struct WalkOptions {
    pub roots: Vec<PathBuf>,
    /// Do not enter folders on another device than their root (Unix; ignored elsewhere).
    pub one_file_system: bool,
    /// How many of the biggest files to keep.
    pub top_files: usize,
    /// Worker threads for this walk's own pool (0: one per CPU).
    pub threads: usize,
    /// Least time between two progress reports.
    pub progress_interval: Duration,
    /// Test hook (see [`EntryHook`]).
    pub entry_hook: Option<EntryHook>,
}

impl WalkOptions {
    /// Defaults for the given roots: stay on one file system, 200 top files, progress at most
    /// ten times a second.
    pub fn new(roots: Vec<PathBuf>) -> WalkOptions {
        WalkOptions {
            roots,
            one_file_system: true,
            top_files: 200,
            threads: 0,
            progress_interval: Duration::from_millis(100),
            entry_hook: None,
        }
    }
}

/// A progress report.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Progress {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub elapsed_ms: u64,
    /// The folder being read (display-safe).
    pub current: String,
}

/// Why a walk produced no tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkError {
    Cancelled,
    NoRoots,
    /// A root is not an absolute path to an existing folder.
    Root {
        path: String,
        message: String,
    },
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Cancelled => write!(f, "the scan was cancelled"),
            WalkError::NoRoots => write!(f, "choose at least one folder to scan"),
            WalkError::Root { path, message } => write!(f, "{path}: {message}"),
        }
    }
}

impl std::error::Error for WalkError {}

/// Lets a report through at most once per interval, plus the final one.
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(interval: Duration) -> Throttle {
        Throttle {
            interval,
            last: None,
        }
    }

    /// True when a report may be sent at `now`; records it.
    pub fn ready(&mut self, now: Instant) -> bool {
        match self.last {
            Some(t) if now.duration_since(t) < self.interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// Check that a root is an absolute path to an existing folder (not a symlink to one).
pub fn check_root(path: &Path) -> Result<(), WalkError> {
    let shown = display_name(&path.to_string_lossy());
    if !path.is_absolute() {
        return Err(WalkError::Root {
            path: shown,
            message: "is not an absolute path".into(),
        });
    }
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => Ok(()),
        Ok(_) => Err(WalkError::Root {
            path: shown,
            message: "is not a folder".into(),
        }),
        Err(e) => Err(WalkError::Root {
            path: shown,
            message: format!("cannot be read ({e})"),
        }),
    }
}

#[derive(Debug, Clone, Default)]
struct Meta {
    ok: bool,
    alloc: u64,
    logical: u64,
    dev: u64,
    ino: u64,
    nlink: u64,
    mtime: i64,
    mount_skipped: bool,
}

#[cfg(unix)]
fn meta_from(md: &Metadata) -> Meta {
    use std::os::unix::fs::MetadataExt;
    Meta {
        ok: true,
        alloc: md.blocks().saturating_mul(512),
        logical: md.len(),
        dev: md.dev(),
        ino: md.ino(),
        nlink: md.nlink(),
        mtime: md.mtime(),
        mount_skipped: false,
    }
}

#[cfg(not(unix))]
fn meta_from(md: &Metadata) -> Meta {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Meta {
        ok: true,
        alloc: md.len(),
        logical: md.len(),
        dev: 0,
        ino: 0,
        nlink: 1,
        mtime,
        mount_skipped: false,
    }
}

/// Drop roots that lie inside another root, and duplicates.
fn distinct_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = roots.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for r in sorted {
        if !out.iter().any(|o| r.starts_with(o)) {
            out.push(r);
        }
    }
    out
}

fn kind_of(ft: &std::fs::FileType) -> EntryKind {
    if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

struct Accum {
    builder: TreeBuilder,
    stats: WalkStats,
    seen_inodes: HashSet<(u64, u64)>,
    heap: BinaryHeap<Reverse<(u64, NodeId)>>,
    top_n: usize,
}

impl Accum {
    fn note_file(&mut self, id: NodeId, alloc: u64) {
        if self.top_n == 0 || alloc == 0 {
            return;
        }
        if self.heap.len() < self.top_n {
            self.heap.push(Reverse((alloc, id)));
        } else if let Some(Reverse((smallest, _))) = self.heap.peek() {
            if alloc > *smallest {
                self.heap.pop();
                self.heap.push(Reverse((alloc, id)));
            }
        }
    }
}

/// Walk the roots. `progress` is called at most once per `progress_interval` and once at the
/// end. Setting `cancel` stops the walk: no further folder is entered and `Cancelled` is
/// returned.
pub fn walk(
    opts: &WalkOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Progress),
) -> Result<Tree, WalkError> {
    if opts.roots.is_empty() {
        return Err(WalkError::NoRoots);
    }
    for r in &opts.roots {
        check_root(r)?;
    }
    let started = Instant::now();
    let roots = distinct_roots(&opts.roots);
    let mut acc = Accum {
        builder: TreeBuilder::with_capacity(1024, true),
        stats: WalkStats::default(),
        seen_inodes: HashSet::new(),
        heap: BinaryHeap::new(),
        top_n: opts.top_files,
    };
    let mut throttle = Throttle::new(opts.progress_interval);
    let one_fs = opts.one_file_system && cfg!(unix);
    let cancelled = Arc::new(AtomicBool::new(false));

    for root in &roots {
        let root_md = std::fs::symlink_metadata(root).map_err(|e| WalkError::Root {
            path: display_name(&root.to_string_lossy()),
            message: format!("cannot be read ({e})"),
        })?;
        let root_meta = meta_from(&root_md);
        let root_dev = root_meta.dev;
        let threads = if opts.threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        } else {
            opts.threads
        };
        let hook = opts.entry_hook.clone();
        // The walk's threads read the caller's flag through this shared copy.
        let stop = Arc::clone(&cancelled);
        let walker = WalkDirGeneric::<((), Meta)>::new(root)
            .skip_hidden(false)
            .follow_links(false)
            .sort(false)
            .parallelism(jwalk::Parallelism::RayonNewPool(threads))
            .process_read_dir(move |_depth, _path, _state, children| {
                if stop.load(Ordering::Relaxed) {
                    children.clear();
                    return;
                }
                for entry in children.iter_mut().flatten() {
                    if stop.load(Ordering::Relaxed) {
                        entry.read_children = None;
                        continue;
                    }
                    let path = entry.path();
                    if let Some(h) = &hook {
                        h(&path);
                    }
                    match std::fs::symlink_metadata(&path) {
                        Ok(md) => {
                            let mut m = meta_from(&md);
                            if entry.file_type.is_dir() && one_fs && m.dev != root_dev {
                                entry.read_children = None;
                                m.mount_skipped = true;
                            }
                            entry.client_state = m;
                        }
                        Err(_) => entry.client_state = Meta::default(),
                    }
                }
            });

        // Ancestors of the next entry: (depth, id). jwalk yields depth first.
        let mut stack: Vec<(usize, NodeId)> = Vec::new();
        for item in walker {
            if cancel.load(Ordering::Relaxed) {
                cancelled.store(true, Ordering::Relaxed);
                break;
            }
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => {
                    acc.stats.unreadable += 1;
                    continue;
                }
            };
            let depth = entry.depth;
            let meta = if depth == 0 {
                root_meta.clone()
            } else {
                entry.client_state.clone()
            };
            while stack.last().is_some_and(|&(d, _)| d >= depth) {
                stack.pop();
            }
            let parent = stack.last().map(|&(_, id)| id);
            if depth > 0 && parent.is_none() {
                // Out of order: cannot place it. Count it so the totals say so.
                acc.stats.unreadable += 1;
                continue;
            }
            let raw_name = if depth == 0 {
                root.to_string_lossy().into_owned()
            } else {
                entry.file_name.to_string_lossy().into_owned()
            };
            let mut bits = 0u8;
            if depth > 0 && entry.file_name.to_str().is_none() {
                bits |= flags::LOSSY_NAME;
            }
            if !meta.ok {
                bits |= flags::NO_METADATA;
                acc.stats.unreadable += 1;
            }
            let kind = kind_of(&entry.file_type);
            let (alloc, logical) = if kind == EntryKind::Dir {
                (meta.alloc, meta.logical)
            } else if meta.nlink > 1 && !acc.seen_inodes.insert((meta.dev, meta.ino)) {
                bits |= flags::HARDLINK_DUPLICATE;
                acc.stats.hardlink_duplicates_skipped += 1;
                (0, 0)
            } else {
                (meta.alloc, meta.logical)
            };
            if kind == EntryKind::Dir {
                if entry
                    .read_children
                    .as_ref()
                    .and_then(|rc| rc.error())
                    .is_some()
                {
                    bits |= flags::UNREADABLE;
                    acc.stats.unreadable += 1;
                }
                if meta.mount_skipped {
                    bits |= flags::MOUNT_SKIPPED;
                    acc.stats.mount_points_skipped += 1;
                }
            }
            let Some(id) = acc.builder.push(NewEntry {
                parent,
                name: &raw_name,
                kind,
                size: logical,
                alloc,
                files: 0,
                mtime: meta.mtime,
                flags: bits,
            }) else {
                acc.stats.unreadable += 1;
                continue;
            };
            acc.stats.allocated_bytes += alloc;
            acc.stats.logical_bytes += logical;
            match kind {
                EntryKind::Dir => {
                    acc.stats.dirs += 1;
                    stack.push((depth, id));
                }
                EntryKind::Symlink => {
                    acc.stats.files += 1;
                    acc.stats.symlinks += 1;
                }
                _ => {
                    acc.stats.files += 1;
                    acc.note_file(id, alloc);
                }
            }
            if throttle.ready(Instant::now()) {
                progress(&Progress {
                    files: acc.stats.files,
                    dirs: acc.stats.dirs,
                    bytes: acc.stats.allocated_bytes,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    current: current_path(&stack, root),
                });
            }
        }
        if cancel.load(Ordering::Relaxed) {
            cancelled.store(true, Ordering::Relaxed);
            return Err(WalkError::Cancelled);
        }
    }
    acc.stats.elapsed_ms = started.elapsed().as_millis() as u64;
    progress(&Progress {
        files: acc.stats.files,
        dirs: acc.stats.dirs,
        bytes: acc.stats.allocated_bytes,
        elapsed_ms: acc.stats.elapsed_ms,
        current: String::new(),
    });
    let mut heap = acc.heap.into_vec();
    heap.sort_by(|a, b| b.cmp(a));
    let ids: Vec<(u64, NodeId)> = heap.into_iter().map(|Reverse(x)| x).collect();
    let tree = acc.builder.finish(Vec::new(), acc.stats);
    Ok(with_top_files(tree, &ids))
}

fn with_top_files(mut tree: Tree, ids: &[(u64, NodeId)]) -> Tree {
    tree.top_files = ids
        .iter()
        .map(|&(bytes, id)| TopFile {
            id: Some(id),
            path: display_name(&tree.path(id).to_string_lossy()),
            bytes,
        })
        .collect();
    tree
}

/// The folder at the top of the stack, as a display-safe path. Only called when a progress
/// report is due, so its cost does not matter.
fn current_path(stack: &[(usize, NodeId)], root: &Path) -> String {
    match stack.len() {
        0 => display_name(&root.to_string_lossy()),
        n => format!(
            "{} ({} folders deep)",
            display_name(&root.to_string_lossy()),
            n - 1
        ),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests write fixtures
mod tests {
    use super::*;

    #[test]
    fn throttle_lets_one_report_through_per_interval() {
        let mut t = Throttle::new(Duration::from_millis(100));
        let start = Instant::now();
        let mut sent = 0;
        for ms in 0..1000u64 {
            if t.ready(start + Duration::from_millis(ms)) {
                sent += 1;
            }
        }
        assert_eq!(sent, 10);
    }

    #[test]
    fn nested_and_duplicate_roots_collapse() {
        let r = distinct_roots(&[
            PathBuf::from("/a/b"),
            PathBuf::from("/a"),
            PathBuf::from("/a"),
            PathBuf::from("/c"),
        ]);
        assert_eq!(r, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }

    #[test]
    fn relative_and_missing_roots_are_refused() {
        assert!(matches!(
            check_root(Path::new("relative/path")),
            Err(WalkError::Root { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(matches!(check_root(&missing), Err(WalkError::Root { .. })));
        let file = dir.path().join("f");
        std::fs::File::create(&file).unwrap();
        assert!(matches!(check_root(&file), Err(WalkError::Root { .. })));
        assert!(check_root(dir.path()).is_ok());
    }
}
