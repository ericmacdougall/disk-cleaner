//! Repo safety facts (design section 3), gathered with read-only git commands.
//!
//! Every fact is `Yes`, `No` or `Unknown`. Any git failure, timeout or unparseable output is
//! `Unknown`; it is never promoted to the safe value. Remote URLs lose embedded credentials
//! before they are stored.

use crate::git::{Git, GitError};
use crate::model::{Class, Facts, Remote, RepoInfo, Tri};
use crate::redact::{config_key_label, diag, strip_url_credentials};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Decides whether a (credential-free) remote URL is a network location.
pub type NetworkPredicate = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How facts are gathered. The network predicate is injectable so tests can treat a local
/// bare repository as "the network" without changing production behaviour.
#[derive(Clone)]
pub struct FactsConfig {
    pub git: Git,
    pub is_network: NetworkPredicate,
}

impl Default for FactsConfig {
    fn default() -> Self {
        FactsConfig {
            git: Git::default(),
            is_network: Arc::new(is_network_url),
        }
    }
}

impl FactsConfig {
    /// Use a different git binary or timeout.
    pub fn with_git(mut self, git: Git) -> Self {
        self.git = git;
        self
    }

    /// Replace the network predicate (tests only; production uses [`is_network_url`]).
    pub fn with_network_predicate(
        mut self,
        f: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.is_network = Arc::new(f);
        self
    }
}

/// Production rule: a local path, a `file://` URL or a `.bundle` file is not a network remote
/// (design section 3: it is not a backup); `scheme://host`, `user@host:path` and `host:path`
/// are.
pub fn is_network_url(url: &str) -> bool {
    let u = url.trim();
    if u.is_empty() {
        return false;
    }
    let lower = u.to_ascii_lowercase();
    if lower.ends_with(".bundle") || lower.starts_with("file:") {
        return false;
    }
    if let Some(idx) = lower.find("://") {
        return idx > 0;
    }
    if u == "." || u == ".." || u.starts_with(['/', '\\', '~']) {
        return false;
    }
    if u.starts_with("./") || u.starts_with("../") || u.starts_with(".\\") || u.starts_with("..\\")
    {
        return false;
    }
    let b = u.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
    {
        return false; // Windows drive path
    }
    match (u.find(':'), u.find('/')) {
        (None, _) => false, // plain relative path
        (Some(c), Some(s)) if s < c => false,
        _ => true, // scp-style host:path
    }
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn note(what: &str, e: &GitError) -> String {
    diag(&format!("{what}: {e}"))
}

/// Diagnostic lines of one repo. Every line passes through [`diag`] on the way in, so nothing a
/// repository controls (a file name, a git error) reaches a stored note unredacted.
#[derive(Default)]
struct Notes(Vec<String>);

impl Notes {
    fn push(&mut self, text: impl AsRef<str>) {
        self.0.push(diag(text.as_ref()));
    }
}

fn parse_remotes(out: &str, cfg: &FactsConfig) -> Vec<Remote> {
    let mut remotes = Vec::new();
    for line in out.lines() {
        let (name, rest) = match line.split_once('\t') {
            Some(x) => x,
            None => continue,
        };
        let url = match rest.strip_suffix(" (fetch)") {
            Some(u) => strip_url_credentials(u.trim()),
            None => continue,
        };
        let network = (cfg.is_network)(&url);
        remotes.push(Remote {
            name: diag(name),
            url,
            network,
        });
    }
    remotes
}

fn rel_slash(path: &Path, base: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

/// What an entry is, by `lstat`: never followed.
enum Lk {
    Missing,
    Dir,
    File,
    Link,
    Other,
}

fn lk(p: &Path) -> Result<Lk, String> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => Ok(Lk::Link),
        Ok(m) if m.is_dir() => Ok(Lk::Dir),
        Ok(m) if m.is_file() => Ok(Lk::File),
        Ok(_) => Ok(Lk::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Lk::Missing),
        Err(e) => Err(e.to_string()),
    }
}

/// Refs a plain repository may have: branches, tags, remote-tracking refs and the stash.
fn plain_ref_name(name: &str) -> bool {
    name == "refs/stash"
        || name.starts_with("refs/heads/")
        || name.starts_with("refs/tags/")
        || name.starts_with("refs/remotes/")
}

/// Does this repository have the plain layout? `Yes` only when every check holds; `No` (with the
/// first failing check) when one fails; `Unknown` when a check could not be made. All checks are
/// `lstat`, directory listings, names and bounded reads, plus one `for-each-ref` through the
/// hardened runner; nothing is followed.
///
/// Checks, in order: `.git` is a real directory; `HEAD`, `config`, `refs`, `objects`,
/// `objects/info` and `objects/pack` exist as the right kind of entry and none is a symlink; none
/// of `objects/info/alternates`, `objects/info/http-alternates`, `commondir`, `info/grafts` or
/// `annex` exists; `worktrees` and `modules` are absent or empty; no unfinished operation and no
/// local-only LFS or annex content ([`pending_state`]); every ref lies under `refs/heads/`,
/// `refs/tags/`, `refs/remotes/` or is `refs/stash`, and points at a commit or at an annotated tag
/// whose target is a commit. (The config allowlist runs before any of this.)
/// Outcome of the structural part of the layout check.
enum Shape {
    Fine,
    /// A check failed. `skip_git` is true when git must not be run in this repo at all: an
    /// entry git itself reads (`HEAD`, `config`, `refs`, `objects`) is missing, a symlink or the
    /// wrong kind, or a `commondir` would send git to another repository. It is false for a
    /// layout that is merely unusual but healthy (a `.git` file, alternates, worktrees, modules).
    No {
        issue: String,
        skip_git: bool,
    },
    Unknown(String),
}

