//! Per-item evidence (goodqa#16711; docs/DESIGN.md, Per-item evidence): the one place that
//! decides, for every plan item, whether it is in use, which session and project own it, how
//! recently it was active and whether the scan could see all of it. Nothing is inherited from a
//! parent node except through the owner relations defined here; plan (and, later, apply) call
//! this module and copy no judgement.
//!
//! In use (any of):
//! 1. a readable process's working directory or open file lies inside the node, or inside its
//!    owning project when the node is a regenerable or cache directory;
//! 2. a path in any process's command line (readable or not; `--flag=/path` values and
//!    `:`-separated lists split, relative paths resolved against the process's working
//!    directory) lies inside the node or, for a regenerable or cache directory, its project;
//! 3. a running container's bind source contains the node or lies inside it;
//! 4. the node's owning session (`<harness root>/work/<s>/`) is live.
//!
//! A working directory or argument that is an ancestor of the node, or inside a sibling, never
//! makes it in use.
//!
//! What this module reads besides the scan and the inputs it is given: the agent-harness
//! session directories' names, the Claude Code session registry (`sessions/<pid>.json`, only the
//! `pid`, `sessionId` and `cwd` fields; never a `.key` file), and the first 64 KiB or 20 lines of
//! recent transcripts, from which only the top-level `cwd` string is kept. Transcript text is
//! never kept, logged or printed.
//!
//! Every directory walk that feeds agent evidence goes through the one walker in
//! `evidence/walk.rs` (goodqa#16711 gate F1): a listing, entry, type or metadata error, or a walk
//! bound reached, is never dropped; each becomes an `agent-dir-unreadable` gap of unknown age,
//! counted as recent, so removals under the agent directories ask. A source lint in the tests
//! keeps every other filesystem call out of this file.

mod walk;

use crate::model::*;
use crate::procs::{Proc, ProcKind, ProcessSnapshot};
use crate::protect::{comp_eq, normalize, rel_below};
use crate::words::count;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use walk::{Kind, Walker};

// ------------------------------------------------------------------------------------ inputs

/// One running container and the host paths it binds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    #[serde(default)]
    pub binds: Vec<String>,
}

/// What the container runtime said, queried by the command line (like the process snapshot)
/// when a runtime process is running.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ContainerState {
    /// Not queried (no runtime process was seen, or the caller did not ask).
    #[default]
    NotQueried,
    /// The running containers and their bind sources.
    Queried { containers: Vec<Container> },
    /// The runtime could not be queried.
    Unqueryable { reason: String },
}

/// What the command line asks of the system for the plan (and what a test passes instead).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInputs {
    #[serde(default)]
    pub processes: ProcessSnapshot,
    #[serde(default)]
    pub containers: ContainerState,
}

/// Executable names of container runtime children.
pub fn is_runtime_child(name: &str) -> bool {
    name.starts_with("containerd-shim")
        || name == "conmon"
        || name == "runc"
        || name.starts_with("com.docker.")
}

/// A per-container shim (one container each, named on its command line).
fn is_shim(name: &str) -> bool {
    name.starts_with("containerd-shim") || name == "conmon"
}

/// The container id a shim's command line names (`-id X`, `-c X`, `--cid X`).
pub fn shim_container_id(cmdline: &[String]) -> Option<String> {
    let mut it = cmdline.iter();
    while let Some(a) = it.next() {
        if matches!(a.as_str(), "-id" | "--id" | "-c" | "--cid") {
            return it.next().cloned().filter(|v| !v.is_empty());
        }
        for flag in ["-id=", "--id=", "--cid="] {
            if let Some(v) = a.strip_prefix(flag) {
                return Some(v.to_string()).filter(|v| !v.is_empty());
            }
        }
    }
    None
}

/// Parse `docker inspect` / `podman inspect` JSON (an array of containers): each one's `Id` and
/// its mounts' `Source` paths.
pub fn parse_inspect(json: &str) -> Result<Vec<Container>, String> {
    #[derive(Deserialize)]
    struct Mount {
        #[serde(rename = "Source", default)]
        source: Option<String>,
    }
    #[derive(Deserialize)]
    struct Inspected {
        #[serde(rename = "Id")]
        id: String,
        #[serde(rename = "Mounts", default)]
        mounts: Option<Vec<Mount>>,
    }
    let list: Vec<Inspected> =
        serde_json::from_str(json).map_err(|e| format!("inspect output is not valid: {e}"))?;
    Ok(list
        .into_iter()
        .map(|c| Container {
            id: c.id,
            binds: c
                .mounts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| m.source)
                .filter(|s| s.starts_with('/'))
                .collect(),
        })
        .collect())
}

// ----------------------------------------------------------------------------------- outputs

/// What an item's evidence was, in the plan JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemEvidence {
    /// The in-use rule that matched (1 to 4), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<u8>,
    /// The path that matched it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What matched, in words (`working directory of process 123`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The owning session, for a node below `<harness root>/work/<s>/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The owning project, when it is not the node itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Unreadable folders, entries and skipped mount points inside the node.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gaps_inside: u64,
    /// Repositories and worktrees: whole days since the newest commit on any ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_days: Option<u64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One agent-harness session in the plan totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTotal {
    pub session: String,
    pub live: bool,
    /// Why it is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Whole days since its newest evidence (files below it, same-named entries), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_age_days: Option<u64>,
    /// Bytes offered for removal below it (nested items counted once).
    pub bytes_offered: u64,
    /// Bytes kept below it (nested items counted once).
    pub bytes_kept: u64,
}

/// The process check in the plan header: what the per-item rules could use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCounts {
    /// Working directory readable (rules 1 and 2).
    pub readable: u64,
    /// Working directory unreadable, command line readable (rule 2).
    pub by_command_line: u64,
    pub kernel_threads: u64,
    /// Exited, zombie or vanished while read.
    pub gone: u64,
    /// Container runtime processes seen.
    pub container_processes: u64,
    /// `not running`, `queried (N containers)`, or why it could not be queried.
    pub runtime: String,
    /// Agent processes (Claude Code, Codex, Kimi, Cursor agent).
    pub agents: u64,
    /// Of those, idle ones with no locus (no child, no open transcript or file): they hold
    /// nothing.
    pub idle_agents: u64,
    /// Agent processes with no locus that are not idle: removing items under the harness work
    /// directories ask (pids).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmapped_agents: Vec<u32>,
    /// Neither working directory nor command line readable (not kernel threads).
    pub fully_unknown: u64,
}

/// Coverage gaps in the plan header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapSummary {
    pub privacy: u64,
    pub permission: u64,
    pub mount: u64,
    pub other: u64,
    /// The record overflowed (or is incomplete): every repository asks unless accepted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overflowed: bool,
    /// The folders holding the most gaps (path, gaps), up to five.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_folders: Vec<(String, u64)>,
    /// Agent data the live-session rule could not use, by class: `transcript-cwd-unknown` (a
    /// transcript head in no known shape: not a top-level `cwd`, not a Codex `session_meta`
    /// line with `payload.cwd`), `transcript-unreadable` (it could not be opened or read),
    /// `registry-entry-unknown` (a registry file without a readable integer `pid`),
    /// `transcripts-not-read` (beyond the read or listing limit, or no modification time).
    /// Each may hide a live session; the header lists them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_data: BTreeMap<String, u64>,
    /// Those of `agent_data` modified within the active window (or of unknown age, or past a
    /// limit): they make every removing item under the agent directories ask.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_data_recent: BTreeMap<String, u64>,
}

impl GapSummary {
    pub fn total(&self) -> u64 {
        self.privacy + self.permission + self.mount + self.other
    }
}

/// The class of a directory an agent-evidence walk could not see all of (goodqa#16711 F1): a
/// listing, entry, type or metadata read failed, or the walk reached its bound. Of unknown age,
/// so always counted as recent.
pub const AGENT_DIR_UNREADABLE: &str = "agent-dir-unreadable";

/// How many transcripts the plan read, against its caps (goodqa#16711 F4). The live-agent
/// reads (a registry entry's session transcript, a transcript an agent holds open) have their
/// own budget and run first; the bulk pass over every transcript modified within
/// `window_days` has another. Past either cap the rest is `transcripts-not-read`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptReads {
    /// The bulk pass's window: transcripts modified this many days back are read (the larger
    /// of the active window and the 14-day idle ask).
    pub window_days: u64,
    /// Transcripts found modified within that window.
    pub found: u64,
    /// Of those, read.
    pub read: u64,
    /// The bulk pass's cap.
    pub cap: u64,
    /// Live-agent reads made, and their cap.
    pub live_read: u64,
    pub live_cap: u64,
}

