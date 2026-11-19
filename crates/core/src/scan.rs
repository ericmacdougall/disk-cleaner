//! Parallel, read-only scan and classification (design section 3).
//!
//! Phase 1 walks the tree with `jwalk` and keeps one small aggregate per directory (never a
//! record per file, apart from the top-N biggest files and output files). Phase 2 sorts the
//! directories, assigns each to its innermost classified ancestor and rolls sizes up. Phase 3
//! gathers git facts for the repos found. Nothing here writes, moves or deletes anything.

use crate::error::{Error, Result};
use crate::facts::{collect_repo_info, FactsConfig, RepoCheck};
use crate::model::*;
use crate::redact::diag;
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
    /// A `.git` entry that is neither a regular directory nor a regular file (symlink, socket,
    /// fifo, device): ambiguous evidence of a repository. Never followed.
    git_odd: bool,
    has_head: bool,
    has_objects: bool,
    has_refs: bool,
    has_source: bool,
    has_manifest: bool,
    owner: Option<usize>,
    in_git: bool,
    /// First direct file whose name is a unique-state pattern (name, index into the rules'
    /// state files), split by whether package fixtures routinely use the name. Names only.
    state_hard: Option<(String, usize)>,
    state_soft: Option<(String, usize)>,
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
    /// Directories on another device that `--one-file-system` did not enter (bounded).
    skipped_paths: Vec<PathBuf>,
    /// More skipped mount points than `MAX_ISSUE_PATHS`: every node counts as incomplete.
    skipped_overflow: bool,
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
                if self.skipped_paths.len() < MAX_ISSUE_PATHS {
                    self.skipped_paths.push(path.clone());
                } else {
                    self.skipped_overflow = true;
                }
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
        if name == ".git" && !is_file {
            agg.git_odd = true;
        }
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
        if !is_dir {
            if let Some(i) = rules.state_file(name) {
                let slot = if rules.state_files()[i].fixture_prone {
                    &mut self.state_soft
                } else {
                    &mut self.state_hard
                };
                if slot.is_none() {
                    *slot = Some((name.to_string(), i));
                }
            }
        }
    }
}

/// A node under construction.
struct Build {
    /// This repo node's `.git` is a symlink or other special entry: git is never run through it.
    git_odd: bool,
    node: Node,
    path: PathBuf,
    scan_outputs: bool,
    /// The rule's package evidence (`fixture_files`, `fixture_dirs`); empty means none.
    fixture_files: Vec<String>,
    fixture_dirs: Vec<String>,
    git_bytes: u64,
    regen_bytes: u64,
}