/// The structural checks of [`layout_check`] (the real `.git` directory, the expected entries, the
/// forbidden entries, empty `worktrees` and `modules`). They are `lstat` and directory listings
/// only, so they run before any git command and decide whether git may be run at all.
fn shape(path: &Path, class: Class) -> Shape {
    macro_rules! no {
        ($($a:tt)*) => {
            return Shape::No {
                issue: format!($($a)*),
                skip_git: true,
            }
        };
    }
    macro_rules! no_soft {
        ($($a:tt)*) => {
            return Shape::No {
                issue: format!($($a)*),
                skip_git: false,
            }
        };
    }
    let gd = if class == Class::BareRepo {
        path.to_path_buf()
    } else {
        let g = path.join(".git");
        match lk(&g) {
            Ok(Lk::Dir) => g,
            Ok(Lk::Missing) => no!(".git is missing"),
            Ok(Lk::File) => {
                return Shape::No {
                    issue: ".git is a file, not a real directory (a linked worktree, submodule \
                            checkout or --separate-git-dir repository: not handled in this phase)"
                        .to_string(),
                    skip_git: false,
                }
            }
            Ok(_) => no!(
                ".git is not a real directory (a linked worktree, submodule checkout, \
                 --separate-git-dir repository or a symlink: not handled in this phase)"
            ),
            Err(e) => return Shape::Unknown(format!("cannot read .git ({e})")),
        }
    };
    let mut unknown: Option<String> = None;
    for (name, want_dir) in [
        ("HEAD", false),
        ("config", false),
        ("refs", true),
        ("objects", true),
        ("objects/info", true),
        ("objects/pack", true),
    ] {
        match lk(&gd.join(name)) {
            Ok(Lk::Missing) => no!("{name} is missing from the git directory"),
            Ok(Lk::Link) => no!("{name} is a symlink"),
            Ok(Lk::Dir) if want_dir => {}
            Ok(Lk::File) if !want_dir => {}
            Ok(_) => no!("{name} is not the expected kind of entry"),
            Err(e) => {
                unknown.get_or_insert(format!("cannot read {name} ({e})"));
            }
        }
    }
    for (name, skip_git) in [
        ("objects/info/alternates", false),
        ("objects/info/http-alternates", false),
        ("commondir", true),
        ("info/grafts", false),
        ("annex", false),
    ] {
        match lk(&gd.join(name)) {
            Ok(Lk::Missing) => {}
            Ok(_) => {
                return Shape::No {
                    issue: format!(
                        "{name} exists (shared objects, a moved git directory or other local \
                         state)"
                    ),
                    skip_git,
                }
            }
            Err(e) => {
                unknown.get_or_insert(format!("cannot read {name} ({e})"));
            }
        }
    }
    for name in ["worktrees", "modules"] {
        match lk(&gd.join(name)) {
            Ok(Lk::Missing) => {}
            Ok(Lk::Dir) => match std::fs::read_dir(gd.join(name)) {
                Ok(mut rd) => {
                    if rd.next().is_some() {
                        no_soft!("{name} is not empty");
                    }
                }
                Err(e) => {
                    unknown.get_or_insert(format!("cannot read {name} ({e})"));
                }
            },
            Ok(_) => no_soft!("{name} is not a plain directory"),
            Err(e) => {
                unknown.get_or_insert(format!("cannot read {name} ({e})"));
            }
        }
    }
    match unknown {
        Some(why) => Shape::Unknown(why),
        None => Shape::Fine,
    }
}

fn layout_check(git: &Git, path: &Path, class: Class) -> (Tri, Option<String>) {
    macro_rules! no {
        ($($a:tt)*) => {
            return (Tri::No, Some(format!($($a)*)))
        };
    }
    let mut unknown: Option<String> = None;
    match shape(path, class) {
        Shape::Fine => {}
        Shape::No { issue, .. } => return (Tri::No, Some(issue)),
        Shape::Unknown(why) => unknown = Some(why),
    }
    match pending_state(git, path) {
        Ok(found) if found.is_empty() => {}
        Ok(found) => no!(
            "an unfinished operation or local-only content is present ({})",
            found.join(", ")
        ),
        Err(why) => {
            unknown.get_or_insert(why);
        }
    }
    match git.run(
        path,
        &[
            "for-each-ref",
            // `%(type)` is a tag object's direct target type; `%(*objecttype)` would peel a
            // tag of a tag all the way to the commit and hide it.
            "--format=%(refname) %(objecttype) %(type) %(*objecttype)",
        ],
    ) {
        Ok(out) => {
            for line in out.lines() {
                let mut it = line.split(' ');
                let name = it.next().unwrap_or("");
                let ty = it.next().unwrap_or("");
                let direct = it.next().unwrap_or("");
                let peeled = it.next().unwrap_or("");
                if !plain_ref_name(name) {
                    no!("the ref {name} is outside refs/heads, refs/tags, refs/remotes and refs/stash");
                }
                match (ty, direct) {
                    ("commit", _) | ("tag", "commit") => {}
                    _ => no!(
                        "the ref {name} does not point at a commit (it is a {ty} whose target is \
                         a {direct}, peeling to a {peeled})"
                    ),
                }
            }
        }
        Err(e) => {
            unknown.get_or_insert(note("refs", &e));
        }
    }
    match unknown {
        Some(why) => (Tri::Unknown, Some(why)),
        None => (Tri::Yes, None),
    }
}

