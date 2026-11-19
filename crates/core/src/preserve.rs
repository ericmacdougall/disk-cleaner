//! The preserve obligation: what a removal of a git node must save, or verify, first.
//!
//! Every file of a git directory belongs to exactly one of two classes, decided by its path
//! alone (no content is read to decide):
//!
//! - **the object store** ([`object_store_files`]): files in `objects/` whose names are those of
//!   object-store files: loose objects, packs, their indexes, reverse indexes, bitmaps and cruft
//!   mtimes, multi-pack-indexes and their sidecars and layers, commit-graphs. They can be very
//!   large, so they are not copied by default; a removal must either verify each of them as git
//!   data whose objects are all on the re-verified remote, or back it up. A name confers no
//!   exemption, only membership in this verify-or-back-up class.
//! - **the git directory's own files** ([`git_admin_files`]): every other non-directory entry
//!   (config, hooks, refs, logs, the index, messages, rerere's records, `.keep` and `.promisor`
//!   markers, chain files, `objects/info` lists, and anything a person put anywhere). A removal
//!   always backs these up.
//!
//! [`verify`] (opt-in, `scan --verify-objects`) has git itself check the object store, read-only,
//! and accounts for each object-store file against what git verified: a file git does not
//! account for moves to the git directory's own files (and the repository asks).
//!
//! [`sort`] is the one classifier: the scan's [`survey`], [`git_admin_files`] and
//! [`object_store_files`] all go through it, so they always agree.

use crate::git::{Git, GitError};
use crate::gitshape::{fanout, loose_object, named, oid};
use crate::model::{AdminFile, Class, GitAdminSurvey, Node, ObjectStoreSurvey};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Most entries walked in one git directory; more cannot be enumerated and is an error.
pub const MAX_ENTRIES: u64 = 2_000_000;
/// Examples kept in a [`GitAdminSurvey`].
const EXAMPLES: usize = 5;
/// Largest files kept in a [`GitAdminSurvey`].
const LARGEST: usize = 3;
/// Most bytes of git's error text kept in a finding.
const ERROR_TEXT: usize = 160;

/// The hooks `git init` installs from its stock template (git 2.56).
const STOCK_HOOKS: [&str; 14] = [
    "applypatch-msg.sample",
    "commit-msg.sample",
    "fsmonitor-watchman.sample",
    "post-update.sample",
    "pre-applypatch.sample",
    "pre-commit.sample",
    "pre-merge-commit.sample",
    "pre-push.sample",
    "pre-rebase.sample",
    "pre-receive.sample",
    "prepare-commit-msg.sample",
    "push-to-checkout.sample",
    "sendemail-validate.sample",
    "update.sample",
];

/// The pack-side files of the object store (`.keep` and `.promisor` are markers a person or a
/// partial clone set: they are the git directory's own).
const PACK_STORE_EXTS: [&str; 5] = ["pack", "idx", "rev", "bitmap", "mtimes"];

/// One git directory's files, sorted into the two classes.
#[derive(Debug, Default)]
pub struct Sorted {
    /// Its own files: path relative to the git directory with `/` separators, and allocated
    /// bytes. Sorted by path.
    pub own: Vec<(String, u64)>,
    /// The object store's files, the same way.
    pub store: Vec<(String, u64)>,
    /// Entries with an object-store name that are not regular files (a symlink, a socket): they
    /// are not git data, so they are in `own`, and the repository asks.
    pub not_objects: Vec<String>,
}

/// What git's verification of an object store found (see [`verify`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Verification {
    /// Every check ran and passed, git accounted for every object-store file, and no object is
    /// unreachable from the repository's refs, reflogs and index.
    pub verified: bool,
    /// Object-store-named files git does not account for: the git directory's own content.
    pub unaccounted: Vec<String>,
    /// A check that ran and failed: git reports the store damaged (or could not read it).
    pub damaged: Option<String>,
    /// Why the store is not verified when nothing is damaged or unaccounted (a timeout,
    /// unreachable objects, a format this version does not verify).
    pub unverified: Vec<String>,
}

