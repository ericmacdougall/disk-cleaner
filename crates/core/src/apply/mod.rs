//! `apply` lane 1: the guarded, journalled removal of `remove-regenerable` items of a reviewed
//! plan (GoodQA goodqa#16692).
//!
//! This is the only part of the library that changes anything on disk, and it is built around a
//! few choke points so that the defect classes the earlier designs fell to cannot come back:
//!
//! - [`journal`]: the one owner of state. A mutation needs a [`journal::Proof`], which only an
//!   fsynced intent record produces.
//! - [`fsops`]: the one place that touches the file system, by directory fds, never through links.
//! - [`mounts`]: the one reader of the mount table.
//! - The seams [`GuardState`], [`Reach`] and [`MountTable`](mounts::MountTable): what the host
//!   install, the in-use rules and the mount table say. Their production defaults refuse; a test
//!   injects what it needs, and the `cfg(test)`-only [`hooks`] never exist in a build of the
//!   library or the binary.
//!
//! The run order is the spec's class A: courtesy check, plan id, host lock, resume or the
//! unfinished-journal refusal, then (new work only) the guard state, the plan's schema and age, a
//! fresh scan and plan, and the items both plans agree on.

pub mod classify;
pub mod fsops;
pub mod gates;
#[cfg(test)]
mod hooks;
pub mod item;
pub mod journal;
pub mod mounts;
pub mod resume;
pub mod run;
#[cfg(test)]
mod tests;

pub use run::run;

use crate::evidence::HostInputs;
use crate::facts::FactsConfig;
use crate::protect::ScanContext;
use fsops::FileId;
use mounts::MountTable;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Errors of the apply engine (a refusal is an [`Outcome`], not an error).
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid JSON: {0}")]
    Json(serde_json::Error),
    #[error("journal {path} is corrupt at line {line}: {message}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("{0}")]
    Core(#[from] crate::Error),
}

impl ApplyError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> ApplyError {
        ApplyError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, ApplyError>;

/// What the host's install says about the guard (the harness hook that denies an agent running
/// `apply`). New work needs `Ok`; `--resume` only prints a drift. The production default
/// [`Unverified`] refuses until the install check exists and has passed.
pub trait GuardState {
    fn check(&self) -> std::result::Result<(), String>;
}

/// The guard state as it is until `scripts/install.sh` has written and verified one: refuses.
pub struct Unverified;

impl GuardState for Unverified {
    fn check(&self) -> std::result::Result<(), String> {
        Err(
            "no verified guard state for this host: apply stays plan-only until the install \
             check has passed"
                .to_string(),
        )
    }
}

/// What a process-reach test is asked about.
pub struct ReachQuery<'a> {
    /// The item's original path (the owning project is computed from it).
    pub original: &'a Path,
    /// Its quarantine path, once it has been moved.
    pub quarantine: Option<&'a Path>,
    /// Identities of everything inside, once inventoried (for matching open files by inode).
    pub ids: Option<&'a HashSet<FileId>>,
}

/// Why a process could reach an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachFinding {
    /// The reach rule (`M`, `R0`, `R2` to `R5`).
    pub rule: &'static str,
    /// Pid and command name, or what could not be ruled out.
    pub detail: String,
}

impl std::fmt::Display for ReachFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.rule, self.detail)
    }
}

/// The in-use test, run at plan time through the evidence module and again at every step of an
/// item (class D). The production default [`ReachUnbuilt`] reaches everything.
pub trait Reach {
    fn reaches(&self, q: &ReachQuery) -> Option<ReachFinding>;
}

/// Until the reach rules are wired in, nothing is known to be idle: every item is skipped.
pub struct ReachUnbuilt;

impl Reach for ReachUnbuilt {
    fn reaches(&self, _q: &ReachQuery) -> Option<ReachFinding> {
        Some(ReachFinding {
            rule: "R5",
            detail: "the in-use rules of apply are not built into this binary".to_string(),
        })
    }
}