/// Names that, present in a worktree's own git directory, show an operation that is not finished.
/// Its state (a rebase todo list, conflict resolutions, the commits a sequence still has to apply)
/// lives in these files and not in any ref, and is deleted with the repo.
const IN_PROGRESS: [&str; 9] = [
    "rebase-merge",
    "rebase-apply",
    "sequencer",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "BISECT_START",
    "BISECT_TERMS",
];

/// Directories of the shared git directory that hold content not recorded in any ref: Git LFS
/// objects and git-annex content that may never have been uploaded.
const LOCAL_CONTENT: [&str; 2] = ["lfs/objects", "annex/objects"];

/// Does `dir` exist and hold at least one entry? `Err` for anything but a plain directory or a
/// missing one: a symlink or an unreadable path is not looked through.
fn non_empty_dir(dir: &Path) -> Result<bool, String> {
    use std::io::ErrorKind::NotFound;
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => std::fs::read_dir(dir)
            .map(|mut rd| rd.next().is_some())
            .map_err(|e| format!("cannot read {}: {e}", dir.display())),
        Ok(_) => Err(format!("{} is not a plain directory", dir.display())),
        Err(e) if e.kind() == NotFound => Ok(false),
        Err(e) => Err(format!("cannot read {}: {e}", dir.display())),
    }
}

