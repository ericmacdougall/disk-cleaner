//! Data model shared by scan, plan and the JSON files. See `docs/SCAN-FORMAT.md`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Version of the scan and plan JSON schemas. Bumped on any incompatible change.
pub const SCHEMA_VERSION: u32 = 1;

/// Primary class of a directory (design section 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Repo,
    Worktree,
    BareRepo,
    Regenerable,
    Cache,
    Output,
    LooseCode,
    Other,
}

impl Class {
    /// The name used in JSON and in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Repo => "repo",
            Class::Worktree => "worktree",
            Class::BareRepo => "bare-repo",
            Class::Regenerable => "regenerable",
            Class::Cache => "cache",
            Class::Output => "output",
            Class::LooseCode => "loose-code",
            Class::Other => "other",
        }
    }

    /// repo, worktree or bare-repo.
    pub fn is_git(self) -> bool {
        matches!(self, Class::Repo | Class::Worktree | Class::BareRepo)
    }

    /// regenerable or cache.
    pub fn is_artifact(self) -> bool {
        matches!(self, Class::Regenerable | Class::Cache)
    }
}

/// A three-valued fact. `Unknown` is never treated as the safe value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tri {
    Yes,
    No,
    #[default]
    Unknown,
}

impl Tri {
    /// `Yes` for true, `No` for false.
    pub fn from_bool(b: bool) -> Tri {
        if b {
            Tri::Yes
        } else {
            Tri::No
        }
    }

    /// Display word.
    pub fn as_str(self) -> &'static str {
        match self {
            Tri::Yes => "yes",
            Tri::No => "no",
            Tri::Unknown => "unknown",
        }
    }
}

/// The repo safety facts of design section 3. Each fact is phrased so that `Yes` is the safe value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Facts {
    /// At least one remote that is a network location (a local path or bundle is not a backup).
    pub has_network_remote: Tri,
    /// The worktree's own HEAD (including a detached HEAD) is contained in a network remote branch.
    pub head_on_remote: Tri,
    /// Every local branch and tag is contained in a network remote ref.
    pub all_branches_on_remote: Tri,
    /// The stash is empty.
    pub stash_empty: Tri,
    /// No tracked changes and no untracked files.
    pub tree_clean: Tri,
    /// No ignored files outside regenerable rule directories (`.env`, local databases...).
    pub no_unique_ignored: Tri,
    /// No linked worktree or alternates clone depends on this object store.
    pub no_dependents: Tri,
}

impl Facts {
    /// Every fact paired with its JSON name.
    pub fn entries(&self) -> [(&'static str, Tri); 7] {
        [
            ("has_network_remote", self.has_network_remote),
            ("head_on_remote", self.head_on_remote),
            ("all_branches_on_remote", self.all_branches_on_remote),
            ("stash_empty", self.stash_empty),
            ("tree_clean", self.tree_clean),
            ("no_unique_ignored", self.no_unique_ignored),
            ("no_dependents", self.no_dependents),
        ]
    }

    /// True only when every fact is `Yes`.
    pub fn all_yes(&self) -> bool {
        self.entries().iter().all(|(_, t)| *t == Tri::Yes)
    }

    /// True when at least one fact is `Unknown`.
    pub fn any_unknown(&self) -> bool {
        self.entries().iter().any(|(_, t)| *t == Tri::Unknown)
    }

    /// Names of the facts that are `No`.
    pub fn failing(&self) -> Vec<&'static str> {
        self.entries()
            .iter()
            .filter(|(_, t)| *t == Tri::No)
            .map(|(n, _)| *n)
            .collect()
    }

    /// Names of the facts that are `Unknown`.
    pub fn unknown(&self) -> Vec<&'static str> {
        self.entries()
            .iter()
            .filter(|(_, t)| *t == Tri::Unknown)
            .map(|(n, _)| *n)
            .collect()
    }
}

/// A git remote with credentials already removed from the URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: String,
    /// Whether the URL counts as a network location.
    pub network: bool,
}