/// What a run needs from the host. No field has a default that touches the real home: the state
/// directory is always given.
pub struct Env<'a> {
    /// Holds `apply.lock`, `journal/` and the guard state (`~/.prunedisk`, or a fixture home's).
    pub state_dir: PathBuf,
    /// The home directory (for the sandbox's outside-the-home rule).
    pub home: PathBuf,
    /// Current time, unix seconds.
    pub now: i64,
    /// Plans older than this many days are refused as new work.
    pub max_plan_age_days: u64,
    /// The process snapshot and container state for the fresh plan.
    pub host: HostInputs,
    /// How to resolve protected roots for the fresh scan.
    pub scan_context: ScanContext,
    /// Git for the fresh scan (its timeout).
    pub facts: FactsConfig,
    pub guard: &'a dyn GuardState,
    pub reach: &'a dyn Reach,
    pub mounts: &'a dyn MountTable,
}

/// What the command line asked.
pub struct Request<'a> {
    pub plan_file: Option<&'a Path>,
    pub plan_id: Option<&'a str>,
    /// `--only`: only `regenerable` is a lane.
    pub only: Option<&'a str>,
    pub resume: bool,
    /// `--journal PLANID`: resume without the plan file.
    pub journal: Option<&'a str>,
    pub max_items: Option<usize>,
    pub sandbox: Option<&'a Path>,
    pub stdin_is_terminal: bool,
    pub getenv: &'a dyn Fn(&str) -> Option<String>,
}

/// How a run ended.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing was changed and nothing was refused: the plan id (and a dry run) were printed.
    NothingChanged(String),
    /// A refusal; nothing was changed.
    Refused(String),
    /// Another apply holds the host lock.
    Busy,
    /// New work ran.
    Applied(Report),
    /// `--resume` ran.
    Resumed(Report),
}

/// What a run did, for the person at the terminal. Paths, sizes and reasons only.
#[derive(Debug, Default)]
pub struct Report {
    pub removed: u64,
    pub bytes: u64,
    pub skipped: Vec<(usize, String, journal::SkipReason, String)>,
    pub restored: Vec<(usize, String, String)>,
    pub kept: Vec<(usize, String, String)>,
    /// `.prunedisk-q-*` directories no journal names: never touched.
    pub unjournaled: Vec<PathBuf>,
    /// Items the fresh plan no longer agrees on (never started).
    pub not_agreed: Vec<(usize, String, String)>,
    /// Journals of other plan ids with unfinished items (never touched by this run).
    pub other_pending: Vec<(String, usize)>,
    /// Guard-state drift printed before a resume.
    pub notes: Vec<String>,
    pub lines: Vec<String>,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for n in &self.notes {
            writeln!(f, "{n}")?;
        }
        for l in self.lines.iter().filter(|l| !l.is_empty()) {
            writeln!(f, "{l}")?;
        }
        writeln!(
            f,
            "removed {} ({} bytes); skipped {}; restored in place {}; kept in quarantine {}; \
             not agreed by the fresh plan {}",
            self.removed,
            self.bytes,
            self.skipped.len(),
            self.restored.len(),
            self.kept.len(),
            self.not_agreed.len()
        )?;
        for (idx, path, reason, detail) in &self.skipped {
            writeln!(f, "  skipped item {idx} {path}: {reason:?}: {detail}")?;
        }
        for (idx, path, why) in &self.kept {
            writeln!(f, "  KEPT IN QUARANTINE item {idx} {path}: {why}")?;
        }
        for (idx, path, why) in &self.not_agreed {
            writeln!(f, "  not agreed item {idx} {path}: {why}")?;
        }
        for p in &self.unjournaled {
            writeln!(
                f,
                "  quarantine directory with no journal entry (not touched): {}",
                p.display()
            )?;
        }
        for (id, n) in &self.other_pending {
            writeln!(
                f,
                "  plan {id} has {n} unfinished item(s): run `apply --resume --plan-id {id}`"
            )?;
        }
        Ok(())
    }
}
