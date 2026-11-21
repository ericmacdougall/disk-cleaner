//! The size table: a [`Tree`] shown as rows, with expansion, a focus folder for drill-down,
//! sorting and paging. Only expanded folders are in the row list, and a folder's children are
//! sorted when it is first shown under the current order, so a tree of a million entries costs
//! nothing until it is opened.

use crate::sanitize::display_name;
use crate::tree::{flags, EntryKind, NodeId, Tree};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

/// Most rows one page may hold.
pub const MAX_PAGE: usize = 1000;

/// A 1-based place among siblings from a 0-based index.
fn place(i: usize) -> u32 {
    u32::try_from(i.saturating_add(1)).unwrap_or(u32::MAX)
}

/// Column to sort by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    /// Logical bytes.
    Size,
    /// Allocated bytes (also the order of "percent of parent").
    Allocated,
    Files,
    Modified,
    Name,
}

/// A sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort {
            key: SortKey::Allocated,
            descending: true,
        }
    }
}

impl Sort {
    fn code(self) -> u8 {
        let k = match self.key {
            SortKey::Size => 0,
            SortKey::Allocated => 1,
            SortKey::Files => 2,
            SortKey::Modified => 3,
            SortKey::Name => 4,
        };
        1 + k * 2 + u8::from(self.descending)
    }
}

/// Compare two names case-insensitively for ASCII, then exactly.
fn name_cmp(a: &str, b: &str) -> Ordering {
    let fold = a
        .bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()));
    fold.then_with(|| a.cmp(b))
}

fn compare(tree: &Tree, sort: Sort, a: NodeId, b: NodeId) -> Ordering {
    let primary = match sort.key {
        SortKey::Size => tree.size(a).cmp(&tree.size(b)),
        SortKey::Allocated => tree.alloc(a).cmp(&tree.alloc(b)),
        SortKey::Files => tree.files(a).cmp(&tree.files(b)),
        SortKey::Modified => tree.mtime(a).cmp(&tree.mtime(b)),
        SortKey::Name => name_cmp(tree.name(a), tree.name(b)),
    };
    let primary = if sort.descending {
        primary.reverse()
    } else {
        primary
    };
    // Ties: name, then id, so the order is stable and deterministic.
    primary
        .then_with(|| name_cmp(tree.name(a), tree.name(b)))
        .then_with(|| a.cmp(&b))
}

/// One visible row.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub id: NodeId,
    /// Depth below the focus (0 for its children or the roots).
    pub depth: u16,
    /// Display-safe name.
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub alloc: u64,
    pub files: u64,
    pub mtime: Option<i64>,
    /// Allocated bytes as a percentage of the parent's (of all roots, for a root).
    pub percent_of_parent: f64,
    pub has_children: bool,
    pub expanded: bool,
    pub unreadable: bool,
    pub mount_skipped: bool,
    pub hardlink_duplicate: bool,
    pub summary: bool,
    /// A class or tag from the developer view (`repo`, `cache`, `agent-session`...).
    pub tag: Option<String>,
    /// Row index of the parent's row (`None` at depth 0), so a client that holds only a page of
    /// rows can still move to the parent (the treegrid's Left arrow).
    pub parent_row: Option<usize>,
    /// 1-based position among its siblings, and how many siblings there are (itself included):
    /// a screen reader's "3 of 40", which a paged client cannot count (aria-posinset/setsize).
    pub posinset: usize,
    pub setsize: usize,
}

/// A child for the treemap.
#[derive(Debug, Clone, Serialize)]
pub struct Tile {
    /// `None` for the remainder tile that stands for every child not listed.
    pub id: Option<NodeId>,
    pub name: String,
    pub alloc: u64,
    pub is_dir: bool,
    pub tag: Option<String>,
}

/// One element of the breadcrumb.
#[derive(Debug, Clone, Serialize)]
pub struct Crumb {
    pub id: NodeId,
    pub name: String,
}

