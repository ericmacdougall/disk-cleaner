//! Parallel, read-only scan and classification (design section 3).
//!
//! Phase 1 walks the tree with `jwalk` and keeps one small aggregate per directory (never a
//! record per file, apart from the top-N biggest files and output files). Phase 2 sorts the
//! directories, assigns each to its innermost classified ancestor and rolls sizes up. Phase 3
//! gathers git facts for the repos found. Nothing here writes, moves or deletes anything.

use crate::error::{Error, Result};
use crate::facts::{collect_repo_info, FactsConfig, RepoCheck};
use crate::model::*;
use crate::rules::{marker_matches, Rule, RuleSet};
use jwalk::WalkDirGeneric;
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ISSUES: usize = 1000;

/// Scan configuration.
#[derive(Clone)]
pub struct ScanOptions {
    /// Directories to scan. Relative paths are made absolute.
    pub roots: Vec<PathBuf>,
    /// Do not descend into directories on a different device than their scan root (Unix).
    pub one_file_system: bool,
    /// Keep this many of the biggest files.
    pub top_files: usize,
    /// Output files at least this large are listed wherever they are; inside build directories
    /// they are listed whatever their size.
    pub min_output_bytes: u64,
    /// Run git for every repo found. When false every fact is `Unknown`.
    pub collect_facts: bool,
    /// Git binary, timeout and network predicate.
    pub facts: FactsConfig,
    /// Concurrent git processes.
    pub git_jobs: usize,
    /// Artifact and cache rules.
    pub rules: Arc<RuleSet>,
}

impl ScanOptions {
    /// Defaults for the given roots.
    pub fn new(roots: Vec<PathBuf>) -> ScanOptions {
        ScanOptions {
            roots,
            one_file_system: false,
            top_files: 50,
            min_output_bytes: 100 * 1024 * 1024,
            collect_facts: true,
            facts: FactsConfig::default(),
            git_jobs: 4,
            rules: Arc::new(RuleSet::builtin()),
        }
    }
}

/// Per-entry state captured in the parallel part of the walk.
#[derive(Debug, Clone, Default)]
struct EntryMeta {
    ok: bool,
    alloc: u64,
    logical: u64,
    dev: u64,
    ino: u64,
    nlink: u64,
    mtime: i64,
    mount_skipped: bool,
}

type ClientState = ((), EntryMeta);