/// What made an item in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hit {
    pub rule: u8,
    pub path: String,
    pub detail: String,
}

/// Per-node evidence (indexed like `scan.nodes`).
#[derive(Debug, Clone, Default)]
pub(crate) struct NodeEvidence {
    pub in_use: Option<Hit>,
    /// A locus holds it at the plan's one choke point: a recent transcript's working directory
    /// (rule 5) anywhere, or a process locus inside its live session (rule 4), that equals,
    /// contains or lies inside it.
    pub locus_hold: Option<Hit>,
    /// A checkout that asks while its activity is under [`IDLE_ASK_DAYS`] days old: "session
    /// live" (in a live session) or "agent worktree" (in an agent home's `worktrees/`).
    pub idle_ask: Option<&'static str>,
    /// Index into [`Evidence::sessions`].
    pub session: Option<usize>,
    /// Below a harness `work/` directory.
    pub under_work: bool,
    /// Below a harness `work/` directory or an agent home's `worktrees/` directory.
    pub under_agent_dir: bool,
    pub activity: Option<i64>,
    /// The owning project directory, when the node is judged by it.
    pub project: Option<String>,
    pub gaps_inside: u64,
}

/// A checkout in a live session or an agent worktree with no locus asks until its own
/// activity is this many days old (E1b, G1).
pub(crate) const IDLE_ASK_DAYS: u64 = 14;
/// The same, in seconds: the ask applies while `age < IDLE_ASK_SECS`, on exact durations
/// (goodqa#16804 item 5); at exactly 14 days the normal rules apply.
pub(crate) const IDLE_ASK_SECS: i64 = IDLE_ASK_DAYS as i64 * 86_400;

/// One session's liveness.
#[derive(Debug, Clone, Default)]
pub(crate) struct Session {
    pub name: String,
    pub live: Option<String>,
    pub newest: Option<i64>,
}

/// Everything the plan needs from this module.
#[derive(Debug, Clone, Default)]
pub(crate) struct Evidence {
    pub nodes: Vec<NodeEvidence>,
    pub sessions: Vec<Session>,
    pub counts: ProcessCounts,
    pub gaps: GapSummary,
    pub transcripts: TranscriptReads,
    /// Reasons every removing item asks (process information unreadable, fully unknown
    /// processes, container runtime).
    pub global_asks: Vec<String>,
    /// Reasons every removing item under a harness work directory asks (agents with no locus).
    pub work_asks: Vec<String>,
    /// Reasons every removing item under an agent directory (harness work, agent worktrees)
    /// asks (recent agent data that could not be read).
    pub agent_asks: Vec<String>,
    /// The gap record is incomplete: the whole-scan coverage rule applies.
    pub gaps_incomplete: bool,
    /// Short notes for the plan header (an unreadable transcript, by path only).
    pub notes: Vec<String>,
}

/// Options this module takes from the plan.
pub(crate) struct Opts<'a> {
    pub now: i64,
    pub window_days: u64,
    pub host: &'a HostInputs,
    pub trust_partial_process_check: bool,
}

// ------------------------------------------------------------------------------------ paths

fn key_of(p: &Path) -> String {
    let s = normalize(p).to_string_lossy().into_owned();
    if cfg!(target_os = "macos") {
        s.to_lowercase()
    } else {
        s
    }
}

/// A path's identities: as given (normalized) and its real path when that differs.
fn ids_of(p: &Path) -> Vec<PathBuf> {
    let n = normalize(p);
    let mut v = vec![n.clone()];
    if let Ok(c) = std::fs::canonicalize(p) {
        let c = normalize(&c);
        if key_of(&c) != key_of(&n) {
            v.push(c);
        }
    }
    v
}

fn under(p: &Path, root: &Path) -> bool {
    rel_below(p, root).is_some()
}

/// Is `seg` a whole path component of `p`?
fn has_segment(p: &Path, seg: &str) -> bool {
    p.components()
        .any(|c| comp_eq(c.as_os_str(), OsStr::new(seg)))
}

/// Paths named on a command line: every argument (and `argv[0]`), the value of a
/// `--flag=value`, each piece of a `:`-separated list; relative ones resolved against `cwd`.
pub(crate) fn arg_paths(args: &[String], cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in args.iter().take(512) {
        if a.len() > 4096 {
            continue;
        }
        let value = if a.starts_with('-') {
            match a.split_once('=') {
                Some((_, v)) => v,
                None => continue,
            }
        } else {
            a.as_str()
        };
        for piece in value.split(':') {
            if piece.is_empty() || piece.chars().any(char::is_whitespace) {
                continue;
            }
            let p = Path::new(piece);
            if p.is_absolute() {
                out.push(normalize(p));
            } else if let Some(c) = cwd {
                out.push(normalize(&c.join(p)));
            }
        }
    }
    out
}

/// The words of a command line a session id may be: each whole argument, the value of a
/// `--flag=value`, and each piece of a `:`-separated value (`--resume=s1`, `--dirs=a:s1`).
fn arg_tokens(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    for a in args.iter().take(512) {
        out.push(a.as_str());
        let value = match a.split_once('=') {
            Some((_, v)) if a.starts_with('-') => v,
            _ => a.as_str(),
        };
        if value != a.as_str() {
            out.push(value);
        }
        if value.contains(':') {
            out.extend(value.split(':').filter(|v| !v.is_empty()));
        }
    }
    out
}

/// The paths an agent's arguments name, as loci: `argv[0]` is the program, not a place it
/// works; absolute paths count as given, and a relative argument counts only when it names
/// something that exists below the working directory (a bare word such as a prompt or a
/// subcommand is not a place). A relative argument that cannot be looked at (a prompt word
/// longer than a file name may be, a path through a file) is not counted as a place: the safe
/// direction, since an agent left with no place fails closed. This is not a walk of agent data,
/// so it records no gap.
fn agent_arg_paths(args: &[String], cwd: Option<&Path>) -> Vec<PathBuf> {
    let rest = args.get(1..).unwrap_or(&[]);
    let mut out = arg_paths(rest, None);
    for p in arg_paths(rest, cwd) {
        if !out.contains(&p) && matches!(walk::stat(&p), Ok(Some(_))) {
            out.push(p);
        }
    }
    out
}

// ------------------------------------------------------------------------- bounded reading

/// Most bytes of a transcript read.
pub const TRANSCRIPT_HEAD_BYTES: usize = 64 * 1024;
/// Most lines of a transcript read.
pub const TRANSCRIPT_HEAD_LINES: usize = 20;

/// The class of a transcript whose working directory could not be taken.
pub const TRANSCRIPT_CWD_UNKNOWN: &str = "transcript-cwd-unknown";
/// The class of a transcript that could not be opened or read.
pub const TRANSCRIPT_UNREADABLE: &str = "transcript-unreadable";

/// The working directory a transcript records, from its first 64 KiB or 20 lines, whichever
/// is smaller. The one extractor for every transcript source. Two shapes are known: a line
/// with a top-level `cwd` string (Claude Code `projects/**/<id>.jsonl`), and a Codex
/// `session_meta` line (`"type": "session_meta"`) with `payload.cwd` (`rollout-*.jsonl`).
/// Only that one string is kept; nothing else of the file is held, logged or returned. A file
/// in neither shape within the bound (another line kind, a first line longer than 64 KiB,
/// text that is not JSON) is `Err(TRANSCRIPT_CWD_UNKNOWN)`; one that cannot be read is
/// `Err(TRANSCRIPT_UNREADABLE)`. Callers fall back to a Claude Code transcript's project
/// directory name ([`slug_candidates`]) and otherwise count a gap, noting the path only.
pub fn transcript_cwd(path: &Path) -> Result<String, &'static str> {
    use std::io::Read;
    // Deserialize into structs that hold only these fields: every other field is skipped.
    #[derive(Deserialize)]
    struct PayloadCwd {
        #[serde(default)]
        cwd: Option<String>,
    }
    #[derive(Deserialize)]
    struct Head {
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default, rename = "type")]
        kind: Option<String>,
        #[serde(default)]
        payload: Option<PayloadCwd>,
    }
    let file = std::fs::File::open(path).map_err(|_| TRANSCRIPT_UNREADABLE)?;
    let mut buf = Vec::with_capacity(8192);
    file.take(TRANSCRIPT_HEAD_BYTES as u64)
        .read_to_end(&mut buf)
        .map_err(|_| TRANSCRIPT_UNREADABLE)?;
    let full = buf.len() < TRANSCRIPT_HEAD_BYTES;
    let mut lines: Vec<&[u8]> = buf.split(|b| *b == b'\n').collect();
    if !full {
        // The last line was cut by the byte bound (a first line longer than the bound leaves
        // nothing).
        lines.pop();
    }
    for line in lines.into_iter().take(TRANSCRIPT_HEAD_LINES) {
        let Ok(h) = serde_json::from_slice::<Head>(line) else {
            continue;
        };
        if let Some(c) = h.cwd.filter(|c| c.starts_with('/')) {
            return Ok(c);
        }
        if h.kind.as_deref() == Some("session_meta") {
            if let Some(c) = h.payload.and_then(|p| p.cwd).filter(|c| c.starts_with('/')) {
                return Ok(c);
            }
        }
    }
    Err(TRANSCRIPT_CWD_UNKNOWN)
}

