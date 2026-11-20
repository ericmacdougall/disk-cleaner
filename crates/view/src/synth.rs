//! Seeded synthetic data for tests and performance measurements. Every name is made up; nothing
//! here comes from a real disk.

use crate::tree::{EntryKind, NewEntry, NodeId, Tree, TreeBuilder, WalkStats};
use disk_cleaner_core::{
    Class, Coverage, Facts, Node, RepoInfo, RepoSizes, ScanOptionsRecord, ScanResult, Totals,
    SCHEMA_VERSION,
};
use std::collections::BTreeMap;

/// The seed the performance thresholds were registered with (the ticket number).
pub const SEED: u64 = 16452;

/// A fixed point in time for synthetic data: 2026-01-01T00:00:00Z.
pub const EPOCH: i64 = 1_767_225_600;

/// SplitMix64: small, fast, and the same on every platform.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

fn file_entry<'a>(parent: NodeId, name: &'a str, rng: &mut SplitMix64) -> NewEntry<'a> {
    let alloc = 4096 * (1 + rng.below(256));
    NewEntry {
        parent: Some(parent),
        name,
        kind: EntryKind::File,
        size: alloc - rng.below(4096),
        alloc,
        files: 0,
        mtime: EPOCH - rng.below(400 * 86_400) as i64,
        flags: 0,
    }
}

/// A tree of exactly `n` entries (at least one): one root folder, then entries that are folders
/// one time in ten, each placed under a folder chosen at random among those made so far.
pub fn synthetic_tree(n: usize, seed: u64) -> Tree {
    let n = n.max(1);
    let mut rng = SplitMix64::new(seed);
    let mut b = TreeBuilder::with_capacity(n, true);
    let root = b
        .push(NewEntry {
            parent: None,
            name: "/synthetic",
            kind: EntryKind::Dir,
            size: 0,
            alloc: 4096,
            files: 0,
            mtime: EPOCH,
            flags: 0,
        })
        .expect("root");
    let mut dirs: Vec<NodeId> = vec![root];
    let mut name = String::new();
    for i in 1..n {
        let parent = dirs[rng.below(dirs.len() as u64) as usize];
        name.clear();
        if rng.below(10) == 0 {
            name.push_str(&format!("dir-{i}"));
            let id = b
                .push(NewEntry {
                    parent: Some(parent),
                    name: &name,
                    kind: EntryKind::Dir,
                    size: 0,
                    alloc: 4096,
                    files: 0,
                    mtime: EPOCH - rng.below(400 * 86_400) as i64,
                    flags: 0,
                })
                .expect("dir");
            dirs.push(id);
        } else {
            name.push_str(&format!("file-{i}.bin"));
            let e = file_entry(parent, &name, &mut rng);
            b.push(e).expect("file");
        }
    }
    b.finish(Vec::new(), WalkStats::default())
}

/// One root folder holding `children` files directly.
pub fn wide_tree(children: usize, seed: u64) -> Tree {
    let mut rng = SplitMix64::new(seed);
    let mut b = TreeBuilder::with_capacity(children + 1, true);
    let root = b
        .push(NewEntry {
            parent: None,
            name: "/wide",
            kind: EntryKind::Dir,
            size: 0,
            alloc: 4096,
            files: 0,
            mtime: EPOCH,
            flags: 0,
        })
        .expect("root");
    let mut name = String::new();
    for i in 0..children {
        name.clear();
        name.push_str(&format!("file-{i}.bin"));
        let e = file_entry(root, &name, &mut rng);
        b.push(e).expect("file");
    }
    b.finish(Vec::new(), WalkStats::default())
}

const CLASSES: [Class; 7] = [
    Class::Regenerable,
    Class::Cache,
    Class::Output,
    Class::LooseCode,
    Class::Repo,
    Class::Worktree,
    Class::Other,
];

/// A valid scan document with `nodes` classified folders under `/synthetic`, of every class,
/// with git facts left unknown (so repositories ask). It passes core's `validate_scan`.
pub fn synthetic_scan(nodes: usize, seed: u64) -> ScanResult {
    let mut rng = SplitMix64::new(seed);
    let mut out: Vec<Node> = Vec::with_capacity(nodes);
    let mut by_class: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    let mut files = 0u64;
    for i in 0..nodes {
        let class = CLASSES[rng.below(CLASSES.len() as u64) as usize];
        // A parent among the earlier nodes, a quarter of the time.
        let parent = if i > 0 && rng.below(4) == 0 {
            Some(rng.below(i as u64) as usize)
        } else {
            None
        };
        let path = match parent {
            Some(p) => format!("{}/n{i}", out[p].path),
            None => format!("/synthetic/p{i}"),
        };
        let mut node = Node::new(path, class);
        node.parent = parent;
        let bytes = 4096 * (1 + rng.below(10_000));
        node.bytes_self = bytes;
        node.bytes_total = bytes;
        node.files_total = 1 + rng.below(500);
        node.newest_mtime = Some(EPOCH - rng.below(400 * 86_400) as i64);
        if class.is_artifact() {
            node.rule = Some("synthetic".into());
        }
        if class.is_git() {
            node.repo = Some(RepoInfo {
                facts: Facts::default(),
                sizes: RepoSizes {
                    total: bytes,
                    git: bytes / 4,
                    regenerable: 0,
                    output: 0,
                },
                ..RepoInfo::default()
            });
        }
        *by_class.entry(class.as_str().to_string()).or_default() += bytes;
        total += bytes;
        files += node.files_total;
        out.push(node);
    }
    scan_document("/synthetic", out, by_class, total, files)
}