/// What [`survey`] found: the counts for the plan, and what the scan must report.
#[derive(Debug, Default)]
pub struct Survey {
    pub admin: GitAdminSurvey,
    pub store: ObjectStoreSurvey,
    pub not_objects: Vec<String>,
    /// Present when the object store was verified (`scan --verify-objects`).
    pub verification: Option<Verification>,
}

/// The git directory a node's removal deletes: `<path>/.git` for a repo or worktree whose `.git`
/// is a real directory, the directory itself for a bare repo.
pub fn git_dir_of(node: &Node) -> io::Result<PathBuf> {
    let path = Path::new(&node.path);
    match node.class {
        Class::BareRepo => Ok(path.to_path_buf()),
        Class::Repo | Class::Worktree => {
            let gd = path.join(".git");
            if fs::symlink_metadata(&gd)?.is_dir() {
                Ok(gd)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the .git entry is not a directory, so the git directory lies elsewhere",
                ))
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a repo, worktree or bare repo",
        )),
    }
}

/// Every file in `node`'s git directory that is not an object-store file, as paths relative to
/// the git directory with `/` separators, sorted. A removal must back up, and verify the backup
/// of, every one of them before it removes anything. An error (an unreadable directory, a name
/// that is not UTF-8, too many entries) means the list is not complete: nothing may be removed
/// then.
pub fn git_admin_files(node: &Node) -> io::Result<Vec<String>> {
    let gd = git_dir_of(node)?;
    Ok(sort(&gd)?.own.into_iter().map(|(p, _)| p).collect())
}

/// Every file in `node`'s git directory whose name is that of an object-store file, the same
/// way. A removal must verify each of them as git data whose objects are on the re-verified
/// remote, or back it up, before it removes anything. Together with [`git_admin_files`] it lists
/// every non-directory entry of the git directory exactly once.
pub fn object_store_files(node: &Node) -> io::Result<Vec<String>> {
    let gd = git_dir_of(node)?;
    Ok(sort(&gd)?.store.into_iter().map(|(p, _)| p).collect())
}

/// Count `git_dir`'s two classes for the plan (see [`sort`]). With `verify_with` (git and the
/// directory to run it in), the object store is verified first (see [`verify`]), and files git
/// does not account for are counted as the git directory's own.
pub fn survey(git_dir: &Path, verify_with: Option<(&Git, &Path)>) -> io::Result<Survey> {
    let mut sorted = sort(git_dir)?;
    let verification = verify_with.map(|(git, cwd)| verify(git, cwd, git_dir, &sorted.store));
    if let Some(v) = &verification {
        let moved: BTreeSet<&str> = v.unaccounted.iter().map(String::as_str).collect();
        let (out, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut sorted.store)
            .into_iter()
            .partition(|(p, _)| moved.contains(p.as_str()));
        sorted.store = keep;
        sorted.own.extend(out);
        sorted.own.sort();
    }
    let store = ObjectStoreSurvey {
        files: sorted.store.len() as u64,
        bytes: sorted.store.iter().map(|(_, b)| *b).sum(),
        verified: verification.as_ref().is_some_and(|v| v.verified),
    };
    Ok(Survey {
        admin: admin_survey(&sorted.own),
        store,
        not_objects: sorted.not_objects,
        verification,
    })
}

/// The plan's counts of a git directory's own files.
fn admin_survey(own: &[(String, u64)]) -> GitAdminSurvey {
    let mut admin = GitAdminSurvey {
        files: own.len() as u64,
        bytes: own.iter().map(|(_, b)| *b).sum(),
        ..GitAdminSurvey::default()
    };
    admin.examples = own
        .iter()
        .filter(|(p, _)| !everyday(p))
        .take(EXAMPLES)
        .map(|(p, _)| p.clone())
        .collect();
    let mut by_size: Vec<&(String, u64)> = own.iter().collect();
    by_size.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    admin.largest = by_size
        .into_iter()
        .take(LARGEST)
        .map(|(p, b)| AdminFile {
            path: p.clone(),
            bytes: *b,
        })
        .collect();
    admin
}

