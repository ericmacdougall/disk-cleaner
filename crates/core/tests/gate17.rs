//! Regression tests for the coordinator's round Z after the gate review of 36f9698 (PR #2).
//!
//! Z1: a file in `objects/` was left out of the backup when its first bytes looked like an
//! object file's (a `RIDX` sidecar holding notes, a pack or index with valid magic and junk
//! after it, a loose object whose header inflates but whose remainder is corrupt). Now every
//! file of a git directory is in exactly one of two obligations, decided by name alone:
//! `preserve.git_admin` (always backed up) or `preserve.object_store` (verified against the
//! remote or backed up; never exempt). The scan marks an object store verified only when
//! `--verify-objects` ran git's own verification of all of it and git accounted for every file;
//! a file git does not account for is the repository's own and makes it ask, and a store git
//! finds damaged asks.
//!
//! Every fixture uses real git.

mod common;

use common::*;
use disk_cleaner_core::{
    git_admin_files, object_store_files, plan, Action, Plan, PlanItem, PlanOptions, ScanResult,
};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

fn plan_of(res: &ScanResult) -> Plan {
    plan(res, &PlanOptions::new(7, far_future()))
}

fn item<'a>(p: &'a Plan, path: &Path) -> &'a PlanItem {
    p.items
        .iter()
        .find(|i| i.path == path.to_string_lossy())
        .unwrap_or_else(|| panic!("no plan item for {}", path.display()))
}

/// The default scan (no object verification).
fn scan_default(root: &Path) -> ScanResult {
    scan_with(root, network_everywhere())
}

/// `scan --verify-objects`.
fn scan_verified(root: &Path) -> ScanResult {
    scan_with(root, network_everywhere().with_verify_objects(true))
}

/// A clone, inside the scan root, of a repository pushed to a bare remote outside it.
fn clone_of(tmp: &Path, root: &Path, remotes: &Path, name: &str) -> PathBuf {
    pushed_repo(&tmp.join("src"), remotes, name);
    let clone = root.join(name);
    let url = remotes.join(format!("{name}.git"));
    git(
        root,
        &[
            "clone",
            "-q",
            url.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    clone
}

fn admin(res: &ScanResult, repo: &Path) -> Vec<String> {
    git_admin_files(node(res, repo)).expect("the git directory's own files can be listed")
}

fn store(res: &ScanResult, repo: &Path) -> Vec<String> {
    object_store_files(node(res, repo)).expect("the object store's files can be listed")
}

/// Every non-directory entry of the git directory, `/`-separated, sorted.
fn every_file(repo: &Path) -> Vec<String> {
    let gd = repo.join(".git");
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![gd.clone()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(&gd).unwrap().to_string_lossy();
            out.push(rel.replace('\\', "/"));
        }
    }
    out.sort();
    out
}

/// The two obligations partition the git directory: every file is in exactly one.
fn assert_partition(res: &ScanResult, repo: &Path) {
    let a = admin(res, repo);
    let s = store(res, repo);
    for f in &s {
        assert!(!a.contains(f), "{f} is in both classes");
    }
    let mut both: Vec<String> = a.iter().chain(s.iter()).cloned().collect();
    both.sort();
    assert_eq!(
        both,
        every_file(repo),
        "the two classes must cover every file"
    );
}

/// A removing item for `repo` that carries both obligations, with counts matching the two public
/// lists, the store's verification as `verified`, and a reason that says all of it.
fn assert_obligations(res: &ScanResult, it: &PlanItem, repo: &Path, verified: bool) {
    assert_eq!(it.action, Action::BackupThenRemove, "{}", it.reason);
    let pre = it
        .preserve
        .as_ref()
        .unwrap_or_else(|| panic!("a removing item has no preserve obligation: {}", it.reason));
    assert_eq!(pre.git_admin.files, admin(res, repo).len() as u64);
    let s = store(res, repo);
    assert!(!s.is_empty(), "a clone has an object store");
    assert_eq!(pre.object_store.files, s.len() as u64, "{s:?}");
    assert!(pre.object_store.bytes > 0);
    assert_eq!(pre.object_store.verified, verified, "{}", it.reason);
    for want in [
        "local git file",
        "are saved with the manifest first",
        "its object store (",
        "must be verified against the remote or saved too",
        "then it is removed",
    ] {
        assert!(it.reason.contains(want), "{want:?} not in {}", it.reason);
    }
    let said = if verified {
        "verified by this scan"
    } else {
        "not yet verified"
    };
    assert!(it.reason.contains(said), "{said:?} not in {}", it.reason);
}

/// The plan asks for `repo` and reclaims nothing.
fn assert_asks(res: &ScanResult, repo: &Path) -> String {
    let p = plan_of(res);
    let it = item(&p, repo);
    assert_eq!(it.action, Action::Ask, "{}", it.reason);
    assert_eq!(it.reclaimable_bytes, 0);
    let s = node(res, repo).repo.as_ref().unwrap().object_store.clone();
    assert!(
        !s.map(|s| s.verified).unwrap_or(false),
        "a store that asks is not verified"
    );
    it.reason.clone()
}

fn pack_dir(repo: &Path) -> PathBuf {
    repo.join(".git").join("objects").join("pack")
}

// ---------------------------------------------------------------- the reviewer's three cases

const MIDX_REV: &str = "multi-pack-index-1111111111111111111111111111111111111111.rev";

#[test]
fn z1_a_midx_sidecar_holding_notes_is_under_the_object_store_obligation() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let mut notes = b"RIDX".to_vec();
    notes.extend_from_slice(b"my only copy of these notes\n");
    fs::write(pack_dir(&repo).join(MIDX_REV), &notes).unwrap();
    let rel = format!("objects/pack/{MIDX_REV}");

    let res = scan_default(&root);
    assert!(
        store(&res, &repo).contains(&rel),
        "{:?}",
        store(&res, &repo)
    );
    assert!(!admin(&res, &repo).contains(&rel));
    assert_partition(&res, &repo);
    let p = plan_of(&res);
    assert_obligations(&res, item(&p, &repo), &repo, false);
}

