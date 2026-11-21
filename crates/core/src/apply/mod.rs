//! Apply: the one module that removes anything (DESIGN 10). It executes an approved lane plan
//! (docs/SCAN-FORMAT.md, "Pieces and lanes") and re-verifies every piece at the moment of acting.
//!
//! Entry points: [`apply`] and [`restore`](restore::restore). Dry run is the default; nothing is
//! written, uploaded, pushed or removed unless [`ApplyOptions::act`] is set.
//!
//! The invariant (spec I-1): a member is unlinked only when its bytes are accounted for, that is
//! in a shard verified by size and storage hash (or read-back), or (git pieces) raw-identical to a
//! HEAD blob whose object store is itself backed up, or noise whose evidence holds now. Removal
//! is member-wise through directory handles; a directory is removed only when empty; excluded
//! paths are never touched.

#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

pub mod input;
pub mod names;

#[cfg(unix)]
pub mod fsops;
#[cfg(unix)]
mod gitlane;
#[cfg(unix)]
pub use gitlane::utc_date;
#[cfg(unix)]
pub mod manifest;
#[cfg(unix)]
pub mod recheck;
#[cfg(unix)]
pub mod restore;
#[cfg(unix)]
pub mod shard;
#[cfg(unix)]
pub mod storage;

use input::{Inputs, Lane};
use std::path::PathBuf;
use std::time::Duration;

/// Errors that stop an apply before or while it acts.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The scan or plan is malformed, inconsistent or hostile; nothing was done.
    #[error("refusing the plan: {0}")]
    Plan(String),
    /// The manifest cannot be read or written; nothing further is removed.
    #[error("manifest: {0}")]
    Manifest(String),
    /// Another apply holds the lock.
    #[error("another apply is running (lock {0})")]
    Locked(PathBuf),
    /// This platform cannot apply.
    #[error("apply and restore are supported on macOS and Linux only")]
    Unsupported,
    /// Storage could not be reached for a step that needs it.
    #[error("storage: {0}")]
    Storage(String),
    /// A test-injected crash (test-hooks only).
    #[error("injected crash at {0}")]
    InjectedCrash(&'static str),
    /// Other I/O.
    #[error("{0}")]
    Io(String),
}

/// Where a test asks apply to stop as if the process died (AC-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    AfterBlobRows,
    AfterFileRows,
    AfterRemoveBegin,
    AfterMirror,
    MidUnlink,
}

/// Re-derives the lanes of noise and git pieces from the scan (spec revision 3, F3). Returns,
/// per plan piece id, a canonical description (lane, evidence, members) to compare, or `None`
/// when re-derivation is not available (then noise and git pieces are kept).
pub trait PlanOracle: Send + Sync {
    fn rederive(
        &self,
        scan_text: &str,
        plan_text: &str,
    ) -> Option<std::collections::BTreeMap<usize, String>>;
}

/// The production oracle (spec revision 3, F3): runs core `plan()` again on the scan, with the
/// plan's own inputs (its time, window, active paths and the choices the user made when planning)
/// and a process snapshot taken now, then compares each piece's canonical form. Whatever the scan
/// does not support, or what a process now running changes, differs and is kept. A scan that is
/// not a full scan result, or a re-derived plan that does not parse, re-derives nothing (every
/// noise and git piece is then kept).
pub struct CoreOracle {
    /// Most allocated bytes of one git directory's own files (config `max_git_admin_bytes`; the
    /// plan does not record it).
    pub max_git_admin_bytes: u64,
    /// The process snapshot for the re-derivation.
    pub process: ProcessProbe,
}

impl Default for CoreOracle {
    fn default() -> Self {
        CoreOracle {
            max_git_admin_bytes: crate::config::DEFAULT_MAX_GIT_ADMIN_BYTES,
            process: Box::new(crate::procs::running_cwds),
        }
    }
}