/// Unfinished operations of this worktree and local-only LFS or annex content, found by name
/// only (no file is read). `rr-cache` is not listed: it records conflict resolutions that can be
/// derived again, not work.
fn pending_state(git: &Git, path: &Path) -> Result<Vec<String>, String> {
    let absolute = |args: &[&str], what: &str| -> Result<PathBuf, String> {
        let out = git.run(path, args).map_err(|e| note(what, &e))?;
        let p = PathBuf::from(out.trim());
        Ok(if p.is_absolute() { p } else { path.join(p) })
    };
    let gitdir = absolute(&["rev-parse", "--absolute-git-dir"], "git dir")?;
    let common = absolute(&["rev-parse", "--git-common-dir"], "git dir")?;
    let mut found: Vec<String> = Vec::new();
    for name in IN_PROGRESS {
        // `symlink_metadata` succeeds for any kind of entry, including a symlink or a FIFO.
        match std::fs::symlink_metadata(gitdir.join(name)) {
            Ok(_) => found.push(name.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot check for {name}: {e}")),
        }
    }
    for rel in LOCAL_CONTENT {
        let dir = rel.split('/').fold(common.clone(), |p, part| p.join(part));
        if non_empty_dir(&dir)? {
            found.push(rel.to_string());
        }
    }
    Ok(found)
}

/// Is a local config key one that cannot make git run a program of the repo's choosing?
///
/// This is an allowlist: a repository we scan may be an unpacked download, and its `.git/config`
/// is attacker-controlled. Keys (`section.subsection.name`, lowercased section and name) that are
/// not known to be plain data are refused, and the repo is then not inspected at all.
pub fn config_key_is_safe(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let (section, rest) = match key.split_once('.') {
        Some(x) => x,
        None => return false,
    };
    let name = rest.rsplit('.').next().unwrap_or(rest);
    let has_subsection = rest.contains('.');
    match section {
        "core" => matches!(
            name,
            "repositoryformatversion"
                | "filemode"
                | "bare"
                | "logallrefupdates"
                | "ignorecase"
                | "precomposeunicode"
                | "symlinks"
                | "autocrlf"
                | "eol"
                | "safecrlf"
                | "sparsecheckout"
                | "sparsecheckoutcone"
                | "worktree"
                | "longpaths"
                | "protectntfs"
                | "protecthfs"
                | "untrackedcache"
                | "splitindex"
                | "commentchar"
                | "abbrev"
                | "excludesfile"
                | "attributesfile"
                | "quotepath"
                | "trustctime"
                | "checkstat"
                | "preloadindex"
                | "fscache"
        ),
        "remote" => {
            has_subsection
                && matches!(
                    name,
                    "url"
                        | "pushurl"
                        | "fetch"
                        | "push"
                        | "tagopt"
                        | "prune"
                        | "promisor"
                        | "partialclonefilter"
                )
        }
        "branch" => {
            has_subsection
                && matches!(
                    name,
                    "remote" | "merge" | "rebase" | "description" | "pushremote"
                )
        }
        "user" => matches!(name, "name" | "email" | "signingkey" | "useconfigonly"),
        "submodule" => matches!(name, "url" | "path" | "active" | "branch" | "ignore"),
        "url" => has_subsection && matches!(name, "insteadof" | "pushinsteadof"),
        "extensions" => true,
        "init" | "gc" | "pack" | "receive" | "transfer" | "fetch" | "pull" | "push" | "color"
        | "advice" | "gui" | "include" | "includeif" => !has_subsection,
        "commit" | "tag" => name == "gpgsign",
        _ => false,
    }
}

/// Keys of the repo-local config (including included files), or why they could not be read.
fn local_config_keys(git: &crate::git::Git, path: &Path) -> Result<Vec<String>, String> {
    let list = |extra: &[&str]| -> Result<Vec<String>, String> {
        let mut args = vec!["config"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--includes", "--name-only", "--list"]);
        git.run(path, &args)
            .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
            .map_err(|e| note("config", &e))
    };
    let mut keys = list(&["--local"])?;
    if keys
        .iter()
        .any(|k| k.eq_ignore_ascii_case("extensions.worktreeconfig"))
    {
        keys.extend(list(&["--worktree"])?);
    }
    Ok(keys)
}

fn unsafe_key(keys: &[String], whose: &str) -> Result<(), String> {
    match keys.iter().find(|k| !config_key_is_safe(k)) {
        Some(bad) => {
            // Only the section and variable name: the subsection can be a URL carrying a token.
            let shown = config_key_label(bad);
            Err(format!(
                "{whose} can run programs or is not understood ({shown}); \
                 no git command was run in this repo"
            ))
        }
        None => Ok(()),
    }
}

/// Is every config in the repo, and in every submodule git directory under `modules`, free of
/// keys that could run a program? `git status` recurses into submodules and honours their config.
fn check_module_configs(git: &crate::git::Git, cwd: &Path, modules: &Path) -> Result<(), String> {
    use std::io::ErrorKind::NotFound;
    match std::fs::symlink_metadata(modules) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err("the submodule directory .git/modules is not a directory".into()),
        Err(e) if e.kind() == NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read .git/modules: {e}")),
    }
    let mut stack = vec![modules.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        visited += 1;
        if visited > 2000 {
            return Err("too many submodule directories under .git/modules to check".into());
        }
        let is_gitdir = std::fs::symlink_metadata(dir.join("HEAD"))
            .map(|m| m.is_file())
            .unwrap_or(false);
        if is_gitdir {
            for name in ["config", "config.worktree"] {
                let file = dir.join(name);
                match std::fs::symlink_metadata(&file) {
                    Ok(m) if m.is_file() => {
                        let file_s = file.to_string_lossy().into_owned();
                        let out = git
                            .run(
                                cwd,
                                &[
                                    "config",
                                    "--file",
                                    file_s.as_str(),
                                    "--includes",
                                    "--name-only",
                                    "--list",
                                ],
                            )
                            .map_err(|e| note("submodule config", &e))?;
                        let keys: Vec<String> = out.lines().map(|l| l.trim().to_string()).collect();
                        unsafe_key(&keys, "a submodule's config")?;
                    }
                    Ok(_) => return Err("a submodule config is not a regular file".into()),
                    Err(e) if e.kind() == NotFound => {}
                    Err(e) => return Err(format!("cannot read a submodule config: {e}")),
                }
            }
            stack.push(dir.join("modules"));
        } else {
            let rd = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) if e.kind() == NotFound => continue,
                Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
            };
            for entry in rd {
                let entry = entry.map_err(|e| format!("cannot read .git/modules: {e}"))?;
                let ft = entry
                    .file_type()
                    .map_err(|e| format!("cannot read .git/modules: {e}"))?;
                if ft.is_symlink() {
                    return Err("a symlink under .git/modules".into());
                }
                if ft.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
    }
    Ok(())
}

/// Refuse to run git in a repo whose config, or whose submodules' configs, could execute
/// programs, or whose `core.worktree` points at some other directory. Returns the repo's local
/// config keys.
fn check_repo_config(
    git: &crate::git::Git,
    path: &Path,
    class: Class,
) -> Result<Vec<String>, String> {
    let keys = local_config_keys(git, path)?;
    unsafe_key(&keys, "repo config")?;
    if keys.iter().any(|k| k.eq_ignore_ascii_case("core.worktree")) {
        // The effective value: with `extensions.worktreeConfig` a value in `config.worktree`
        // overrides the shared one, and `--local` would not see it. System and global config
        // are switched off, so the only scopes left are the two the allowlist already read.
        let value = git
            .run(path, &["config", "--get", "core.worktree"])
            .map_err(|e| note("config", &e))?;
        let gitdir = git
            .run(path, &["rev-parse", "--absolute-git-dir"])
            .map_err(|e| note("git dir", &e))?;
        let v = PathBuf::from(value.trim());
        let resolved = if v.is_absolute() {
            v
        } else {
            PathBuf::from(gitdir.trim()).join(v)
        };
        if canon(&resolved) != canon(path) {
            return Err(
                "core.worktree points to a different directory than this one; facts would \
                 describe another tree"
                    .into(),
            );
        }
    }
    if class != Class::BareRepo {
        // Whatever the config says, the tree git inspects must be the scanned directory.
        let top = git
            .run(path, &["rev-parse", "--show-toplevel"])
            .map_err(|e| note("work tree", &e))?;
        if canon(Path::new(top.trim())) != canon(path) {
            return Err(
                "git's work tree is not the scanned directory; facts would describe another tree"
                    .into(),
            );
        }
    }
    let common = git
        .run(path, &["rev-parse", "--git-common-dir"])
        .map_err(|e| note("git dir", &e))?;
    let common = PathBuf::from(common.trim());
    let common = if common.is_absolute() {
        common
    } else {
        path.join(common)
    };
    check_module_configs(git, path, &common.join("modules"))?;
    Ok(keys)
}

/// Gitlinks (submodule entries, mode 160000) recorded in an `ls-files -s -z` listing:
/// `(commit, stage, path)`.
fn index_gitlinks(out: &str) -> Vec<(String, String, String)> {
    out.split('\0')
        .filter_map(|e| {
            let (meta, p) = e.split_once('\t')?;
            let mut it = meta.split(' ');
            if it.next()? != "160000" {
                return None;
            }
            let sha = it.next()?.to_string();
            let stage = it.next()?.to_string();
            Some((sha, stage, p.to_string()))
        })
        .collect()
}

/// Gitlinks in an `ls-tree -r -z` listing: path to commit.
fn tree_gitlinks(out: &str) -> std::collections::BTreeMap<String, String> {
    out.split('\0')
        .filter_map(|e| {
            let (meta, p) = e.split_once('\t')?;
            let mut it = meta.split(' ');
            if it.next()? != "160000" {
                return None;
            }
            let _kind = it.next()?;
            Some((p.to_string(), it.next()?.to_string()))
        })
        .collect()
}

/// Does each checked-out submodule sit at the commit the parent's index records, and does the
/// index agree with HEAD about every gitlink? `status --ignore-submodules=all` hides both.
///
/// `Ok(None)` is clean, `Ok(Some(why))` is a difference, `Err` means it could not be decided.
/// A recorded submodule with no checkout (missing or empty directory) is fine; a directory with
/// files but no `.git` is not. The submodule's HEAD is read with the hardened runner after its own
/// config passed the same allowlist as any repo; no status is run inside it.
fn gitlink_state(git: &Git, path: &Path, has_head: bool) -> Result<Option<String>, String> {
    let listing = git
        .run(path, &["ls-files", "-s", "-z"])
        .map_err(|e| note("gitlinks", &e))?;
    let index = index_gitlinks(&listing);
    let head = if has_head {
        let tree = git
            .run(path, &["ls-tree", "-r", "-z", "HEAD"])
            .map_err(|e| note("gitlinks", &e))?;
        tree_gitlinks(&tree)
    } else {
        Default::default()
    };
    if index.len() > 200 {
        return Err("more than 200 submodules; their commits were not compared".into());
    }
    for (sha, stage, sub) in &index {
        if stage != "0" {
            return Ok(Some(format!("submodule {sub} has an unmerged index entry")));
        }
        if head.get(sub) != Some(sha) {
            return Ok(Some(format!(
                "the index records submodule {sub} differently from HEAD (a staged change)"
            )));
        }
        let dir = path.join(sub);
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read submodule {sub}: {e}")),
            Ok(m) if !m.is_dir() => {
                return Ok(Some(format!("submodule {sub} is not a directory")));
            }
            Ok(_) => {}
        }
        let empty = std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot read submodule {sub}: {e}"))?
            .next()
            .is_none();
        if empty {
            continue; // recorded, not checked out
        }
        match std::fs::symlink_metadata(dir.join(".git")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(format!(
                    "the path of submodule {sub} is a plain directory with files, not a checkout"
                )));
            }
            Err(e) => return Err(format!("cannot read submodule {sub}: {e}")),
            Ok(_) => {}
        }
        check_repo_config(git, &dir, Class::Worktree)
            .map_err(|why| format!("submodule {sub}: {why}"))?;
        let at = git
            .run(&dir, &["rev-parse", "--verify", "HEAD"])
            .map_err(|e| note("submodule HEAD", &e))?;
        if at.trim() != sha {
            return Ok(Some(format!(
                "submodule {sub} is checked out at another commit than the index records"
            )));
        }
    }
    if let Some(gone) = head.keys().find(|p| !index.iter().any(|(_, _, q)| q == *p)) {
        return Ok(Some(format!(
            "the index no longer records submodule {gone} (a staged removal)"
        )));
    }
    Ok(None)
}

