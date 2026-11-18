//! Repo safety facts (design section 3), gathered with read-only git commands.
//!
//! Every fact is `Yes`, `No` or `Unknown`. Any git failure, timeout or unparseable output is
//! `Unknown`; it is never promoted to the safe value. Remote URLs lose embedded credentials
//! before they are stored.

use crate::git::{Git, GitError};
use crate::model::{Class, Facts, Remote, RepoInfo, Tri};
use crate::redact::{redact_text, strip_url_credentials};
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
    redact_text(&format!("{what}: {e}"))
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
            name: name.to_string(),
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
        | "advice" | "gui" | "lfs" | "include" | "includeif" => !has_subsection || section == "lfs",
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
    if keys.iter().any(|k| k.eq_ignore_ascii_case("extensions.worktreeconfig")) {
        keys.extend(list(&["--worktree"])?);
    }
    Ok(keys)
}

/// Result of inspecting one repo.
pub struct RepoCheck {
    pub info: RepoInfo,
    /// Regenerable directories inside the repo that git does not prove are ignored, untracked
    /// build output: `(path, reason)`.
    pub unverified_artifacts: Vec<(PathBuf, String)>,
}

fn all_unknown(reason: String, unverified: &[PathBuf]) -> RepoCheck {
    let why = reason.clone();
    RepoCheck {
        info: RepoInfo {
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
    // The repo's own config is untrusted. Refuse to run anything else if it could make git
    // execute a program (filter drivers, gpg.program, ...).
    match local_config_keys(git, path) {
        Err(e) => return all_unknown(e, regen_paths),
        Ok(keys) => {
            if let Some(bad) = keys.iter().find(|k| !config_key_is_safe(k)) {
                let shown: String = bad.chars().take(80).collect();
                return all_unknown(
                    format!(
                        "repo config can run programs or is not understood ({shown}); \
                         no git command was run in it"
                    ),
                    regen_paths,
                );
            }
        }
    }
    let mut unverified: Vec<(PathBuf, String)> = Vec::new();
    let mut info = RepoInfo::default();
    let mut facts = Facts::default();
    let mut notes: Vec<String> = Vec::new();

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
                        &["for-each-ref", "--count=1", "--format=%(refname)", "refs/tags"],
                    ) {
                        Ok(tags) if tags.trim().is_empty() => Tri::Yes,
                        Ok(_) => {
                            notes.push(
                                "all_branches_on_remote: the repo has tags, whose presence on the \
                                 remote cannot be verified from local refs"
                                    .to_string(),
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
        notes.push("tree_clean and no_unique_ignored: a bare repo has no working tree".into());
    } else {
        let status_args = [
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
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
                            Tri::Yes
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
                        None => continue,
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
                info.parent_repo = entries.first().cloned();
                Tri::Yes
            } else {
                let has_none = others.is_empty();
                info.worktrees = others;
                Tri::from_bool(has_none)
            }
        }
        Err(e) => {
            notes.push(note("no_dependents", &e));
            Tri::Unknown
        }
    };

    // Activity evidence: newest commit on any ref.
    info.last_commit_time = git
        .run(path, &["log", "-1", "--all", "--format=%ct"])
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());

    info.facts = facts;
    info.notes = notes;
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