/// A name every clone has (or git writes in ordinary use): not worth naming as an example.
/// Examples go by name only; the backup covers every file whatever its name or content.
fn everyday(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        [top] => matches!(
            *top,
            "HEAD"
                | "config"
                | "description"
                | "index"
                | "packed-refs"
                | "FETCH_HEAD"
                | "ORIG_HEAD"
                | "shallow"
        ),
        ["info", "exclude" | "refs"] | ["objects", "info", "packs"] | ["logs", "HEAD"] => true,
        ["hooks", name] => STOCK_HOOKS.contains(name),
        ["refs", rest @ ..] | ["logs", "refs", rest @ ..] => {
            !rest.is_empty() && rest.iter().all(|p| !p.starts_with('.'))
        }
        _ => false,
    }
}

/// Is this path (relative to the git directory, split at `/`) the name of an object-store file?
fn store_name(parts: &[&str]) -> bool {
    match parts {
        ["objects", d, f] if fanout(d) && loose_object(f) => true,
        ["objects", "pack", "multi-pack-index"] => true,
        ["objects", "pack", f] => {
            named(f, "pack-", &PACK_STORE_EXTS) || named(f, "multi-pack-index-", &["bitmap", "rev"])
        }
        ["objects", "pack", "multi-pack-index.d", f] => {
            named(f, "multi-pack-index-", &["midx", "bitmap", "rev"])
        }
        ["objects", "info", "commit-graph"] => true,
        ["objects", "info", "commit-graphs", f] => named(f, "graph-", &["graph"]),
        _ => false,
    }
}

/// Allocated bytes of an entry (blocks on Unix, the length elsewhere).
#[cfg(unix)]
fn allocated(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated(md: &fs::Metadata) -> u64 {
    md.len()
}

/// Sort every non-directory entry of `git_dir` into its own files and the object store's files,
/// by path alone. Symlinks are never followed; a symlink or any other non-directory entry is a
/// file of the git directory's own (and, under an object-store name, also in `not_objects`).
/// Bounded: more than [`MAX_ENTRIES`] entries is an error, as is any entry that cannot be listed
/// or named in UTF-8.
pub fn sort(git_dir: &Path) -> io::Result<Sorted> {
    let mut out = Sorted::default();
    let mut seen = 0u64;
    let mut stack: Vec<(PathBuf, String)> = vec![(git_dir.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for e in fs::read_dir(&dir)? {
            let e = e?;
            seen += 1;
            if seen > MAX_ENTRIES {
                return Err(io::Error::other(format!(
                    "more than {MAX_ENTRIES} entries in the git directory"
                )));
            }
            let name = e.file_name().into_string().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "an entry in the git directory has a name that is not UTF-8",
                )
            })?;
            let md = e.metadata()?;
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if md.is_dir() {
                stack.push((dir.join(&name), child));
                continue;
            }
            let parts: Vec<&str> = child.split('/').collect();
            if store_name(&parts) {
                if md.is_file() {
                    out.store.push((child, allocated(&md)));
                    continue;
                }
                out.not_objects.push(child.clone());
            }
            out.own.push((child, allocated(&md)));
        }
    }
    out.own.sort();
    out.store.sort();
    out.not_objects.sort();
    Ok(out)
}

/// How one git check ended.
enum Outcome {
    /// It ran and passed; its standard output.
    Passed(Vec<u8>),
    /// It ran and failed: git reported a problem.
    Failed(String),
    /// It could not run or finish (a timeout, git missing): nothing is known.
    Inconclusive(String),
}

fn run_check(git: &Git, cwd: &Path, args: &[OsString]) -> Outcome {
    match git.run_bytes(cwd, args) {
        Ok(out) => Outcome::Passed(out),
        Err(GitError::Failed { stderr, .. }) => {
            let first = stderr.lines().next().unwrap_or("").trim();
            let text: String = if first.is_empty() {
                "git exited with a failure status".to_string()
            } else {
                first.chars().take(ERROR_TEXT).collect()
            };
            Outcome::Failed(text)
        }
        Err(e) => Outcome::Inconclusive(e.to_string()),
    }
}

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

/// The hex checksum at the end of a file git checksums (a multi-pack-index): its last
/// `hexsz / 2` bytes.
fn trailing_checksum(path: &Path, hexsz: usize) -> io::Result<String> {
    let n = (hexsz / 2) as u64;
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len < n {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too short"));
    }
    f.seek(SeekFrom::Start(len - n))?;
    let mut buf = Vec::with_capacity(n as usize);
    f.take(n).read_to_end(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The ids listed one per line in a chain file (`commit-graph-chain`, `multi-pack-index-chain`).
fn chain_ids(path: &Path) -> io::Result<BTreeSet<String>> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| oid(l))
        .map(str::to_string)
        .collect())
}