/// The class of a transcript whose project directory name could not be fully decoded: the walk
/// hit its bound, a listing cap, or a directory it had to list could not be listed.
pub const TRANSCRIPT_SLUG_INCOMPLETE: &str = "transcript-slug-incomplete";

/// Most directories listed while decoding one project directory name.
#[cfg(not(test))]
pub const MAX_SLUG_DIRS: usize = 2048;
/// Small in tests, so the bound can be reached.
#[cfg(test)]
pub const MAX_SLUG_DIRS: usize = 32;
/// Most entries looked at in all while decoding one project directory name (names only).
const MAX_SLUG_ENTRIES: usize = 4_000_000;

/// Claude Code's project directory name for a path: every character that is not an ASCII
/// letter or digit becomes `-` (so `/`, `.`, `_` and spaces all do; the encoding is lossy).
pub fn claude_slug(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// What decoding a project directory name found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlugMatch {
    /// Every existing directory whose encoding equals the name, sorted.
    pub candidates: Vec<PathBuf>,
    /// The walk was incomplete (bound, listing cap, or a matching directory not listable), so
    /// the true directory may be one it did not see.
    pub incomplete: bool,
}

/// Every existing directory whose [`claude_slug`] equals `slug`, by a walk from `/` that reads
/// directory names only (no file is opened) and lists at most `max_dirs` directories.
pub fn slug_candidates(slug: &str, max_dirs: usize) -> SlugMatch {
    let mut out = SlugMatch::default();
    let s = slug.as_bytes();
    if s.first() != Some(&b'-') || s.len() > 4096 {
        return out;
    }
    let mut stack: Vec<(PathBuf, usize)> = vec![(PathBuf::from("/"), 0)];
    let mut listed = 0usize;
    let mut seen = 0usize;
    while let Some((dir, at)) = stack.pop() {
        if at == s.len() {
            out.candidates.push(dir);
            continue;
        }
        if listed >= max_dirs {
            out.incomplete = true;
            break;
        }
        listed += 1;
        let left = MAX_SLUG_ENTRIES.saturating_sub(seen);
        if left == 0 {
            out.incomplete = true;
            break;
        }
        // Every listing, entry or type error (and the entry bound) leaves the walk incomplete:
        // the true directory may be one it did not see. The caller counts that as a gap aged
        // by its transcript.
        for r in walk::list(&dir, left) {
            seen += 1;
            let e = match r {
                Ok(e) => e,
                Err(_unseen) => {
                    out.incomplete = true;
                    continue;
                }
            };
            let enc = claude_slug(&e.name_lossy());
            let rest = &s[at..];
            if rest.len() < 1 + enc.len()
                || rest[0] != b'-'
                || !rest[1..].starts_with(enc.as_bytes())
            {
                continue;
            }
            let end = at + 1 + enc.len();
            if end < s.len() && s[end] != b'-' {
                continue;
            }
            if e.kind == Kind::Dir {
                stack.push((e.path, end));
            }
        }
    }
    // By bytes, as the oracle sorts.
    out.candidates
        .sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    out
}

/// The project directory name of a Claude Code transcript
/// (`projects/<name>/<id>.jsonl` or `projects/<name>/<id>/subagents/<agent>.jsonl`).
fn claude_project_slug(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".jsonl") || name.starts_with("rollout-") {
        return None;
    }
    let mut cur = path.parent()?;
    loop {
        let parent = cur.parent()?;
        if parent.file_name().is_some_and(|n| n == "projects") {
            return cur.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        cur = parent;
    }
}

/// The newest mtime of a checkout's git admin files (E1a), metadata only: for a `.git`
/// directory its `index`, `HEAD` and `FETCH_HEAD`; for a `.git` file (a linked worktree) the
/// file's ctime and its gitdir's `HEAD`, `index` and `logs/HEAD`, the gitdir taken from the
/// file's first 4 KiB. `Err` when a `.git` file cannot be read or names no gitdir: the caller
/// counts the checkout as active.
fn git_admin_activity(path: &Path) -> Result<Option<i64>, ()> {
    use std::io::Read;
    let dot = path.join(".git");
    // An admin file that does not exist says nothing; one that cannot be read makes the whole
    // answer unknown, so the caller counts the checkout as active (never old).
    let md = match walk::lstat(&dot) {
        Ok(Some(md)) => md,
        Ok(None) => return Ok(None),
        Err(_unknown) => return Err(()),
    };
    let newest = |dir: &Path, names: &[&str]| -> Result<Option<i64>, ()> {
        let mut t: Option<i64> = None;
        for n in names {
            match walk::stat(&dir.join(n)) {
                Ok(Some(m)) => t = t.max(Some(m.mtime)),
                Ok(None) => {}
                Err(_unknown) => return Err(()),
            }
        }
        Ok(t)
    };
    if md.kind == Kind::Dir {
        return newest(&dot, &["index", "HEAD", "FETCH_HEAD"]);
    }
    if md.kind != Kind::File {
        return Err(());
    }
    let ctime = Some(md.ctime);
    let mut buf = Vec::new();
    std::fs::File::open(&dot)
        .and_then(|f| f.take(4096).read_to_end(&mut buf))
        .map_err(|_| ())?;
    let text = String::from_utf8_lossy(&buf);
    let gitdir = text
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .ok_or(())?;
    let gd = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        path.join(gitdir)
    };
    Ok(ctime.max(newest(&gd, &["HEAD", "index", "logs/HEAD"])?))
}

