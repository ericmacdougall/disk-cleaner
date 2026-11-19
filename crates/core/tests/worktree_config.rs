//! Regression tests for GoodQA #16418: `extensions.worktreeConfig` without a `config.worktree`.
//!
//! git creates `config.worktree` only when something is first written with `--worktree`, so a
//! repository with the extension on and no such file is normal. The config allowlist ran
//! `git config --worktree --list`, which exits 128 when the file does not exist; the repository
//! then had every fact unknown (71 repos and worktrees on the first real run). A missing file is
//! no worktree-scoped keys. A file that exists is read and judged as before, and one that is not
//! a regular file (a symlink, a directory) is refused, as `.git/modules` configs are. Every
//! fixture is built with real git commands.

mod common;

use common::*;
use disk_cleaner_core::{ScanResult, Tri};
use std::fs;
use std::path::{Path, PathBuf};

/// The repo's layout issue and notes, as one text.
fn issues(res: &ScanResult, repo: &Path) -> String {
    let info = node(res, repo).repo.as_ref().expect("repo info");
    let mut text = info.layout_issue.clone().unwrap_or_default();
    for n in &info.notes {
        text.push('\n');
        text.push_str(n);
    }
    text
}

/// The config allowlist let git run: the facts that need git are known, and no note says a
/// config could not be read or was refused.
fn assert_inspected(res: &ScanResult, repo: &Path) {
    let f = facts_of(res, repo);
    let why = issues(res, repo);
    assert_eq!(f.has_network_remote, Tri::Yes, "{}: {why}", repo.display());
    assert_eq!(f.head_on_remote, Tri::Yes, "{}: {why}", repo.display());
    assert_eq!(f.tree_clean, Tri::Yes, "{}: {why}", repo.display());
    for bad in ["unable to read", "can run programs", "not a regular file"] {
        assert!(
            !why.contains(bad),
            "{}: no config problem expected: {why}",
            repo.display()
        );
    }
}

/// The repo was refused before git ran: every fact is unknown, and the reason contains `want`.
/// `plain_layout` may be `no` instead: the scan's own walk sets it for a symlink in the git
/// directory whatever the config check found, and a `no` never offers anything.
fn assert_refused(res: &ScanResult, repo: &Path, want: &str) {
    let f = facts_of(res, repo);
    let why = issues(res, repo);
    let unknown = |(name, t): &(&str, Tri)| {
        *t == Tri::Unknown || (*name == "plain_layout" && *t == Tri::No)
    };
    assert!(
        f.entries().iter().all(unknown),
        "{}: every fact must be unknown: {f:?} ({why})",
        repo.display()
    );
    assert!(
        why.contains(want),
        "{}: the reason must contain {want:?}: {why}",
        repo.display()
    );
}

/// A pushed repo `app` with `extensions.worktreeConfig` on (through `git config`, as a person
/// would) and no `config.worktree`.
fn extension_repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "app");
    git(&repo, &["config", "extensions.worktreeConfig", "true"]);
    assert!(!repo.join(".git").join("config.worktree").exists());
    (tmp, root, repo)
}

/// A linked worktree `wt` of `repo`, detached at the pushed commit; returns it and its own git
/// directory.
fn linked(root: &Path, repo: &Path) -> (PathBuf, PathBuf) {
    let wt = root.join("wt");
    git(
        repo,
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    let gd = repo.join(".git").join("worktrees").join("wt");
    assert!(gd.join("HEAD").is_file(), "{}", gd.display());
    (wt, gd)
}

/// Append raw text to a config file.
fn append(file: &Path, text: &str) {
    let mut s = fs::read_to_string(file).unwrap_or_default();
    s.push_str(text);
    fs::write(file, s).unwrap();
}

// ---------------------------------------------------------------- the defect

#[test]
fn missing_worktree_config_in_the_main_worktree_means_no_worktree_keys() {
    let (_tmp, root, repo) = extension_repo();
    let res = scan_with(&root, network_everywhere());
    assert_inspected(&res, &repo);
}

#[test]
fn missing_worktree_config_in_a_linked_worktree_means_no_worktree_keys() {
    let (_tmp, root, repo) = extension_repo();
    let (wt, gd) = linked(&root, &repo);
    assert!(!gd.join("config.worktree").exists());
    let res = scan_with(&root, network_everywhere());
    assert_inspected(&res, &wt);
    assert_inspected(&res, &repo);
}

#[test]
fn the_extension_spelled_in_another_case_with_the_file_missing_means_no_worktree_keys() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "app");
    append(
        &repo.join(".git").join("config"),
        "[Extensions]\n\tWorkTreeCONFIG = true\n",
    );
    let res = scan_with(&root, network_everywhere());
    assert_inspected(&res, &repo);
}

#[test]
fn absent_in_the_main_worktree_and_present_in_a_linked_one_reads_each_as_it_is() {
    let (_tmp, root, repo) = extension_repo();
    let (wt, gd) = linked(&root, &repo);
    git(&wt, &["config", "--worktree", "core.abbrev", "12"]);
    assert!(gd.join("config.worktree").is_file());
    assert!(!repo.join(".git").join("config.worktree").exists());
    let res = scan_with(&root, network_everywhere());
    assert_inspected(&res, &repo);
    assert_inspected(&res, &wt);
}

// ---------------------------------------------------------------- an existing file is still judged