#[cfg(unix)]
fn meta_from(md: &Metadata) -> EntryMeta {
    use std::os::unix::fs::MetadataExt;
    EntryMeta {
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
fn meta_from(md: &Metadata) -> EntryMeta {
    // No cheap allocated size or inode identity on this platform: logical size, no hard-link
    // de-duplication (documented in docs/SCAN-FORMAT.md).
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    EntryMeta {
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

/// What the walk learns about one directory.
#[derive(Default)]
struct DirAgg {
    /// Allocated bytes of the directory entry itself plus its direct non-directory entries.
    bytes: u64,
    files: u64,
    output_bytes: u64,
    newest: Option<i64>,
    /// Names of direct entries that some rule uses as a marker.
    markers: Vec<String>,
    has_git_dir: bool,
    has_git_file: bool,
    has_head: bool,
    has_objects: bool,
    has_refs: bool,
    has_source: bool,
    has_manifest: bool,
    owner: Option<usize>,
    in_git: bool,
}

struct Walk {
    aggs: HashMap<PathBuf, DirAgg>,
    seen_inodes: HashSet<(u64, u64)>,
    heap: BinaryHeap<Reverse<(u64, PathBuf)>>,
    outputs_raw: Vec<(PathBuf, u64)>,
    totals: Totals,
    issues: Vec<ScanIssue>,
    /// Paths of every unreadable entry (bounded), used to mark the nodes that hold them.
    issue_paths: Vec<PathBuf>,
    /// More unreadable entries than `MAX_ISSUE_PATHS`: every node counts as incomplete.
    issue_overflow: bool,
}

const MAX_ISSUE_PATHS: usize = 100_000;

impl Walk {
    fn issue(&mut self, path: &Path, message: String) {
        self.totals.unreadable += 1;
        if self.issue_paths.len() < MAX_ISSUE_PATHS {
            self.issue_paths.push(path.to_path_buf());
        } else {
            self.issue_overflow = true;
        }
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(ScanIssue {
                path: path.to_string_lossy().into_owned(),
                message,
            });
        }
    }

    fn ingest(&mut self, entry: &jwalk::DirEntry<ClientState>, opts: &ScanOptions) {
        let path = entry.path();
        let root_meta;
        let meta: &EntryMeta = if entry.depth == 0 {
            root_meta = std::fs::symlink_metadata(&path)
                .map(|md| meta_from(&md))
                .unwrap_or_default();
            &root_meta
        } else {
            &entry.client_state
        };
        if !meta.ok {
            self.issue(&path, "cannot read metadata".to_string());
        }
        if let Some(err) = entry.read_children.as_ref().and_then(|rc| rc.error()) {
            self.issue(&path, format!("cannot read directory: {err}"));
        }
        let ft = entry.file_type;
        let name = entry.file_name.to_string_lossy().into_owned();
        let rules = &opts.rules;

        if ft.is_dir() {
            self.totals.dirs += 1;
            self.totals.allocated_bytes += meta.alloc;
            self.totals.logical_bytes += meta.logical;
            if meta.mount_skipped {
                self.totals.mount_points_skipped += 1;
            }
            let agg = self.aggs.entry(path.clone()).or_default();
            agg.bytes += meta.alloc;
            if entry.depth > 0 {
                let parent = parent_agg(&mut self.aggs, entry.parent_path());
                parent.note_entry(&name, true, rules);
            }
            return;
        }

        // A file, symlink or other non-directory entry. Symlinks are never followed; they
        // count as the small link object itself.
        let counted = if meta.nlink > 1 {
            self.seen_inodes.insert((meta.dev, meta.ino))
        } else {
            true
        };
        let (alloc, logical) = if counted {
            (meta.alloc, meta.logical)
        } else {
            self.totals.hardlink_duplicates_skipped += 1;
            (0, 0)
        };
        self.totals.files += 1;
        self.totals.allocated_bytes += alloc;
        self.totals.logical_bytes += logical;
        if ft.is_symlink() {
            self.totals.symlinks += 1;
        }
        let is_file = ft.is_file();
        let is_output = is_file && rules.is_output_file(&name);
        if is_output {
            self.totals.output_bytes += alloc;
            // Listing candidates: big ones anywhere, any size inside a build-directory
            // candidate. The owner is only known after classification, which filters again.
            if alloc >= opts.min_output_bytes || rules.in_output_scan_candidate(entry.parent_path())
            {
                self.outputs_raw.push((path.clone(), alloc));
            }
        }
        if is_file && opts.top_files > 0 {
            let smallest = self.heap.peek().map(|r| (r.0).0).unwrap_or(0);
            if self.heap.len() < opts.top_files || alloc > smallest {
                self.heap.push(Reverse((alloc, path.clone())));
                if self.heap.len() > opts.top_files {
                    self.heap.pop();
                }
            }
        }
        let agg = parent_agg(&mut self.aggs, entry.parent_path());
        agg.bytes += alloc;
        agg.files += 1;
        if is_output {
            agg.output_bytes += alloc;
        }
        if is_file {
            agg.newest = Some(agg.newest.map_or(meta.mtime, |n| n.max(meta.mtime)));
            agg.note_entry(&name, false, rules);
        }
    }
}

fn parent_agg<'a>(aggs: &'a mut HashMap<PathBuf, DirAgg>, parent: &Path) -> &'a mut DirAgg {
    if !aggs.contains_key(parent) {
        aggs.insert(parent.to_path_buf(), DirAgg::default());
    }
    aggs.get_mut(parent).expect("just inserted")
}

impl DirAgg {
    /// Record a direct child by name: git markers, rule markers, manifests, source files.
    fn note_entry(&mut self, name: &str, is_dir: bool, rules: &RuleSet) {
        match (name, is_dir) {
            (".git", true) => self.has_git_dir = true,
            (".git", false) => self.has_git_file = true,
            ("objects", true) => self.has_objects = true,
            ("refs", true) => self.has_refs = true,
            ("HEAD", false) => self.has_head = true,
            _ => {}
        }
        if rules.is_marker(name) && !self.markers.iter().any(|m| m == name) {
            self.markers.push(name.to_string());
        }
        if rules.is_manifest(name) {
            self.has_manifest = true;
        }
        if !is_dir && rules.is_source_file(name) {
            self.has_source = true;
        }
    }
}

/// A node under construction.
struct Build {
    node: Node,
    path: PathBuf,
    scan_outputs: bool,
    git_bytes: u64,
    regen_bytes: u64,
}

#[derive(Clone, Copy)]
struct Frame {
    idx: usize,
    owner: Option<usize>,
    in_git: bool,
}

/// Is one of the rule's markers in the directory beside `path` (its parent)? A marker further up
/// proves nothing about this directory: `src/build/` is not `build/` just because the project
/// root has a `package.json`.
fn marker_beside(dirs: &[(PathBuf, DirAgg)], path: &Path, markers: &[String]) -> bool {
    let parent = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    match dirs.binary_search_by(|(q, _)| q.as_path().cmp(parent)) {
        Ok(k) => dirs[k]
            .1
            .markers
            .iter()
            .any(|name| markers.iter().any(|m| marker_matches(m, name))),
        Err(_) => false,
    }
}

fn match_rule<'a>(dirs: &[(PathBuf, DirAgg)], i: usize, rules: &'a RuleSet) -> Option<&'a Rule> {
    let path = &dirs[i].0;
    rules
        .candidates(path)
        .into_iter()
        .find(|r| r.markers.is_empty() || marker_beside(dirs, path, &r.markers))
}