/// The three fields of a Claude Code session registry entry this module reads.
#[derive(Debug, Clone, Deserialize)]
struct RegistryEntry {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// Is `name` a registry entry file (`<pid>.json`)? Never a `.key` file or anything else.
fn is_registry_file(name: &str) -> bool {
    name.strip_suffix(".json")
        .is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
}

// --------------------------------------------------------------------------------- the host

/// Homes, harness roots and protected roots, on every identity the scan recorded.
struct Host {
    homes: Vec<PathBuf>,
    harness: Vec<PathBuf>,
    work_dirs: Vec<PathBuf>,
    /// Harness work directories and agent homes' `worktrees/` directories.
    agent_dirs: Vec<PathBuf>,
    /// Agent homes' `worktrees/` directories.
    worktree_dirs: Vec<PathBuf>,
    protected: Vec<PathBuf>,
    claude_bases: Vec<PathBuf>,
    codex_bases: Vec<PathBuf>,
    uid: Option<u32>,
    /// Keys of the shared places that name no place besides those above a work dir: the
    /// system temp roots and every ancestor of a home (wal:waf84dc6656a).
    no_place: HashSet<String>,
}

/// The system temp roots of the ancestor-exception list (`$TMPDIR` is added at run time).
const TEMP_ROOTS: [&str; 4] = ["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"];

impl Host {
    fn new(scan: &ScanResult, walker: &mut Walker) -> Host {
        let pc = &scan.protect;
        let mut homes: Vec<PathBuf> = Vec::new();
        let push = |v: &mut Vec<PathBuf>, p: &Path| {
            for id in ids_of(p) {
                if !v.iter().any(|x| key_of(x) == key_of(&id)) {
                    v.push(id);
                }
            }
        };
        if !pc.home.is_empty() {
            push(&mut homes, Path::new(&pc.home));
        }
        if let Some(h) = &pc.passwd_home {
            push(&mut homes, Path::new(h));
        }
        let mut harness = Vec::new();
        let mut protected = Vec::new();
        let mut claude_bases = Vec::new();
        let mut codex_bases = Vec::new();
        let mut worktree_dirs: Vec<PathBuf> = Vec::new();
        for r in pc.roots.iter().filter(|r| r.exists) {
            let mut ids = vec![PathBuf::from(&r.spelled)];
            ids.extend(r.canonical.iter().map(PathBuf::from));
            for id in ids {
                if r.code == HoldCode::Protected {
                    push(&mut protected, &id);
                }
                if r.carve == CarveLayout::AgentHarness {
                    push(&mut harness, &id);
                }
                if r.carve == CarveLayout::Worktrees {
                    push(&mut worktree_dirs, &id.join("worktrees"));
                }
                let name = id
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if name.starts_with(".claude") && walker.is_dir(&id) {
                    push(&mut claude_bases, &id);
                }
                if name.starts_with(".codex") && walker.is_dir(&id) {
                    push(&mut codex_bases, &id);
                }
            }
        }
        if let Some(c) = &pc.claude_config_dir {
            push(&mut claude_bases, Path::new(c));
        }
        if let Some(c) = &pc.codex_home {
            push(&mut codex_bases, Path::new(c));
        }
        let work_dirs: Vec<PathBuf> = harness.iter().map(|h| h.join("work")).collect();
        let mut agent_dirs = work_dirs.clone();
        agent_dirs.extend(worktree_dirs.iter().cloned());
        let mut no_place: HashSet<String> = HashSet::new();
        let mut temps: Vec<PathBuf> = TEMP_ROOTS.iter().map(PathBuf::from).collect();
        if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
            temps.push(PathBuf::from(t));
        }
        for t in &temps {
            for id in ids_of(t) {
                no_place.insert(key_of(&id));
            }
        }
        for h in &homes {
            for a in h.ancestors() {
                no_place.insert(key_of(a));
            }
        }
        Host {
            worktree_dirs,
            no_place,
            homes,
            harness,
            work_dirs,
            agent_dirs,
            protected,
            claude_bases,
            codex_bases,
            uid: pc.uid,
        }
    }

    /// The session `<s>` of a path at or below `<harness root>/work/<s>/`.
    fn session_of(&self, p: &Path) -> Option<String> {
        let n = normalize(p);
        self.harness.iter().find_map(|h| {
            let rel = rel_below(&n, h)?;
            (rel.len() >= 2 && comp_eq(&rel[0], OsStr::new("work")))
                .then(|| rel[1].to_string_lossy().into_owned())
        })
    }

    fn under_work(&self, p: &Path) -> bool {
        let n = normalize(p);
        self.work_dirs.iter().any(|w| under(&n, w))
    }

    fn under_worktree_dir(&self, p: &Path) -> bool {
        let n = normalize(p);
        self.worktree_dirs
            .iter()
            .any(|w| under(&n, w) && key_of(w) != key_of(&n))
    }

    fn under_agent_dir(&self, p: &Path) -> bool {
        let n = normalize(p);
        self.agent_dirs.iter().any(|w| under(&n, w))
    }

    /// `/`, the home, the harness root or `work/` itself: an ancestor of a harness work dir.
    fn ancestor_of_work(&self, p: &Path) -> bool {
        let n = normalize(p);
        self.work_dirs.iter().any(|w| under(w, &n))
    }

    /// The one ancestor-exception list, shared by process and transcript loci: `/`, a home and
    /// every ancestor of one (`/home`, `/Users`), the harness root, `work/` (anything at or
    /// above a harness work dir), and the system temp roots (`/tmp`, `/var/tmp`,
    /// `/private/tmp`, `/private/var/tmp`, `$TMPDIR`). A working directory there names no
    /// place, so it holds nothing below it.
    fn holds_nothing_below(&self, p: &Path) -> bool {
        let n = normalize(p);
        self.ancestor_of_work(&n)
            || ids_of(&n)
                .iter()
                .any(|id| self.no_place.contains(&key_of(id)))
    }

    fn under_home(&self, p: &Path) -> bool {
        let n = normalize(p);
        self.homes
            .iter()
            .any(|h| under(&n, h) && key_of(&n) != key_of(h))
    }

    /// Inside a protected root but outside every harness work dir: agent configuration and
    /// state, which says nothing about where an agent works.
    fn agent_config(&self, p: &Path) -> bool {
        let n = normalize(p);
        self.protected.iter().any(|r| under(&n, r)) && !self.under_work(&n)
    }

    /// Tool installs, binaries and libraries below the home.
    fn install_path(&self, p: &Path, install_dirs: &[PathBuf]) -> bool {
        let n = normalize(p);
        if install_dirs.iter().any(|d| under(&n, d)) {
            return true;
        }
        if n.components().any(|c| c.as_os_str() == "node_modules") {
            return true;
        }
        let ext = n
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "so" | "dylib" | "node" | "dll") {
            return true;
        }
        const TOOL_DIRS: [&str; 10] = [
            ".local",
            ".nvm",
            ".npm",
            ".volta",
            ".bun",
            ".cargo",
            ".rustup",
            ".pyenv",
            ".cache",
            "Library/Caches",
        ];
        self.homes
            .iter()
            .any(|h| TOOL_DIRS.iter().any(|t| under(&n, &h.join(t))))
    }
}

/// Is this an agent process (Claude Code, Codex, Kimi, Cursor agent)?
fn is_agent(p: &Proc) -> bool {
    const NAMES: [&str; 4] = ["claude", "codex", "kimi", "cursor-agent"];
    if p.names.iter().any(|n| NAMES.contains(&n.as_str())) {
        return true;
    }
    let node = p.names.iter().any(|n| n == "node" || n == "bun");
    node && p.cmdline.as_ref().is_some_and(|args| {
        args.iter().skip(1).take(4).any(|a| {
            a.contains("@anthropic-ai/claude-code")
                || a.contains("claude-code/cli")
                || a.contains("@openai/codex")
                || a.ends_with("/bin/codex")
                || a.ends_with("/codex.js")
        })
    })
}

/// A transcript by its name and place: `rollout-*.jsonl`, or a `.jsonl` below `projects/`.
fn is_transcript_path(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.ends_with(".jsonl") && (name.starts_with("rollout-") || has_segment(p, "projects"))
}

/// Newest file time below each `<harness root>/work/<s>/` among the walked directories (the
/// scan calls this with every directory's newest direct-file time).
pub fn work_sessions<'a>(
    roots: &[ProtectedRoot],
    dirs: impl Iterator<Item = (&'a Path, Option<i64>)>,
) -> Vec<WorkSession> {
    let harness: Vec<(String, Vec<PathBuf>)> = roots
        .iter()
        .filter(|r| r.exists && r.carve == CarveLayout::AgentHarness)
        .map(|r| {
            let mut ids = vec![normalize(Path::new(&r.spelled))];
            ids.extend(r.canonical.iter().map(|c| normalize(Path::new(c))));
            (r.spelled.clone(), ids)
        })
        .collect();
    if harness.is_empty() {
        return Vec::new();
    }
    let mut found: BTreeMap<(String, String), Option<i64>> = BTreeMap::new();
    for (dir, newest) in dirs {
        let n = normalize(dir);
        for (spelled, ids) in &harness {
            let Some(rel) = ids.iter().find_map(|h| rel_below(&n, h)) else {
                continue;
            };
            if rel.len() >= 2 && comp_eq(&rel[0], OsStr::new("work")) {
                let slot = found
                    .entry((spelled.clone(), rel[1].to_string_lossy().into_owned()))
                    .or_insert(None);
                if let Some(t) = newest {
                    *slot = Some(slot.map_or(t, |s| s.max(t)));
                }
            }
        }
    }
    found
        .into_iter()
        .map(|((root, name), newest_mtime)| WorkSession {
            root,
            name,
            newest_mtime,
        })
        .collect()
}

// ------------------------------------------------------------------------------- gathering

/// What points an index entry at a node: the node itself, or its owning project.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    Node,
    Owner,
}