/// A size-walk tree and a matching scan for the widest case the desktop installs: one root
/// folder `/wide` holding `dirs` folders `d0`, `d1`, ..., each holding one file, and a scan in
/// which every one of those folders is a regenerable folder (so each becomes a plan item).
pub fn wide_dir_scan(dirs: usize, seed: u64) -> (Tree, ScanResult) {
    let mut rng = SplitMix64::new(seed);
    let mut b = TreeBuilder::with_capacity(2 * dirs + 1, true);
    let root = b
        .push(NewEntry {
            parent: None,
            name: "/wide",
            kind: EntryKind::Dir,
            size: 0,
            alloc: 4096,
            files: 0,
            mtime: EPOCH,
            flags: 0,
        })
        .expect("root");
    let mut out: Vec<Node> = Vec::with_capacity(dirs);
    let mut by_class: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    for i in 0..dirs {
        let name = format!("d{i}");
        let dir = b
            .push(NewEntry {
                parent: Some(root),
                name: &name,
                kind: EntryKind::Dir,
                size: 0,
                alloc: 4096,
                files: 0,
                mtime: EPOCH - 400 * 86_400,
                flags: 0,
            })
            .expect("dir");
        let e = file_entry(dir, "f.bin", &mut rng);
        let bytes = e.alloc;
        b.push(e).expect("file");
        let mut node = Node::new(format!("/wide/{name}"), Class::Regenerable);
        node.bytes_self = bytes;
        node.bytes_total = bytes;
        node.files_total = 1;
        node.newest_mtime = Some(EPOCH - 400 * 86_400);
        node.rule = Some("synthetic".into());
        let key = Class::Regenerable.as_str().to_string();
        *by_class.entry(key).or_default() += bytes;
        total += bytes;
        out.push(node);
    }
    let tree = b.finish(Vec::new(), WalkStats::default());
    (
        tree,
        scan_document("/wide", out, by_class, total, dirs as u64),
    )
}

fn scan_document(
    root: &str,
    out: Vec<Node>,
    by_class: BTreeMap<String, u64>,
    total: u64,
    files: u64,
) -> ScanResult {
    let nodes = out.len();
    ScanResult {
        schema_version: SCHEMA_VERSION,
        tool_version: "synthetic".into(),
        generated_at: EPOCH,
        roots: vec![root.into()],
        options: ScanOptionsRecord {
            one_file_system: true,
            top_files: 0,
            min_output_bytes: 0,
            git_facts: false,
        },
        totals: Totals {
            files,
            dirs: nodes as u64,
            allocated_bytes: total,
            logical_bytes: total,
            bytes_by_class: by_class,
            ..Totals::default()
        },
        nodes: out,
        outputs: Vec::new(),
        big_files: Vec::new(),
        issues: Vec::new(),
        warnings: Vec::new(),
        enclosing_repos: Vec::new(),
        scope: "subtree".into(),
        coverage: Coverage {
            complete: true,
            ..Coverage::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generator_is_deterministic() {
        let a: Vec<u64> = {
            let mut r = SplitMix64::new(SEED);
            (0..5).map(|_| r.next_u64()).collect()
        };
        let b: Vec<u64> = {
            let mut r = SplitMix64::new(SEED);
            (0..5).map(|_| r.next_u64()).collect()
        };
        assert_eq!(a, b);
        assert_ne!(a[0], a[1]);
    }

    #[test]
    fn synthetic_tree_has_the_requested_size_and_rolls_up() {
        let t = synthetic_tree(1000, SEED);
        assert_eq!(t.len(), 1000);
        let root = t.roots()[0];
        assert_eq!(t.files(root) as usize + count_dirs(&t), 1000);
    }

    fn count_dirs(t: &Tree) -> usize {
        (0..t.len() as u32)
            .filter(|&i| t.kind(i) == EntryKind::Dir)
            .count()
    }

    #[test]
    fn synthetic_scan_is_valid_for_core() {
        let s = synthetic_scan(500, SEED);
        disk_cleaner_core::validate_scan(&s).unwrap();
        let text = serde_json::to_string(&s).unwrap();
        let back = disk_cleaner_core::scan_from_json(&text).unwrap();
        assert_eq!(back.nodes.len(), 500);
    }
}