/// Size breakdown of a repo or worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSizes {
    /// Everything under the repo directory, including nested classified directories.
    pub total: u64,
    /// The `.git` directory.
    pub git: u64,
    /// Regenerable and cache directories directly inside the repo.
    pub regenerable: u64,
    /// Files with output extensions anywhere inside the repo.
    pub output: u64,
}

/// Git-derived information about a repo, worktree or bare repo.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoInfo {
    pub facts: Facts,
    /// Why each non-`Yes` fact is not `Yes` (redacted).
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub remotes: Vec<Remote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    /// Commit time (unix seconds) of the newest commit on any ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_time: Option<i64>,
    /// Live linked worktrees of this repo, as reported by git (may lie outside the scan).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktrees: Vec<String>,
    /// For a linked worktree: the repo it belongs to, as reported by git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_repo: Option<String>,
    /// For a linked worktree whose parent is inside the scan: index into `nodes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_node: Option<usize>,
    /// Clones inside the scan that borrow objects from this repo (alternates).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependents: Vec<String>,
    pub sizes: RepoSizes,
}

/// A classified directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub path: String,
    pub class: Class,
    /// Index of the innermost classified directory that encloses this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<usize>,
    /// Name of the rule that matched (regenerable and cache only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Bytes attributed to this node itself, excluding nested classified directories.
    pub bytes_self: u64,
    /// Bytes of this node including nested classified directories.
    pub bytes_total: u64,
    pub files_total: u64,
    /// Files with output extensions in this node's subtree (allocated bytes).
    pub output_bytes: u64,
    /// A build directory that holds output files: never offered for removal.
    pub contains_outputs: bool,
    /// Newest modification time (unix seconds) of files attributed to this node, excluding
    /// `.git` and nested classified directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_mtime: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoInfo>,
}

impl Node {
    /// A node with zeroed numbers.
    pub fn new(path: impl Into<String>, class: Class) -> Node {
        Node {
            path: path.into(),
            class,
            parent: None,
            rule: None,
            bytes_self: 0,
            bytes_total: 0,
            files_total: 0,
            output_bytes: 0,
            contains_outputs: false,
            newest_mtime: None,
            repo: None,
        }
    }
}

/// An output file (video, archive, model weight...) listed individually.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFile {
    pub path: String,
    pub bytes: u64,
    /// Index of the node that owns the directory it sits in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<usize>,
}

/// One of the biggest files seen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BigFile {
    pub path: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<usize>,
}

/// A problem met during the scan (unreadable entry...). Never fatal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanIssue {
    pub path: String,
    pub message: String,
}

/// Totals over the whole scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Allocated bytes (blocks on Unix), each hard-linked inode counted once.
    pub allocated_bytes: u64,
    /// Sum of file lengths, each hard-linked inode counted once.
    pub logical_bytes: u64,
    pub hardlink_duplicates_skipped: u64,
    /// Entries that could not be read; see `issues`.
    pub unreadable: u64,
    pub mount_points_skipped: u64,
    /// Output-extension files anywhere in the scan (allocated bytes).
    pub output_bytes: u64,
    /// `bytes_self` summed per class, plus `other` for unclassified space. Output bytes are
    /// not a separate slice here; see `output_bytes`.
    pub bytes_by_class: BTreeMap<String, u64>,
}

/// Options recorded in the scan file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanOptionsRecord {
    pub one_file_system: bool,
    pub top_files: usize,
    pub min_output_bytes: u64,
    pub git_facts: bool,
}

/// The result of `scan`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub schema_version: u32,
    pub tool_version: String,
    /// Unix seconds when the scan finished.
    pub generated_at: i64,
    pub roots: Vec<String>,
    pub options: ScanOptionsRecord,
    pub totals: Totals,
    pub nodes: Vec<Node>,
    pub outputs: Vec<OutputFile>,
    pub big_files: Vec<BigFile>,
    /// At most the first 1000 issues; `totals.unreadable` has the full count.
    pub issues: Vec<ScanIssue>,
    pub warnings: Vec<String>,
}