#[test]
fn z1_with_verify_objects_an_unaccounted_midx_sidecar_asks() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let mut notes = b"RIDX".to_vec();
    notes.extend_from_slice(b"my only copy of these notes\n");
    fs::write(pack_dir(&repo).join(MIDX_REV), &notes).unwrap();

    let res = scan_verified(&root);
    let reason = assert_asks(&res, &repo);
    assert!(reason.contains(MIDX_REV), "{reason}");
    assert_eq!(
        facts_of(&res, &repo).no_local_admin_content,
        disk_cleaner_core::Tri::No
    );
}

const FAKE: &str = "pack-3333333333333333333333333333333333333333";

/// A pack and an index of that name whose magic and version are right and whose rest is not.
fn malformed_pack_pair(repo: &Path) -> (String, String) {
    let dir = pack_dir(repo);
    let mut pack = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
    pack.extend_from_slice(b"these are notes, not a compressed object\n");
    fs::write(dir.join(format!("{FAKE}.pack")), &pack).unwrap();
    let mut idx = vec![0xff, b't', b'O', b'c', 0, 0, 0, 2];
    idx.extend_from_slice(&[0u8; 1024]);
    idx.extend_from_slice(b"notes again\n");
    fs::write(dir.join(format!("{FAKE}.idx")), &idx).unwrap();
    (
        format!("objects/pack/{FAKE}.pack"),
        format!("objects/pack/{FAKE}.idx"),
    )
}

#[test]
fn z1_a_pack_and_index_malformed_after_their_magic_are_never_exempt() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let (pack, idx) = malformed_pack_pair(&repo);

    let res = scan_default(&root);
    assert_obligated_or_asks(&res, &repo, &[pack.as_str(), idx.as_str()]);

    let res = scan_verified(&root);
    assert_asks(&res, &repo);
}

/// Write a blob as a loose object with real git and return its path below the git directory.
fn loose_blob(repo: &Path, text: &str) -> (String, PathBuf) {
    let src = repo.join("blob.txt");
    write(&src, text);
    let id = git(repo, &["hash-object", "-w", "blob.txt"]);
    fs::remove_file(&src).unwrap();
    let rel = format!("objects/{}/{}", &id[..2], &id[2..]);
    let path = repo.join(".git").join(&rel);
    assert!(path.is_file(), "{rel} was not written loose");
    make_writable(&path);
    (rel, path)
}