/// Most transcripts the bulk pass reads per plan.
#[cfg(not(test))]
const MAX_TRANSCRIPTS: usize = 5000;
/// Small in tests, so the limit can be reached.
#[cfg(test)]
const MAX_TRANSCRIPTS: usize = 8;
/// Most live-agent transcript reads per plan (their own budget, spent first).
#[cfg(not(test))]
const MAX_LIVE_TRANSCRIPTS: usize = 1000;
#[cfg(test)]
const MAX_LIVE_TRANSCRIPTS: usize = 4;
/// Most entries one agent-evidence listing looks at.
const MAX_LIST: usize = 10_000;
/// Most entries one transcript or same-named walk looks at in all.
const MAX_WALK: usize = 1_000_000;

/// Which budget a transcript read spends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Budget {
    Live,
    Bulk,
}

/// Decide the per-item evidence for every node of `scan`. `parent` is the cleaned parent links.
pub(crate) fn gather(scan: &ScanResult, parent: &[Option<usize>], o: &Opts) -> Evidence {
    let n = scan.nodes.len();
    // The one walker every agent-evidence walk below goes through (F1).
    let mut walker = Walker::default();
    let host = Host::new(scan, &mut walker);
    let window = i64::try_from(o.window_days)
        .unwrap_or(i64::MAX)
        .saturating_mul(86_400);
    let recent = |t: Option<i64>| t.is_some_and(|t| o.now.saturating_sub(t) <= window);
    let mut ev = Evidence {
        nodes: vec![NodeEvidence::default(); n],
        ..Evidence::default()
    };
    let mut notes_more = 0usize;
    let mut note = |ev: &mut Evidence, s: String| {
        if ev.notes.len() < 20 {
            ev.notes.push(s);
        } else {
            notes_more += 1;
        }
    };

    // `fresh`: modified within the active window, of unknown age, or past a limit (unknown).
    let gap = |ev: &mut Evidence, class: &str, fresh: bool| {
        *ev.gaps.agent_data.entry(class.to_string()).or_insert(0) += 1;
        if fresh {
            *ev.gaps
                .agent_data_recent
                .entry(class.to_string())
                .or_insert(0) += 1;
        }
    };

    // ---- node identities: the path as the scan spelled it and its real path.
    let ids: Vec<Vec<PathBuf>> = scan
        .nodes
        .iter()
        .map(|nd| {
            let mut v = vec![normalize(Path::new(&nd.path))];
            if let Some(r) = &nd.real_path {
                let r = normalize(Path::new(r));
                if key_of(&r) != key_of(&v[0]) {
                    v.push(r);
                }
            }
            v
        })
        .collect();
    let is_item = |i: usize| !matches!(scan.nodes[i].class, Class::Output | Class::Other);

    // ---- sessions: those that own at least one plan item (a session with nothing below it to
    // offer or keep needs no verdict). Without an item below a harness work directory none of
    // the session sources (registry, transcripts, agents' loci) is read at all.
    let mut names: Vec<String> = Vec::new();
    let mut need = false;
    let any_item = (0..n).any(is_item);
    for i in (0..n).filter(|&i| is_item(i)) {
        if let Some(s) = ids[i].iter().find_map(|id| host.session_of(id)) {
            names.push(s);
        }
        need |= ids[i].iter().any(|id| host.under_agent_dir(id));
    }
    names.sort();
    names.dedup();
    let sidx: HashMap<String, usize> = names
        .iter()
        .enumerate()
        .map(|(i, s)| (s.clone(), i))
        .collect();
    ev.sessions = names
        .iter()
        .map(|s| Session {
            name: s.clone(),
            ..Session::default()
        })
        .collect();
    let live = |ev: &mut Evidence, s: &str, why: String| {
        if let Some(&k) = sidx.get(s) {
            if ev.sessions[k].live.is_none() {
                ev.sessions[k].live = Some(why);
            }
        }
    };
    for w in &scan.work_sessions {
        if let Some(&k) = sidx.get(&w.name) {
            let s = &mut ev.sessions[k];
            s.newest = s.newest.max(w.newest_mtime);
        }
    }

    // ---- processes: classify, count.
    let snap = &o.host.processes;
    let procs: Vec<&Proc> = snap
        .procs
        .iter()
        .filter(|p| p.kind == ProcKind::Live)
        .collect();
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, *p)).collect();
    for p in &snap.procs {
        match p.kind {
            ProcKind::KernelThread => ev.counts.kernel_threads += 1,
            ProcKind::Gone => ev.counts.gone += 1,
            ProcKind::Live if p.cwd.is_some() => ev.counts.readable += 1,
            ProcKind::Live if p.cmdline.is_some() => ev.counts.by_command_line += 1,
            ProcKind::Live => ev.counts.fully_unknown += 1,
        }
    }
    if let Some(e) = &snap.error {
        ev.global_asks
            .push(format!("running processes could not be inspected ({e})"));
    } else if ev.counts.fully_unknown > 0 && !o.trust_partial_process_check {
        let k = ev.counts.fully_unknown;
        ev.global_asks.push(format!(
            "{} could not be inspected (neither working directory nor command line could be \
             read), so {} may be using it (--trust-partial-process-check accepts that risk)",
            count(k, "running process", "running processes"),
            if k == 1 { "it" } else { "one of them" }
        ));
    }

    // ---- containers.
    let runtime: Vec<&Proc> = procs
        .iter()
        .copied()
        .filter(|p| p.names.iter().any(|n| is_runtime_child(n)))
        .collect();
    ev.counts.container_processes = runtime.len() as u64;
    let mut binds: Vec<(PathBuf, String)> = Vec::new();
    match (&o.host.containers, runtime.is_empty()) {
        (ContainerState::Queried { containers }, _) => {
            ev.counts.runtime = format!(
                "queried ({})",
                count(
                    containers.len() as u64,
                    "running container",
                    "running containers"
                )
            );
            for c in containers {
                for b in &c.binds {
                    binds.push((normalize(Path::new(b)), c.id.chars().take(12).collect()));
                }
            }
            for s in runtime
                .iter()
                .filter(|p| p.names.iter().any(|n| is_shim(n)))
            {
                let id = s.cmdline.as_deref().and_then(shim_container_id);
                let mapped = id.as_ref().is_some_and(|id| {
                    containers
                        .iter()
                        .any(|c| c.id.starts_with(id.as_str()) || id.starts_with(c.id.as_str()))
                });
                if !mapped {
                    ev.global_asks.push(format!(
                        "container runtime process {} maps to no container the runtime \
                         returned, so a container it runs may be using it",
                        s.pid
                    ));
                    break;
                }
            }
        }
        (ContainerState::Unqueryable { reason }, _) => {
            ev.counts.runtime = format!("could not be queried: {reason}");
            ev.global_asks.push(format!(
                "the container runtime could not be queried ({reason}), so a container may be \
                 using it"
            ));
        }
        (ContainerState::NotQueried, true) => ev.counts.runtime = "not running".to_string(),
        (ContainerState::NotQueried, false) => {
            ev.counts.runtime = "running, not queried".to_string();
            ev.global_asks.push(
                "a container runtime is running but its containers were not listed, so a \
                 container may be using it"
                    .to_string(),
            );
        }
    }

    // ---- the Claude Code session registry: live pids (pid reuse checked), their cwd and id.
    let mut registry: Vec<(u32, RegistryEntry)> = Vec::new();
    let mut projects_dirs: Vec<PathBuf> = Vec::new();
    for base in host.claude_bases.iter().filter(|_| any_item) {
        projects_dirs.extend(walker.named_dirs(base, "projects", 2, MAX_LIST));
        if !need {
            continue;
        }
        for sessions in walker.named_dirs(base, "sessions", 2, MAX_LIST) {
            for e in walker.list(&sessions, MAX_LIST) {
                let name = e.name_lossy();
                if !is_registry_file(&name) {
                    continue;
                }
                // An entry whose metadata cannot be read is a walk error (a gap of unknown
                // age); one gone since the listing was a session that ended.
                let md = match walk::stat(&e.path) {
                    Ok(Some(md)) => md,
                    Ok(None) => continue,
                    Err(unseen) => {
                        walker.errors.push(unseen);
                        continue;
                    }
                };
                let fresh_md = recent(Some(md.mtime));
                if md.kind != Kind::File || md.len > 64 * 1024 {
                    gap(&mut ev, "registry-entry-unknown", fresh_md);
                    continue;
                }
                let Ok(text) = std::fs::read(&e.path) else {
                    gap(&mut ev, "registry-entry-unknown", fresh_md);
                    continue;
                };
                let Ok(entry) = serde_json::from_slice::<RegistryEntry>(&text) else {
                    gap(&mut ev, "registry-entry-unknown", fresh_md);
                    continue;
                };
                drop(text);
                let Some(pid) = entry.pid else {
                    gap(&mut ev, "registry-entry-unknown", fresh_md);
                    continue;
                };
                let Some(p) = by_pid.get(&pid) else { continue };
                // Pid reuse: the process must have started no later than the entry was written.
                let ok = p.start_time.is_some_and(|start| start <= md.mtime + 1);
                if ok {
                    registry.push((pid, entry));
                }
            }
        }
    }
    let mut read_count = 0usize;
    let mut live_count = 0usize;
    let mut slug_cache: HashMap<String, SlugMatch> = HashMap::new();
    // The working directories a transcript records. A Claude Code transcript with no cwd
    // within the bound falls back to its project directory name: every existing directory
    // whose encoding equals it is a candidate, and all are held (rule 5 and liveness). No
    // match, or an incomplete walk, is a gap, recent when the transcript is.
    // Live-agent reads and the bulk pass spend separate budgets (F4).
    let mut read_cwd = |ev: &mut Evidence, path: &Path, budget: Budget| -> Vec<String> {
        let (used, cap) = match budget {
            Budget::Live => (&mut live_count, MAX_LIVE_TRANSCRIPTS),
            Budget::Bulk => (&mut read_count, MAX_TRANSCRIPTS),
        };
        if *used >= cap {
            gap(ev, "transcripts-not-read", true);
            return Vec::new();
        }
        *used += 1;
        let class = match transcript_cwd(path) {
            Ok(c) => return vec![c],
            Err(class) => class,
        };
        // A time that cannot be read is unknown: recent (never old).
        let fresh = match walk::stat(path) {
            Ok(Some(m)) => recent(Some(m.mtime)),
            Ok(None) | Err(_) => true,
        };
        let Some(slug) = claude_project_slug(path) else {
            gap(ev, class, fresh);
            note(ev, format!("{class}: {}", path.display()));
            return Vec::new();
        };
        let sm = slug_cache
            .entry(slug)
            .or_insert_with_key(|s| slug_candidates(s, MAX_SLUG_DIRS))
            .clone();
        if sm.incomplete {
            gap(ev, TRANSCRIPT_SLUG_INCOMPLETE, fresh);
            note(
                ev,
                format!("{TRANSCRIPT_SLUG_INCOMPLETE}: {}", path.display()),
            );
        }
        if sm.candidates.is_empty() && !sm.incomplete {
            gap(ev, class, fresh);
            note(ev, format!("{class}: {}", path.display()));
        } else if !sm.candidates.is_empty() {
            note(
                ev,
                format!(
                    "transcript-cwd-from-project-name ({}): {}",
                    count(sm.candidates.len() as u64, "candidate", "candidates"),
                    path.display()
                ),
            );
        }
        sm.candidates
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect()
    };
    // Process loci (rule 4 within a live session): live registry entries' and agents' loci,
    // and every process's working directory, open files and command-line paths.
    let mut proc_loci: Vec<(PathBuf, String)> = Vec::new();
    for (pid, entry) in &registry {
        if let Some(c) = &entry.cwd {
            proc_loci.push((
                PathBuf::from(c),
                format!("working directory of Claude Code session {pid} (registry)"),
            ));
        }
        let mut via_id = false;
        if let Some(id) = &entry.session_id {
            if sidx.contains_key(id) {
                live(
                    &mut ev,
                    id,
                    format!("Claude Code session {pid} has this session id"),
                );
                via_id = true;
            }
        }
        if let Some(s) = entry
            .cwd
            .as_deref()
            .and_then(|c| host.session_of(Path::new(c)))
        {
            if !via_id {
                live(
                    &mut ev,
                    &s,
                    format!("Claude Code session {pid} works in it (registry)"),
                );
            }
        }
    }

    // ---- agents: loci, sessions they make live, and fail closed for those with none.
    for p in procs.iter().filter(|p| is_agent(p)) {
        if host.uid.is_some() && p.uid.is_some() && p.uid != host.uid {
            continue;
        }
        ev.counts.agents += 1;
        if !need {
            continue;
        }
        let cwd = p.cwd.as_deref().map(Path::new);
        let args = p.cmdline.clone().unwrap_or_default();
        let mut install_dirs: Vec<PathBuf> = Vec::new();
        if let Some(a0) = args.first() {
            let a0 = Path::new(a0);
            if a0.is_absolute() {
                if let Some(d) = a0.parent() {
                    install_dirs.push(normalize(d));
                }
                if let Ok(real) = std::fs::canonicalize(a0) {
                    if let Some(d) = real.parent() {
                        install_dirs.push(normalize(d));
                    }
                }
            }
        }
        // (path, counts as a locus even inside agent configuration)
        let mut loci: Vec<(PathBuf, bool)> = Vec::new();
        let mut open_transcript = false;
        for (rp, entry) in &registry {
            if *rp != p.pid {
                continue;
            }
            if let Some(c) = &entry.cwd {
                loci.push((PathBuf::from(c), true));
            }
            if let Some(id) = &entry.session_id {
                for pd in &projects_dirs {
                    for e in walker.list(pd, MAX_LIST) {
                        if e.kind != Kind::Dir {
                            continue;
                        }
                        let t = e.path.join(format!("{id}.jsonl"));
                        if walker.stat(&t).is_some_and(|m| m.kind == Kind::File) {
                            for c in read_cwd(&mut ev, &t, Budget::Live) {
                                loci.push((PathBuf::from(c), true));
                            }
                        }
                    }
                }
            }
        }
        if let Some(c) = cwd {
            loci.push((c.to_path_buf(), true));
        }
        for f in p.open_files.as_deref().unwrap_or_default() {
            let fp = Path::new(f);
            if is_transcript_path(fp) {
                open_transcript = true;
                for c in read_cwd(&mut ev, fp, Budget::Live) {
                    loci.push((PathBuf::from(c), true));
                }
                continue;
            }
            loci.push((fp.to_path_buf(), false));
        }
        for a in agent_arg_paths(&args, cwd) {
            loci.push((a, false));
        }
        let mut has_locus = false;
        for (l, always) in &loci {
            if let Some(s) = host.session_of(l) {
                live(
                    &mut ev,
                    &s,
                    format!("agent process {} works in it ({})", p.pid, l.display()),
                );
                proc_loci.push((l.clone(), format!("a locus of agent process {}", p.pid)));
                has_locus = true;
                continue;
            }
            if host.holds_nothing_below(l) || !host.under_home(l) {
                continue;
            }
            if host.install_path(l, &install_dirs) || (!always && host.agent_config(l)) {
                continue;
            }
            has_locus = true;
        }
        // A path segment equal to a session id names that session.
        for (l, _) in &loci {
            for s in sidx.keys() {
                if has_segment(l, s) && host.session_of(l).is_none() {
                    live(
                        &mut ev,
                        s,
                        format!("agent process {} names this session id", p.pid),
                    );
                }
            }
        }
        for a in arg_tokens(&args) {
            if sidx.contains_key(a) {
                live(
                    &mut ev,
                    a,
                    format!("agent process {} names this session id", p.pid),
                );
            }
        }
        if has_locus {
            continue;
        }
        let child = procs.iter().any(|q| q.ppid == Some(p.pid));
        let idle = !child && !open_transcript && p.open_files.is_some();
        if idle {
            ev.counts.idle_agents += 1;
        } else {
            ev.counts.unmapped_agents.push(p.pid);
        }
    }
    if !ev.counts.unmapped_agents.is_empty() {
        let pids: Vec<String> = ev
            .counts
            .unmapped_agents
            .iter()
            .map(|p| p.to_string())
            .collect();
        ev.work_asks.push(format!(
            "agent {} {} no locus (no working directory, open file or argument the plan can \
             place) but {} child processes or open files, so it may be working in any session \
             (fail closed)",
            if pids.len() == 1 {
                "process"
            } else {
                "processes"
            },
            pids.join(", "),
            if pids.len() == 1 { "has" } else { "have" }
        ));
    }

    // ---- any process's working directory inside a session.
    for p in &procs {
        if let Some(s) = p.cwd.as_deref().and_then(|c| host.session_of(Path::new(c))) {
            live(
                &mut ev,
                &s,
                format!("process {} has its working directory in it", p.pid),
            );
        }
    }

    // ---- recent transcripts: a recorded cwd in a session makes it live (no process needed),
    // and every recorded cwd outside the ancestor-exception list holds the items it equals,
    // contains or lies inside (below). Read whenever there is an item, so that unreadable
    // recent agent data is seen even when no session owns one.
    let mut recent_cwds: Vec<(PathBuf, String)> = Vec::new();
    // Every transcript read: (recorded cwd or candidate, mtime), for agent-worktree activity.
    let mut seen_cwds: Vec<(PathBuf, i64)> = Vec::new();
    let idle_secs = (IDLE_ASK_DAYS as i64).saturating_mul(86_400).max(window);
    ev.transcripts = TranscriptReads {
        window_days: u64::try_from(idle_secs / 86_400).unwrap_or(0),
        cap: MAX_TRANSCRIPTS as u64,
        live_cap: MAX_LIVE_TRANSCRIPTS as u64,
        ..TranscriptReads::default()
    };
    if any_item {
        let mut transcripts: Vec<(PathBuf, Option<i64>)> = Vec::new();
        // Claude Code: projects/<project>/<id>.jsonl and, for subagents,
        // projects/<project>/<id>/subagents/<agent>.jsonl.
        for pd in &projects_dirs {
            transcripts.extend(walker.files_below(pd, 4, MAX_WALK, &|n| n.ends_with(".jsonl")));
        }
        // Codex: sessions/YYYY/MM/DD/rollout-*.jsonl and archived_sessions/rollout-*.jsonl.
        for base in &host.codex_bases {
            for name in ["sessions", "archived_sessions"] {
                for sd in walker.named_dirs(base, name, 1, MAX_LIST) {
                    transcripts.extend(walker.files_below(&sd, 5, MAX_WALK, &|n| {
                        n.starts_with("rollout-") && n.ends_with(".jsonl")
                    }));
                }
            }
        }
        // A transcript whose time could not be read is already a recorded walk error.
        transcripts.retain(|(_, m)| m.is_some_and(|m| o.now.saturating_sub(m) <= idle_secs));
        transcripts.sort_by_key(|t| std::cmp::Reverse(t.1));
        ev.transcripts.found = transcripts.len() as u64;
        for (t, m) in transcripts {
            let m = m.unwrap_or(o.now);
            for c in read_cwd(&mut ev, &t, Budget::Bulk) {
                seen_cwds.push((PathBuf::from(&c), m));
                if !recent(Some(m)) {
                    continue;
                }
                if let Some(s) = host.session_of(Path::new(&c)) {
                    live(&mut ev, &s, "a transcript modified within the active window records its working directory in it".to_string());
                }
                if !host.holds_nothing_below(Path::new(&c)) {
                    recent_cwds.push((
                        PathBuf::from(&c),
                        format!("working directory of a transcript modified within the active window ({})", t.display()),
                    ));
                }
            }
        }
    }
    ev.transcripts.read = read_count as u64;
    ev.transcripts.live_read = live_count as u64;

    // ---- files below the session directory, newest within the window.
    for s in ev.sessions.clone() {
        if recent(s.newest) {
            live(
                &mut ev,
                &s.name,
                "files below its work directory changed within the active window".to_string(),
            );
        }
    }

    // ---- an entry named exactly <s> or <s>.jsonl within depth 3 of a protected root.
    if !ev.sessions.is_empty() {
        let mut named: HashMap<String, i64> = HashMap::new();
        let mut walked: HashSet<String> = HashSet::new();
        for r in &host.protected {
            if !walked.insert(key_of(r)) {
                continue;
            }
            let mut stack = vec![(r.clone(), 1usize)];
            let mut seen = 0usize;
            while let Some((d, lvl)) = stack.pop() {
                let left = MAX_WALK.saturating_sub(seen);
                if left == 0 {
                    walker.limit(&d);
                    break;
                }
                let entries = walker.list(&d, left);
                seen += entries.len();
                for e in entries {
                    let name = e.name_lossy();
                    let stem = name.strip_suffix(".jsonl").unwrap_or(&name).to_string();
                    if sidx.contains_key(&stem) {
                        // An unreadable time is a recorded walk error (unknown, so recent).
                        if let Some(m) = walker.stat(&e.path) {
                            let slot = named.entry(stem).or_insert(m.mtime);
                            *slot = (*slot).max(m.mtime);
                        }
                    }
                    if e.kind == Kind::Dir && lvl < 3 {
                        stack.push((e.path, lvl + 1));
                    }
                }
            }
        }
        for (s, m) in named {
            if let Some(&k) = sidx.get(&s) {
                let ses = &mut ev.sessions[k];
                ses.newest = ses.newest.max(Some(m));
            }
            if recent(Some(m)) {
                live(
                    &mut ev,
                    &s,
                    "an entry of this name in a protected root changed within the active window"
                        .to_string(),
                );
            }
        }
    }
    // ---- every error the walker met: a directory of agent data not fully seen. Of unknown
    // age, so counted as recent (removals under the agent directories ask), each noted by
    // path only. The walker is consumed here, so no walk can run after its errors are counted.
    for e in walker.errors {
        gap(&mut ev, AGENT_DIR_UNREADABLE, true);
        note(&mut ev, format!("{AGENT_DIR_UNREADABLE}: {}", e.describe()));
    }
    if notes_more > 0 {
        ev.notes.push(format!("and {notes_more} more"));
    }

    // ---- nodes: session, owning project, activity.
    let is_project_node = |i: usize| matches!(scan.nodes[i].class, Class::Repo | Class::Worktree);
    let own_activity = |i: usize| {
        let nd = &scan.nodes[i];
        let commit = nd.repo.as_ref().and_then(|r| r.last_commit_time);
        let mut t = match (nd.newest_mtime, commit) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, None) => a,
            (None, b) => b,
        };
        if is_project_node(i) {
            // E1a: git's own admin files; unknown ones count as active now (fail safe).
            let admin = git_admin_activity(Path::new(&nd.path)).unwrap_or(Some(o.now));
            t = t.max(admin);
        }
        t
    };
    let live_set: Vec<bool> = ev.sessions.iter().map(|s| s.live.is_some()).collect();
    let mut owners: Vec<Vec<PathBuf>> = vec![Vec::new(); n];
    for i in 0..n {
        let nd = &scan.nodes[i];
        let e = &mut ev.nodes[i];
        e.session = ids[i]
            .iter()
            .find_map(|id| host.session_of(id))
            .and_then(|s| sidx.get(&s).copied());
        e.under_work = ids[i].iter().any(|id| host.under_work(id));
        e.under_agent_dir = ids[i].iter().any(|id| host.under_agent_dir(id));
        if is_project_node(i) {
            if ids[i].iter().any(|id| host.under_worktree_dir(id)) {
                e.idle_ask = Some("agent worktree");
            } else if e.session.is_some_and(|k| live_set[k]) {
                e.idle_ask = Some("session live");
            }
        }
        // Enclosing git work trees, nearest first (bounded walk: links may be corrupt).
        let mut gits: Vec<usize> = Vec::new();
        let mut cur = parent[i];
        let mut steps = 0usize;
        while let Some(p) = cur {
            if steps > n {
                break;
            }
            if is_project_node(p) {
                gits.push(p);
            }
            cur = parent[p];
            steps += 1;
        }
        if nd.class.is_artifact() {
            if let Some(proj) = &nd.project {
                owners[i].push(normalize(Path::new(proj)));
                if let Some(real) = nd.real_path.as_deref().and_then(|r| Path::new(r).parent()) {
                    owners[i].push(normalize(real));
                }
                e.project = Some(proj.clone());
                e.activity = nd.project_newest_mtime.or(nd.newest_mtime);
            } else if let Some(&g) = gits.first() {
                owners[i].extend(ids[g].iter().cloned());
                e.project = Some(scan.nodes[g].path.clone());
                e.activity = own_activity(g);
            } else {
                e.activity = own_activity(i);
            }
            // A monorepo: the outermost enclosing git work tree also owns it, unless that is
            // the home or an ancestor of a harness work directory.
            if let Some(&outer) = gits.last() {
                let ok = ids[outer].iter().all(|id| !host.holds_nothing_below(id));
                if ok {
                    owners[i].extend(ids[outer].iter().cloned());
                }
            }
        } else {
            e.activity = own_activity(i);
        }
    }

    // ---- in use: an index from every node identity (and artifact owner) to its node.
    let mut index: HashMap<String, Vec<(usize, Via)>> = HashMap::new();
    for i in 0..n {
        if matches!(scan.nodes[i].class, Class::Output | Class::Other) {
            continue;
        }
        for id in &ids[i] {
            index.entry(key_of(id)).or_default().push((i, Via::Node));
        }
        for id in &owners[i] {
            index.entry(key_of(id)).or_default().push((i, Via::Owner));
        }
    }
    let mark =
        |ev: &mut Evidence, p: &Path, rule: u8, owner_ok: bool, detail: &dyn Fn() -> String| {
            for id in ids_of(p) {
                for anc in id.ancestors() {
                    let Some(hits) = index.get(&key_of(anc)) else {
                        continue;
                    };
                    for &(i, via) in hits {
                        if via == Via::Owner && !owner_ok {
                            continue;
                        }
                        let e = &mut ev.nodes[i];
                        if e.in_use.is_none() {
                            e.in_use = Some(Hit {
                                rule,
                                path: p.to_string_lossy().into_owned(),
                                detail: detail(),
                            });
                        }
                    }
                }
            }
        };
    // Rule 1: readable processes' working directories and open files.
    for p in procs.iter().filter(|p| p.cwd.is_some()) {
        let c = p.cwd.as_deref().unwrap_or_default();
        mark(&mut ev, Path::new(c), 1, true, &|| {
            format!("working directory of process {}", p.pid)
        });
        for f in p.open_files.as_deref().unwrap_or_default() {
            mark(&mut ev, Path::new(f), 1, true, &|| {
                format!("a file open in process {}", p.pid)
            });
        }
    }
    // Rule 2: every process's command line.
    for p in &procs {
        let Some(args) = &p.cmdline else { continue };
        let cwd = p.cwd.as_deref().map(Path::new);
        for a in arg_paths(args, cwd) {
            mark(&mut ev, &a, 2, true, &|| {
                format!("named on the command line of process {}", p.pid)
            });
        }
    }
    // Rule 3: container binds, in either direction.
    for (b, id) in &binds {
        mark(&mut ev, b, 3, false, &|| {
            format!("bound into running container {id}")
        });
    }
    if !binds.is_empty() {
        let bind_keys: HashMap<String, (&PathBuf, &String)> =
            binds.iter().map(|(b, id)| (key_of(b), (b, id))).collect();
        for i in 0..n {
            if ev.nodes[i].in_use.is_some() {
                continue;
            }
            let hit = ids[i].iter().find_map(|id| {
                id.ancestors()
                    .find_map(|a| bind_keys.get(&key_of(a)).copied())
            });
            if let Some((b, cid)) = hit {
                ev.nodes[i].in_use = Some(Hit {
                    rule: 3,
                    path: b.to_string_lossy().into_owned(),
                    detail: format!("inside a folder bound into running container {cid}"),
                });
            }
        }
    }

    // ---- locus holds, applied by the plan at one choke point to every removing action.
    // Rule 5: a recent transcript's cwd (or a candidate of its project directory name) holds
    // every item it equals, contains or lies inside. Rule 4: a process locus (any process's
    // working directory, open files and command-line paths, and agents' loci in a session)
    // holds what it equals or contains (rules 1 and 2 already) and, inside a live session,
    // what lies inside it. One ancestor-exception list applies to both.
    for p in procs.iter() {
        if let Some(c) = p.cwd.as_deref() {
            proc_loci.push((
                PathBuf::from(c),
                format!("inside the working directory of process {}", p.pid),
            ));
        }
        for f in p.open_files.as_deref().unwrap_or_default() {
            proc_loci.push((
                PathBuf::from(f),
                format!("a file open in process {}", p.pid),
            ));
        }
        if let Some(args) = &p.cmdline {
            for a in arg_paths(args, p.cwd.as_deref().map(Path::new)) {
                proc_loci.push((a, format!("named on the command line of process {}", p.pid)));
            }
        }
    }
    let loci: Vec<(PathBuf, String, u8)> = recent_cwds
        .into_iter()
        .map(|(p, d)| (p, d, 5))
        .chain(proc_loci.into_iter().map(|(p, d)| (p, d, 4)))
        .filter(|(p, _, _)| !host.holds_nothing_below(p))
        .collect();
    let hold = |ev: &mut Evidence, i: usize, c: &(PathBuf, String, u8)| {
        let e = &mut ev.nodes[i];
        if e.locus_hold.is_none() {
            e.locus_hold = Some(Hit {
                rule: c.2,
                path: c.0.to_string_lossy().into_owned(),
                detail: c.1.clone(),
            });
        }
    };
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (k, c) in loci.iter().enumerate() {
        for id in ids_of(&c.0) {
            by_key.entry(key_of(&id)).or_default().push(k);
            // Equal or containing: the locus's ancestors through the index (owners included).
            for anc in id.ancestors() {
                for &(i, _) in index.get(&key_of(anc)).map(Vec::as_slice).unwrap_or_default() {
                    hold(&mut ev, i, c);
                }
            }
        }
    }
    if !by_key.is_empty() {
        // Inside: each item's ancestors. A process locus holds below only in a live session.
        for i in (0..n).filter(|&i| is_item(i)) {
            let in_live = ev.nodes[i]
                .session
                .is_some_and(|k| ev.sessions[k].live.is_some());
            let hit = ids[i].iter().find_map(|id| {
                id.ancestors().find_map(|a| {
                    by_key
                        .get(&key_of(a))
                        .and_then(|ks| ks.iter().copied().find(|&k| loci[k].2 == 5 || in_live))
                })
            });
            if let Some(k) = hit {
                hold(&mut ev, i, &loci[k]);
            }
        }
    }
    // G1: an agent worktree's activity includes the transcripts that record a cwd inside it.
    let worktrees: Vec<usize> = (0..n)
        .filter(|&i| ev.nodes[i].idle_ask == Some("agent worktree"))
        .collect();
    for i in worktrees {
        for (c, m) in &seen_cwds {
            if ids_of(c)
                .iter()
                .any(|c| ids[i].iter().any(|id| under(c, id)))
            {
                let e = &mut ev.nodes[i];
                e.activity = e.activity.max(Some(*m));
            }
        }
    }

    // ---- recent agent data that could not be read: removals under agent dirs ask.
    if !ev.gaps.agent_data_recent.is_empty() {
        let total: u64 = ev.gaps.agent_data_recent.values().sum();
        let classes: Vec<String> = ev
            .gaps
            .agent_data_recent
            .iter()
            .map(|(c, n)| format!("{n} {c}"))
            .collect();
        ev.agent_asks.push(format!(
            "agent data unreadable: {} modified within the active window ({}) could not be \
             read for a working directory, so a live agent session may be using it",
            count(total, "entry", "entries"),
            classes.join(", ")
        ));
    }

    // ---- coverage gaps inside each node.
    let c = &scan.coverage;
    let recorded = c.gaps.len() as u64;
    let counted = c
        .unreadable_dirs
        .saturating_add(c.other_unreadable_entries)
        .saturating_add(c.mount_points_skipped);
    ev.gaps_incomplete = !c.complete && (c.gaps_overflowed || recorded < counted || counted == 0);
    ev.gaps.overflowed = ev.gaps_incomplete;
    let mut folders: HashMap<PathBuf, u64> = HashMap::new();
    for g in &c.gaps {
        match g.class {
            GapClass::Privacy => ev.gaps.privacy += 1,
            GapClass::Permission => ev.gaps.permission += 1,
            GapClass::Mount => ev.gaps.mount += 1,
            GapClass::Other => ev.gaps.other += 1,
        }
        let gp = normalize(Path::new(&g.path));
        let top = gp.parent().unwrap_or(&gp).to_path_buf();
        *folders.entry(top).or_insert(0) += 1;
        let mut counted_nodes: HashSet<usize> = HashSet::new();
        for anc in gp.ancestors() {
            if let Some(hits) = index.get(&key_of(anc)) {
                for &(i, via) in hits {
                    if via == Via::Node && counted_nodes.insert(i) {
                        ev.nodes[i].gaps_inside += 1;
                    }
                }
            }
        }
    }
    let mut top: Vec<(PathBuf, u64)> = folders.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ev.gaps.top_folders = top
        .into_iter()
        .take(5)
        .map(|(p, k)| (p.to_string_lossy().into_owned(), k))
        .collect();
    ev
}

#[cfg(test)]
mod tests;
