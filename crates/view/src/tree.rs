//! An arena holding a scanned tree: one compact record per folder or file, children grouped by
//! parent, totals rolled up from the leaves. Built once by the size walk ([`crate::walk`]), a
//! synthetic generator ([`crate::synth`]) or an imported scan ([`crate::import`]); read by
//! [`crate::table::TreeView`].

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Index of an entry in a [`Tree`].
pub type NodeId = u32;

/// The parent of a root.
pub const NO_PARENT: NodeId = u32::MAX;

/// Bits of [`Tree::flags`].
pub mod flags {
    /// The folder's contents could not be listed.
    pub const UNREADABLE: u8 = 1;
    /// A mount point that the walk did not enter.
    pub const MOUNT_SKIPPED: u8 = 2;
    /// Another link to an inode already counted: its bytes count as 0 here.
    pub const HARDLINK_DUPLICATE: u8 = 4;
    /// The name was not valid Unicode and is shown with replacement characters.
    pub const LOSSY_NAME: u8 = 8;
    /// A summary line standing for what an imported scan did not list one by one.
    pub const SUMMARY: u8 = 16;
    /// The entry's metadata could not be read.
    pub const NO_METADATA: u8 = 32;
}

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
struct Entry {
    name_start: u32,
    name_len: u32,
    parent: NodeId,
    kind: EntryKind,
    flags: u8,
    size: u64,
    alloc: u64,
    files: u64,
    mtime: i64,
    first_child: u32,
    child_count: u32,
}

/// One of the biggest files.
#[derive(Debug, Clone, Serialize)]
pub struct TopFile {
    /// The entry in the tree, when the file is in it.
    pub id: Option<NodeId>,
    /// Display-safe path.
    pub path: String,
    /// Allocated bytes.
    pub bytes: u64,
}

/// Counts gathered while building the tree.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WalkStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub hardlink_duplicates_skipped: u64,
    /// Folders whose contents could not be listed plus entries whose metadata could not be read.
    pub unreadable: u64,
    pub mount_points_skipped: u64,
    pub elapsed_ms: u64,
}

/// A built tree. Entries are read through methods taking a [`NodeId`]; an id out of range
/// panics, as with slice indexing.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    entries: Vec<Entry>,
    names: String,
    children: Vec<NodeId>,
    roots: Vec<NodeId>,
    /// The biggest files, largest first.
    pub top_files: Vec<TopFile>,
    pub stats: WalkStats,
}

impl Tree {
    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the tree has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Top-level entries (scan roots).
    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }

    /// Raw name as stored (not yet display-safe; see [`crate::sanitize::display_name`]).
    pub fn name(&self, id: NodeId) -> &str {
        let e = &self.entries[id as usize];
        let start = e.name_start as usize;
        &self.names[start..start + e.name_len as usize]
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.entries[id as usize].parent;
        (p != NO_PARENT).then_some(p)
    }

    pub fn kind(&self, id: NodeId) -> EntryKind {
        self.entries[id as usize].kind
    }

    pub fn flags(&self, id: NodeId) -> u8 {
        self.entries[id as usize].flags
    }

    /// Logical bytes (file lengths), the subtree's total for a folder.
    pub fn size(&self, id: NodeId) -> u64 {
        self.entries[id as usize].size
    }

    /// Allocated bytes, the subtree's total for a folder.
    pub fn alloc(&self, id: NodeId) -> u64 {
        self.entries[id as usize].alloc
    }

    /// Files (every non-folder entry) in the subtree; 1 for a file.
    pub fn files(&self, id: NodeId) -> u64 {
        self.entries[id as usize].files
    }

    /// Newest modification time in the subtree, unix seconds.
    pub fn mtime(&self, id: NodeId) -> Option<i64> {
        let m = self.entries[id as usize].mtime;
        (m != 0).then_some(m)
    }

    /// Children in their current order.
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        let e = &self.entries[id as usize];
        let start = e.first_child as usize;
        &self.children[start..start + e.child_count as usize]
    }

    /// Children, mutable for sorting in place.
    pub(crate) fn children_mut(&mut self, id: NodeId) -> &mut [NodeId] {
        let e = &self.entries[id as usize];
        let start = e.first_child as usize;
        let count = e.child_count as usize;
        &mut self.children[start..start + count]
    }

    /// Sort the roots in place.
    pub(crate) fn roots_mut(&mut self) -> &mut [NodeId] {
        &mut self.roots
    }

    /// Number of ancestors.
    pub fn depth(&self, id: NodeId) -> usize {
        let mut d = 0;
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            d += 1;
            cur = p;
        }
        d
    }

    /// The entry and its ancestors, root first.
    pub fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = vec![id];
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            out.push(p);
            cur = p;
        }
        out.reverse();
        out
    }

    /// The entry's path: the root's name (a full path) joined with each name below it.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut path = PathBuf::new();
        for a in self.ancestry(id) {
            path.push(self.name(a));
        }
        path
    }

    /// Find the entry at `path` (as spelled by the walk), descending from the root whose name
    /// is a prefix of it.
    pub fn find_path(&self, path: &std::path::Path) -> Option<NodeId> {
        for &r in &self.roots {
            let Ok(rest) = path.strip_prefix(self.name(r)) else {
                continue;
            };
            let mut cur = r;
            let mut found = true;
            for comp in rest.components() {
                let want = comp.as_os_str().to_string_lossy();
                match self
                    .children(cur)
                    .iter()
                    .copied()
                    .find(|&c| self.name(c) == want)
                {
                    Some(c) => cur = c,
                    None => {
                        found = false;
                        break;
                    }
                }
            }
            if found {
                return Some(cur);
            }
        }
        None
    }

    /// Sum of the roots' allocated bytes.
    pub fn total_alloc(&self) -> u64 {
        self.roots.iter().map(|&r| self.alloc(r)).sum()
    }

    /// Approximate heap bytes held by the tree (for the memory report).
    pub fn heap_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.names.capacity()
            + self.children.capacity() * std::mem::size_of::<NodeId>()
            + self.roots.capacity() * std::mem::size_of::<NodeId>()
    }
}

