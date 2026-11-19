//! Regression tests for the sixth merge-gate review of PR #2: repositories that share an object
//! directory (M1) and alternates files written in git's full syntax (M2).
#![cfg(unix)]

mod common;

use common::*;
use disk_cleaner_core::{Action, Tri};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

fn not_removing(a: Action) -> bool {
    !matches!(a, Action::Archive | Action::RemoveRegenerable)
}

/// A repo in `root` whose `.git/objects` is a symlink to `target_objects`, with one unpushed
/// commit (whose objects therefore land in the target's directory).
fn repo_with_shared_objects(root: &Path, name: &str, target_objects: &Path) -> PathBuf {
    let b = root.join(name);
    fs::create_dir_all(&b).unwrap();
    git(&b, &["init", "-q", "-b", "main"]);
    let objs = b.join(".git").join("objects");
    fs::remove_dir_all(&objs).unwrap();
    symlink(target_objects, &objs).unwrap();
    write(&b.join("f.txt"), &format!("work of {name}\n"));
    git(&b, &["add", "f.txt"]);
    git(&b, &["commit", "-q", "-m", "unpushed work"]);
    b
}

fn dependents(res: &disk_cleaner_core::ScanResult, p: &Path) -> Vec<String> {
    node(res, p).repo.as_ref().unwrap().dependents.clone()
}

// ---------------------------------------------------------------- M1

#[test]
fn m1_two_repos_sharing_one_object_directory_depend_on_each_other() {
    let (_tmp, root, remotes) = workspace();
    let a = pushed_repo(&root, &remotes, "a");
    let b = repo_with_shared_objects(&root, "b", &a.join(".git").join("objects"));
    let res = scan_with(&root, network_everywhere());
    assert_eq!(facts_of(&res, &a).no_dependents, Tri::No);
    assert_eq!(facts_of(&res, &b).no_dependents, Tri::No);
    assert!(not_removing(action_for(&res, &a)), "{:?}", action_for(&res, &a));
    assert!(dependents(&res, &a).iter().any(|d| d.ends_with("/b")));
    assert!(dependents(&res, &b).iter().any(|d| d.ends_with("/a")));
}

#[test]
fn m1_three_repos_sharing_one_object_directory_all_report_the_others() {
    let (_tmp, root, remotes) = workspace();
    let a = pushed_repo(&root, &remotes, "a");
    let objs = a.join(".git").join("objects");
    let b = repo_with_shared_objects(&root, "b", &objs);
    let c = repo_with_shared_objects(&root, "c", &objs);
    let res = scan_with(&root, network_everywhere());
    for r in [&a, &b, &c] {
        assert_eq!(facts_of(&res, r).no_dependents, Tri::No, "{}", r.display());
        assert_eq!(dependents(&res, r).len(), 2, "{}", r.display());
    }
}

#[test]
fn m1_an_objects_symlink_to_a_place_outside_the_scan_makes_every_repo_ask() {
    let (tmp, root, remotes) = workspace();
    let other = pushed_repo(&root, &remotes, "other");
    let ext = tmp.path().join("ext");
    init_repo(&ext);
    let _b = repo_with_shared_objects(&root, "b", &ext.join(".git").join("objects"));
    let res = scan_with(&root, network_everywhere());
    assert_eq!(facts_of(&res, &other).no_dependents, Tri::Unknown);
    assert_eq!(action_for(&res, &other), Action::Ask);
}

#[test]
fn m1_a_symlinked_objects_info_directory_into_another_repo_is_a_dependency() {
    let (_tmp, root, remotes) = workspace();
    let a = pushed_repo(&root, &remotes, "a");
    let b = root.join("b");
    fs::create_dir_all(&b).unwrap();
    git(&b, &["init", "-q", "-b", "main"]);
    let info = b.join(".git").join("objects").join("info");
    fs::remove_dir_all(&info).unwrap();
    symlink(a.join(".git").join("objects").join("info"), &info).unwrap();
    let res = scan_with(&root, network_everywhere());
    assert!(not_removing(action_for(&res, &a)), "{:?}", action_for(&res, &a));
}

#[test]
fn m1_a_commondir_file_pointing_at_another_repo_is_a_dependency() {
    let (_tmp, root, remotes) = workspace();
    let a = pushed_repo(&root, &remotes, "a");
    let b = root.join("b");
    fs::create_dir_all(&b).unwrap();
    git(&b, &["init", "-q", "-b", "main"]);
    write(&b.join(".git").join("commondir"), "../../a/.git\n");
    let res = scan_with(&root, network_everywhere());
    assert!(not_removing(action_for(&res, &a)), "{:?}", action_for(&res, &a));
}

#[test]
fn m1_control_two_unrelated_repos_are_unaffected() {
    let (_tmp, root, remotes) = workspace();
    let a = pushed_repo(&root, &remotes, "a");
    let b = pushed_repo(&root, &remotes, "b");
    let res = scan_with(&root, network_everywhere());
    assert_eq!(action_for(&res, &a), Action::Archive);
    assert_eq!(action_for(&res, &b), Action::Archive);
}