/// `path` below `base` with `/` separators (`base` itself is the empty string).
fn rel_slash(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .map(|rel| {
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default()
}

/// Is the directory `dirs[i]` part of an installed package of the artifact `build`? Some directory
/// from it up to (not including) the artifact root must hold one of the rule's `fixture_files`
/// (a `package.json`), or sit directly below a directory named in `fixture_dirs`
/// (`site-packages`). A file straight in the artifact root has no evidence.
fn has_package_evidence(build: &Build, dirs: &[(PathBuf, DirAgg)], i: usize) -> bool {
    if build.fixture_files.is_empty() && build.fixture_dirs.is_empty() {
        return false;
    }
    let mut p = dirs[i].0.as_path();
    while p != build.path && p.starts_with(&build.path) {
        let below_named = p
            .parent()
            .and_then(|par| par.file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| build.fixture_dirs.iter().any(|d| d == n));
        if below_named {
            return true;
        }
        if let Ok(k) = dirs.binary_search_by(|(q, _)| q.as_path().cmp(p)) {
            let has = dirs[k]
                .1
                .markers
                .iter()
                .any(|m| build.fixture_files.iter().any(|f| marker_matches(f, m)));
            if has {
                return true;
            }
        }
        match p.parent() {
            Some(par) => p = par,
            None => break,
        }
    }
    false
}

/// A directory inside (or being) a regenerable or cache directory is looked at for things that
/// say it is not only generated files, which a rule match on the outer directory would hide:
/// a nested directory that its own rule says can hold unique content (`ask`, `ask_if_contains`),
/// and file names from the unique-state list. Names only; nothing is opened or read. The first
/// finding becomes the owner's `ask_reason`; it names the kind of thing, never its contents.
fn note_nested_state(build: &mut Build, dirs: &[(PathBuf, DirAgg)], i: usize, rules: &RuleSet) {
    let (path, agg) = (&dirs[i].0, &dirs[i].1);
    let rel = rel_slash(path, &build.path);
    if !rel.is_empty() {
        if let Some(rule) = match_rule(dirs, i, rules) {
            if let Some(why) = rule.ask.as_deref() {
                build.node.ask_reason.get_or_insert_with(|| {
                    diag(&format!(
                        "contains a {} directory ({rel}) that {why}",
                        rule.name
                    ))
                });
            } else if let Some(name) = agg
                .markers
                .iter()
                .find(|name| rule.ask_if_contains.iter().any(|m| marker_matches(m, name)))
            {
                build.node.ask_reason.get_or_insert_with(|| {
                    diag(&format!(
                        "contains a {} directory ({rel}) that holds {name}, which a generated \
                         directory does not: it may be a user-level directory with settings or \
                         credentials",
                        rule.name
                    ))
                });
            }
        }
    }
    let hits = [
        agg.state_hard.as_ref(),
        agg.state_soft
            .as_ref()
            .filter(|_| !has_package_evidence(build, dirs, i)),
    ];
    for (name, k) in hits.into_iter().flatten() {
        let kind = &rules.state_files()[*k].kind;
        let at = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        build.node.ask_reason.get_or_insert_with(|| {
            diag(&format!(
                "holds a file that looks like {kind} ({at}), which can exist nowhere else; it is \
                 not offered for removal until a person has looked"
            ))
        });
    }
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
    odd_dirs: &mut Vec<PathBuf>,
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
            if agg.git_odd {
                odd_dirs.push(path.clone());
            }
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
                        diag(&format!(
                            "contains a git repository ({rel_path}) that may hold unsaved work"
                        ))
                    });
                }
            }
            if opaque && agg.git_odd {
                // A `.git` that is a symlink or special file: probably a checkout whose git
                // directory lives elsewhere. It is not followed and git is not run through it.
                if let Some(o) = owner {
                    let rel_path = path.to_string_lossy();
                    builds[o].node.ask_reason.get_or_insert_with(|| {
                        diag(&format!(
                            "contains a .git entry that is not a plain directory or file \
                             ({rel_path}), so it may be a checkout holding unsaved work"
                        ))
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
                } else if agg.git_odd {
                    // Ambiguous: classified as a repo whose facts are unknown, never as loose
                    // code that could be offered for removal with its regenerable children.
                    found = Some((Class::Repo, None));
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
                    if let Some(r) = rule {
                        if let Some(why) = r.ask.as_deref() {
                            node.ask_reason.get_or_insert_with(|| why.to_string());
                        }
                        let unexpected = agg
                            .markers
                            .iter()
                            .find(|name| r.ask_if_contains.iter().any(|m| marker_matches(m, name)));
                        if let Some(name) = unexpected {
                            node.ask_reason.get_or_insert_with(|| {
                                format!(
                                    "holds {name}, which a {} directory does not: it may be a \
                                     user-level directory with settings or credentials, not \
                                     generated files",
                                    r.name
                                )
                            });
                        }
                    }
                    builds.push(Build {
                        node,
                        path: path.clone(),
                        scan_outputs: rule.map(|r| r.scan_outputs).unwrap_or(false),
                        fixture_files: rule.map(|r| r.fixture_files.clone()).unwrap_or_default(),
                        fixture_dirs: rule.map(|r| r.fixture_dirs.clone()).unwrap_or_default(),
                        git_odd: agg.git_odd,
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
            // Inside (or being) an artifact directory nothing is classified, but what rule
            // evaluation would have said about it still counts.
            if let Some(o) = new_owner {
                if builds[o].node.class.is_artifact() && !now_in_git {
                    note_nested_state(&mut builds[o], dirs, i, rules);
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
        skipped_paths: Vec::new(),
        skipped_overflow: false,
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
        skipped_paths,
        skipped_overflow,
        ..
    } = walk;
    issues.truncate(MAX_ISSUES);

    // Phase 2: classification.
    let mut dirs: Vec<(PathBuf, DirAgg)> = aggs.into_iter().collect();
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let root_set: HashSet<PathBuf> = roots.iter().cloned().collect();
    let mut odd_dirs: Vec<PathBuf> = Vec::new();
    let mut builds = classify(&mut dirs, &opts.rules, &root_set, &mut odd_dirs);

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
    // A mount point that was not entered: its contents were never seen, so the node that holds
    // it (the mount point itself when a rule matched it) is incomplete in the same way. Enclosing
    // nodes follow through the plan, where a nested `ask` blocks its parent.
    if skipped_overflow {
        for b in builds.iter_mut() {
            b.node.skipped_mounts += 1;
        }
    }
    for p in &skipped_paths {
        let owner = owner_of(p.as_path()).or_else(|| p.parent().and_then(&owner_of));
        if let Some(o) = owner {
            builds[o].node.skipped_mounts += 1;
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
                        && c.node.skipped_mounts == 0
                })
                .map(|c| c.path.clone())
                .collect();
            (i, b.path.clone(), b.node.class, regen)
        })
        .collect();
    let mut unverified: Vec<(usize, PathBuf, String)> = Vec::new();
    // A repo whose `.git` is a symlink or special file: git is not run through it, every fact is
    // unknown and the artifacts inside ask.
    let (odd_tasks, tasks): (Vec<_>, Vec<_>) = tasks.into_iter().partition(|t| builds[t.0].git_odd);
    for (i, _, _, regen) in &odd_tasks {
        infos.insert(
            *i,
            RepoInfo {
                notes: vec![
                    "the .git entry is a symlink or special file, so it was not followed and no \
                     git command was run here"
                        .to_string(),
                ],
                layout_issue: Some(
                    "the .git entry is a symlink or special file, not a real directory".to_string(),
                ),
                ..RepoInfo::default()
            },
        );
        for p in regen {
            unverified.push((
                *i,
                p.clone(),
                "has a .git entry above it that is not a plain directory or file, so git was \
                 not asked whether it is ignored"
                    .to_string(),
            ));
        }
    }
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
                layout_issue: Some("git facts were not collected".to_string()),
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
            if b.node.class == Class::Repo && !b.git_odd {
                info.submodule_gitdirs = list_module_gitdirs(&b.path.join(".git").join("modules"));
            }
            if !b.git_odd {
                info.git_dir = node_git_dir(&b.path, b.node.class);
            }
            info.sizes = RepoSizes {
                total: b.node.bytes_total,
                git: b.git_bytes,
                regenerable: b.regen_bytes,
                output: b.node.output_bytes,
            };
            b.node.repo = Some(info);
        }
    }
    link_repos(&mut builds, &roots, &mut warnings);
    link_odd_gits(&mut builds, &odd_dirs, &mut warnings);

    // Scan roots inside a repository the scan does not cover: the artifacts there are checked
    // against that repository's git, exactly as a full-repo scan would.
    let enclosing_repos = check_enclosing_repos(&mut builds, &roots, opts);

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
        enclosing_repos,
        scope: roots_scope(&roots).to_string(),
    })
}