/// `<prefix><id>.<ext>` split into its id and extension.
fn id_ext<'a>(name: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    name.strip_prefix(prefix)?.split_once('.')
}

/// Have git verify the object store of the repository at `cwd` (whose git directory is
/// `git_dir`), read-only, and account for each of `store` (from [`sort`]):
///
/// - `git rev-parse --show-object-format` gives the hash length; a loose object of another
///   length is a file git never reads, so it is unaccounted.
/// - `git verify-pack <idx>` for each pack whose `.pack` and `.idx` are both present: the index
///   and the pack are each checked against their checksums and every object in the pack is
///   read. A `.pack` or `.idx` without its partner is unaccounted, as are a pack's `.rev`,
///   `.bitmap` and `.mtimes` without a verified pack.
/// - `git multi-pack-index verify` when a multi-pack-index exists; its `.bitmap` and `.rev` are
///   accounted only when named by its checksum. Incremental layers are accounted when their
///   chain lists them, but this version does not verify them, so the store stays unverified.
/// - `git commit-graph verify` when a commit-graph exists. git verifies the single
///   `commit-graph` when there is one and the split chain only otherwise, so split graph files
///   are accounted only when there is no single file and their chain lists them.
/// - `git fsck --full --strict --no-dangling --unreachable --no-progress`: every loose object is
///   inflated and hashed (no trailing bytes allowed) and every object is checked; any
///   unreachable object leaves the store unverified, as it may exist nowhere else.
///
/// The commit-graph and multi-pack-index checks run with their `core.*` switch on, so a config
/// that turns the feature off cannot make the check pass without looking.
pub fn verify(git: &Git, cwd: &Path, git_dir: &Path, store: &[(String, u64)]) -> Verification {
    let mut v = Verification::default();
    let mut damaged: Vec<String> = Vec::new();
    let mut unaccounted: Vec<String> = Vec::new();

    let hexsz = match run_check(git, cwd, &args(&["rev-parse", "--show-object-format"])) {
        Outcome::Passed(out) => match String::from_utf8_lossy(&out).trim() {
            "sha1" => Some(40),
            "sha256" => Some(64),
            other => {
                v.unverified.push(format!(
                    "git uses an object format this tool does not know ({other})"
                ));
                None
            }
        },
        Outcome::Failed(e) | Outcome::Inconclusive(e) => {
            v.unverified
                .push(format!("the object format could not be read ({e})"));
            None
        }
    };

    // Group the object store's files by what accounts for them.
    let mut packs: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut midx: Option<String> = None;
    let mut midx_side: Vec<(String, String)> = Vec::new();
    let mut layers: Vec<(String, String)> = Vec::new();
    let mut graph: Option<String> = None;
    let mut graphs: Vec<(String, String)> = Vec::new();
    for (rel, _) in store {
        let parts: Vec<&str> = rel.split('/').collect();
        match parts.as_slice() {
            ["objects", "pack", "multi-pack-index"] => midx = Some(rel.clone()),
            ["objects", "pack", "multi-pack-index.d", f] => {
                if let Some((id, _)) = id_ext(f, "multi-pack-index-") {
                    layers.push((id.to_string(), rel.clone()));
                }
            }
            ["objects", "pack", f] => {
                if let Some((id, _)) = id_ext(f, "multi-pack-index-") {
                    midx_side.push((id.to_string(), rel.clone()));
                } else if let Some((id, ext)) = id_ext(f, "pack-") {
                    packs
                        .entry(id.to_string())
                        .or_default()
                        .push((ext.to_string(), rel.clone()));
                }
            }
            ["objects", "info", "commit-graph"] => graph = Some(rel.clone()),
            ["objects", "info", "commit-graphs", f] => {
                if let Some((id, _)) = id_ext(f, "graph-") {
                    graphs.push((id.to_string(), rel.clone()));
                }
            }
            ["objects", _, f] => {
                if let Some(n) = hexsz {
                    if f.len() + 2 != n {
                        unaccounted.push(rel.clone());
                    }
                }
            }
            _ => unaccounted.push(rel.clone()),
        }
    }

    // Packs: a pair git verifies accounts for itself and its sidecars.
    for (id, files) in &packs {
        let has = |ext: &str| files.iter().any(|(e, _)| e == ext);
        if !(has("pack") && has("idx")) {
            unaccounted.extend(files.iter().map(|(_, r)| r.clone()));
            continue;
        }
        let idx = git_dir
            .join("objects")
            .join("pack")
            .join(format!("pack-{id}.idx"));
        let a = [OsString::from("verify-pack"), idx.into_os_string()];
        match run_check(git, cwd, &a) {
            Outcome::Passed(_) => {}
            Outcome::Failed(e) => damaged.push(format!("pack-{id}: {e}")),
            Outcome::Inconclusive(e) => v
                .unverified
                .push(format!("pack-{id} could not be verified ({e})")),
        }
    }

    // The multi-pack-index and the sidecars named by its checksum.
    let mut midx_sum: Option<String> = None;
    if let Some(rel) = &midx {
        let a = args(&[
            "-c",
            "core.multiPackIndex=true",
            "multi-pack-index",
            "verify",
        ]);
        match run_check(git, cwd, &a) {
            Outcome::Passed(_) => match hexsz.map(|n| trailing_checksum(&git_dir.join(rel), n)) {
                Some(Ok(sum)) => midx_sum = Some(sum),
                Some(Err(e)) => damaged.push(format!("multi-pack-index: {e}")),
                None => {}
            },
            Outcome::Failed(e) => damaged.push(format!("multi-pack-index: {e}")),
            Outcome::Inconclusive(e) => v
                .unverified
                .push(format!("the multi-pack-index could not be verified ({e})")),
        }
    }
    for (id, rel) in &midx_side {
        match (&midx, &midx_sum) {
            (None, _) => unaccounted.push(rel.clone()),
            (Some(_), Some(sum)) if sum != id => unaccounted.push(rel.clone()),
            _ => {}
        }
    }
    if !layers.is_empty() {
        let chain = git_dir
            .join("objects")
            .join("pack")
            .join("multi-pack-index.d")
            .join("multi-pack-index-chain");
        match chain_ids(&chain) {
            Ok(ids) => unaccounted.extend(
                layers
                    .iter()
                    .filter(|(id, _)| !ids.contains(id))
                    .map(|(_, r)| r.clone()),
            ),
            Err(_) => unaccounted.extend(layers.iter().map(|(_, r)| r.clone())),
        }
        v.unverified.push(
            "the incremental multi-pack-index layers are not verified by this version".to_string(),
        );
    }

    // Commit-graphs.
    if graph.is_some() || !graphs.is_empty() {
        let a = args(&["-c", "core.commitGraph=true", "commit-graph", "verify"]);
        match run_check(git, cwd, &a) {
            Outcome::Passed(_) => {}
            Outcome::Failed(e) => damaged.push(format!("commit-graph: {e}")),
            Outcome::Inconclusive(e) => v
                .unverified
                .push(format!("the commit-graph could not be verified ({e})")),
        }
    }
    if graph.is_some() {
        // git reads the single commit-graph and opens the split chain only when it is absent
        // (builtin/commit-graph.c), so no split graph file beside it was read.
        unaccounted.extend(graphs.iter().map(|(_, r)| r.clone()));
    } else if !graphs.is_empty() {
        let chain = git_dir
            .join("objects")
            .join("info")
            .join("commit-graphs")
            .join("commit-graph-chain");
        let ids = chain_ids(&chain).unwrap_or_default();
        unaccounted.extend(
            graphs
                .iter()
                .filter(|(id, _)| !ids.contains(id))
                .map(|(_, r)| r.clone()),
        );
    }

    // Every object, loose ones by their hash, and what nothing refers to.
    let a = args(&[
        "fsck",
        "--full",
        "--strict",
        "--no-dangling",
        "--unreachable",
        "--no-progress",
    ]);
    match run_check(git, cwd, &a) {
        Outcome::Passed(out) => {
            let n = String::from_utf8_lossy(&out)
                .lines()
                .filter(|l| l.starts_with("unreachable "))
                .count();
            if n > 0 {
                v.unverified.push(format!(
                    "{} not reachable from any ref, reflog or the index, so {} may exist \
                     nowhere else",
                    crate::words::count(n as u64, "object is", "objects are"),
                    crate::words::agree(n as u64, "it", "they")
                ));
            }
        }
        Outcome::Failed(e) => damaged.push(format!("fsck: {e}")),
        Outcome::Inconclusive(e) => v
            .unverified
            .push(format!("git fsck could not finish ({e})")),
    }

    unaccounted.sort();
    unaccounted.dedup();
    v.unaccounted = unaccounted;
    v.damaged = (!damaged.is_empty()).then(|| damaged.join("; "));
    v.verified = hexsz.is_some()
        && v.damaged.is_none()
        && v.unaccounted.is_empty()
        && v.unverified.is_empty();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> Vec<&str> {
        s.split('/').collect()
    }

    #[test]
    fn names_alone_say_which_files_are_the_object_stores() {
        let id = "0123456789abcdef0123456789abcdef01234567";
        let store = |s: String| store_name(&parts(&s));
        assert!(store(format!("objects/ab/{}", &id[2..])));
        assert!(store(format!("objects/pack/pack-{id}.pack")));
        assert!(store(format!("objects/pack/pack-{id}.mtimes")));
        assert!(store(format!("objects/pack/multi-pack-index-{id}.bitmap")));
        assert!(store(format!("objects/pack/multi-pack-index-{id}.rev")));
        assert!(store(format!(
            "objects/pack/multi-pack-index.d/multi-pack-index-{id}.midx"
        )));
        assert!(store(format!(
            "objects/info/commit-graphs/graph-{id}.graph"
        )));
        assert!(store_name(&parts("objects/pack/multi-pack-index")));
        assert!(store_name(&parts("objects/info/commit-graph")));
        // Markers, lists, chains and anything else are the git directory's own.
        assert!(!store(format!("objects/pack/pack-{id}.keep")));
        assert!(!store(format!("objects/pack/pack-{id}.promisor")));
        assert!(!store(format!("objects/pack/pack-{id}.pack.notes")));
        for own in [
            "objects/info/packs",
            "objects/info/alternates",
            "objects/info/commit-graphs/commit-graph-chain",
            "objects/pack/multi-pack-index.d/multi-pack-index-chain",
            "objects/notes.txt",
            "objects/ab/README",
            "objects/pack/pack-x.pack",
            "HEAD",
            "hooks/pre-commit.sample",
            "lfs/objects/ab/cd/abcd",
        ] {
            assert!(!store_name(&parts(own)), "{own}");
        }
    }

    #[test]
    fn examples_skip_names_every_clone_has() {
        for every in [
            "HEAD",
            "config",
            "index",
            "info/exclude",
            "hooks/pre-commit.sample",
            "refs/heads/main",
            "logs/refs/remotes/origin/HEAD",
            "logs/HEAD",
        ] {
            assert!(everyday(every), "{every}");
        }
        for unusual in [
            "COMMIT_EDITMSG",
            "hooks/custom.sample",
            "hooks/pre-commit",
            "logs/refs/.notes",
            "rr-cache/x/postimage",
            "objects/pack/pack-x.keep",
        ] {
            assert!(!everyday(unusual), "{unusual}");
        }
    }

    #[test]
    fn a_checksum_is_read_from_the_end_and_chains_list_ids() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("multi-pack-index");
        let mut bytes = b"MIDX and some body".to_vec();
        bytes.extend((0u8..20).collect::<Vec<u8>>());
        fs::write(&f, &bytes).unwrap();
        assert_eq!(
            trailing_checksum(&f, 40).unwrap(),
            "000102030405060708090a0b0c0d0e0f10111213"
        );
        assert!(trailing_checksum(&f, 128).is_err());
        let chain = dir.path().join("chain");
        let id = "0123456789abcdef0123456789abcdef01234567";
        fs::write(&chain, format!("{id}\nnot an id\n")).unwrap();
        let ids = chain_ids(&chain).unwrap();
        assert!(ids.contains(id));
        assert_eq!(ids.len(), 1);
        assert_eq!(id_ext("pack-ab.idx", "pack-"), Some(("ab", "idx")));
    }
}