impl PlanOracle for CoreOracle {
    fn rederive(
        &self,
        scan_text: &str,
        plan_text: &str,
    ) -> Option<std::collections::BTreeMap<usize, String>> {
        let scan = crate::scan_from_json(scan_text).ok()?;
        let header: crate::Plan = serde_json::from_str(plan_text).ok()?;
        let opts = crate::PlanOptions {
            active: header.active.iter().map(PathBuf::from).collect(),
            active_window_days: header.active_window_days,
            window_source: header.window_source.clone(),
            now: header.generated_at,
            process: (self.process)(),
            trust_partial_process_check: header.trusted_partial_process_check,
            accept_incomplete_coverage: header.accepted_incomplete_coverage,
            max_git_admin_bytes: self.max_git_admin_bytes,
        };
        let again = crate::plan(&scan, &opts);
        let text = serde_json::to_string(&again).ok()?;
        let inputs = input::parse(scan_text, &text, None, None).ok()?;
        Some(inputs.pieces.iter().map(|p| (p.id, canonical(p))).collect())
    }
}

/// The canonical description of a plan piece compared against the re-derived one.
pub fn canonical(p: &input::PlanPiece) -> String {
    let ev = match &p.evidence {
        Some(input::Evidence::Rule {
            rule,
            markers,
            node,
        }) => format!("rule:{rule}:{markers:?}:{node}"),
        Some(input::Evidence::Duplicate {
            sha256,
            size,
            retained_copy,
            ..
        }) => {
            format!("dup:{sha256}:{size}:{}", retained_copy.display())
        }
        Some(input::Evidence::Archive { sha256, size, .. }) => format!("archive:{sha256}:{size}"),
        None => String::new(),
    };
    let git = p
        .git
        .as_ref()
        .map(|g| {
            format!(
                "{}:{}:{:?}:{:?}",
                g.action, g.preserve, g.excludes_secret, g.recovery
            )
        })
        .unwrap_or_default();
    format!(
        "{}|{}|{:?}|{}|{:?}|{:?}|{}|{}",
        p.lane.as_str(),
        p.scan_piece,
        p.kind,
        p.path.display(),
        p.files,
        p.excludes,
        ev,
        git
    )
}

/// Options for one apply run.
pub struct ApplyOptions {
    /// rclone remote root, for example `b2-archive2:prunebackup/disk-cleaner-archive`.
    pub remote: String,
    /// The local directory of the remote when it is a plain path (refused inside any piece).
    pub remote_local: Option<PathBuf>,
    pub rclone: PathBuf,
    pub git: PathBuf,
    pub gh: PathBuf,
    /// Concurrent upload pipelines.
    pub jobs: usize,
    /// Shard bound in bytes.
    pub shard_bytes: u64,
    /// Per-rclone-command deadline.
    pub shard_timeout: Duration,
    pub manifest: PathBuf,
    /// Lock and scratch directory.
    pub state_dir: PathBuf,
    /// Label in shard, manifest and recovery ref names.
    pub label: String,
    /// Act (`--apply`). Without it nothing is changed.
    pub act: bool,
    /// The user's opt-in to upload secret pieces (`--secrets upload`).
    pub secrets_upload: bool,
    /// URLs (redacted form) the user declares private.
    pub private_remotes: Vec<String>,
    /// Hosts recovery refs may be pushed to.
    pub git_hosts: Vec<String>,
    /// Untracked files above this size are uploaded raw instead of entering the WIP tree.
    pub wip_max_file_bytes: u64,
    /// Current time (unix seconds).
    pub now: i64,
    pub home: Option<PathBuf>,
    /// Reads the running processes' working directories. Called at the start of a run and again
    /// before each removal batch (the contract repeats the process check after the backup).
    pub process: ProcessProbe,
    pub oracle: Box<dyn PlanOracle>,
    /// Test seams. Present only with the `test-hooks` feature.
    #[cfg(feature = "test-hooks")]
    pub hooks: TestHooks,
}

/// A process snapshot source.
pub type ProcessProbe = Box<dyn Fn() -> crate::ProcessCheck + Send + Sync>;

/// Test seams (feature `test-hooks`, never enabled by the CLI).
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Default)]
pub struct TestHooks {
    /// file:// remotes count as network remotes (AC-5 against a local bare repo).
    pub file_remotes_are_network: bool,
    pub crash_at: Option<CrashPoint>,
    pub force_readback: bool,
    /// Fail the manifest append after this many appends (NEG-12).
    pub manifest_fail_after: Option<usize>,
    /// Called between the backup and the removal of each piece (NEG-4 races).
    pub before_remove: Option<fn(&std::path::Path)>,
}