fn bare_shape(agg: &DirAgg) -> bool {
    agg.has_head && agg.has_objects && agg.has_refs
}

/// Assign every directory to its innermost classified ancestor, creating nodes.
fn classify(
    dirs: &mut [(PathBuf, DirAgg)],
    rules: &RuleSet,
    roots: &HashSet<PathBuf>,
) -> Vec<Build> {
    let mut builds: Vec<Build> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    for i in 0..dirs.len() {
        while let Some(top) = stack.last().copied() {
            if dirs[i].0.starts_with(&dirs[top.idx].0) {
                break;
            }
            stack.pop();
        }
        let (owner, in_git) = stack
            .last()
            .map(|f| (f.owner, f.in_git))
            .unwrap_or((None, false));
        let mut new_owner = owner;
        let mut now_in_git = in_git;

        if !in_git {
            let path = &dirs[i].0;
            let agg = &dirs[i].1;
            let opaque = owner
                .map(|o| builds[o].node.class.is_artifact())
                .unwrap_or(false);
            let is_git_admin = !opaque
                && path.file_name() == Some(OsStr::new(".git"))
                && owner
                    .map(|o| {
                        builds[o].node.class == Class::Repo
                            && Some(builds[o].path.as_path()) == path.parent()
                    })
                    .unwrap_or(false);
            if opaque && (agg.has_git_dir || agg.has_git_file || bare_shape(agg)) {
                // A git repository inside a "generated" directory (a FetchContent checkout,
                // a patched dependency): it may hold unpushed work.
                if let Some(o) = owner {
                    let rel_path = path.to_string_lossy();
                    builds[o].node.ask_reason.get_or_insert_with(|| {
                        format!("contains a git repository ({rel_path}) that may hold unsaved work")
                    });
                }
            }
            if is_git_admin {
                now_in_git = true;
            } else if !opaque {
                let mut found: Option<(Class, Option<&Rule>)> = None;
                if agg.has_git_dir {
                    found = Some((Class::Repo, None));
                } else if agg.has_git_file {
                    found = Some((Class::Worktree, None));
                } else if bare_shape(agg) {
                    found = Some((Class::BareRepo, None));
                } else if let Some(rule) = match_rule(dirs, i, rules) {
                    found = Some((rule.class, Some(rule)));
                } else if owner.is_none()
                    && (agg.has_manifest || (agg.has_source && !roots.contains(path)))
                {
                    found = Some((Class::LooseCode, None));
                }
                if let Some((class, rule)) = found {
                    let idx = builds.len();
                    let mut node = Node::new(path.to_string_lossy().into_owned(), class);
                    node.parent = owner;
                    node.rule = rule.map(|r| r.name.clone());
                    let parent_is_loose = match owner {
                        None => true,
                        Some(o) => builds[o].node.class == Class::LooseCode,
                    };
                    if rule.map(|r| r.scan_outputs).unwrap_or(false) && parent_is_loose {
                        node.ask_reason = Some(
                            "no git history proves this build-named directory holds only \
                             generated files and not hand-written source"
                                .to_string(),
                        );
                    }
                    builds.push(Build {
                        node,
                        path: path.clone(),
                        scan_outputs: rule.map(|r| r.scan_outputs).unwrap_or(false),
                        git_bytes: 0,
                        regen_bytes: 0,
                    });
                    new_owner = Some(idx);
                    if class == Class::BareRepo {
                        // A bare repo is all git data.
                        now_in_git = true;
                    }
                }
            }
        }
        dirs[i].1.owner = new_owner;
        dirs[i].1.in_git = now_in_git;
        stack.push(Frame {
            idx: i,
            owner: new_owner,
            in_git: now_in_git,
        });
    }
    builds
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Scan the roots, classify directories and (optionally) gather repo facts.
pub fn scan(opts: &ScanOptions) -> Result<ScanResult> {
    if opts.roots.is_empty() {
        return Err(Error::NoRoots);
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for r in &opts.roots {
        let abs = std::path::absolute(r).map_err(|e| Error::Root {
            path: r.clone(),
            source: e,
        })?;
        std::fs::symlink_metadata(&abs).map_err(|e| Error::Root {
            path: abs.clone(),
            source: e,
        })?;
        if !roots.contains(&abs) {
            roots.push(abs);
        }
    }

    let mut walk = Walk {
        aggs: HashMap::new(),
        seen_inodes: HashSet::new(),
        heap: BinaryHeap::new(),
        outputs_raw: Vec::new(),
        totals: Totals::default(),
        issues: Vec::new(),
        issue_paths: Vec::new(),
        issue_overflow: false,
    };
    let mut warnings: Vec<String> = Vec::new();
    if opts.one_file_system && !cfg!(unix) {
        warnings.push("--one-file-system is not supported on this platform and was ignored".into());
    }

    for root in &roots {
        let root_dev = std::fs::symlink_metadata(root)
            .map(|md| meta_from(&md).dev)
            .unwrap_or(0);
        let one_fs = opts.one_file_system && cfg!(unix);
        let walker = WalkDirGeneric::<ClientState>::new(root)
            .skip_hidden(false)
            .follow_links(false)
            .sort(false)
            .process_read_dir(move |_depth, _path, _state, children| {
                for entry in children.iter_mut().flatten() {
                    match std::fs::symlink_metadata(entry.path()) {
                        Ok(md) => {
                            let mut m = meta_from(&md);
                            if one_fs && entry.file_type.is_dir() && m.dev != root_dev {
                                entry.read_children = None;
                                m.mount_skipped = true;
                            }
                            entry.client_state = m;
                        }
                        Err(_) => entry.client_state = EntryMeta::default(),
                    }
                }
            });
        for item in walker {
            match item {
                Ok(entry) => walk.ingest(&entry, opts),
                Err(err) => {
                    let p = err.path().map(Path::to_path_buf).unwrap_or_default();
                    walk.issue(&p, err.to_string());
                }
            }
        }
    }

    let Walk {
        aggs,
        heap,
        outputs_raw,
        mut totals,
        mut issues,
        issue_paths,
        issue_overflow,
        ..
    } = walk;
    issues.truncate(MAX_ISSUES);

    // Phase 2: classification.
    let mut dirs: Vec<(PathBuf, DirAgg)> = aggs.into_iter().collect();
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let root_set: HashSet<PathBuf> = roots.iter().cloned().collect();
    let mut builds = classify(&mut dirs, &opts.rules, &root_set);

    // Attribute directory aggregates to their owners.
    let mut other_bytes: u64 = 0;
    let mut files_self: Vec<u64> = vec![0; builds.len()];
    for (_, agg) in dirs.iter() {
        match agg.owner {
            Some(o) => {
                let b = &mut builds[o];
                b.node.bytes_self += agg.bytes;
                b.node.output_bytes += agg.output_bytes;
                files_self[o] += agg.files;
                if agg.in_git {
                    b.git_bytes += agg.bytes;
                } else if let Some(t) = agg.newest {
                    b.node.newest_mtime = Some(b.node.newest_mtime.map_or(t, |n| n.max(t)));
                }
            }
            None => other_bytes += agg.bytes,
        }
    }
    // Roll totals up the node tree (children always come after their parent).
    for (i, b) in builds.iter_mut().enumerate() {
        b.node.bytes_total = b.node.bytes_self;
        b.node.files_total = files_self[i];
    }
    for i in (0..builds.len()).rev() {
        if let Some(p) = builds[i].node.parent {
            let (bt, ft, ob) = (
                builds[i].node.bytes_total,
                builds[i].node.files_total,
                builds[i].node.output_bytes,
            );
            let is_artifact = builds[i].node.class.is_artifact();
            let parent = &mut builds[p];
            parent.node.bytes_total += bt;
            parent.node.files_total += ft;
            parent.node.output_bytes += ob;
            if is_artifact {
                parent.regen_bytes += bt;
            }
        }
    }

    // Output files: keep those inside build directories (and flag the directory) and big ones.
    let owner_of = |dir: &Path| -> Option<usize> {
        dirs.binary_search_by(|(q, _)| q.as_path().cmp(dir))
            .ok()
            .and_then(|k| dirs[k].1.owner)
    };
    // Entries that could not be read make the node that holds them incomplete.
    if issue_overflow {
        for b in builds.iter_mut() {
            b.node.unreadable += 1;
        }
    }
    for p in &issue_paths {
        let owner = owner_of(p.as_path()).or_else(|| p.parent().and_then(&owner_of));
        if let Some(o) = owner {
            builds[o].node.unreadable += 1;
        }
    }
    let mut outputs: Vec<OutputFile> = Vec::new();
    for (path, bytes) in outputs_raw {
        let owner = path.parent().and_then(&owner_of);
        let in_build_dir = owner.map(|o| builds[o].scan_outputs).unwrap_or(false);
        if in_build_dir {
            if let Some(o) = owner {
                builds[o].node.contains_outputs = true;
            }
        }
        if in_build_dir || bytes >= opts.min_output_bytes {
            outputs.push(OutputFile {
                path: path.to_string_lossy().into_owned(),
                bytes,
                node: owner,
            });
        }
    }
    outputs.sort_by(|a, b| a.path.cmp(&b.path));

    let mut big: Vec<(u64, PathBuf)> = heap.into_iter().map(|Reverse(x)| x).collect();
    big.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let big_files: Vec<BigFile> = big
        .into_iter()
        .map(|(bytes, p)| BigFile {
            node: p.parent().and_then(&owner_of),
            path: p.to_string_lossy().into_owned(),
            bytes,
        })
        .collect();

    // Phase 3: git facts.
    let mut infos: HashMap<usize, RepoInfo> = HashMap::new();
    // Only directories that are still believed removable are excluded from the "unique ignored
    // files" check; one that holds outputs, a nested repo or unreadable parts is not.
    let tasks: Vec<(usize, PathBuf, Class, Vec<PathBuf>)> = builds
        .iter()
        .enumerate()
        .filter(|(_, b)| b.node.class.is_git())
        .map(|(i, b)| {
            let regen: Vec<PathBuf> = builds
                .iter()
                .filter(|c| {
                    c.node.parent == Some(i)
                        && c.node.class.is_artifact()
                        && !c.node.contains_outputs
                        && c.node.ask_reason.is_none()
                        && c.node.unreadable == 0
                })
                .map(|c| c.path.clone())
                .collect();
            (i, b.path.clone(), b.node.class, regen)
        })
        .collect();
    let mut unverified: Vec<(usize, PathBuf, String)> = Vec::new();
    if opts.collect_facts && !tasks.is_empty() {
        let jobs = opts.git_jobs.max(1);
        let gathered: Vec<(usize, RepoCheck)> =
            match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
                Ok(pool) => pool.install(|| {
                    tasks
                        .par_iter()
                        .map(|(i, p, c, regen)| (*i, collect_repo_info(&opts.facts, p, *c, regen)))
                        .collect()
                }),
                Err(_) => tasks
                    .iter()
                    .map(|(i, p, c, regen)| (*i, collect_repo_info(&opts.facts, p, *c, regen)))
                    .collect(),
            };
        for (i, check) in gathered {
            for (p, why) in check.unverified_artifacts {
                unverified.push((i, p, why));
            }
            infos.insert(i, check.info);
        }
    } else {
        for (i, _, _, regen) in &tasks {
            let info = RepoInfo {
                notes: vec!["git facts were not collected".to_string()],
                ..RepoInfo::default()
            };
            infos.insert(*i, info);
            for p in regen {
                unverified.push((
                    *i,
                    p.clone(),
                    "git facts were not collected, so it is not verified to be ignored".to_string(),
                ));
            }
        }
    }
    for (i, p, why) in unverified {
        if let Some(c) = builds
            .iter_mut()
            .find(|c| c.node.parent == Some(i) && c.path == p)
        {
            c.node.ask_reason.get_or_insert(why);
        }
    }
    for (i, b) in builds.iter_mut().enumerate() {
        if let Some(mut info) = infos.remove(&i) {
            if b.node.class == Class::Repo {
                info.submodule_gitdirs = list_module_gitdirs(&b.path.join(".git").join("modules"));
            }
            info.git_dir = node_git_dir(&b.path, b.node.class);
            info.sizes = RepoSizes {
                total: b.node.bytes_total,
                git: b.git_bytes,
                regenerable: b.regen_bytes,
                output: b.node.output_bytes,
            };
            b.node.repo = Some(info);
        }
    }
    link_repos(&mut builds, &mut warnings);

    // Totals by class.
    for b in &builds {
        *totals
            .bytes_by_class
            .entry(b.node.class.as_str().to_string())
            .or_insert(0) += b.node.bytes_self;
    }
    *totals
        .bytes_by_class
        .entry(Class::Other.as_str().to_string())
        .or_insert(0) += other_bytes;

    Ok(ScanResult {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        generated_at: unix_now(),
        roots: roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect(),
        options: ScanOptionsRecord {
            one_file_system: opts.one_file_system,
            top_files: opts.top_files,
            min_output_bytes: opts.min_output_bytes,
            git_facts: opts.collect_facts,
        },
        totals,
        nodes: builds.into_iter().map(|b| b.node).collect(),
        outputs,
        big_files,
        issues,
        warnings,
    })
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Largest file `read_small_regular_file` will read.
const MAX_SMALL_FILE: u64 = 64 * 1024;

/// Read a regular file of at most 64 KiB. A missing file is `Ok(None)`. A symlink, FIFO, device
/// or oversized file is an error: it is never followed or read to the end, so a hostile tree
/// cannot make the scan block or grow without bound.
fn read_small_regular_file(path: &Path) -> std::result::Result<Option<String>, String> {
    use std::io::Read;
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !md.file_type().is_file() {
        return Err("not a regular file".to_string());
    }
    if md.len() > MAX_SMALL_FILE {
        return Err("file is too large".to_string());
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = String::new();
    file.take(MAX_SMALL_FILE)
        .read_to_string(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(Some(buf))
}

/// This node's own git directory as a real path: `<dir>/.git`, or what its `.git` file names.
fn node_git_dir(path: &Path, class: Class) -> Option<String> {
    match class {
        Class::Repo => Some(canon(&path.join(".git")).to_string_lossy().into_owned()),
        Class::BareRepo => Some(canon(path).to_string_lossy().into_owned()),
        Class::Worktree => {
            let text = read_small_regular_file(&path.join(".git")).ok()??;
            let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
            let target = PathBuf::from(target);
            let full = if target.is_absolute() {
                target
            } else {
                path.join(target)
            };
            Some(canon(&full).to_string_lossy().into_owned())
        }
        _ => None,
    }
}

/// Real paths of the git directories under `.git/modules` (submodule object stores, stashes and
/// local branches). Bounded and never follows symlinks; if the bound is hit a placeholder that
/// matches no node is added, so the caller asks.
fn list_module_gitdirs(modules: &Path) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut visited = 0usize;
    let mut stack = vec![modules.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 5000 {
                found.push("<more than 5000 entries under .git/modules>".to_string());
                return found;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            let p = entry.path();
            if p.join("HEAD").is_file() {
                found.push(canon(&p).to_string_lossy().into_owned());
                // Nested submodules live in this module's own `modules` directory.
                stack.push(p.join("modules"));
            } else {
                stack.push(p);
            }
        }
    }
    found.sort();
    found
}

/// Tie linked worktrees to their parent repo and mark repos that other clones in the scan
/// borrow objects from (alternates).
fn link_repos(builds: &mut [Build], warnings: &mut Vec<String>) {
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    let mut by_objects: HashMap<PathBuf, usize> = HashMap::new();
    for (i, b) in builds.iter().enumerate() {
        match b.node.class {
            Class::Repo => {
                by_path.insert(canon(&b.path), i);
                by_objects.insert(canon(&b.path.join(".git").join("objects")), i);
            }
            Class::BareRepo => {
                by_path.insert(canon(&b.path), i);
                by_objects.insert(canon(&b.path.join("objects")), i);
            }
            Class::Worktree => {
                by_path.insert(canon(&b.path), i);
            }
            _ => {}
        }
    }

    let mut parent_links: Vec<(usize, usize)> = Vec::new();
    let mut dependents: Vec<(usize, String)> = Vec::new();
    for (i, b) in builds.iter().enumerate() {
        if let Some(parent) = b.node.repo.as_ref().and_then(|r| r.parent_repo.as_ref()) {
            if let Some(&p) = by_path.get(&canon(Path::new(parent))) {
                if p != i {
                    parent_links.push((i, p));
                }
            }
        }
        let objects_dir = match b.node.class {
            Class::Repo => b.path.join(".git").join("objects"),
            Class::BareRepo => b.path.join("objects"),
            _ => continue,
        };
        let alt_path = objects_dir.join("info").join("alternates");
        let text = match read_small_regular_file(&alt_path) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(why) => {
                warnings.push(format!(
                    "alternates file {} was not read ({why}); clones borrowing from other repos \
                     through it are not detected",
                    alt_path.display()
                ));
                continue;
            }
        };
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let target = canon(&objects_dir.join(line));
            if let Some(&owner) = by_objects.get(&target) {
                if owner != i {
                    dependents.push((owner, b.node.path.clone()));
                }
            }
        }
    }
    for (child, parent) in parent_links {
        if let Some(info) = builds[child].node.repo.as_mut() {
            info.parent_node = Some(parent);
        }
    }
    for (owner, who) in dependents {
        if let Some(info) = builds[owner].node.repo.as_mut() {
            if !info.dependents.contains(&who) {
                info.dependents.push(who);
            }
            info.facts.no_dependents = Tri::No;
        }
    }
}