/// Result of inspecting one repo.
pub struct RepoCheck {
    pub info: RepoInfo,
    /// Regenerable directories inside the repo that git does not prove are ignored, untracked
    /// build output: `(path, reason)`.
    pub unverified_artifacts: Vec<(PathBuf, String)>,
}

fn all_unknown(reason: String, unverified: &[PathBuf]) -> RepoCheck {
    let reason = diag(&reason);
    let why = reason.clone();
    RepoCheck {
        info: RepoInfo {
            layout_issue: Some(reason.clone()),
            notes: vec![reason],
            ..RepoInfo::default()
        },
        unverified_artifacts: unverified
            .iter()
            .map(|p| (p.clone(), format!("cannot verify against git: {why}")))
            .collect(),
    }
}

/// Gather facts for one repo, worktree or bare repo at `path`.
///
/// `regen_paths` are the regenerable and cache directories inside it that the scan found
/// removable; ignored files under them do not count as unique, and each is checked to be ignored
/// by git.
pub fn collect_repo_info(
    cfg: &FactsConfig,
    path: &Path,
    class: Class,
    regen_paths: &[PathBuf],
) -> RepoCheck {
    let git = &cfg.git;
    // A damaged or unusual structure is found by `lstat` alone and git is not run in it at all
    // (a repo that is only unusual, such as a `.git` file or shared objects, is still read and
    // asks for its layout).
    if let Shape::No {
        issue,
        skip_git: true,
    } = shape(path, class)
    {
        let mut rc = all_unknown(
            format!("layout: {issue}; no git command was run in this repo"),
            regen_paths,
        );
        rc.info.facts.plain_layout = Tri::No;
        rc.info.layout_issue = Some(diag(&issue));
        return rc;
    }
    // The repo's own config is untrusted. Refuse to run anything else if it could make git
    // execute a program (filter drivers, gpg.program, ...).
    let config_keys = match check_repo_config(git, path, class) {
        Ok(keys) => keys,
        Err(why) => return all_unknown(why, regen_paths),
    };
    let mut unverified: Vec<(PathBuf, String)> = Vec::new();
    let mut info = RepoInfo::default();
    let mut facts = Facts::default();
    let mut notes = Notes::default();

    // Remotes and the network predicate.
    let remotes: Option<Vec<Remote>> = match git.run(path, &["remote", "-v"]) {
        Ok(out) => Some(parse_remotes(&out, cfg)),
        Err(e) => {
            notes.push(note("remotes", &e));
            None
        }
    };
    let network_names: Option<Vec<String>> = remotes.as_ref().map(|rs| {
        rs.iter()
            .filter(|r| r.network)
            .map(|r| r.name.clone())
            .collect()
    });
    facts.has_network_remote = match &network_names {
        Some(names) => Tri::from_bool(!names.is_empty()),
        None => Tri::Unknown,
    };

    // The worktree's own HEAD, including a detached HEAD.
    let head: Option<String> = match git.run(path, &["rev-parse", "--verify", "HEAD"]) {
        Ok(out) => Some(out.trim().to_string()),
        Err(e) => {
            notes.push(note("HEAD", &e));
            None
        }
    };
    facts.head_on_remote = match (&head, &network_names) {
        (Some(h), Some(names)) => {
            if names.is_empty() {
                Tri::No
            } else {
                let args = [
                    "for-each-ref",
                    "--contains",
                    h.as_str(),
                    "--format=%(refname)",
                    "refs/remotes",
                ];
                match git.run(path, &args) {
                    Ok(out) => Tri::from_bool(out.lines().any(|l| {
                        names
                            .iter()
                            .any(|n| l.starts_with(&format!("refs/remotes/{n}/")))
                    })),
                    Err(e) => {
                        notes.push(note("head_on_remote", &e));
                        Tri::Unknown
                    }
                }
            }
        }
        _ => Tri::Unknown,
    };
    info.head_commit = head;

    // Every local branch and tag is contained in a network remote ref.
    facts.all_branches_on_remote = match &network_names {
        Some(names) if names.is_empty() => Tri::No,
        Some(names) => {
            // Every local ref (branches, tags, notes, replace refs, ...) except the stash, which
            // has its own fact. HEAD is judged by `head_on_remote`.
            let pats: Vec<String> = names.iter().map(|n| format!("--remotes={n}")).collect();
            let mut args: Vec<&str> = vec![
                "rev-list",
                "-n",
                "1",
                "--exclude=refs/stash",
                "--glob=refs/*",
                "--not",
            ];
            args.extend(pats.iter().map(|s| s.as_str()));
            match git.run(path, &args) {
                Ok(out) if !out.trim().is_empty() => Tri::No,
                Ok(_) => {
                    // Tag names (and tag objects) are not visible in remote-tracking refs, so a
                    // local tag cannot be shown to exist on the remote without `ls-remote`.
                    match git.run(
                        path,
                        &[
                            "for-each-ref",
                            "--count=1",
                            "--format=%(refname)",
                            "refs/tags",
                        ],
                    ) {
                        Ok(tags) if tags.trim().is_empty() => Tri::Yes,
                        Ok(_) => {
                            notes.push(
                                "all_branches_on_remote: the repo has tags, whose presence on the \
                                 remote cannot be verified from local refs",
                            );
                            Tri::Unknown
                        }
                        Err(e) => {
                            notes.push(note("all_branches_on_remote", &e));
                            Tri::Unknown
                        }
                    }
                }
                Err(e) => {
                    notes.push(note("all_branches_on_remote", &e));
                    Tri::Unknown
                }
            }
        }
        None => Tri::Unknown,
    };

    // Commits that only a reflog still reaches (an unpushed commit after `reset --hard`, an amended
    // or rebased-away commit). No ref names them, so every check above can read as saved while
    // the work is recoverable only from the reflog, which goes with the repo.
    facts.reflog_saved = match &network_names {
        // No network remote can hold anything.
        Some(names) if names.is_empty() => Tri::No,
        // An unborn HEAD (no commit yet) is not shown to be saved: HEAD is only unknown when
        // `rev-parse` failed, and an empty reflog would otherwise read as vacuously saved.
        Some(names) if info.head_commit.is_some() => {
            // `rev-list` only reads refs, reflogs and objects (git-rev-list(1)): it takes no lock
            // and writes nothing. `--reflog` adds every reflog entry (HEAD of this worktree and
            // all refs of the shared repo) to the revisions; `--not --remotes=<n>` removes
            // whatever a network remote's tracking refs contain; `-n 1` stops at the first
            // survivor, and the command timeout bounds the walk (a timeout is `Unknown`).
            let pats: Vec<String> = names.iter().map(|n| format!("--remotes={n}")).collect();
            let mut args: Vec<&str> = vec!["rev-list", "-n", "1", "--reflog", "--not"];
            args.extend(pats.iter().map(|s| s.as_str()));
            match git.run(path, &args) {
                Ok(out) => Tri::from_bool(out.trim().is_empty()),
                Err(e) => {
                    notes.push(note("reflog_saved", &e));
                    Tri::Unknown
                }
            }
        }
        _ => Tri::Unknown,
    };

    // Unfinished operations and local-only content that no ref records.
    facts.no_pending_state = match pending_state(git, path) {
        Ok(found) if found.is_empty() => Tri::Yes,
        Ok(found) => {
            notes.push(format!(
                "no_pending_state: found {} in the git directory (an unfinished rebase, \
                 cherry-pick, revert or bisect, or Git LFS or git-annex content); it holds work \
                 that no ref records and cannot be shown to be saved",
                found.join(", ")
            ));
            Tri::Unknown
        }
        Err(why) => {
            notes.push(format!("no_pending_state: {why}"));
            Tri::Unknown
        }
    };

    // Stash.
    facts.stash_empty = match git.run(path, &["stash", "list"]) {
        Ok(out) => Tri::from_bool(out.trim().is_empty()),
        Err(e) => {
            notes.push(note("stash_empty", &e));
            Tri::Unknown
        }
    };

    // Working tree facts need a working tree.
    if class == Class::BareRepo {
        notes.push("tree_clean and no_unique_ignored: a bare repo has no working tree");
    } else {
        let status_args = [
            "status",
            "--porcelain",
            "--untracked-files=all",
            // Each checked-out submodule is its own node with its own config check; a recursive
            // status would run inside it using its config.
            "--ignore-submodules=all",
        ];
        facts.tree_clean = match git.run(path, &status_args) {
            Ok(out) if !out.trim().is_empty() => Tri::No,
            Ok(_) => {
                // `status` hides edits to files marked skip-worktree or assume-unchanged.
                match git.run(path, &["ls-files", "-v", "-z"]) {
                    Ok(listing) => {
                        let hidden = listing
                            .split('\0')
                            .filter(|e| {
                                matches!(e.chars().next(),
                                    Some(c) if c.is_ascii_lowercase() || c == 'S')
                            })
                            .count();
                        if hidden == 0 {
                            // `status` runs with --ignore-submodules=all, which also hides a
                            // submodule checked out at another commit than the index records.
                            match gitlink_state(git, path, info.head_commit.is_some()) {
                                Ok(None) => Tri::Yes,
                                Ok(Some(why)) => {
                                    notes.push(format!("tree_clean: {why}"));
                                    Tri::No
                                }
                                Err(why) => {
                                    notes.push(format!("tree_clean: {why}"));
                                    Tri::Unknown
                                }
                            }
                        } else {
                            notes.push(format!(
                                "tree_clean: {hidden} file(s) are marked skip-worktree or \
                                 assume-unchanged, so local edits would be invisible"
                            ));
                            Tri::No
                        }
                    }
                    Err(e) => {
                        notes.push(note("tree_clean", &e));
                        Tri::Unknown
                    }
                }
            }
            Err(e) => {
                notes.push(note("tree_clean", &e));
                Tri::Unknown
            }
        };
        let ignored_args = [
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ];
        facts.no_unique_ignored = match git.run(path, &ignored_args) {
            Ok(out) => {
                let entries: Vec<&str> = out
                    .split('\0')
                    .map(|e| e.trim_end_matches('/'))
                    .filter(|e| !e.is_empty())
                    .collect();
                let mut regen: Vec<String> = Vec::new();
                for p in regen_paths {
                    let rel = match rel_slash(p, path) {
                        Some(r) => r,
                        None => {
                            // Not below the repo as spelled: git cannot be asked about it.
                            unverified.push((
                                p.clone(),
                                "cannot relate this directory to the repository path, so it is \
                                 not verified to be ignored"
                                    .to_string(),
                            ));
                            continue;
                        }
                    };
                    // A generated directory is wholly ignored and untracked, so git lists it
                    // as one ignored entry (or inside one). Anything else may hold source.
                    let ignored = entries
                        .iter()
                        .any(|e| rel == *e || rel.starts_with(&format!("{e}/")));
                    if ignored {
                        regen.push(rel);
                    } else {
                        unverified.push((
                            p.clone(),
                            "git does not show this directory as wholly ignored: it is tracked, \
                             partly tracked or not ignored, so it may hold source"
                                .to_string(),
                        ));
                    }
                }
                let unique: Vec<&str> = entries
                    .iter()
                    .copied()
                    .filter(|e| {
                        !regen
                            .iter()
                            .any(|r| *e == r.as_str() || e.starts_with(&format!("{r}/")))
                    })
                    .collect();
                if unique.is_empty() {
                    Tri::Yes
                } else {
                    let shown: Vec<&str> = unique.iter().take(5).copied().collect();
                    notes.push(format!(
                        "{} ignored path(s) outside regenerable directories, e.g. {}",
                        unique.len(),
                        shown.join(", ")
                    ));
                    Tri::No
                }
            }
            Err(e) => {
                notes.push(note("no_unique_ignored", &e));
                for p in regen_paths {
                    unverified.push((
                        p.clone(),
                        "cannot verify against git that this directory is ignored".to_string(),
                    ));
                }
                Tri::Unknown
            }
        };
    }

    // Dependents: linked worktrees. (Alternates clones are linked by the scanner.)
    facts.no_dependents = match git.run(path, &["worktree", "list", "--porcelain"]) {
        Ok(out) => {
            let entries: Vec<String> = out
                .lines()
                .filter_map(|l| l.strip_prefix("worktree "))
                .map(|s| s.to_string())
                .collect();
            let me = canon(path);
            let others: Vec<String> = entries
                .iter()
                .filter(|e| canon(Path::new(e.as_str())) != me)
                .cloned()
                .collect();
            let main_is_me = entries
                .first()
                .map(|e| canon(Path::new(e.as_str())) == me)
                .unwrap_or(false);
            if class == Class::Worktree && !main_is_me {
                // A linked worktree: it belongs to the first listed worktree, and nothing
                // can depend on it.
                info.parent_repo = entries.first().map(|e| diag(e));
                Tri::Yes
            } else {
                let has_none = others.is_empty();
                info.worktrees = others.iter().map(|o| diag(o)).collect();
                Tri::from_bool(has_none)
            }
        }
        Err(e) => {
            notes.push(note("no_dependents", &e));
            Tri::Unknown
        }
    };

    // `extensions.preciousObjects = true` is the repository saying that others borrow its
    // objects (it stops git from ever deleting them). A linked worktree only shares the setting
    // of the repository it belongs to; nothing depends on the worktree itself.
    let declared = config_keys
        .iter()
        .any(|k| k.eq_ignore_ascii_case("extensions.preciousobjects"));
    if declared && info.parent_repo.is_none() {
        match git.run(
            path,
            &[
                "config",
                "--type=bool",
                "--get",
                "extensions.preciousObjects",
            ],
        ) {
            Ok(v) if v.trim() == "true" => {
                info.precious_objects = true;
                facts.no_dependents = Tri::No;
                notes.push(
                    "no_dependents: this repository sets extensions.preciousObjects, which \
                     declares that other repositories borrow its objects",
                );
            }
            Ok(_) => {}
            Err(e) => {
                if facts.no_dependents == Tri::Yes {
                    facts.no_dependents = Tri::Unknown;
                }
                notes.push(note("no_dependents (extensions.preciousObjects)", &e));
            }
        }
    }

    // Activity evidence: newest commit on any ref.
    info.last_commit_time = git
        .run(path, &["log", "-1", "--all", "--format=%ct"])
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());

    let (plain, issue) = layout_check(git, path, class);
    facts.plain_layout = plain;
    info.layout_issue = issue.map(|i| diag(&i));
    info.facts = facts;
    info.notes = notes.0;
    info.remotes = remotes.unwrap_or_default();
    RepoCheck {
        info,
        unverified_artifacts: unverified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lfs_keys_are_not_allowed() {
        // An LFS repo defines filter.lfs.* (a program) and is reported unknown; lfs.* itself
        // is not allowlisted either.
        for k in [
            "lfs.url",
            "lfs.concurrenttransfers",
            "filter.lfs.clean",
            "lfs.x.url",
        ] {
            assert!(!config_key_is_safe(k), "{k}");
        }
        assert!(config_key_is_safe("remote.origin.url"));
    }

    #[test]
    fn network_predicate() {
        for u in [
            "https://example.invalid/x.git",
            "ssh://git@example.invalid/x.git",
            "git@example.invalid:o/r.git",
            "example.invalid:o/r.git",
            "git://example.invalid/r.git",
        ] {
            assert!(is_network_url(u), "{u} should be network");
        }
        for u in [
            "/srv/git/r.git",
            "../r.git",
            "./r.git",
            "r.git",
            "~/r.git",
            "file:///srv/r.git",
            "C:\\repos\\r.git",
            "C:/repos/r.git",
            "/srv/backup.bundle",
            "https://example.invalid/backup.bundle",
            "",
        ] {
            assert!(!is_network_url(u), "{u:?} should not be network");
        }
    }
}