/// Builds a [`Tree`]. Every parent must be pushed before its children.
#[derive(Debug, Default)]
pub struct TreeBuilder {
    entries: Vec<Entry>,
    names: String,
    rollup: bool,
}

/// What [`TreeBuilder::push`] records for one entry.
#[derive(Debug, Clone)]
pub struct NewEntry<'a> {
    pub parent: Option<NodeId>,
    pub name: &'a str,
    pub kind: EntryKind,
    /// The entry's own logical bytes (for a folder in a rolled-up tree: its own directory
    /// entry; in a tree that is not rolled up: the subtree total).
    pub size: u64,
    /// The entry's own allocated bytes (or the subtree total, as for `size`).
    pub alloc: u64,
    /// Files in the subtree when not rolled up; ignored when rolled up.
    pub files: u64,
    pub mtime: i64,
    pub flags: u8,
}

impl TreeBuilder {
    /// A builder. With `rollup`, folder totals are computed from their children in
    /// [`finish`](Self::finish); without it, every entry's totals are taken as given.
    pub fn new(rollup: bool) -> TreeBuilder {
        TreeBuilder {
            entries: Vec::new(),
            names: String::new(),
            rollup,
        }
    }

    /// A builder with room for `n` entries.
    pub fn with_capacity(n: usize, rollup: bool) -> TreeBuilder {
        TreeBuilder {
            entries: Vec::with_capacity(n),
            names: String::with_capacity(n.saturating_mul(12)),
            rollup,
        }
    }

    /// Entries pushed so far.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing was pushed.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add an entry. Returns `None` when the tree is full (4 billion entries) or the parent was
    /// not pushed yet.
    pub fn push(&mut self, e: NewEntry<'_>) -> Option<NodeId> {
        let id = u32::try_from(self.entries.len()).ok().filter(|&i| i < NO_PARENT)?;
        let parent = match e.parent {
            Some(p) if p < id => p,
            Some(_) => return None,
            None => NO_PARENT,
        };
        let (name_start, name_len) = self.store_name(e.name);
        let files = if self.rollup {
            u64::from(e.kind != EntryKind::Dir)
        } else {
            e.files
        };
        self.entries.push(Entry {
            name_start,
            name_len,
            parent,
            kind: e.kind,
            flags: e.flags,
            size: e.size,
            alloc: e.alloc,
            files,
            mtime: e.mtime,
            first_child: 0,
            child_count: 0,
        });
        Some(id)
    }

    /// Store a name; past 4 GiB of names, further entries get an empty name.
    fn store_name(&mut self, name: &str) -> (u32, u32) {
        let start = self.names.len();
        match (u32::try_from(start), u32::try_from(start + name.len())) {
            (Ok(s), Ok(_)) => {
                self.names.push_str(name);
                (s, name.len() as u32)
            }
            _ => (0, 0),
        }
    }

    /// Set flag bits on an entry already pushed.
    pub fn add_flags(&mut self, id: NodeId, bits: u8) {
        if let Some(e) = self.entries.get_mut(id as usize) {
            e.flags |= bits;
        }
    }

    /// The allocated bytes recorded for an entry so far (its own, before roll-up).
    pub fn own_alloc(&self, id: NodeId) -> u64 {
        self.entries.get(id as usize).map(|e| e.alloc).unwrap_or(0)
    }