#[test]
fn a_linked_worktrees_own_config_worktree_with_an_unsafe_key_is_refused() {
    let (_tmp, root, repo) = extension_repo();
    let (wt, gd) = linked(&root, &repo);
    git(&wt, &["config", "--worktree", "core.pager", "cat"]);
    assert!(gd.join("config.worktree").is_file());
    let res = scan_with(&root, network_everywhere());
    assert_refused(&res, &wt, "can run programs");
    // The main worktree does not read the linked worktree's file.
    assert_inspected(&res, &repo);
}

#[test]
fn the_extension_in_another_case_still_reads_an_existing_file() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "app");
    append(
        &repo.join(".git").join("config"),
        "[EXTENSIONS]\n\tworktreeconfig = true\n",
    );
    fs::write(
        repo.join(".git").join("config.worktree"),
        "[core]\n\tpager = cat\n",
    )
    .unwrap();
    let res = scan_with(&root, network_everywhere());
    assert_refused(&res, &repo, "can run programs");
}

/// `core.bare` in the main worktree's file: git then sees no work tree, and the scanned
/// directory is refused as before. (`core.worktree` there is gate3's
/// `j2_worktree_config_overriding_core_worktree_makes_every_fact_unknown`.)
#[test]
fn core_bare_in_the_main_worktrees_config_worktree_is_still_refused() {
    let (_tmp, root, repo) = extension_repo();
    git(&repo, &["config", "--worktree", "core.bare", "true"]);
    assert!(repo.join(".git").join("config.worktree").is_file());
    let res = scan_with(&root, network_everywhere());
    assert_refused(&res, &repo, "work tree");
}

#[test]
fn control_a_safe_key_in_the_main_worktrees_config_worktree_is_read() {
    let (_tmp, root, repo) = extension_repo();
    git(&repo, &["config", "--worktree", "core.abbrev", "12"]);
    assert!(repo.join(".git").join("config.worktree").is_file());
    let res = scan_with(&root, network_everywhere());
    assert_inspected(&res, &repo);
}

// ---------------------------------------------------------------- not a regular file

#[test]
fn a_config_worktree_that_is_a_directory_is_refused() {
    let (_tmp, root, repo) = extension_repo();
    fs::create_dir(repo.join(".git").join("config.worktree")).unwrap();
    let res = scan_with(&root, network_everywhere());
    assert_refused(&res, &repo, "config.worktree");
}

#[test]
fn a_linked_worktrees_config_worktree_that_is_a_directory_is_refused() {
    let (_tmp, root, repo) = extension_repo();
    let (wt, gd) = linked(&root, &repo);
    fs::create_dir(gd.join("config.worktree")).unwrap();
    let res = scan_with(&root, network_everywhere());
    assert_refused(&res, &wt, "config.worktree");
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A symlink to a file git would read as harmless is still not read: what it points at can
    /// change between the check and the next git command.
    #[test]
    fn a_config_worktree_that_is_a_symlink_is_refused() {
        let (tmp, root, repo) = extension_repo();
        let target = tmp.path().join("elsewhere.config");
        fs::write(&target, "[core]\n\tabbrev = 12\n").unwrap();
        symlink(&target, repo.join(".git").join("config.worktree")).unwrap();
        let res = scan_with(&root, network_everywhere());
        assert_refused(&res, &repo, "config.worktree");
    }

    /// A dangling symlink is not a missing file.
    #[test]
    fn a_config_worktree_that_is_a_dangling_symlink_is_refused() {
        let (tmp, root, repo) = extension_repo();
        symlink(
            tmp.path().join("no-such-file"),
            repo.join(".git").join("config.worktree"),
        )
        .unwrap();
        let res = scan_with(&root, network_everywhere());
        assert_refused(&res, &repo, "config.worktree");
    }

    #[test]
    fn a_linked_worktrees_config_worktree_that_is_a_symlink_is_refused() {
        let (tmp, root, repo) = extension_repo();
        let (wt, gd) = linked(&root, &repo);
        let target = tmp.path().join("elsewhere.config");
        fs::write(&target, "[core]\n\tabbrev = 12\n").unwrap();
        symlink(&target, gd.join("config.worktree")).unwrap();
        let res = scan_with(&root, network_everywhere());
        assert_refused(&res, &wt, "config.worktree");
    }
}

// ---------------------------------------------------------------- git uses another git directory

/// A `.git` directory git does not accept (its `HEAD` names no ref) inside a repo whose
/// `config.worktree` holds an unsafe key: git walks up and reads the outer repo's config,
/// `config.worktree` included. That this directory's own `.git/config.worktree` is missing must
/// not let the outer file go unjudged: the refusal is the config's, not a later check's.
#[test]
fn a_missing_file_where_git_reads_another_git_directorys_file_is_refused() {
    let (_tmp, root, remotes) = workspace();
    let outer = pushed_repo(&root, &remotes, "outer");
    git(&outer, &["config", "extensions.worktreeConfig", "true"]);
    git(&outer, &["config", "--worktree", "core.pager", "cat"]);
    let inner = outer.join("inner");
    let gd = inner.join(".git");
    for d in ["refs/heads", "refs/tags", "objects/info", "objects/pack"] {
        fs::create_dir_all(gd.join(d)).unwrap();
    }
    fs::write(gd.join("HEAD"), "ref: nowhere\n").unwrap();
    fs::write(gd.join("config"), "[core]\n\trepositoryformatversion = 0\n").unwrap();
    assert!(!gd.join("config.worktree").exists());
    let res = scan_with(&root, network_everywhere());
    let why = issues(&res, &inner);
    assert!(
        !why.contains("work tree is not the scanned directory"),
        "refused only after git ran with an unjudged config: {why}"
    );
    assert_refused(&res, &inner, "config");
}