impl ApplyOptions {
    pub fn new(
        remote: impl Into<String>,
        manifest: PathBuf,
        state_dir: PathBuf,
        now: i64,
    ) -> ApplyOptions {
        ApplyOptions {
            remote: remote.into(),
            remote_local: None,
            rclone: PathBuf::from("rclone"),
            git: PathBuf::from("git"),
            gh: PathBuf::from("gh"),
            jobs: 4,
            shard_bytes: 64 << 20,
            shard_timeout: Duration::from_secs(1800),
            manifest,
            state_dir,
            label: short_hostname(),
            act: false,
            secrets_upload: false,
            private_remotes: Vec::new(),
            git_hosts: vec![
                "github.com".into(),
                "gitlab.com".into(),
                "bitbucket.org".into(),
            ],
            wip_max_file_bytes: 50 << 20,
            now,
            home: std::env::var_os("HOME").map(PathBuf::from),
            process: Box::new(crate::procs::running_cwds),
            oracle: Box::new(CoreOracle::default()),
            #[cfg(feature = "test-hooks")]
            hooks: TestHooks::default(),
        }
    }

    pub(crate) fn crash(&self, _at: CrashPoint) -> Result<(), ApplyError> {
        #[cfg(feature = "test-hooks")]
        if self.hooks.crash_at == Some(_at) {
            return Err(ApplyError::InjectedCrash(match _at {
                CrashPoint::AfterBlobRows => "after blob rows",
                CrashPoint::AfterFileRows => "after file rows",
                CrashPoint::AfterRemoveBegin => "after remove-begin",
                CrashPoint::AfterMirror => "after the manifest mirror",
                CrashPoint::MidUnlink => "in the middle of unlinking",
            }));
        }
        Ok(())
    }

    pub(crate) fn file_remotes_are_network(&self) -> bool {
        #[cfg(feature = "test-hooks")]
        {
            self.hooks.file_remotes_are_network
        }
        #[cfg(not(feature = "test-hooks"))]
        {
            false
        }
    }
}

/// The short host name (first label), used as the default label.
pub fn short_hostname() -> String {
    let h = rustix_hostname();
    let s = h.split('.').next().unwrap_or("").trim().to_string();
    let clean: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "host".into()
    } else {
        clean
    }
}

#[cfg(unix)]
fn rustix_hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .to_string()
}

#[cfg(not(unix))]
fn rustix_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// What happened to one piece.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PieceReport {
    pub id: usize,
    pub path: PathBuf,
    pub lane: Lane,
    /// `would back up and remove`, `removed`, `kept`, `partial`...
    pub outcome: String,
    pub reason: String,
    /// How a removed path comes back.
    pub restore: String,
    pub bytes: u64,
}

/// The result of a run.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ApplyReport {
    pub dry_run: bool,
    pub run: String,
    pub pieces: Vec<PieceReport>,
    pub uploaded_bytes: u64,
    pub shards_uploaded: usize,
    pub shards_failed: Vec<String>,
    pub removed_entries: u64,
    /// Pieces kept because their scan piece no longer matches the scan after an earlier apply
    /// run (or batch) of the same scan removed entries from it. The scan is stale; only a new
    /// scan lets apply continue there. The CLI exits with its own status when this is not empty.
    pub rescan_needed: Vec<usize>,
}

/// The reason given for a piece kept because an earlier run of the same scan changed its
/// folder (main's ruling on stream A's directory re-check, goodqa#16529).
pub const RESCAN_HINT: &str = "an earlier apply run changed this folder; scan again to continue";

/// Validate inputs without acting (exposed for the CLI and tests).
pub fn parse_inputs(
    scan_text: &str,
    plan_text: &str,
    opts: &ApplyOptions,
) -> Result<Inputs, ApplyError> {
    input::parse(
        scan_text,
        plan_text,
        opts.home.as_deref(),
        opts.remote_local.as_deref(),
    )
}

/// Run apply. `scan_text` and `plan_text` are the two documents.
#[cfg(unix)]
pub fn apply(
    scan_text: &str,
    plan_text: &str,
    opts: &ApplyOptions,
) -> Result<ApplyReport, ApplyError> {
    run::apply(scan_text, plan_text, opts)
}

#[cfg(not(unix))]
pub fn apply(
    _scan_text: &str,
    _plan_text: &str,
    _opts: &ApplyOptions,
) -> Result<ApplyReport, ApplyError> {
    Err(ApplyError::Unsupported)
}

#[cfg(unix)]
mod run;