// ---------------------------------------------------------------- M2

/// `git clone --shared` of `provider` into `root/name`; returns the borrower and the path of its
/// alternates file.
fn shared_clone(root: &Path, provider: &Path, name: &str) -> (PathBuf, PathBuf) {
    let b = root.join(name);
    git(
        root,
        &[
            "clone",
            "-q",
            "--shared",
            provider.to_str().unwrap(),
            b.to_str().unwrap(),
        ],
    );
    let alt = b.join(".git").join("objects").join("info").join("alternates");
    assert!(alt.exists(), "the clone must have an alternates file");
    (b, alt)
}

/// The object must still be readable by the borrower: the rewritten file is live.
fn assert_borrower_reads_provider_objects(borrower: &Path, provider: &Path) {
    let head = git(provider, &["rev-parse", "HEAD"]);
    git(borrower, &["cat-file", "-e", &head]);
}

fn c_quote(p: &str) -> String {
    let mut out = String::from("\"");
    for &b in p.as_bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push('"');
    out
}

#[test]
fn m2_a_quoted_alternates_entry_with_a_space_and_a_non_ascii_name_is_followed() {
    let (_tmp, root, remotes) = workspace();
    let prov = pushed_repo(&root, &remotes, "pro vider \u{e9}");
    let (b, alt) = shared_clone(&root, &prov, "borrower");
    let objs = fs::canonicalize(prov.join(".git").join("objects")).unwrap();
    fs::write(&alt, format!("{}\n", c_quote(objs.to_str().unwrap()))).unwrap();
    assert!(fs::read_to_string(&alt).unwrap().contains("\\303\\251"));
    assert_borrower_reads_provider_objects(&b, &prov);
    let res = scan_with(&root, network_everywhere());
    assert_eq!(facts_of(&res, &prov).no_dependents, Tri::No);
    assert!(not_removing(action_for(&res, &prov)));
}

#[test]
fn m2_a_relative_alternates_entry_is_resolved_against_the_objects_directory() {
    let (_tmp, root, remotes) = workspace();
    let prov = pushed_repo(&root, &remotes, "prov");
    let (b, alt) = shared_clone(&root, &prov, "borrower");
    // objects dir = <root>/borrower/.git/objects; three levels up is <root>.
    fs::write(&alt, "../../../prov/.git/objects\n").unwrap();
    assert_borrower_reads_provider_objects(&b, &prov);
    let res = scan_with(&root, network_everywhere());
    assert_eq!(facts_of(&res, &prov).no_dependents, Tri::No);
}

#[test]
fn m2_comment_and_blank_lines_are_ignored() {
    let (_tmp, root, remotes) = workspace();
    let prov = pushed_repo(&root, &remotes, "prov");
    let (b, alt) = shared_clone(&root, &prov, "borrower");
    let objs = fs::canonicalize(prov.join(".git").join("objects")).unwrap();
    fs::write(&alt, format!("# a comment\n\n{}\n\n", objs.display())).unwrap();
    assert_borrower_reads_provider_objects(&b, &prov);
    let res = scan_with(&root, network_everywhere());
    assert_eq!(facts_of(&res, &prov).no_dependents, Tri::No);
}

#[test]
fn m2_a_malformed_quoted_entry_makes_every_repo_unknown() {
    let (_tmp, root, remotes) = workspace();
    let prov = pushed_repo(&root, &remotes, "prov");
    let other = pushed_repo(&root, &remotes, "other");
    let (_b, alt) = shared_clone(&root, &prov, "borrower");
    fs::write(&alt, "\"/unterminated\\q\n").unwrap();
    let res = scan_with(&root, network_everywhere());
    for r in [&prov, &other] {
        assert_eq!(facts_of(&res, r).no_dependents, Tri::Unknown, "{}", r.display());
        assert_eq!(action_for(&res, r), Action::Ask);
    }
}

#[test]
fn m2_an_entry_outside_the_scan_makes_every_repo_unknown() {
    let (tmp, root, remotes) = workspace();
    let other = pushed_repo(&root, &remotes, "other");
    let ext = tmp.path().join("ext");
    init_repo(&ext);
    let (_b, _alt) = shared_clone(&root, &ext, "borrower");
    let res = scan_with(&root, network_everywhere());
    assert_eq!(facts_of(&res, &other).no_dependents, Tri::Unknown);
    assert_eq!(action_for(&res, &other), Action::Ask);
}

#[test]
fn m2_chained_alternates_make_the_last_provider_report_every_borrower() {
    let (_tmp, root, remotes) = workspace();
    let a = pushed_repo(&root, &remotes, "a");
    let (b, _) = shared_clone(&root, &a, "b");
    let (c, _) = shared_clone(&root, &b, "c");
    let res = scan_with(&root, network_everywhere());
    let d = dependents(&res, &a);
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d.iter().any(|x| x.ends_with("/c")));
    assert_eq!(facts_of(&res, &a).no_dependents, Tri::No);
    assert_eq!(facts_of(&res, &c).no_dependents, Tri::Yes);
}