/// The tree as a sortable, expandable, pageable table.
///
/// The tree itself is shared and never changed: the view keeps its own copy of the child
/// order (one id per entry) and sorts that, so another thread can read the tree (to look up
/// paths, say) while the view is in use.
#[derive(Debug, Clone)]
pub struct TreeView {
    tree: Arc<Tree>,
    /// The tree's child lists in this view's order, laid out as [`Tree::child_slab`].
    order: Vec<NodeId>,
    roots: Vec<NodeId>,
    sort: Sort,
    /// Per entry: the sort code its children were last sorted with (0: never).
    sorted_as: Vec<u8>,
    expanded: Vec<bool>,
    rows: Vec<NodeId>,
    depths: Vec<u16>,
    /// Per row: its 1-based place among its siblings (spliced with `rows`).
    places: Vec<u32>,
    focus: Option<NodeId>,
    tags: HashMap<NodeId, String>,
}

impl TreeView {
    /// A view of `tree`, roots shown, nothing expanded, largest first.
    pub fn new(tree: Tree) -> TreeView {
        TreeView::shared(Arc::new(tree))
    }

    /// A view of a shared tree.
    pub fn shared(tree: Arc<Tree>) -> TreeView {
        let n = tree.len();
        let mut v = TreeView {
            order: tree.child_slab().to_vec(),
            roots: tree.roots().to_vec(),
            tree,
            sort: Sort::default(),
            sorted_as: vec![0; n],
            expanded: vec![false; n],
            rows: Vec::new(),
            depths: Vec::new(),
            places: Vec::new(),
            focus: None,
            tags: HashMap::new(),
        };
        v.rebuild();
        v
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// The tree, to read on another thread.
    pub fn tree_arc(&self) -> Arc<Tree> {
        Arc::clone(&self.tree)
    }

    /// `id`'s children in this view's order.
    fn kids(&self, id: NodeId) -> &[NodeId] {
        &self.order[self.tree.child_span(id)]
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn focus(&self) -> Option<NodeId> {
        self.focus
    }

    /// Visible rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Attach developer-view tags to entries. Returns the tags they replace.
    pub fn set_tags(&mut self, tags: HashMap<NodeId, String>) -> HashMap<NodeId, String> {
        std::mem::replace(&mut self.tags, tags)
    }

    fn valid(&self, id: NodeId) -> bool {
        (id as usize) < self.tree.len()
    }

    fn ensure_sorted(&mut self, id: NodeId) {
        let code = self.sort.code();
        if self.sorted_as[id as usize] == code {
            return;
        }
        let sort = self.sort;
        let tree = &*self.tree;
        self.order[tree.child_span(id)].sort_unstable_by(|&a, &b| compare(tree, sort, a, b));
        self.sorted_as[id as usize] = code;
    }

    fn top_level(&mut self) -> Vec<NodeId> {
        match self.focus {
            Some(f) => {
                self.ensure_sorted(f);
                self.kids(f).to_vec()
            }
            None => {
                let sort = self.sort;
                let tree = &*self.tree;
                self.roots
                    .sort_unstable_by(|&a, &b| compare(tree, sort, a, b));
                self.roots.clone()
            }
        }
    }

    /// Rebuild the row list from the focus, keeping expanded folders open.
    fn rebuild(&mut self) {
        self.rows.clear();
        self.depths.clear();
        self.places.clear();
        let top = self.top_level();
        // Depth-first, children pushed in reverse so they come out in order.
        let mut stack: Vec<(NodeId, u16, u32)> = top
            .into_iter()
            .enumerate()
            .rev()
            .map(|(i, id)| (id, 0, place(i)))
            .collect();
        while let Some((id, depth, at)) = stack.pop() {
            self.rows.push(id);
            self.depths.push(depth);
            self.places.push(at);
            if self.expanded[id as usize] {
                self.ensure_sorted(id);
                for (i, &c) in self.kids(id).iter().enumerate().rev() {
                    stack.push((c, depth.saturating_add(1), place(i)));
                }
            }
        }
    }

    /// Change the order; open folders stay open.
    pub fn set_sort(&mut self, sort: Sort) {
        if sort != self.sort {
            self.sort = sort;
            self.rebuild();
        }
    }

    /// Show the children of `id` as the top level (drill down), or the roots for `None`.
    /// Returns false for an id that is not a folder.
    pub fn set_focus(&mut self, id: Option<NodeId>) -> bool {
        if let Some(i) = id {
            if !self.valid(i) || self.tree.kind(i) != EntryKind::Dir {
                return false;
            }
        }
        self.focus = id;
        for e in self.expanded.iter_mut() {
            *e = false;
        }
        self.rebuild();
        true
    }

    /// Drill up one level. Returns the folder that was the focus, to select it.
    pub fn focus_parent(&mut self) -> Option<NodeId> {
        let old = self.focus?;
        let parent = self.tree.parent(old);
        self.set_focus(parent);
        Some(old)
    }

    /// Row index of an entry, when it is visible.
    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.rows.iter().position(|&r| r == id)
    }

    /// Open a folder's row. Returns false when it is not visible or not a folder.
    pub fn expand(&mut self, id: NodeId) -> bool {
        if !self.valid(id) || self.tree.kind(id) != EntryKind::Dir || self.expanded[id as usize] {
            return false;
        }
        let Some(pos) = self.row_of(id) else {
            return false;
        };
        self.ensure_sorted(id);
        let depth = self.depths[pos].saturating_add(1);
        let kids = self.kids(id).to_vec();
        let n = kids.len();
        self.rows.splice(pos + 1..pos + 1, kids);
        self.depths
            .splice(pos + 1..pos + 1, std::iter::repeat_n(depth, n));
        self.places.splice(pos + 1..pos + 1, (0..n).map(place));
        self.expanded[id as usize] = true;
        true
    }

    /// Open every folder under the focus (or under every root) in one pass.
    pub fn expand_all(&mut self) {
        let top: Vec<NodeId> = match self.focus {
            Some(f) => self.kids(f).to_vec(),
            None => self.roots.clone(),
        };
        let mut stack = top;
        while let Some(id) = stack.pop() {
            if self.tree.kind(id) == EntryKind::Dir {
                self.expanded[id as usize] = true;
                stack.extend_from_slice(self.tree.children(id));
            }
        }
        self.rebuild();
    }

    /// Close a folder's row and every open folder below it.
    pub fn collapse(&mut self, id: NodeId) -> bool {
        if !self.valid(id) || !self.expanded[id as usize] {
            return false;
        }
        let Some(pos) = self.row_of(id) else {
            return false;
        };
        let depth = self.depths[pos];
        let mut end = pos + 1;
        while end < self.rows.len() && self.depths[end] > depth {
            self.expanded[self.rows[end] as usize] = false;
            end += 1;
        }
        self.rows.drain(pos + 1..end);
        self.depths.drain(pos + 1..end);
        self.places.drain(pos + 1..end);
        self.expanded[id as usize] = false;
        true
    }

    /// Expand or collapse.
    pub fn toggle(&mut self, id: NodeId) -> bool {
        if self.valid(id) && self.expanded[id as usize] {
            self.collapse(id)
        } else {
            self.expand(id)
        }
    }

    fn row(&self, id: NodeId, depth: u16, parent_row: Option<usize>, posinset: usize) -> Row {
        let t = &self.tree;
        let setsize = match t.parent(id) {
            Some(p) => t.children(p).len(),
            None => self.roots.len(),
        };
        let parent_alloc = match t.parent(id) {
            Some(p) => t.alloc(p),
            None => t.total_alloc(),
        };
        let pct = if parent_alloc == 0 {
            0.0
        } else {
            (t.alloc(id) as f64 / parent_alloc as f64 * 100.0).min(100.0)
        };
        let f = t.flags(id);
        Row {
            id,
            depth,
            name: display_name(t.name(id)),
            kind: t.kind(id),
            size: t.size(id),
            alloc: t.alloc(id),
            files: t.files(id),
            mtime: t.mtime(id),
            percent_of_parent: pct,
            has_children: !t.children(id).is_empty(),
            expanded: self.expanded[id as usize],
            unreadable: f & flags::UNREADABLE != 0,
            mount_skipped: f & flags::MOUNT_SKIPPED != 0,
            hardlink_duplicate: f & flags::HARDLINK_DUPLICATE != 0,
            summary: f & flags::SUMMARY != 0,
            tag: self.tags.get(&id).cloned(),
            parent_row,
            posinset,
            setsize: setsize.max(posinset),
        }
    }

    /// Rows `offset .. offset + limit` (limit capped at [`MAX_PAGE`]).
    pub fn page(&self, offset: usize, limit: usize) -> Vec<Row> {
        let limit = limit.min(MAX_PAGE);
        let start = offset.min(self.rows.len());
        let end = start.saturating_add(limit).min(self.rows.len());
        // Open ancestors of the row being read, nearest last: found once for the first row by
        // walking back to its top-level row, then kept as the page is read in order.
        let mut open: Vec<usize> = Vec::new();
        if start < end {
            let mut want = self.depths[start];
            let mut j = start;
            while want > 0 && j > 0 {
                j -= 1;
                if self.depths[j] < want {
                    open.push(j);
                    want = self.depths[j];
                }
            }
            open.reverse();
        }
        (start..end)
            .map(|i| {
                let depth = self.depths[i];
                while open.last().is_some_and(|&a| self.depths[a] >= depth) {
                    open.pop();
                }
                let parent = if depth == 0 {
                    None
                } else {
                    open.last().copied()
                };
                open.push(i);
                self.row(self.rows[i], depth, parent, self.places[i] as usize)
            })
            .collect()
    }

    /// One entry as a row (depth 0), whether visible or not.
    pub fn entry_row(&self, id: NodeId) -> Option<Row> {
        if !self.valid(id) {
            return None;
        }
        let siblings = match self.tree.parent(id) {
            Some(p) => self.kids(p),
            None => &self.roots[..],
        };
        let at = siblings.iter().position(|&s| s == id).map_or(1, |i| i + 1);
        Some(self.row(id, 0, None, at))
    }

    /// The focus and its ancestors, root first.
    pub fn breadcrumb(&self) -> Vec<Crumb> {
        match self.focus {
            None => Vec::new(),
            Some(f) => self
                .tree
                .ancestry(f)
                .into_iter()
                .map(|id| Crumb {
                    id,
                    name: display_name(self.tree.name(id)),
                })
                .collect(),
        }
    }

    /// The largest children of `id` (or of every root) for the treemap: at most `limit`
    /// tiles, the rest summed into one remainder tile.
    pub fn tiles(&self, id: Option<NodeId>, limit: usize) -> Vec<Tile> {
        let limit = limit.clamp(1, MAX_PAGE);
        let mut kids: Vec<NodeId> = match id {
            Some(i) if self.valid(i) => self.tree.children(i).to_vec(),
            Some(_) => return Vec::new(),
            None => self.tree.roots().to_vec(),
        };
        let by_alloc = Sort {
            key: SortKey::Allocated,
            descending: true,
        };
        kids.sort_unstable_by(|&a, &b| compare(&self.tree, by_alloc, a, b));
        let mut tiles: Vec<Tile> = kids
            .iter()
            .take(limit)
            .filter(|&&k| self.tree.alloc(k) > 0)
            .map(|&k| Tile {
                id: Some(k),
                name: display_name(self.tree.name(k)),
                alloc: self.tree.alloc(k),
                is_dir: self.tree.kind(k) == EntryKind::Dir,
                tag: self.tags.get(&k).cloned(),
            })
            .collect();
        let rest: u64 = kids.iter().skip(limit).map(|&k| self.tree.alloc(k)).sum();
        if rest > 0 {
            tiles.push(Tile {
                id: None,
                name: format!("{} smaller items", kids.len() - limit),
                alloc: rest,
                is_dir: false,
                tag: None,
            });
        }
        tiles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{NewEntry, TreeBuilder, WalkStats};

    fn sample() -> Tree {
        let mut b = TreeBuilder::new(true);
        let mut push = |parent: Option<NodeId>, name: &str, kind: EntryKind, alloc: u64, mtime| {
            b.push(NewEntry {
                parent,
                name,
                kind,
                size: alloc / 2,
                alloc,
                files: 0,
                mtime,
                flags: 0,
            })
            .unwrap()
        };
        let r = push(None, "/root", EntryKind::Dir, 0, 1);
        let a = push(Some(r), "alpha", EntryKind::Dir, 0, 1);
        push(Some(a), "a1.bin", EntryKind::File, 300, 5);
        push(Some(a), "a2.bin", EntryKind::File, 100, 9);
        let b2 = push(Some(r), "Beta", EntryKind::Dir, 0, 1);
        push(Some(b2), "b1.bin", EntryKind::File, 50, 7);
        push(Some(r), "zeta.txt", EntryKind::File, 1000, 2);
        drop(push);
        b.finish(Vec::new(), WalkStats::default())
    }

    fn names(v: &TreeView) -> Vec<String> {
        v.page(0, 100).into_iter().map(|r| r.name).collect()
    }

    #[test]
    fn expand_sort_and_collapse() {
        let mut v = TreeView::new(sample());
        assert_eq!(names(&v), vec!["/root"]);
        assert!(v.expand(0));
        assert_eq!(names(&v), vec!["/root", "zeta.txt", "alpha", "Beta"]);
        assert!(v.expand(1));
        assert_eq!(
            names(&v),
            vec!["/root", "zeta.txt", "alpha", "a1.bin", "a2.bin", "Beta"]
        );
        v.set_sort(Sort {
            key: SortKey::Name,
            descending: false,
        });
        assert_eq!(
            names(&v),
            vec!["/root", "alpha", "a1.bin", "a2.bin", "Beta", "zeta.txt"]
        );
        v.set_sort(Sort {
            key: SortKey::Modified,
            descending: true,
        });
        let rows = v.page(0, 100);
        assert_eq!(rows[1].name, "alpha");
        assert_eq!(rows[2].name, "a2.bin");
        assert!(v.collapse(0));
        assert_eq!(names(&v), vec!["/root"]);
        // Collapsing the root closed alpha too.
        assert!(v.expand(0));
        assert_eq!(v.row_count(), 4);
    }

    /// Sorting a view never reorders the shared tree, and two views of one tree keep their
    /// own orders.
    #[test]
    fn views_sort_their_own_order_and_leave_the_tree_alone() {
        let tree = Arc::new(sample());
        let walk_order = tree.children(0).to_vec();
        let mut by_size = TreeView::shared(Arc::clone(&tree));
        let mut by_name = TreeView::shared(Arc::clone(&tree));
        by_name.set_sort(Sort {
            key: SortKey::Name,
            descending: false,
        });
        assert!(by_size.expand(0));
        assert!(by_name.expand(0));
        assert_eq!(names(&by_size), vec!["/root", "zeta.txt", "alpha", "Beta"]);
        assert_eq!(names(&by_name), vec!["/root", "alpha", "Beta", "zeta.txt"]);
        assert_eq!(tree.children(0), walk_order.as_slice());
        assert!(Arc::ptr_eq(&by_size.tree_arc(), &tree));
    }

    #[test]
    fn percent_of_parent_never_divides_by_zero() {
        let mut b = TreeBuilder::new(true);
        let r = b
            .push(NewEntry {
                parent: None,
                name: "/empty",
                kind: EntryKind::Dir,
                size: 0,
                alloc: 0,
                files: 0,
                mtime: 0,
                flags: 0,
            })
            .unwrap();
        b.push(NewEntry {
            parent: Some(r),
            name: "zero",
            kind: EntryKind::File,
            size: 0,
            alloc: 0,
            files: 0,
            mtime: 0,
            flags: 0,
        })
        .unwrap();
        let mut v = TreeView::new(b.finish(Vec::new(), WalkStats::default()));
        v.expand(r);
        for row in v.page(0, 10) {
            assert_eq!(row.percent_of_parent, 0.0);
        }
        assert!(v.tiles(Some(r), 10).is_empty());
    }

    #[test]
    fn focus_drills_down_and_up() {
        let mut v = TreeView::new(sample());
        assert!(v.set_focus(Some(1)));
        assert_eq!(names(&v), vec!["a1.bin", "a2.bin"]);
        assert_eq!(v.breadcrumb().len(), 2);
        assert!(!v.set_focus(Some(2)), "a file cannot be a focus");
        assert_eq!(v.focus_parent(), Some(1));
        assert_eq!(v.focus(), Some(0));
        assert_eq!(names(&v), vec!["zeta.txt", "alpha", "Beta"]);
    }

    #[test]
    fn expand_all_opens_every_folder_in_order() {
        let mut v = TreeView::new(sample());
        v.expand_all();
        assert_eq!(
            names(&v),
            vec!["/root", "zeta.txt", "alpha", "a1.bin", "a2.bin", "Beta", "b1.bin"]
        );
        assert!(v.collapse(0));
        assert_eq!(v.row_count(), 1);
    }

    /// (name, parent_row, posinset, setsize) for a page.
    fn places(
        v: &TreeView,
        offset: usize,
        limit: usize,
    ) -> Vec<(String, Option<usize>, usize, usize)> {
        v.page(offset, limit)
            .into_iter()
            .map(|r| (r.name, r.parent_row, r.posinset, r.setsize))
            .collect()
    }

    fn p(
        name: &str,
        parent: Option<usize>,
        at: usize,
        of: usize,
    ) -> (String, Option<usize>, usize, usize) {
        (name.to_string(), parent, at, of)
    }

    /// The treegrid's Left arrow and "2 of 3" need the parent row and the place among siblings
    /// even when the page holds neither the parent nor the siblings (CT-F4, CT-F5).
    #[test]
    fn rows_know_their_parent_row_and_place_among_siblings() {
        let mut v = TreeView::new(sample());
        assert!(v.expand(0));
        assert!(v.expand(1));
        assert_eq!(
            places(&v, 0, 100),
            vec![
                p("/root", None, 1, 1),
                p("zeta.txt", Some(0), 1, 3),
                p("alpha", Some(0), 2, 3),
                p("a1.bin", Some(2), 1, 2),
                p("a2.bin", Some(2), 2, 2),
                p("Beta", Some(0), 3, 3),
            ]
        );
        // Pages that start below the parent, and below a sibling's open subtree.
        assert_eq!(
            places(&v, 4, 2),
            vec![p("a2.bin", Some(2), 2, 2), p("Beta", Some(0), 3, 3)]
        );
        assert_eq!(places(&v, 5, 1), vec![p("Beta", Some(0), 3, 3)]);
        assert_eq!(places(&v, 3, 1), vec![p("a1.bin", Some(2), 1, 2)]);
        // Opening a folder above shifts the parent's row.
        assert!(v.expand(4));
        assert_eq!(places(&v, 6, 1), vec![p("b1.bin", Some(5), 1, 1)]);
        assert!(v.collapse(1));
        assert_eq!(
            places(&v, 3, 2),
            vec![p("Beta", Some(0), 3, 3), p("b1.bin", Some(3), 1, 1)]
        );
        // A new order renumbers the places; open folders stay open.
        v.set_sort(Sort {
            key: SortKey::Name,
            descending: false,
        });
        assert_eq!(
            places(&v, 0, 100),
            vec![
                p("/root", None, 1, 1),
                p("alpha", Some(0), 1, 3),
                p("Beta", Some(0), 2, 3),
                p("b1.bin", Some(2), 1, 1),
                p("zeta.txt", Some(0), 3, 3),
            ]
        );
        // Drilled in: the focus's children are the top level, with no parent row.
        assert!(v.set_focus(Some(1)));
        assert_eq!(places(&v, 1, 5), vec![p("a2.bin", None, 2, 2)]);
        let e = v.entry_row(3).unwrap();
        assert_eq!((e.posinset, e.setsize, e.parent_row), (2, 2, None));
    }

    #[test]
    fn pages_are_capped_and_bounded() {
        let v = TreeView::new(sample());
        assert_eq!(v.page(0, 10_000_000).len(), 1);
        assert!(v.page(50, 10).is_empty());
    }

    #[test]
    fn tiles_keep_the_largest_and_sum_the_rest() {
        let v = TreeView::new(sample());
        let tiles = v.tiles(Some(0), 2);
        assert_eq!(tiles.len(), 3);
        assert_eq!(tiles[0].name, "zeta.txt");
        assert_eq!(tiles[1].name, "alpha");
        assert_eq!(tiles[2].id, None);
        assert_eq!(tiles[2].alloc, 50);
    }

    #[test]
    fn hostile_names_are_shown_escaped() {
        let mut b = TreeBuilder::new(true);
        let r = b
            .push(NewEntry {
                parent: None,
                name: "/r",
                kind: EntryKind::Dir,
                size: 0,
                alloc: 0,
                files: 0,
                mtime: 0,
                flags: 0,
            })
            .unwrap();
        b.push(NewEntry {
            parent: Some(r),
            name: "evil\u{1b}[2J\u{202E}txt.exe",
            kind: EntryKind::File,
            size: 1,
            alloc: 1,
            files: 0,
            mtime: 0,
            flags: 0,
        })
        .unwrap();
        let mut v = TreeView::new(b.finish(Vec::new(), WalkStats::default()));
        v.expand(r);
        let row = &v.page(1, 1)[0];
        assert_eq!(row.name, "evil\\u{001B}[2J\\u{202E}txt.exe");
    }
}