/// The nearest repository above `root`, looking at each ancestor's `.git` without following
/// symlinks: `(repository directory, kind of .git entry)`. The canonical path is searched first
/// (git works from the real directory), then the spelled one.
fn find_enclosing_repo(root: &Path) -> Option<(PathBuf, GitEntry)> {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for start in [real.as_path(), root] {
        for dir in start.ancestors().skip(1) {
            match std::fs::symlink_metadata(dir.join(".git")) {
                Ok(m) if m.is_dir() => return Some((dir.to_path_buf(), GitEntry::Dir)),
                Ok(m) if m.is_file() => return Some((dir.to_path_buf(), GitEntry::File)),
                Ok(_) => return Some((dir.to_path_buf(), GitEntry::Odd)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Cannot tell: treat as ambiguous evidence.
                Err(_) => return Some((dir.to_path_buf(), GitEntry::Odd)),
            }
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GitEntry {
    Dir,
    File,
    Odd,
}

/// For each scan root that lies inside a repository the scan does not cover, decide the
/// artifacts below it that no scanned repo owns by asking that repository's git (config
/// allowlist, wholly ignored and untracked), or make them ask when that cannot be established.
/// The enclosing repository itself is never a node and never gets a plan item.
fn check_enclosing_repos(
    builds: &mut [Build],
    roots: &[PathBuf],
    opts: &ScanOptions,
) -> Vec<EnclosingRepo> {
    let mut out: Vec<EnclosingRepo> = Vec::new();
    let mut seen: Vec<(PathBuf, GitEntry, Vec<PathBuf>)> = Vec::new();
    for root in roots {
        let (repo, kind) = match find_enclosing_repo(root) {
            Some(x) => x,
            None => continue,
        };
        // A repository inside another scan root is walked and is a node there.
        if roots.iter().any(|r| repo.starts_with(r)) {
            continue;
        }
        out.push(EnclosingRepo {
            root: root.to_string_lossy().into_owned(),
            repo: repo.to_string_lossy().into_owned(),
            note: "this scan root is inside a git repository that the scan did not cover; only \
                   the artifact directories below the root were checked against it, and the \
                   repository itself was not assessed"
                .to_string(),
        });
        // Artifacts under this root that no scanned git node encloses.
        let mut paths: Vec<PathBuf> = Vec::new();
        for i in 0..builds.len() {
            if !builds[i].node.class.is_artifact() || !builds[i].path.starts_with(root) {
                continue;
            }
            let mut cur = builds[i].node.parent;
            let mut owned = false;
            while let Some(p) = cur {
                if builds[p].node.class.is_git() {
                    owned = true;
                    break;
                }
                cur = builds[p].node.parent;
            }
            if owned {
                continue;
            }
            let b = &builds[i];
            let believed_removable = !b.node.contains_outputs
                && b.node.ask_reason.is_none()
                && b.node.unreadable == 0
                && b.node.skipped_mounts == 0;
            if believed_removable {
                paths.push(b.path.clone());
            }
        }
        if let Some(pos) = seen.iter().position(|(r, _, _)| *r == repo) {
            seen[pos].2.extend(paths);
        } else {
            seen.push((repo, kind, paths));
        }
    }
    for (repo, kind, paths) in seen {
        let mut why_not: Vec<(PathBuf, String)> = Vec::new();
        if kind == GitEntry::Odd {
            for p in &paths {
                why_not.push((
                    p.clone(),
                    "is inside a repository whose .git entry is not a plain directory or file, \
                     so git was not asked whether it is ignored"
                        .to_string(),
                ));
            }
        } else if !opts.collect_facts {
            for p in &paths {
                why_not.push((
                    p.clone(),
                    "is inside a repository above the scan root and git facts were not \
                     collected, so it is not verified to be ignored"
                        .to_string(),
                ));
            }
        } else if !paths.is_empty() {
            let class = if kind == GitEntry::File {
                Class::Worktree
            } else {
                Class::Repo
            };
            // Real paths on both sides, so the artifacts can be related to the repository
            // however the root was spelled (a symlinked temp or home directory).
            let real: Vec<PathBuf> = paths.iter().map(|p| canon(p)).collect();
            let check = collect_repo_info(&opts.facts, &canon(&repo), class, &real);
            why_not.extend(check.unverified_artifacts);
        }
        for (p, why) in why_not {
            let hit = builds
                .iter_mut()
                .find(|b| b.path == p || canon(&b.path) == p);
            if let Some(b) = hit {
                b.node
                    .ask_reason
                    .get_or_insert_with(|| diag(&format!("{why} ({})", repo.to_string_lossy())));
            }
        }
    }
    out
}

/// Git directories that other directories reach through a `.git` symlink have dependents:
/// removing them would break those checkouts. The link is read as a path only (`read_link`,
/// then a canonicalisation of the path); nothing is followed to run git or read its contents.
///
/// If the target is, or lies inside, a git node of the scan, that node gets `no_dependents = no`
/// naming the dependent. If a `.git` entry is not a symlink, cannot be resolved, or leads outside
/// every git node of the scan, which git directory it uses is unknown, so every git node that
/// had `no_dependents = yes` gets `unknown` (a known `no` stays).
fn link_odd_gits(builds: &mut [Build], odd_dirs: &[PathBuf], warnings: &mut Vec<String>) {
    if odd_dirs.is_empty() {
        return;
    }
    let real: Vec<Option<PathBuf>> = builds
        .iter()
        .map(|b| b.node.class.is_git().then(|| canon(&b.path)))
        .collect();
    let mut unknown_why: Option<String> = None;
    let mut dependents: Vec<(usize, String)> = Vec::new();
    for dir in odd_dirs {
        let link = dir.join(".git");
        let shown = dir.to_string_lossy().into_owned();
        let target = match std::fs::symlink_metadata(&link) {
            Ok(m) if m.file_type().is_symlink() => std::fs::read_link(&link).ok(),
            _ => None,
        };
        let resolved = target.and_then(|t| {
            let full = if t.is_absolute() { t } else { dir.join(t) };
            std::fs::canonicalize(full).ok()
        });
        match resolved {
            None => {
                unknown_why.get_or_insert(format!(
                    "the .git entry of {shown} is not a symlink that can be resolved, so which \
                     git directory it uses is unknown"
                ));
            }
            Some(r) => {
                let hits: Vec<usize> = real
                    .iter()
                    .enumerate()
                    .filter(|(i, p)| {
                        builds[*i].path != *dir && p.as_ref().is_some_and(|p| r.starts_with(p))
                    })
                    .map(|(i, _)| i)
                    .collect();
                if hits.is_empty() {
                    unknown_why.get_or_insert(format!(
                        "the .git symlink of {shown} leads outside every git directory in the \
                         scan, so which repository depends on it is unknown"
                    ));
                }
                for i in hits {
                    dependents.push((i, shown.clone()));
                }
            }
        }
    }
    for (i, who) in dependents {
        if let Some(info) = builds[i].node.repo.as_mut() {
            info.facts.no_dependents = Tri::No;
            if !info.dependents.contains(&who) {
                info.notes.push(diag(&format!(
                    "no_dependents: the checkout at {who} reaches this git directory through a \
                     .git symlink"
                )));
                info.dependents.push(diag(&who));
            }
        }
    }
    if let Some(why) = unknown_why {
        let why = diag(&why);
        warnings.push(why.clone());
        for b in builds.iter_mut() {
            if let Some(info) = b.node.repo.as_mut() {
                if info.facts.no_dependents == Tri::Yes {
                    info.facts.no_dependents = Tri::Unknown;
                    info.notes.push(format!("no_dependents: {why}"));
                }
            }
        }
    }
}

/// `volume` when every scan root is a mount point (its device differs from its parent's) or a
/// file system root, `subtree` otherwise.
fn roots_scope(roots: &[PathBuf]) -> &'static str {
    let is_volume = |r: &PathBuf| -> bool {
        let real = std::fs::canonicalize(r).unwrap_or_else(|_| r.clone());
        let parent = match real.parent() {
            Some(p) => p,
            None => return true,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            match (std::fs::metadata(&real), std::fs::metadata(parent)) {
                (Ok(a), Ok(b)) => a.dev() != b.dev(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = parent;
            false
        }
    };
    if !roots.is_empty() && roots.iter().all(is_volume) {
        "volume"
    } else {
        "subtree"
    }
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

/// One entry of an alternates file after parsing git's syntax.
#[derive(Debug, PartialEq, Eq)]
enum Alt {
    /// An object directory, absolute or relative to the borrower's `objects` directory.
    Path(PathBuf),
    /// An entry that could not be interpreted (malformed quoting, not valid as a path).
    Bad(String),
}

/// Turn raw bytes of a path into a `PathBuf`, or `None` if the platform cannot represent them.
fn path_from_bytes(bytes: Vec<u8>) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

/// Undo git's C-style quoting of one entry that starts with a double quote: `\\`, `\"`, `\a`,
/// `\b`, `\f`, `\n`, `\r`, `\t`, `\v` and three-digit octal `\ooo` (first digit 0 to 3). Returns the
/// bytes and the text after the closing quote, or `None` when the quoting is malformed.
fn unquote_c_style(s: &str) -> Option<(Vec<u8>, &str)> {
    let b = s.as_bytes();
    if b.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'"' => return Some((out, &s[i + 1..])),
            b'\\' => {
                i += 1;
                let c = *b.get(i)?;
                match c {
                    b'\\' | b'"' => out.push(c),
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(10),
                    b'r' => out.push(13),
                    b't' => out.push(9),
                    b'v' => out.push(11),
                    b'0'..=b'3' => {
                        let d1 = *b.get(i + 1)?;
                        let d2 = *b.get(i + 2)?;
                        if !(b'0'..=b'7').contains(&d1) || !(b'0'..=b'7').contains(&d2) {
                            return None;
                        }
                        out.push((c - b'0') * 64 + (d1 - b'0') * 8 + (d2 - b'0'));
                        i += 2;
                    }
                    _ => return None,
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None // no closing quote
}

/// Parse an alternates file: one entry per line; blank lines and lines starting with `#` are
/// skipped; an entry starting with `"` is C-style quoted; anything else is a path taken as written.
/// Nothing is dropped: an entry that cannot be interpreted is returned as `Alt::Bad`.
fn parse_alternates(text: &str) -> Vec<Alt> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('"') {
            match unquote_c_style(line) {
                Some((bytes, rest)) if rest.trim().is_empty() => match path_from_bytes(bytes) {
                    Some(p) if !p.as_os_str().is_empty() => out.push(Alt::Path(p)),
                    _ => out.push(Alt::Bad("a quoted entry is not a usable path".to_string())),
                },
                _ => out.push(Alt::Bad("a quoted entry is malformed".to_string())),
            }
        } else {
            out.push(Alt::Path(PathBuf::from(line)));
        }
    }
    out
}

/// What kind of thing a path is, without following a final symlink.
enum Kind {
    Missing,
    Dir,
    Symlink,
    Other,
}

fn kind_of(p: &Path) -> std::result::Result<Kind, String> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => Ok(Kind::Symlink),
        Ok(m) if m.is_dir() => Ok(Kind::Dir),
        Ok(_) => Ok(Kind::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Kind::Missing),
        Err(e) => Err(e.to_string()),
    }
}

/// The object store a `.git` file leads to, read with bounded reads and no git command:
/// `gitdir: <path>` (relative to the repo), then that directory's `commondir` file if any, then
/// `objects`. Everything is canonicalised; anything unreadable is an error.
fn resolve_gitfile_store(path: &Path) -> std::result::Result<PathBuf, String> {
    let file = path.join(".git");
    let text = read_small_regular_file(&file)
        .map_err(|e| format!("the .git file was not read ({e})"))?
        .ok_or_else(|| "the .git file is missing".to_string())?;
    let line = text.lines().next().unwrap_or("").trim_end();
    let rest = line
        .strip_prefix("gitdir:")
        .ok_or_else(|| "the .git file does not name a git directory".to_string())?
        .trim();
    let gd = std::fs::canonicalize(path.join(rest))
        .map_err(|e| format!("the git directory it names cannot be resolved ({e})"))?;
    let cd = gd.join("commondir");
    let common = match kind_of(&cd) {
        Ok(Kind::Missing) => gd.clone(),
        Ok(Kind::Other) => {
            let t = read_small_regular_file(&cd)
                .map_err(|e| format!("the commondir was not read ({e})"))?
                .unwrap_or_default();
            std::fs::canonicalize(gd.join(t.lines().next().unwrap_or("")))
                .map_err(|e| format!("the commondir cannot be resolved ({e})"))?
        }
        _ => return Err("the commondir is not a plain file".to_string()),
    };
    std::fs::canonicalize(common.join("objects"))
        .map_err(|e| format!("the object store cannot be resolved ({e})"))
}

fn add_member(members: &mut HashMap<PathBuf, Vec<usize>>, key: PathBuf, i: usize) {
    let list = members.entry(key).or_default();
    if !list.contains(&i) {
        list.push(i);
    }
}

/// Tie linked worktrees to their parent repo and find which repos use the same objects.
///
/// Every user of each canonical object directory is kept (a multimap), so two repos that share a
/// directory (a symlinked `objects`, or a `commondir` into another repo) are dependents of each
/// other. Alternates entries are parsed in git's own syntax and resolved against the borrower's
/// object directory; chains are followed through scanned repos only. Whatever cannot be placed on
/// a scanned repo (an entry that is malformed, cannot be resolved, or leads outside the scan; a
/// symlinked `objects`, `objects/info` or `objects/pack` that leaves the scan) makes the repo
/// that holds it an "unresolved borrower": every other git node loses a `yes` it can no longer
/// prove. Symlinks are read as paths; nothing is followed to run git.
fn link_repos(builds: &mut [Build], roots: &[PathBuf], warnings: &mut Vec<String>) {
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    for (i, b) in builds.iter().enumerate() {
        if b.node.class.is_git() {
            by_path.insert(canon(&b.path), i);
        }
    }

    // Phase 1: the object directory of every repo and what it declares.
    let mut members: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    let mut symlinked: Vec<(usize, PathBuf)> = Vec::new();
    let mut via_commondir: Vec<(usize, PathBuf)> = Vec::new();
    let mut part_links: Vec<(usize, PathBuf)> = Vec::new();
    // Object directories that are a real directory of a scanned repo: the stores that were
    // inspected as their owner's own.
    let mut own_keys: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // `.git`-file repos (linked worktrees, submodule checkouts, --separate-git-dir) and the
    // object store their effective git directory points at.
    let mut store_users: Vec<(usize, PathBuf)> = Vec::new();
    let mut alt_entries: Vec<(usize, PathBuf, String)> = Vec::new();
    let mut unresolved: Vec<(usize, String)> = Vec::new();
    let mut parent_links: Vec<(usize, usize)> = Vec::new();
    for (i, b) in builds.iter().enumerate() {
        if let Some(parent) = b.node.repo.as_ref().and_then(|r| r.parent_repo.as_ref()) {
            if let Some(&p) = by_path.get(&canon(Path::new(parent))) {
                if p != i {
                    parent_links.push((i, p));
                }
            }
        }
        if b.node.class == Class::Worktree && !b.git_odd {
            match resolve_gitfile_store(&b.path) {
                Ok(key) => store_users.push((i, key)),
                Err(why) => {
                    warnings.push(diag(&format!(
                        "{}: {why}; which object store it uses cannot be identified, so no \
                         other repo in the scan is treated as free of dependents",
                        b.path.display()
                    )));
                    unresolved.push((i, why));
                }
            }
            continue;
        }
        let (objects_dir, git_dir) = match b.node.class {
            Class::Repo if b.git_odd => continue,
            Class::Repo => (
                b.path.join(".git").join("objects"),
                Some(b.path.join(".git")),
            ),
            Class::BareRepo => (b.path.join("objects"), None),
            _ => continue,
        };
        let mut bad: Vec<String> = Vec::new();
        let mut objects_is_link = false;
        // The objects directory itself.
        match kind_of(&objects_dir) {
            Ok(Kind::Dir) => match std::fs::canonicalize(&objects_dir) {
                Ok(k) => {
                    own_keys.insert(k.clone());
                    add_member(&mut members, k, i);
                }
                Err(e) => bad.push(format!(
                    "{} cannot be resolved ({e})",
                    objects_dir.display()
                )),
            },
            Ok(Kind::Symlink) => match std::fs::canonicalize(&objects_dir) {
                Ok(k) => {
                    objects_is_link = true;
                    add_member(&mut members, k.clone(), i);
                    symlinked.push((i, k));
                }
                Err(e) => bad.push(format!(
                    "{} is a symlink that cannot be resolved ({e})",
                    objects_dir.display()
                )),
            },
            Ok(Kind::Missing) => {}
            Ok(Kind::Other) => bad.push(format!(
                "{} is not a plain directory",
                objects_dir.display()
            )),
            Err(e) => bad.push(format!("cannot read {} ({e})", objects_dir.display())),
        }
        // objects/info and objects/pack: plain directories, or a symlink read as a path.
        for sub in ["info", "pack"] {
            let p = objects_dir.join(sub);
            match kind_of(&p) {
                Ok(Kind::Dir) | Ok(Kind::Missing) => {}
                Ok(Kind::Symlink) => match std::fs::canonicalize(&p) {
                    Ok(t) => part_links.push((i, t)),
                    Err(e) => bad.push(format!(
                        "{} is a symlink that cannot be resolved ({e})",
                        p.display()
                    )),
                },
                Ok(Kind::Other) => bad.push(format!("{} is not a plain directory", p.display())),
                Err(e) => bad.push(format!("cannot read {} ({e})", p.display())),
            }
        }
        // A `commondir` file in a repo's git directory moves its refs and objects elsewhere.
        if let Some(gd) = &git_dir {
            let cd = gd.join("commondir");
            match kind_of(&cd) {
                Ok(Kind::Missing) => {}
                Ok(Kind::Other) => match read_small_regular_file(&cd) {
                    Ok(Some(text)) => {
                        let line = text.lines().next().unwrap_or("");
                        match std::fs::canonicalize(gd.join(line).join("objects")) {
                            Ok(k) => {
                                add_member(&mut members, k.clone(), i);
                                via_commondir.push((i, k));
                            }
                            Err(e) => bad.push(format!(
                                "the commondir {} cannot be resolved ({e})",
                                cd.display()
                            )),
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        bad.push(format!("the commondir {} was not read ({e})", cd.display()))
                    }
                },
                Ok(_) | Err(_) => bad.push(format!("{} is not a plain file", cd.display())),
            }
        }
        for why in bad {
            warnings.push(diag(&format!(
                "{why}; which repo it shares objects with cannot be identified, so no other repo \
                 in the scan is treated as free of dependents"
            )));
            unresolved.push((i, why));
        }
        // Alternates. When `objects` is a symlink the file belongs to the directory's owner, who
        // is read as its own repo (or is outside the scan and already unresolved).
        if objects_is_link {
            continue;
        }
        let alt_path = objects_dir.join("info").join("alternates");
        let text = match read_small_regular_file(&alt_path) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(why) => {
                warnings.push(diag(&format!(
                    "alternates file {} was not read ({why}); the repo it borrows objects from \
                     cannot be identified, so no other repo in the scan is treated as free of \
                     dependents",
                    alt_path.display()
                )));
                unresolved.push((i, alt_path.display().to_string()));
                continue;
            }
        };
        for entry in parse_alternates(&text) {
            let problem = match entry {
                Alt::Bad(why) => Some(why),
                Alt::Path(p) => match std::fs::canonicalize(objects_dir.join(&p)) {
                    Ok(c) => {
                        alt_entries.push((i, c, alt_path.display().to_string()));
                        None
                    }
                    Err(e) => Some(format!(
                        "the entry {} cannot be resolved ({e})",
                        p.display()
                    )),
                },
            };
            if let Some(why) = problem {
                warnings.push(diag(&format!(
                    "alternates file {}: {why}; the repo it borrows objects from cannot be \
                     identified, so no other repo in the scan is treated as free of dependents",
                    alt_path.display()
                )));
                unresolved.push((i, alt_path.display().to_string()));
            }
        }
    }

    // Phase 2: relate everything to scanned repos.
    let real_roots: Vec<PathBuf> = roots.iter().map(|r| canon(r)).collect();
    // Stores reached through a symlink, a commondir or a `.git` file that are not a scanned
    // repo's own object directory.
    let mut foreign: Vec<(usize, PathBuf)> = Vec::new();
    for (i, k) in symlinked
        .iter()
        .chain(via_commondir.iter())
        .chain(store_users.iter())
    {
        if !own_keys.contains(k) {
            foreign.push((*i, k.clone()));
        }
    }
    for (i, k) in &foreign {
        if !real_roots.iter().any(|r| k.starts_with(r)) {
            let why = format!(
                "the object store {} used by {} lies outside the scan",
                k.display(),
                builds[*i].path.display()
            );
            warnings.push(diag(&why));
            unresolved.push((*i, why));
        }
    }
    // Each such store's alternates are inspected once, whoever reaches it.
    let mut inspected: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut alt_from_stores: Vec<(usize, PathBuf, String)> = Vec::new();
    for (_, k) in &foreign {
        if !inspected.insert(k.clone()) {
            continue;
        }
        let users: Vec<usize> = foreign
            .iter()
            .filter(|(_, q)| q == k)
            .map(|(u, _)| *u)
            .collect();
        let alt_path = k.join("info").join("alternates");
        let text = match read_small_regular_file(&alt_path) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(why) => {
                warnings.push(diag(&format!(
                    "alternates file {} was not read ({why}); the repo it borrows objects from \
                     cannot be identified",
                    alt_path.display()
                )));
                for u in &users {
                    unresolved.push((*u, alt_path.display().to_string()));
                }
                continue;
            }
        };
        for entry in parse_alternates(&text) {
            let resolved = match entry {
                Alt::Path(p) => std::fs::canonicalize(k.join(&p)).ok(),
                Alt::Bad(_) => None,
            };
            match resolved {
                Some(c) => {
                    for u in &users {
                        alt_from_stores.push((*u, c.clone(), alt_path.display().to_string()));
                    }
                }
                None => {
                    warnings.push(diag(&format!(
                        "an entry of the alternates file {} cannot be interpreted; the repo it \
                         borrows objects from cannot be identified",
                        alt_path.display()
                    )));
                    for u in &users {
                        unresolved.push((*u, alt_path.display().to_string()));
                    }
                }
            }
        }
    }
    alt_entries.extend(alt_from_stores);
    let mut shared: Vec<(usize, usize)> = Vec::new();
    for list in members.values() {
        for &a in list {
            for &b in list {
                if a != b {
                    shared.push((a, b));
                }
            }
        }
    }
    for (i, t) in &part_links {
        let owners: Vec<usize> = members
            .iter()
            .filter(|(k, _)| t.starts_with(k.as_path()))
            .flat_map(|(_, l)| l.iter().copied())
            .filter(|j| j != i)
            .collect();
        if owners.is_empty() {
            let why = format!(
                "an objects subdirectory of {} is a symlink that leads outside the scan",
                builds[*i].path.display()
            );
            warnings.push(diag(&why));
            unresolved.push((*i, why));
        }
        for j in owners {
            shared.push((*i, j));
            shared.push((j, *i));
        }
    }
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (i, c, file) in &alt_entries {
        match members.get(c) {
            Some(list) => {
                for &p in list.iter().filter(|p| **p != *i) {
                    edges.push((*i, p));
                }
            }
            None => {
                warnings.push(diag(&format!(
                    "an entry of the alternates file {file} is not the object directory of any \
                     repo in the scan ({}); the repo it borrows objects from cannot be identified",
                    c.display()
                )));
                unresolved.push((*i, file.clone()));
            }
        }
    }
    // A `.git`-file repo whose effective object store is a scanned repo's own depends on it.
    for (w, k) in &store_users {
        if let Some(list) = members.get(k) {
            for &p in list.iter().filter(|p| **p != *w) {
                edges.push((*w, p));
            }
        }
    }
    // A linked worktree keeps its refs in, and borrows objects from, the repo git says it
    // belongs to.
    for &(child, parent) in &parent_links {
        if builds[child].node.class == Class::Worktree {
            edges.push((child, parent));
        }
    }
    // Chains: a repo also depends on whatever its providers borrow from.
    let n = builds.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(b, p) in &edges {
        if !adj[b].contains(&p) {
            adj[b].push(p);
        }
    }
    let mut chained: Vec<(usize, usize, bool)> = Vec::new(); // (provider, user, direct)
    for start in 0..n {
        if adj[start].is_empty() {
            continue;
        }
        let mut seen = vec![false; n];
        let mut stack: Vec<usize> = adj[start].clone();
        while let Some(p) = stack.pop() {
            if seen[p] || p == start {
                continue;
            }
            seen[p] = true;
            chained.push((p, start, adj[start].contains(&p)));
            stack.extend(adj[p].iter().copied());
        }
    }

    for (child, parent) in parent_links {
        if let Some(info) = builds[child].node.repo.as_mut() {
            info.parent_node = Some(parent);
        }
    }
    let paths: Vec<String> = builds.iter().map(|b| b.node.path.clone()).collect();
    let mark = |builds: &mut [Build], owner: usize, who: &str, why: &str| {
        if let Some(info) = builds[owner].node.repo.as_mut() {
            if !info.dependents.iter().any(|d| d == who) {
                info.dependents.push(who.to_string());
                info.notes
                    .push(diag(&format!("no_dependents: {who} {why}")));
            }
            info.facts.no_dependents = Tri::No;
        }
    };
    for (a, b) in shared {
        mark(
            builds,
            a,
            &paths[b],
            "shares an object directory with this repo, so its commits may live here",
        );
    }
    for (p, user, direct) in chained {
        let why = if direct {
            "borrows objects from this repo"
        } else {
            "borrows objects from this repo through a chain of alternates"
        };
        mark(builds, p, &paths[user], why);
    }

    // An entry that could not be placed may point at any repo in the scan. Every git node other
    // than the borrower itself loses a `yes` it can no longer prove (a known `no` stays `no`), so
    // none of them is offered for removal.
    for (j, b) in builds.iter_mut().enumerate() {
        if !b.node.class.is_git() {
            continue;
        }
        let Some((_, file)) = unresolved.iter().find(|(i, _)| *i != j) else {
            continue;
        };
        if let Some(info) = b.node.repo.as_mut() {
            if info.facts.no_dependents == Tri::Yes {
                info.facts.no_dependents = Tri::Unknown;
                info.notes.push(diag(&format!(
                    "no_dependents: {file} could not be placed on a repo in the scan, so a clone \
                     in the scan may borrow objects from this repo"
                )));
            }
        }
    }
}

#[cfg(test)]
mod alternates_tests {
    use super::*;

    #[test]
    fn plain_quoted_relative_comment_and_blank_entries() {
        let text = "# c\n\n/abs/path\n../rel\n\"/with space/\\303\\251\"\n";
        let got = parse_alternates(text);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], Alt::Path(PathBuf::from("/abs/path")));
        assert_eq!(got[1], Alt::Path(PathBuf::from("../rel")));
        assert_eq!(got[2], Alt::Path(PathBuf::from("/with space/\u{e9}")));
    }

    #[test]
    fn escapes_and_malformed_quoting() {
        let (b, rest) = unquote_c_style("\"a\\\\b\\\"c\\n\\t\\a\\b\\f\\r\\v\" tail").unwrap();
        assert_eq!(b, b"a\\b\"c\n\t\x07\x08\x0c\r\x0b".to_vec());
        assert_eq!(rest, " tail");
        for bad in [
            "\"abc",
            "\"a\\q\"",
            "\"a\\4\"",
            "\"a\\30\"",
            "\"a\\389\"",
            "\"a\\",
        ] {
            assert!(unquote_c_style(bad).is_none(), "{bad}");
        }
        assert!(matches!(
            parse_alternates("\"/x\" trailing\n").as_slice(),
            [Alt::Bad(_)]
        ));
        assert!(matches!(
            parse_alternates("\"\"\n").as_slice(),
            [Alt::Bad(_)]
        ));
    }
}