/// Git writes objects read-only.
#[cfg(unix)]
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

#[cfg(not(unix))]
fn make_writable(path: &Path) {
    let mut perm = fs::metadata(path).unwrap().permissions();
    perm.set_readonly(false);
    fs::set_permissions(path, perm).unwrap();
}

/// Without verification, a damaged object file either makes some git command fail (the repo
/// asks) or is counted under the object-store obligation, unverified. Never exempt.
fn assert_obligated_or_asks(res: &ScanResult, repo: &Path, rels: &[&str]) {
    let s = store(res, repo);
    for rel in rels {
        assert!(s.iter().any(|f| f == rel), "{rel} not in {s:?}");
    }
    assert_partition(res, repo);
    let p = plan_of(res);
    let it = item(&p, repo);
    if it.action == Action::Ask {
        assert_eq!(it.reclaimable_bytes, 0);
    } else {
        assert_obligations(res, it, repo, false);
    }
}

fn some_text() -> String {
    (0..300).map(|i| format!("line {i} of a blob\n")).collect()
}

#[test]
fn z1_a_loose_object_with_a_valid_header_and_a_corrupt_remainder() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let (rel, path) = loose_blob(&repo, &some_text());
    let mut bytes = fs::read(&path).unwrap();
    let n = bytes.len();
    assert!(
        n > 64,
        "the object is long enough to keep its header intact"
    );
    for b in &mut bytes[n - 16..] {
        *b = b'X';
    }
    fs::write(&path, &bytes).unwrap();

    let res = scan_default(&root);
    assert_obligated_or_asks(&res, &repo, &[rel.as_str()]);

    let res = scan_verified(&root);
    assert_asks(&res, &repo);
}

#[test]
fn z1_bytes_after_a_valid_loose_object_are_found_by_verification() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let (rel, path) = loose_blob(&repo, &some_text());
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"notes hidden after the zlib stream\n")
        .unwrap();
    drop(f);

    let res = scan_default(&root);
    assert_obligated_or_asks(&res, &repo, &[rel.as_str()]);

    let res = scan_verified(&root);
    assert_asks(&res, &repo);
}

// ---------------------------------------------------------------- controls

#[test]
fn z1_controls_are_unverified_by_default_and_verified_by_git() {
    let (tmp, root, remotes) = workspace();
    let fresh = clone_of(tmp.path(), &root, &remotes, "fresh");
    let gc = clone_of(tmp.path(), &root, &remotes, "gc");
    git(&gc, &["gc", "-q"]);
    let bitmap = clone_of(tmp.path(), &root, &remotes, "bitmap");
    git(
        &bitmap,
        &["repack", "-a", "-d", "-q", "--write-bitmap-index"],
    );
    let graph = clone_of(tmp.path(), &root, &remotes, "graph");
    git(&graph, &["commit-graph", "write", "--reachable"]);
    let split = clone_of(tmp.path(), &root, &remotes, "split");
    git(&split, &["commit-graph", "write", "--reachable", "--split"]);
    let midx = clone_of(tmp.path(), &root, &remotes, "midx");
    git(&midx, &["repack", "-a", "-d", "-q"]);
    git(&midx, &["multi-pack-index", "write", "--bitmap"]);
    let all = [&fresh, &gc, &bitmap, &graph, &split, &midx];

    let res = scan_default(&root);
    let p = plan_of(&res);
    for repo in all {
        assert_partition(&res, repo);
        assert_obligations(&res, item(&p, repo), repo, false);
    }

    let res = scan_verified(&root);
    let p = plan_of(&res);
    for repo in all {
        let n = node(&res, repo).repo.as_ref().unwrap();
        assert!(
            n.object_store.as_ref().map(|s| s.verified).unwrap_or(false),
            "{}: {:?}",
            repo.display(),
            n.notes
        );
        assert_obligations(&res, item(&p, repo), repo, true);
    }
}

const SPLIT_ID: &str = "4444444444444444444444444444444444444444";