    /// Group children by parent and, for a rolled-up tree, add every entry's totals into its
    /// ancestors. Children start in push order; [`crate::table::TreeView`] sorts them.
    // Index loops: an entry and its parent are both updated in the same pass.
    #[allow(clippy::needless_range_loop)]
    pub fn finish(self, top_files: Vec<TopFile>, stats: WalkStats) -> Tree {
        let TreeBuilder {
            mut entries,
            names,
            rollup,
        } = self;
        let n = entries.len();
        if rollup {
            for i in (0..n).rev() {
                let p = entries[i].parent;
                if p == NO_PARENT {
                    continue;
                }
                let (size, alloc, files, mtime) = {
                    let e = &entries[i];
                    (e.size, e.alloc, e.files, e.mtime)
                };
                let pe = &mut entries[p as usize];
                pe.size = pe.size.saturating_add(size);
                pe.alloc = pe.alloc.saturating_add(alloc);
                pe.files = pe.files.saturating_add(files);
                pe.mtime = pe.mtime.max(mtime);
            }
        }
        let mut roots = Vec::new();
        for (i, e) in entries.iter_mut().enumerate() {
            if e.parent == NO_PARENT {
                roots.push(i as NodeId);
            }
            e.child_count = 0;
        }
        for i in 0..n {
            let p = entries[i].parent;
            if p != NO_PARENT {
                entries[p as usize].child_count += 1;
            }
        }
        let mut next = 0u32;
        for e in entries.iter_mut() {
            e.first_child = next;
            next += e.child_count;
        }
        let mut fill: Vec<u32> = entries.iter().map(|e| e.first_child).collect();
        let mut children = vec![0 as NodeId; next as usize];
        for i in 0..n {
            let p = entries[i].parent;
            if p != NO_PARENT {
                let slot = &mut fill[p as usize];
                children[*slot as usize] = i as NodeId;
                *slot += 1;
            }
        }
        Tree {
            entries,
            names,
            children,
            roots,
            top_files,
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(parent: Option<NodeId>, name: &str, kind: EntryKind, alloc: u64) -> NewEntry<'_> {
        NewEntry {
            parent,
            name,
            kind,
            size: alloc,
            alloc,
            files: 0,
            mtime: 1,
            flags: 0,
        }
    }

    #[test]
    fn rollup_sums_into_every_ancestor() {
        let mut b = TreeBuilder::new(true);
        let root = b.push(entry(None, "/r", EntryKind::Dir, 4)).unwrap();
        let a = b.push(entry(Some(root), "a", EntryKind::Dir, 4)).unwrap();
        b.push(entry(Some(a), "f1", EntryKind::File, 100)).unwrap();
        b.push(entry(Some(a), "f2", EntryKind::File, 50)).unwrap();
        b.push(entry(Some(root), "g", EntryKind::File, 10)).unwrap();
        let t = b.finish(Vec::new(), WalkStats::default());
        assert_eq!(t.alloc(root), 168);
        assert_eq!(t.alloc(a), 154);
        assert_eq!(t.files(root), 3);
        assert_eq!(t.children(root).len(), 2);
        assert_eq!(t.children(a).len(), 2);
        assert_eq!(t.roots(), &[root]);
        assert_eq!(t.depth(4), 1);
        assert_eq!(t.path(2), PathBuf::from("/r").join("a").join("f1"));
        assert_eq!(t.find_path(&PathBuf::from("/r").join("a").join("f2")), Some(3));
        assert_eq!(t.find_path(&PathBuf::from("/r").join("nope")), None);
    }

    #[test]
    fn a_child_before_its_parent_is_refused() {
        let mut b = TreeBuilder::new(true);
        assert!(b.push(entry(Some(0), "orphan", EntryKind::File, 1)).is_none());
        let r = b.push(entry(None, "/r", EntryKind::Dir, 0)).unwrap();
        assert!(b.push(entry(Some(r + 5), "x", EntryKind::File, 1)).is_none());
    }

    #[test]
    fn without_rollup_totals_are_kept() {
        let mut b = TreeBuilder::new(false);
        let mut e = entry(None, "/r", EntryKind::Dir, 1000);
        e.files = 7;
        let r = b.push(e).unwrap();
        b.push(entry(Some(r), "a", EntryKind::Dir, 300)).unwrap();
        let t = b.finish(Vec::new(), WalkStats::default());
        assert_eq!(t.alloc(r), 1000);
        assert_eq!(t.files(r), 7);
    }

    #[test]
    fn empty_tree_is_fine() {
        let t = TreeBuilder::new(true).finish(Vec::new(), WalkStats::default());
        assert!(t.is_empty());
        assert_eq!(t.total_alloc(), 0);
    }
}