/// `git commit-graph verify` reads `objects/info/commit-graph` when it exists and opens the
/// split chain only when it does not (builtin/commit-graph.c), so a split graph file beside a
/// single commit-graph is never read by git, even when the chain file names it.
#[test]
fn z1_a_split_graph_file_beside_a_single_commit_graph_asks() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    git(&repo, &["commit-graph", "write", "--reachable"]);
    let info = repo.join(".git").join("objects").join("info");
    assert!(info.join("commit-graph").is_file());
    let graphs = info.join("commit-graphs");
    fs::create_dir_all(&graphs).unwrap();
    let name = format!("graph-{SPLIT_ID}.graph");
    write(&graphs.join(&name), "CGPH and then my notes\n");
    let chain = graphs.join("commit-graph-chain");
    write(&chain, &format!("{SPLIT_ID}\n"));
    let rel = format!("objects/info/commit-graphs/{name}");

    let res = scan_default(&root);
    assert_obligated_or_asks(&res, &repo, &[rel.as_str()]);

    let res = scan_verified(&root);
    let reason = assert_asks(&res, &repo);
    assert!(reason.contains(&name), "{reason}");
}

/// An object that nothing reaches may exist nowhere else: the store is not verified, but the
/// file is still under the object-store obligation, so the repository is not made to ask.
#[test]
fn z1_an_unreachable_object_leaves_the_store_unverified() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let (rel, _) = loose_blob(&repo, &some_text());

    let res = scan_verified(&root);
    assert!(store(&res, &repo).contains(&rel));
    let n = node(&res, &repo).repo.as_ref().unwrap();
    assert!(
        n.notes.iter().any(|x| x.contains("not reachable")),
        "{:?}",
        n.notes
    );
    let p = plan_of(&res);
    assert_obligations(&res, item(&p, &repo), &repo, false);
}

#[test]
fn z1_a_store_whose_files_were_not_counted_asks() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let mut res = scan_default(&root);
    let i = node_index(&res, &repo);
    res.nodes[i].repo.as_mut().unwrap().object_store = None;
    let p = plan_of(&res);
    let it = item(&p, &repo);
    assert_eq!(it.action, Action::Ask, "{}", it.reason);
    assert!(it.preserve.is_none());
    assert!(it.reason.contains("object store"), "{}", it.reason);
}

/// A check that cannot run (here git cannot be started; a timeout ends the same way) proves
/// nothing either way: the store is not verified, and nothing is reported damaged or
/// unaccounted. Deterministic, unlike a short timeout, which a fast machine can beat.
#[test]
fn z1_a_verification_that_cannot_run_is_not_verified_and_not_damaged() {
    let tmp = tempfile::tempdir().unwrap();
    let gd = tmp.path().join(".git");
    let git_bin = disk_cleaner_core::Git::new(
        tmp.path().join("no-such-git"),
        std::time::Duration::from_secs(5),
    );
    let id = "0123456789abcdef0123456789abcdef01234567";
    let store = vec![
        (format!("objects/ab/{}", &id[2..]), 4096),
        (format!("objects/pack/pack-{id}.pack"), 4096),
        (format!("objects/pack/pack-{id}.idx"), 4096),
    ];
    let v = disk_cleaner_core::preserve::verify(&git_bin, tmp.path(), &gd, &store);
    assert!(!v.verified, "{v:?}");
    assert_eq!(v.damaged, None, "{v:?}");
    assert!(v.unaccounted.is_empty(), "{v:?}");
    assert!(!v.unverified.is_empty(), "{v:?}");
}

// ---------------------------------------------------------------- click-test remark

#[test]
fn z1_large_preserved_files_lead_the_examples() {
    let (tmp, root, remotes) = workspace();
    let repo = clone_of(tmp.path(), &root, &remotes, "app");
    let hooks = repo.join(".git").join("hooks");
    for k in 1..=5 {
        write(&hooks.join(format!("a{k}.sample")), "#!/bin/sh\n");
    }
    write_bytes(&hooks.join("zz-big.sample"), 2 * 1024 * 1024);
    let res = scan_default(&root);
    let mut opts = PlanOptions::new(7, far_future());
    opts.max_git_admin_bytes = 1 << 30;
    let p = plan(&res, &opts);
    let it = item(&p, &repo);
    assert_eq!(it.action, Action::BackupThenRemove, "{}", it.reason);
    let ex = &it.preserve.as_ref().unwrap().git_admin.examples;
    assert_eq!(
        ex.first().map(String::as_str),
        Some(".git/hooks/zz-big.sample"),
        "{ex:?}"
    );
}
