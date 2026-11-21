//! Pieces and lanes (docs/SCAN-FORMAT.md, "Pieces and lanes"), GoodQA goodqa#16529.
//!
//! Test names start with the acceptance criterion they prove: `partition`, `split`, `lane`,
//! `dedup`, `recheck`, `contract`; `neg…` for the negative cases. Every fixture is synthetic and
//! built in a temporary directory.

#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

mod common;

use common::*;
use disk_cleaner_core::pieces::{PieceOptions, NO_IDENTITY};
use disk_cleaner_core::{
    plan, recheck, scan, Action, Evidence, Lane, LanePiece, Lanes, Plan, PlanOptions, ProcessCheck,
    RecheckScope, Recovery, RuleSet, ScanOptions, ScanResult,
};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

const KIB: usize = 1024;

fn lanes_scan_with(
    root: &Path,
    facts: disk_cleaner_core::FactsConfig,
    po: PieceOptions,
) -> ScanResult {
    // Status-change times at or after the survey's start count as "changed after the scan", so
    // the fixture must be older than the second the walk starts in.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut opts = ScanOptions::new(vec![root.to_path_buf()]);
    opts.facts = facts;
    opts.git_jobs = 2;
    opts.lanes = Some(po);
    scan(&opts).expect("scan")
}

fn lanes_scan(root: &Path) -> ScanResult {
    lanes_scan_with(root, network_everywhere(), PieceOptions::default())
}

fn idle_opts() -> PlanOptions {
    PlanOptions::new(7, far_future())
}

fn lanes_of(p: &Plan) -> &Lanes {
    p.lanes
        .as_ref()
        .expect("a scan with pieces gives a plan with lanes")
}

/// Every plan piece whose members include `abs` (a derived piece, or a scan piece that did not
/// move it out).
fn pieces_holding<'a>(res: &ScanResult, l: &'a Lanes, abs: &Path) -> Vec<&'a LanePiece> {
    let survey = res.pieces.as_ref().unwrap();
    let mut out = Vec::new();
    for lp in &l.pieces {
        let sp = &survey.pieces[lp.scan_piece];
        let names: Vec<String> = match &lp.member {
            Some(m) => vec![m.clone()],
            None => sp
                .members
                .iter()
                .map(|m| m.path().to_string())
                .filter(|m| !lp.moved_out.contains(m))
                .collect(),
        };
        if names.iter().any(|n| Path::new(&sp.path).join(n) == abs) {
            out.push(lp);
        }
    }
    out
}

fn the_piece<'a>(res: &ScanResult, l: &'a Lanes, abs: &Path) -> &'a LanePiece {
    let v = pieces_holding(res, l, abs);
    assert_eq!(
        v.len(),
        1,
        "{} must be in exactly one plan piece: {v:#?}",
        abs.display()
    );
    v[0]
}

/// Every non-directory entry under `root`, never following a symlink.
fn all_entries(root: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                stack.push(p);
            } else {
                out.insert(p);
            }
        }
    }
    out
}

/// For a member of a `git-node` piece, which of its path's names are git's own bookkeeping names
/// (docs/SCAN-FORMAT.md, "Git's bookkeeping names are not secret names"): below `refs` or `logs`
/// of a git top, `packed-refs` in a git top, and the directory names below `worktrees` or
/// `modules` of a git top down to an admin directory (one holding `HEAD`), which is a git top
/// again. Name-based secret rules do not apply to those names. An independent model of the
/// layout, from the member list alone.
fn git_exempt_names(sp: &disk_cleaner_core::pieces::Piece, parts: &[&str]) -> Vec<bool> {
    let mut exempt = vec![false; parts.len()];
    if sp.kind != disk_cleaner_core::PieceKind::GitNode {
        return exempt;
    }
    // A bare repository's piece is its git directory; a linked worktree's `.git` is a file.
    let bare = !sp
        .members
        .iter()
        .any(|m| m.path() == ".git" || m.path().starts_with(".git/"));
    let mut i = match parts.iter().position(|p| *p == ".git") {
        Some(i) => i + 1,
        None if bare => 0,
        None => return exempt,
    };
    let is_member = |p: &str| sp.members.iter().any(|m| m.path() == p);
    // `i` is the first name below a git top.
    loop {
        let last = i + 1 == parts.len();
        match parts.get(i) {
            Some(&"packed-refs") if last => exempt[i] = true,
            Some(&("refs" | "logs")) if !last => {
                for e in exempt.iter_mut().skip(i) {
                    *e = true;
                }
            }
            Some(&("worktrees" | "modules")) if !last => {
                // Directory names down to the admin directory; the file name itself is not one.
                let mut k = i + 1;
                let mut admin = None;
                while k + 1 < parts.len() {
                    if is_member(&format!("{}/HEAD", parts[..=k].join("/"))) {
                        admin = Some(k);
                        break;
                    }
                    k += 1;
                }
                let upto = admin.unwrap_or(parts.len() - 2);
                for e in exempt.iter_mut().take(upto + 1).skip(i + 1) {
                    *e = true;
                }
                if let Some(a) = admin {
                    i = a + 1;
                    continue;
                }
            }
            _ => {}
        }
        break;
    }
    exempt
}

/// The invariants every survey and every plan must hold.
fn check_invariants(res: &ScanResult, p: &Plan) {
    let survey = res.pieces.as_ref().expect("pieces");
    // Ids are indexes.
    for (i, sp) in survey.pieces.iter().enumerate() {
        assert_eq!(sp.id, i);
    }
    // Byte accounting of the survey.
    let sum: u64 = survey.pieces.iter().map(|p| p.bytes).sum();
    assert_eq!(
        sum, survey.totals.bytes,
        "survey totals.bytes is the sum of piece bytes"
    );
    assert_eq!(
        sum + survey.totals.dir_overhead_bytes,
        res.totals.allocated_bytes,
        "piece bytes plus dir overhead equal the scan's allocated bytes"
    );
    // Every lane byte is a scanned byte, once.
    let l = lanes_of(p);
    let lane_sum: u64 = l.totals.by_lane.values().map(|t| t.bytes).sum();
    assert_eq!(
        lane_sum + l.totals.dir_overhead_bytes,
        l.totals.scanned_bytes
    );
    assert_eq!(l.totals.scanned_bytes, res.totals.allocated_bytes);
    let lp_sum: u64 = l.pieces.iter().map(|p| p.bytes).sum();
    assert_eq!(lp_sum, lane_sum);
    for lp in &l.pieces {
        assert!(!lp.reason.is_empty(), "piece {} has a reason", lp.id);
        assert!(lp.reclaimable_bytes <= lp.bytes);
        assert!(lp.upload_bytes <= lp.bytes);
        if lp.lane == Lane::Keep {
            assert_eq!(lp.reclaimable_bytes, 0);
            assert_eq!(lp.upload_bytes, 0);
        }
        assert_eq!(
            lp.evidence.is_some(),
            lp.lane == Lane::Noise,
            "piece {}",
            lp.id
        );
        assert_eq!(lp.git.is_some(), lp.lane == Lane::Git, "piece {}", lp.id);
        // NEG-1: a secret piece is always kept.
        if lp.secret.is_some() {
            assert_eq!(lp.lane, Lane::Keep, "secret piece {} is kept", lp.id);
        }
    }
    // NEG-1, NEG-11: no secret-named member in a removing piece, and none in a blob.
    let rules = RuleSet::builtin();
    for lp in l.pieces.iter().filter(|p| p.lane.removes()) {
        let sp = &survey.pieces[lp.scan_piece];
        let in_artifact = sp
            .node
            .map(|n| res.nodes[n].class.is_artifact())
            .unwrap_or(false);
        for m in &sp.members {
            if lp.moved_out.iter().any(|x| x == m.path()) {
                continue;
            }
            let parts: Vec<&str> = m.path().split('/').collect();
            let exempt = git_exempt_names(sp, &parts);
            let n = parts.len();
            let file_ok = exempt[n - 1] || rules.secret_file(parts[n - 1], in_artifact).is_none();
            let dirs_ok = (0..n - 1).all(|i| exempt[i] || rules.secret_dir(parts[i]).is_none());
            assert!(
                file_ok && dirs_ok,
                "secret-named {} in removing piece {}",
                m.path(),
                lp.id
            );
        }
        // A git directory is removed only whole, as part of its repository's git-node piece.
        if sp.kind != disk_cleaner_core::PieceKind::GitNode {
            let in_git = |p: &Path| p.components().any(|c| c.as_os_str() == ".git");
            assert!(
                !in_git(Path::new(&sp.path))
                    && sp.members.iter().all(|m| !in_git(Path::new(m.path()))),
                "removing piece {} lies inside a git directory: {}",
                lp.id,
                sp.path
            );
        }
    }
    for b in &survey.blobs {
        for c in &b.copies {
            assert!(
                survey.pieces[c.piece].secret.is_none(),
                "secret {} in a blob",
                c.path
            );
        }
    }
}

fn check_partition(root: &Path, res: &ScanResult) {
    let survey = res.pieces.as_ref().unwrap();
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    for sp in &survey.pieces {
        for m in &sp.members {
            let abs = Path::new(&sp.path).join(m.path());
            if let Some(prev) = seen.insert(abs.clone(), sp.id) {
                panic!("{} is in pieces {prev} and {}", abs.display(), sp.id);
            }
        }
    }
    let want = all_entries(root);
    let have: BTreeSet<PathBuf> = seen.keys().cloned().collect();
    assert_eq!(
        have, want,
        "every non-directory entry is a member of exactly one piece"
    );
}

// ----------------------------------------------------------------------------------------------
// AC-2: partition.

#[test]
fn partition_every_entry_once_and_hardlinks_count_once() {
    let (_tmp, root, _) = workspace();
    let a = root.join("loose");
    for i in 0..12 {
        write_bytes(&a.join(format!("f{i:02}.dat")), 3000 + i);
    }
    write_bytes(&a.join("deep").join("er").join("x.bin"), 70 * KIB);
    write(&root.join("top.txt"), "root file\n");
    #[cfg(unix)]
    {
        fs::hard_link(a.join("f00.dat"), a.join("deep").join("link-to-f00")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", a.join("dangling")).unwrap();
    }
    let res = lanes_scan(&root);
    check_partition(&root, &res);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let survey = res.pieces.as_ref().unwrap();
    #[cfg(unix)]
    {
        assert_eq!(survey.totals.hardlink_names, 1);
        // The first name in walk order owns the bytes: `deep/link-to-f00` comes before `f00.dat`.
        let owner = survey
            .pieces
            .iter()
            .flat_map(|p| p.members.iter().map(move |m| (p, m)))
            .find(|(p, m)| Path::new(&p.path).join(m.path()) == a.join("deep").join("link-to-f00"))
            .unwrap();
        assert!(owner.1.bytes() > 0);
        let other = survey
            .pieces
            .iter()
            .flat_map(|p| p.members.iter().map(move |m| (p, m)))
            .find(|(p, m)| Path::new(&p.path).join(m.path()) == a.join("f00.dat"))
            .unwrap();
        assert_eq!(other.1.bytes(), 0);
        // The symlink is a member of kind `l`, never followed.
        let link = survey
            .pieces
            .iter()
            .flat_map(|p| p.members.iter().map(move |m| (p, m)))
            .find(|(p, m)| Path::new(&p.path).join(m.path()) == a.join("dangling"))
            .unwrap();
        assert_eq!(link.1.kind(), "l");
    }
    // The scan file round-trips with its members.
    let text = serde_json::to_string(&res).unwrap();
    let back = disk_cleaner_core::scan_from_json(&text).expect("round trip");
    let again = back.pieces.unwrap();
    assert_eq!(again.pieces.len(), survey.pieces.len());
    for (x, y) in again.pieces.iter().zip(&survey.pieces) {
        assert_eq!(x.members, y.members);
    }
    // On a platform without file identity every lane is keep.
    if !cfg!(unix) {
        for sp in &survey.pieces {
            assert_eq!(sp.incomplete.as_deref(), Some(NO_IDENTITY));
        }
        for lp in &lanes_of(&p).pieces {
            assert_eq!(lp.lane, Lane::Keep);
        }
    }
}

// ----------------------------------------------------------------------------------------------
// AC-3: split.

#[test]
fn split_large_folders_into_bounded_pieces() {
    let (_tmp, root, _) = workspace();
    let big = root.join("big");
    for i in 0..25 {
        write_bytes(&big.join(format!("{i:02}.dat")), 2 * KIB);
    }
    for i in 0..4 {
        write_bytes(&big.join("sub").join(format!("{i}.dat")), 2 * KIB);
    }
    write_bytes(&big.join("huge.bin"), 40 * KIB);
    let po = PieceOptions {
        max_piece_bytes: 16 * KIB as u64,
        max_piece_files: 10,
        ..PieceOptions::default()
    };
    let res = lanes_scan_with(&root, network_everywhere(), po.clone());
    check_partition(&root, &res);
    check_invariants(&res, &plan(&res, &idle_opts()));
    let survey = res.pieces.as_ref().unwrap();
    assert!(
        survey.pieces.len() >= 4,
        "split into many pieces: {}",
        survey.pieces.len()
    );
    for sp in &survey.pieces {
        let logical: u64 = sp.members.iter().map(|m| m.length()).sum();
        let n = sp.members.len() as u64;
        let single = sp.members.len() == 1;
        assert!(
            single || (logical <= po.max_piece_bytes && n <= po.max_piece_files),
            "piece {} at {} has {n} members, {logical} bytes",
            sp.id,
            sp.path
        );
    }
    // The big file is a files piece of its own; `sub` fits in one tree piece.
    let huge = survey
        .pieces
        .iter()
        .find(|p| p.members.iter().any(|m| m.path() == "huge.bin"))
        .unwrap();
    assert_eq!(huge.members.len(), 1);
    assert_eq!(huge.kind, disk_cleaner_core::PieceKind::Files);
    let sub = survey
        .pieces
        .iter()
        .find(|p| Path::new(&p.path) == big.join("sub"))
        .expect("sub is one tree piece");
    assert_eq!(sub.kind, disk_cleaner_core::PieceKind::Tree);
    assert_eq!(sub.members.len(), 4);
}

// ----------------------------------------------------------------------------------------------
// AC-4: one lane per piece.

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn lane_backup_for_idle_loose_files() {
    let (_tmp, root, _) = workspace();
    write_bytes(&root.join("notes").join("a.txt"), 5000);
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let lp = the_piece(&res, lanes_of(&p), &root.join("notes").join("a.txt"));
    assert_eq!(lp.lane, Lane::Backup);
    assert!(
        lp.reason.contains("no rule shows it can be regenerated"),
        "{}",
        lp.reason
    );
    assert!(lp.upload_bytes > 0);
    assert_eq!(lp.reclaimable_bytes, lp.bytes);
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn lane_noise_by_rule_for_a_marked_node_modules() {
    let (_tmp, root, _) = workspace();
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("index.js"), "1");
    write_bytes(&proj.join("node_modules").join("dep").join("x.js"), 9000);
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let lp = the_piece(&res, l, &proj.join("node_modules").join("dep").join("x.js"));
    assert_eq!(lp.lane, Lane::Noise, "{}", lp.reason);
    match lp.evidence.as_ref().unwrap() {
        Evidence::Rule {
            rule,
            markers,
            node,
        } => {
            assert_eq!(rule, "node_modules");
            assert!(markers.iter().any(|m| m == "package.json"));
            assert_eq!(
                res.nodes[*node].path,
                proj.join("node_modules").to_string_lossy()
            );
        }
        e => panic!("rule evidence expected, got {e:?}"),
    }
    assert!(lp.reason.contains("regenerated"), "{}", lp.reason);
    assert_eq!(lp.upload_bytes, 0);
    // The project's own files are backed up.
    let src = the_piece(&res, l, &proj.join("index.js"));
    assert_eq!(src.lane, Lane::Backup);
}

/// PR review a2-3 F2, its product effect: a dependency checkout with its own `.git` inside a
/// regenerable directory. The git directory is never removed piecemeal, so the piece that holds
/// it is kept. A sibling package in another piece keeps the lane the plan gives the artifact (the
/// `.git` already makes the artifact ask, so it is backed up, not noise).
#[cfg(unix)]
#[test]
fn neg12_a_git_directory_inside_an_artifact_keeps_its_piece_only() {
    let (_tmp, root, _) = workspace();
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    let nm = proj.join("node_modules");
    let dep = nm.join("dep");
    write(&dep.join("index.js"), "x");
    init_repo(&dep);
    // A sibling package too big to share a piece with dep (max_piece_bytes splits them).
    let po = PieceOptions {
        max_piece_bytes: 64 * KIB as u64,
        ..PieceOptions::default()
    };
    write_bytes(&nm.join("big").join("x.js"), 100 * KIB);
    let res = lanes_scan_with(&root, network_everywhere(), po);
    let p = plan(&res, &idle_opts());
    let l = lanes_of(&p);
    let sibling = the_piece(&res, l, &nm.join("big").join("x.js"));
    assert_eq!(sibling.lane, Lane::Backup, "{}", sibling.reason);
    let held = the_piece(&res, l, &dep.join(".git").join("HEAD"));
    assert_ne!(sibling.id, held.id);
    assert_eq!(held.lane, Lane::Keep, "{}", held.reason);
    assert!(held.reason.contains("git directory"), "{}", held.reason);
    check_invariants(&res, &p);
}

/// The same, for every `[[rule]]` in rules/artifacts.toml: an artifact that holds a dependency
/// checkout with its own `.git` is never `noise` (the scan makes it ask), the piece holding the
/// git directory is kept, and no removing piece holds a file of the git directory.
#[cfg(unix)]
#[test]
fn neg12_every_artifact_rule_keeps_a_nested_git_directory() {
    let (_tmp, root, _) = workspace();
    let rules = RuleSet::builtin();
    let mut cases = Vec::new();
    for (i, rule) in rules.rules().iter().enumerate() {
        let base = root.join(format!("r{i:02}"));
        let dir = base.join(&rule.name);
        if let Some(m) = rule.markers.first() {
            let marker = m.replace('*', "m");
            write(&dir.parent().unwrap().join(marker), "{}");
        }
        write_bytes(&dir.join("big.bin"), 100 * KIB);
        let dep = dir.join("gitdep");
        write(&dep.join("index.js"), "x");
        init_repo(&dep);
        cases.push((rule.name.clone(), dir, dep));
    }
    let po = PieceOptions {
        max_piece_bytes: 64 * KIB as u64,
        ..PieceOptions::default()
    };
    let res = lanes_scan_with(&root, network_everywhere(), po);
    let p = plan(&res, &idle_opts());
    let l = lanes_of(&p);
    let mut unclassified = Vec::new();
    let mut lanes_seen = BTreeSet::new();
    for (name, dir, dep) in &cases {
        let classified = res
            .nodes
            .iter()
            .any(|n| Path::new(&n.path) == dir && n.class.is_artifact());
        if !classified {
            unclassified.push(name.clone());
        }
        let held = the_piece(&res, l, &dep.join(".git").join("HEAD"));
        assert_eq!(held.lane, Lane::Keep, "{name}: {}", held.reason);
        let sibling = the_piece(&res, l, &dir.join("big.bin"));
        assert_ne!(sibling.id, held.id, "{name}");
        assert_ne!(sibling.lane, Lane::Noise, "{name}: {}", sibling.reason);
        lanes_seen.insert(format!("{:?}", sibling.lane));
    }
    // Not vacuous: every rule's directory was classified as the artifact it describes.
    assert!(unclassified.is_empty(), "not classified: {unclassified:?}");
    eprintln!("sibling lanes over {} rules: {lanes_seen:?}", cases.len());
    check_invariants(&res, &p);
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn neg4_noise_without_evidence_falls_back_to_backup() {
    let (_tmp, root, _) = workspace();
    // A derived-looking name with no marker: not classified, so backed up.
    write_bytes(&root.join("a").join("node_modules").join("x.js"), 9000);
    write_bytes(&root.join("a").join("build").join("out.o"), 9000);
    // A regenerable directory whose item asks (it holds an output file).
    let proj = root.join("b");
    write(&proj.join("package.json"), "{}");
    write_bytes(&proj.join("node_modules").join("movie.mp4"), 9000);
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    for f in [
        root.join("a").join("node_modules").join("x.js"),
        root.join("a").join("build").join("out.o"),
    ] {
        assert_eq!(the_piece(&res, l, &f).lane, Lane::Backup, "{}", f.display());
    }
    let nm = proj.join("node_modules");
    let item = p.items.iter().find(|i| Path::new(&i.path) == nm);
    if let Some(it) = item {
        assert_ne!(
            it.action,
            Action::RemoveRegenerable,
            "the fixture must make it ask"
        );
    }
    assert_eq!(the_piece(&res, l, &nm.join("movie.mp4")).lane, Lane::Backup);
    assert!(l.pieces.iter().all(|p| p.lane != Lane::Noise));
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn lane_git_for_a_pushed_idle_repo() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "app");
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let lp = the_piece(&res, l, &repo.join("README.md"));
    assert_eq!(lp.kind, disk_cleaner_core::PieceKind::GitNode);
    assert_eq!(lp.lane, Lane::Git, "{}", lp.reason);
    let g = lp.git.as_ref().unwrap();
    assert_eq!(g.action, Action::BackupThenRemove);
    assert!(g.preserve);
    assert_eq!(g.recovery, Recovery::None);
    assert!(lp.reason.contains("git repository"), "{}", lp.reason);
    // The git directory is part of the same piece.
    assert_eq!(
        the_piece(&res, l, &repo.join(".git").join("HEAD")).id,
        lp.id
    );
}

/// A repo with a commit its remote does not have, the remote URL set to `url`.
fn unpushed_repo(root: &Path, remotes: &Path, url: &str) -> PathBuf {
    let repo = pushed_repo(root, remotes, "wip");
    commit_file(&repo, "work.txt", "unsaved work\n");
    git(&repo, &["remote", "set-url", "origin", url]);
    repo
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn neg2_wip_repo_never_gets_an_unconditional_push() {
    let (_tmp, root, remotes) = workspace();
    let repo = unpushed_repo(
        &root,
        &remotes,
        "https://someone:tok@example.invalid/o/r.git?x=1",
    );
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let item = p.items.iter().find(|i| Path::new(&i.path) == repo).unwrap();
    assert_eq!(
        item.action,
        Action::RecoverBackupThenRemove,
        "{}",
        item.reason
    );
    let lp = the_piece(&res, lanes_of(&p), &repo.join("work.txt"));
    assert_eq!(lp.lane, Lane::Git);
    match &lp.git.as_ref().unwrap().recovery {
        Recovery::PushRecoveryRefs {
            remote,
            url,
            fallback,
            decided_at,
            note,
        } => {
            assert_eq!(remote, "origin");
            assert_eq!(url, "https://example.invalid/o/r.git");
            assert_eq!(fallback, "bundle-to-backup");
            assert_eq!(decided_at, "apply");
            assert!(note.contains("private"), "{note}");
        }
        r => panic!("push-recovery-refs with a fallback expected, got {r:?}"),
    }
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn neg2_wip_repo_without_a_pushable_remote_bundles() {
    let (_tmp, root, remotes) = workspace();
    let repo = unpushed_repo(&root, &remotes, "git://example.invalid/o/r.git");
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    let item = p.items.iter().find(|i| Path::new(&i.path) == repo).unwrap();
    assert_eq!(
        item.action,
        Action::RecoverBackupThenRemove,
        "{}",
        item.reason
    );
    let lp = the_piece(&res, lanes_of(&p), &repo.join("work.txt"));
    assert_eq!(lp.git.as_ref().unwrap().recovery, Recovery::BundleToBackup);
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn neg1_secrets_are_kept_and_excluded_everywhere() {
    let (_tmp, root, remotes) = workspace();
    let loose = root.join("loose");
    write(&loose.join(".env"), "A=1\n");
    write(&loose.join(".env.example"), "A=\n");
    write(&loose.join("prod.pem"), "-----\n");
    write(&loose.join("api_token.txt"), "x\n");
    write(&loose.join(".ssh").join("config"), "Host x\n");
    write(&loose.join("plain.txt"), "hello\n");
    write(&loose.join("notes.txt"), "hello\n");
    let repo = pushed_repo(&root, &remotes, "app");
    write(&repo.join("id_rsa"), "key\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("plain.txt", loose.join("db_password")).unwrap();
    // Inside an artifact the substring patterns do not apply.
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("node_modules").join("tokenizer.js"), "x\n");
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let mut secrets = vec![
        loose.join(".env"),
        loose.join("prod.pem"),
        loose.join("api_token.txt"),
        loose.join(".ssh").join("config"),
        repo.join("id_rsa"),
    ];
    if cfg!(unix) {
        secrets.push(loose.join("db_password"));
        // What a secret-named symlink names is secret too (rescue AC-1, NEG-2; this assertion
        // changed from "plain.txt is not secret" with the context union, AC-9).
        secrets.push(loose.join("plain.txt"));
    }
    for s in &secrets {
        let lp = the_piece(&res, l, s);
        assert_eq!(lp.lane, Lane::Keep, "{}", s.display());
        assert!(lp.secret.is_some(), "{}", s.display());
        assert!(lp.reason.contains("stays local"), "{}", lp.reason);
    }
    for s in [loose.join(".env.example"), loose.join("notes.txt")] {
        assert!(the_piece(&res, l, &s).secret.is_none(), "{}", s.display());
    }
    assert!(
        the_piece(&res, l, &proj.join("node_modules").join("tokenizer.js"))
            .secret
            .is_none()
    );
    // The repo's piece excludes the key, and its git lane names it for every archive.
    let rp = the_piece(&res, l, &repo.join("README.md"));
    assert!(
        rp.excludes.iter().any(|x| x == "id_rsa"),
        "{:?}",
        rp.excludes
    );
    assert_eq!(rp.lane, Lane::Git);
    assert_eq!(
        rp.git.as_ref().unwrap().excludes_secret,
        vec!["id_rsa".to_string()]
    );
    assert!(l.totals.secret_pieces >= secrets.len() as u64);
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn neg7_process_check_partial_or_unknown_keeps_every_piece() {
    let (_tmp, root, remotes) = workspace();
    pushed_repo(&root, &remotes, "app");
    write_bytes(&root.join("loose").join("a.bin"), 5000);
    let res = lanes_scan(&root);
    let mut o = idle_opts();
    o.process = ProcessCheck::Partial {
        cwds: vec![],
        unreadable: 2,
    };
    let p = plan(&res, &o);
    check_invariants(&res, &p);
    for lp in &lanes_of(&p).pieces {
        assert_eq!(lp.lane, Lane::Keep, "{}", lp.path);
        assert!(
            lp.reason.contains("could not be inspected"),
            "{}",
            lp.reason
        );
    }
    o.process = ProcessCheck::Unknown {
        reason: "denied".to_string(),
    };
    let p = plan(&res, &o);
    assert!(lanes_of(&p).pieces.iter().all(|lp| lp.lane == Lane::Keep));
    // Control: trusted, so the pieces are not all kept.
    o.process = ProcessCheck::Partial {
        cwds: vec![],
        unreadable: 2,
    };
    o.trust_partial_process_check = true;
    let p = plan(&res, &o);
    assert!(lanes_of(&p).pieces.iter().any(|lp| lp.lane != Lane::Keep));
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn neg10_one_fresh_file_keeps_its_whole_group() {
    let (_tmp, root, _) = workspace();
    let g = root.join("work");
    for i in 0..12 {
        write_bytes(&g.join("old").join(format!("{i:02}.dat")), 2000);
    }
    write(&g.join("fresh.txt"), "today\n");
    write_bytes(&root.join("other").join("a.dat"), 2000);
    let po = PieceOptions {
        max_piece_files: 5,
        ..PieceOptions::default()
    };
    let res = lanes_scan_with(&root, network_everywhere(), po);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // Make the `other` group idle by judging a year later for it alone is not possible, so
    // check the group rule directly: with a 7-day window and real now, `work` is active.
    let p = plan(&res, &PlanOptions::new(7, now));
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let work_pieces: Vec<&LanePiece> = l
        .pieces
        .iter()
        .filter(|p| Path::new(&p.group) == g)
        .collect();
    assert!(
        work_pieces.len() >= 3,
        "the group was split: {}",
        work_pieces.len()
    );
    for lp in work_pieces {
        assert_eq!(lp.lane, Lane::Keep, "{}", lp.path);
        assert!(lp.reason.contains("is active"), "{}", lp.reason);
    }
    // Control: far in the future every group is idle and the old pieces are backed up.
    let p = plan(&res, &idle_opts());
    let old = the_piece(&res, lanes_of(&p), &g.join("old").join("00.dat"));
    assert_eq!(old.lane, Lane::Backup);
}

#[test]
fn neg8_an_active_folder_gives_only_keep_pieces() {
    let (_tmp, root, remotes) = workspace();
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    pushed_repo(&work, &remotes, "app");
    write_bytes(&work.join("loose").join("a.bin"), 5000);
    let res = lanes_scan(&root);
    let mut o = idle_opts();
    o.active = vec![fs::canonicalize(&work).unwrap()];
    let p = plan(&res, &o);
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    for lp in l
        .pieces
        .iter()
        .filter(|lp| Path::new(&lp.path).starts_with(&work))
    {
        assert_eq!(lp.lane, Lane::Keep, "{}: {}", lp.path, lp.reason);
    }
}

// ----------------------------------------------------------------------------------------------
// AC-5: dedup.

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn dedup_upload_once_within_the_backup_lane() {
    let (_tmp, root, _) = workspace();
    write_bytes(&root.join("a").join("copy.bin"), 100 * KIB);
    write_bytes(&root.join("b").join("copy.bin"), 100 * KIB);
    write_bytes(&root.join("b").join("other.bin"), 90 * KIB);
    let res = lanes_scan(&root);
    let survey = res.pieces.as_ref().unwrap();
    assert_eq!(survey.blobs.len(), 1, "{:#?}", survey.blobs);
    assert_eq!(survey.blobs[0].copies.len(), 2);
    assert!(survey.totals.duplicate_bytes > 0);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let b = &l.blobs[0];
    assert_eq!(
        b.upload_path.as_deref(),
        Some(root.join("a").join("copy.bin").to_string_lossy().as_ref())
    );
    assert!(b.saved_bytes > 0);
    assert_eq!(l.totals.dedup_saved_bytes, b.saved_bytes);
    let backup = &l.totals.by_lane[&Lane::Backup];
    assert!(backup.upload_bytes < backup.bytes);
    // NEG-4: two copies that are both removed never cite each other.
    assert!(l.pieces.iter().all(|p| p.lane != Lane::Noise));
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn dedup_copy_of_a_file_in_a_kept_repo_is_noise() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "kept");
    write_bytes(&repo.join("data.bin"), 100 * KIB);
    write_bytes(&root.join("loose").join("data-copy.bin"), 100 * KIB);
    write_bytes(&root.join("loose").join("mine.bin"), 70 * KIB);
    let res = lanes_scan(&root);
    let mut o = idle_opts();
    // A git directory over this size asks, so the repo is kept (rule 6, durable).
    o.max_git_admin_bytes = 0;
    let p = plan(&res, &o);
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let rp = the_piece(&res, l, &repo.join("README.md"));
    assert_eq!(rp.lane, Lane::Keep, "{}", rp.reason);
    let copy = root.join("loose").join("data-copy.bin");
    let lp = the_piece(&res, l, &copy);
    assert!(lp.derived);
    assert_eq!(lp.lane, Lane::Noise);
    assert_eq!(lp.member.as_deref(), Some("data-copy.bin"));
    match lp.evidence.as_ref().unwrap() {
        Evidence::Duplicate {
            retained_copy,
            retained_piece,
            size,
            ..
        } => {
            assert_eq!(Path::new(retained_copy), repo.join("data.bin"));
            assert_eq!(*retained_piece, rp.id);
            assert_eq!(*size, 100 * KIB as u64);
        }
        e => panic!("duplicate evidence expected, got {e:?}"),
    }
    let parent = &l.pieces[lp.scan_piece];
    assert!(parent.moved_out.iter().any(|m| m == "data-copy.bin"));
    assert_eq!(parent.lane, Lane::Backup);
    assert_eq!(lp.upload_bytes, 0);

    // NEG-4: a copy in a piece kept only because it is active never makes another copy noise.
    let mut o = idle_opts();
    o.active = vec![fs::canonicalize(&repo).unwrap()];
    let p = plan(&res, &o);
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    assert!(
        l.pieces.iter().all(|p| !p.derived),
        "an active keep is not durable"
    );
    assert_eq!(the_piece(&res, l, &copy).lane, Lane::Backup);
}

/// Code inspection F1: a scan root with a secret directory name is a secret directory.
#[test]
fn neg1_a_secret_directory_scanned_as_the_root_stays_secret() {
    for name in ["secrets", ".aws"] {
        let tmp = real_tempdir();
        let root = tmp.path().join(name);
        write_bytes(&root.join("data.bin"), 100 * KIB);
        write_bytes(&root.join("sub").join("copy.bin"), 100 * KIB);
        let res = lanes_scan(&root);
        let p = plan(&res, &idle_opts());
        check_invariants(&res, &p);
        let l = lanes_of(&p);
        for f in [root.join("data.bin"), root.join("sub").join("copy.bin")] {
            let lp = the_piece(&res, l, &f);
            assert_eq!(lp.lane, Lane::Keep, "{name}: {}", f.display());
            assert!(lp.secret.is_some(), "{name}: {}", f.display());
        }
        assert!(
            res.pieces.as_ref().unwrap().blobs.is_empty(),
            "{name}: never hashed"
        );
    }
}

/// Code inspection F2: secrecy crosses the git-directory boundary in both directions and both
/// walk orders, and a secret inode is never hashed.
#[cfg(unix)]
#[test]
fn neg12_secret_inode_across_the_git_directory_boundary() {
    // The loose directory before and after the repository in walk order.
    for loose_name in ["a-loose", "z-loose"] {
        // A secret name inside the git directory, an ordinary name outside.
        let (_tmp, root, remotes) = workspace();
        let repo = pushed_repo(&root, &remotes, "m-repo");
        let loose = root.join(loose_name);
        fs::create_dir_all(&loose).unwrap();
        write_bytes(&repo.join(".git").join("credentials"), 100 * KIB);
        fs::hard_link(
            repo.join(".git").join("credentials"),
            loose.join("data.bin"),
        )
        .unwrap();
        let res = lanes_scan(&root);
        let p = plan(&res, &idle_opts());
        check_invariants(&res, &p);
        let l = lanes_of(&p);
        let lp = the_piece(&res, l, &loose.join("data.bin"));
        assert_eq!(lp.lane, Lane::Keep, "{loose_name}: {}", lp.reason);
        assert!(lp.secret.is_some(), "{loose_name}");
        let gp = the_piece(&res, l, &repo.join("README.md"));
        assert_eq!(gp.lane, Lane::Keep, "{loose_name}: {}", gp.reason);
        assert!(res.pieces.as_ref().unwrap().pieces[gp.scan_piece]
            .incomplete
            .is_some());
        assert!(
            res.pieces.as_ref().unwrap().blobs.is_empty(),
            "{loose_name}"
        );

        // A secret name in the work tree, an ordinary name inside the git directory.
        let (_tmp, root, remotes) = workspace();
        let repo = pushed_repo(&root, &remotes, "m-repo");
        let loose = root.join(loose_name);
        write_bytes(&loose.join(".env"), 100 * KIB);
        fs::hard_link(loose.join(".env"), repo.join(".git").join("data.bin")).unwrap();
        let res = lanes_scan(&root);
        let p = plan(&res, &idle_opts());
        check_invariants(&res, &p);
        let l = lanes_of(&p);
        let gp = the_piece(&res, l, &repo.join("README.md"));
        assert_eq!(gp.lane, Lane::Keep, "{loose_name}: {}", gp.reason);
        assert!(res.pieces.as_ref().unwrap().pieces[gp.scan_piece]
            .incomplete
            .is_some());
        assert!(the_piece(&res, l, &loose.join(".env")).secret.is_some());
        assert!(
            res.pieces.as_ref().unwrap().blobs.is_empty(),
            "{loose_name}"
        );
    }
}

/// Code inspection F4: a secret directory inside a git directory keeps its entries secret (the
/// git piece becomes incomplete) and passes secrecy to hard-link aliases outside, in both walk
/// orders; nothing of it is hashed.
#[cfg(unix)]
#[test]
fn neg12_secret_directory_inside_a_git_directory() {
    for dir_name in ["secrets", ".aws"] {
        for loose_name in ["a-loose", "z-loose"] {
            let (_tmp, root, remotes) = workspace();
            let repo = pushed_repo(&root, &remotes, "m-repo");
            let inner = repo.join(".git").join(dir_name);
            write_bytes(&inner.join("plain.bin"), 100 * KIB);
            write_bytes(&inner.join("other.bin"), 100 * KIB);
            let loose = root.join(loose_name);
            fs::create_dir_all(&loose).unwrap();
            fs::hard_link(inner.join("plain.bin"), loose.join("data.bin")).unwrap();
            let res = lanes_scan(&root);
            let p = plan(&res, &idle_opts());
            check_invariants(&res, &p);
            let l = lanes_of(&p);
            let what = format!("{dir_name} {loose_name}");
            let gp = the_piece(&res, l, &repo.join("README.md"));
            assert_eq!(gp.lane, Lane::Keep, "{what}: {}", gp.reason);
            let survey = res.pieces.as_ref().unwrap();
            assert!(survey.pieces[gp.scan_piece].incomplete.is_some(), "{what}");
            // The directory stays inside the git piece.
            assert_eq!(
                the_piece(&res, l, &inner.join("other.bin")).id,
                gp.id,
                "{what}"
            );
            let alias = the_piece(&res, l, &loose.join("data.bin"));
            assert_eq!(alias.lane, Lane::Keep, "{what}");
            assert!(alias.secret.is_some(), "{what}");
            assert!(survey.blobs.is_empty(), "{what}: {:#?}", survey.blobs);
        }
    }
}

/// Code inspection F5: a scan root spelled through a symlink to a directory is surveyed like
/// the main scan surveys it.
#[cfg(unix)]
#[test]
fn partition_a_symlinked_scan_root_is_surveyed() {
    let (tmp, root, _) = workspace();
    write_bytes(&root.join("notes").join("a.bin"), 5000);
    write(&root.join("top.txt"), "x");
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    let res = lanes_scan(&alias);
    assert!(res.totals.allocated_bytes > 0);
    // The survey records the root's real path, never the alias (AC-16 of the rescue,
    // goodqa#16529): apply opens every recorded path component by component.
    let real_root = fs::canonicalize(&alias).unwrap();
    check_partition(&real_root, &res);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let survey = res.pieces.as_ref().unwrap();
    assert!(!survey.pieces.is_empty());
    for sp in &survey.pieces {
        assert!(Path::new(&sp.path).starts_with(&real_root), "{}", sp.path);
        assert_eq!(recheck(sp, RecheckScope::All), Ok(()), "{}", sp.path);
    }
}

#[cfg(unix)]
#[test]
fn neg5_a_hardlink_kept_elsewhere_frees_nothing() {
    let (_tmp, root, _) = workspace();
    write_bytes(&root.join("a").join("big.bin"), 100 * KIB);
    // `.env` is secret, so its piece is kept; `a/big.bin` shares its inode.
    fs::create_dir_all(root.join("b")).unwrap();
    fs::hard_link(root.join("a").join("big.bin"), root.join("b").join(".env")).unwrap();
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    // A secret inode: every name is secret and kept, none hashed.
    let a = the_piece(&res, l, &root.join("a").join("big.bin"));
    assert_eq!(a.lane, Lane::Keep);
    assert!(a.secret.is_some());
    assert!(res.pieces.as_ref().unwrap().blobs.is_empty());
}

#[cfg(unix)]
#[test]
fn neg5_reclaim_leaves_out_an_inode_named_in_a_keep_piece() {
    // With and without hashing: an inode left unhashed (hash budget spent) still has its kept
    // name, so the accounting must not depend on blobs.
    for budget in [None, Some(0)] {
        neg5_reclaim_case(budget);
    }
}

#[cfg(unix)]
fn neg5_reclaim_case(max_hash_bytes: Option<u64>) {
    let (_tmp, root, _) = workspace();
    let idle = root.join("a-idle");
    let busy = root.join("b-busy");
    write_bytes(&idle.join("shared.bin"), 100 * KIB);
    fs::create_dir_all(&busy).unwrap();
    fs::hard_link(idle.join("shared.bin"), busy.join("shared-link.bin")).unwrap();
    let po = PieceOptions {
        max_hash_bytes,
        ..PieceOptions::default()
    };
    let res = lanes_scan_with(&root, network_everywhere(), po);
    if max_hash_bytes == Some(0) {
        assert!(
            res.pieces.as_ref().unwrap().blobs.is_empty(),
            "nothing hashed"
        );
    }
    let mut o = idle_opts();
    o.active = vec![fs::canonicalize(&busy).unwrap()];
    let p = plan(&res, &o);
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let kept = the_piece(&res, l, &busy.join("shared-link.bin"));
    assert_eq!(kept.lane, Lane::Keep);
    let removed = the_piece(&res, l, &idle.join("shared.bin"));
    assert_eq!(removed.lane, Lane::Backup);
    let survey = res.pieces.as_ref().unwrap();
    let owned: u64 = survey.pieces[removed.scan_piece]
        .members
        .iter()
        .find(|m| m.path() == "shared.bin")
        .unwrap()
        .bytes();
    assert!(owned >= 100 * KIB as u64, "the first name owns the bytes");
    // Only the directory's own blocks are freed: the kept name still holds the inode.
    assert_eq!(removed.reclaimable_bytes, removed.bytes - owned);
}

#[cfg(unix)]
#[test]
fn neg12_secret_hardlink_in_either_walk_order() {
    for other in ["-a.bin", "zz.bin"] {
        let (_tmp, root, _) = workspace();
        let d = root.join("d");
        write_bytes(&d.join(".env"), 100 * KIB);
        fs::hard_link(d.join(".env"), d.join(other)).unwrap();
        write_bytes(&d.join("same-size.bin"), 100 * KIB);
        let res = lanes_scan(&root);
        let p = plan(&res, &idle_opts());
        check_invariants(&res, &p);
        let l = lanes_of(&p);
        for name in [".env", other] {
            let lp = the_piece(&res, l, &d.join(name));
            assert_eq!(lp.lane, Lane::Keep, "{other}: {name}");
            assert!(lp.secret.is_some(), "{other}: {name}");
        }
        let survey = res.pieces.as_ref().unwrap();
        assert!(survey.blobs.is_empty(), "{other}: {:#?}", survey.blobs);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn neg11_sparse_file_is_left_unhashed() {
    let (_tmp, root, _) = workspace();
    let f = fs::File::create(root.join("sparse.bin")).unwrap();
    f.set_len(4 << 20).unwrap();
    drop(f);
    let f = fs::File::create(root.join("sparse2.bin")).unwrap();
    f.set_len(4 << 20).unwrap();
    drop(f);
    let res = lanes_scan(&root);
    let survey = res.pieces.as_ref().unwrap();
    assert!(survey.blobs.is_empty());
    assert_eq!(survey.totals.unhashed_candidates, 2);
}

// ----------------------------------------------------------------------------------------------
// NEG-9: symlinks, non-UTF-8 names, depth.

#[cfg(target_os = "linux")]
#[test]
fn neg9_non_utf8_name_makes_its_piece_incomplete() {
    use std::os::unix::ffi::OsStrExt;
    let (_tmp, root, _) = workspace();
    let d = root.join("d");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(std::ffi::OsStr::from_bytes(b"bad\xffname")), b"x").unwrap();
    write(&d.join("ok.txt"), "x");
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    let l = lanes_of(&p);
    // The bad name is its own incomplete piece, kept; it cannot be written back exactly.
    let survey = res.pieces.as_ref().unwrap();
    let bad: Vec<_> = survey
        .pieces
        .iter()
        .filter(|sp| sp.members.iter().any(|m| m.path().starts_with("bad")))
        .collect();
    assert_eq!(bad.len(), 1);
    assert!(
        bad[0].incomplete.as_deref().unwrap_or("").contains("UTF-8"),
        "{:?}",
        bad[0].incomplete
    );
    assert_eq!(bad[0].members.len(), 1);
    assert_eq!(l.pieces[bad[0].id].lane, Lane::Keep);
    // Control: its sibling with a valid name is judged normally.
    assert_eq!(the_piece(&res, l, &d.join("ok.txt")).lane, Lane::Backup);
}

#[cfg(unix)]
#[test]
fn neg9_a_deep_chain_is_cut_without_recursion() {
    // Built bottom-up with short relative renames, so no path handed to the OS while building is
    // long; the survey must neither recurse nor overflow its stack.
    let (_tmp, root, _) = workspace();
    let depth = 3000;
    let work = root.join("w");
    fs::create_dir_all(&work).unwrap();
    let mut chain = work.join("c0");
    fs::create_dir_all(&chain).unwrap();
    fs::write(chain.join("leaf.txt"), b"leaf").unwrap();
    for i in 1..depth {
        let next = work.join(format!("c{}", i % 2));
        let tmp = work.join("t");
        fs::create_dir(&tmp).unwrap();
        fs::rename(&chain, tmp.join("d")).unwrap();
        fs::rename(&tmp, &next).unwrap();
        chain = next;
    }
    fs::rename(&chain, root.join("deep")).unwrap();
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn({
            let root = root.clone();
            move || {
                disk_cleaner_core::pieces::survey(
                    &[root],
                    &[],
                    false,
                    &RuleSet::builtin(),
                    &PieceOptions::default(),
                )
            }
        })
        .unwrap();
    let survey = handle.join().expect("the survey ran on a small stack");
    // Whatever the OS let it read is partitioned; a part it could not read is incomplete.
    let sum: u64 = survey.pieces.iter().map(|p| p.bytes).sum();
    assert_eq!(sum, survey.totals.bytes);
    assert!(!survey.pieces.is_empty());
}

// ----------------------------------------------------------------------------------------------
// AC-6: recheck.

#[cfg(unix)]
#[test]
fn recheck_ok_when_unchanged_and_drift_on_every_change() {
    use std::os::unix::fs::MetadataExt;
    let (_tmp, root, remotes) = workspace();
    let d = root.join("d");
    write(&d.join("a.txt"), "aaaa");
    write(&d.join("b.txt"), "bbbb");
    write_bytes(&root.join("big.bin"), 100 * KIB);
    let repo = pushed_repo(&root, &remotes, "app");
    let res = lanes_scan(&root);
    let survey = res.pieces.as_ref().unwrap();
    for sp in &survey.pieces {
        assert_eq!(
            recheck(sp, RecheckScope::All),
            Ok(()),
            "unchanged piece {}",
            sp.path
        );
    }
    let piece_of = |abs: &Path| {
        survey
            .pieces
            .iter()
            .find(|p| {
                p.members
                    .iter()
                    .any(|m| Path::new(&p.path).join(m.path()) == abs)
            })
            .unwrap()
            .clone()
    };
    let tree = piece_of(&d.join("a.txt"));
    let gitp = piece_of(&repo.join("README.md"));

    // A same-length overwrite with its mtime put back: only the status-change time shows it.
    let a = d.join("a.txt");
    let before = fs::metadata(&a).unwrap();
    fs::write(&a, "AAAA").unwrap();
    let f = fs::OpenOptions::new().write(true).open(&a).unwrap();
    f.set_modified(before.modified().unwrap()).unwrap();
    drop(f);
    let after = fs::metadata(&a).unwrap();
    assert_eq!(after.mtime(), before.mtime());
    assert_eq!(after.len(), before.len());
    let e = recheck(&tree, RecheckScope::All).unwrap_err();
    assert_eq!(e.path, "a.txt", "{e}");

    // A rename over a member: a new inode.
    let tree2 = piece_of(&d.join("b.txt"));
    let _ = tree2;
    let fresh = lanes_scan(&root);
    let fs2 = fresh.pieces.as_ref().unwrap();
    let t = fs2
        .pieces
        .iter()
        .find(|p| {
            p.members
                .iter()
                .any(|m| Path::new(&p.path).join(m.path()) == d.join("b.txt"))
        })
        .unwrap()
        .clone();
    assert_eq!(recheck(&t, RecheckScope::All), Ok(()));
    write(&d.join("b.new"), "bbbb");
    fs::rename(d.join("b.new"), d.join("b.txt")).unwrap();
    let e = recheck(&t, RecheckScope::All).unwrap_err();
    assert_eq!(e.path, "b.txt", "{e}");

    // A new entry in a tree piece.
    let g = fs2
        .pieces
        .iter()
        .find(|p| {
            p.members
                .iter()
                .any(|m| Path::new(&p.path).join(m.path()) == repo.join("README.md"))
        })
        .unwrap()
        .clone();
    assert_eq!(recheck(&g, RecheckScope::All), Ok(()));
    // A new file inside the git directory: drift for All, fine for the work tree.
    write(&repo.join(".git").join("recovery-note"), "x");
    assert!(recheck(&g, RecheckScope::All).is_err());
    assert_eq!(recheck(&g, RecheckScope::WorkTree), Ok(()));
    // In the work tree it is drift for both.
    write(&repo.join("new.txt"), "x");
    let e = recheck(&g, RecheckScope::WorkTree).unwrap_err();
    assert_eq!(e.path, "new.txt", "{e}");
    let _ = gitp;
}

/// PR review F1: a new empty directory, at the top or nested, is drift.
#[cfg(unix)]
#[test]
fn recheck_a_new_empty_directory_is_drift() {
    let (_tmp, root, _) = workspace();
    let d = root.join("d");
    write(&d.join("a.txt"), "a");
    write(&d.join("sub").join("b.txt"), "b");
    let res = lanes_scan(&root);
    let survey = res.pieces.as_ref().unwrap();
    let tree = survey
        .pieces
        .iter()
        .find(|p| Path::new(&p.path) == d)
        .expect("d is one tree piece")
        .clone();
    assert_eq!(recheck(&tree, RecheckScope::All), Ok(()));
    fs::create_dir(d.join("sub").join("new-nested")).unwrap();
    let e = recheck(&tree, RecheckScope::All).unwrap_err();
    assert_eq!(e.path, "sub", "{e}");
    // A fresh scan, then a new empty directory directly in the piece's directory.
    let res = lanes_scan(&root);
    let tree = res
        .pieces
        .as_ref()
        .unwrap()
        .pieces
        .iter()
        .find(|p| Path::new(&p.path) == d)
        .unwrap()
        .clone();
    assert_eq!(recheck(&tree, RecheckScope::All), Ok(()));
    fs::create_dir(d.join("new-top")).unwrap();
    let e = recheck(&tree, RecheckScope::All).unwrap_err();
    assert_eq!(e.path, "", "{e}");
}

/// PR review F2: the work-tree scope of a bare repository leaves out all of it.
#[cfg(unix)]
#[test]
fn recheck_worktree_scope_of_a_bare_repo() {
    let (_tmp, root, remotes) = workspace();
    // The source and its remote live outside the scan root.
    let src = pushed_repo(&remotes, &remotes.join("r"), "src");
    let bare = root.join("mirror.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let res = lanes_scan(&root);
    let survey = res.pieces.as_ref().unwrap();
    let gp = survey
        .pieces
        .iter()
        .find(|p| Path::new(&p.path) == bare)
        .expect("the bare repo is a piece")
        .clone();
    assert_eq!(gp.kind, disk_cleaner_core::PieceKind::GitNode);
    assert_eq!(recheck(&gp, RecheckScope::All), Ok(()));
    // What apply's recovery step writes into a bare repository.
    write(&bare.join("refs").join("recovery").join("main"), "0000\n");
    assert!(recheck(&gp, RecheckScope::All).is_err());
    assert_eq!(recheck(&gp, RecheckScope::WorkTree), Ok(()));
}

#[cfg(unix)]
#[test]
fn recheck_scan_piece_with_a_derived_exclude_is_ok() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "kept");
    write_bytes(&repo.join("data.bin"), 100 * KIB);
    write_bytes(&root.join("loose").join("data-copy.bin"), 100 * KIB);
    write_bytes(&root.join("loose").join("mine.bin"), 70 * KIB);
    let res = lanes_scan(&root);
    let mut o = idle_opts();
    o.max_git_admin_bytes = 0;
    let p = plan(&res, &o);
    let l = lanes_of(&p);
    let derived = l
        .pieces
        .iter()
        .find(|p| p.derived)
        .expect("a derived piece");
    let sp = &res.pieces.as_ref().unwrap().pieces[derived.scan_piece];
    assert!(!l.pieces[sp.id].moved_out.is_empty());
    assert_eq!(recheck(sp, RecheckScope::All), Ok(()));
}

// ----------------------------------------------------------------------------------------------
// AC-7: the code serializes the documented field names.

/// Backticked names in the first column of every table row of `section` (up to the next `###`).
fn doc_table(doc: &str, heading: &str, from_row: Option<&str>) -> BTreeSet<String> {
    let start = doc
        .find(heading)
        .unwrap_or_else(|| panic!("SCAN-FORMAT.md has no heading {heading}"));
    let rest = &doc[start + heading.len()..];
    let end = rest.find("\n### ").unwrap_or(rest.len());
    let section = &rest[..end];
    let mut out = BTreeSet::new();
    let mut tables = 0;
    for line in section.lines() {
        let line = line.trim();
        if !line.starts_with('|') || line.starts_with("|---") || line.starts_with("| Field") {
            if line.starts_with("| Field") {
                tables += 1;
            }
            continue;
        }
        let cols: Vec<&str> = line.split('|').collect();
        if cols.len() < 3 {
            continue;
        }
        let first = cols[1];
        let wanted = match from_row {
            Some(row) => first.contains(&format!("`{row}`")),
            None => true,
        };
        if !wanted {
            continue;
        }
        let text = if from_row.is_some() { cols[2] } else { first };
        for (k, part) in text.split('`').enumerate() {
            if k % 2 == 1
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
            {
                out.insert(part.to_string());
            }
        }
    }
    assert!(tables > 0, "no field table under {heading}");
    out
}

fn keys(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_object().expect("object").keys().cloned().collect()
}

// Unix only: without file identity every lane is keep (asserted by the partition test).
#[cfg(unix)]
#[test]
fn contract_fields_documented() {
    let doc = include_str!("../../../docs/SCAN-FORMAT.md");
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "kept");
    write_bytes(&repo.join("data.bin"), 100 * KIB);
    write(&repo.join(".env"), "A=1\n");
    write_bytes(&root.join("loose").join("data-copy.bin"), 100 * KIB);
    write_bytes(&root.join("loose").join("mine.bin"), 70 * KIB);
    // A duplicate pair that stays in the backup lane, so a blob has an upload_path.
    write_bytes(&root.join("loose").join("pair.bin"), 90 * KIB);
    write_bytes(&root.join("other").join("pair.bin"), 90 * KIB);
    let res = lanes_scan(&root);
    let mut o = idle_opts();
    o.max_git_admin_bytes = 0;
    let p = plan(&res, &o);
    let scan_json = serde_json::to_value(&res).unwrap();
    let plan_json = serde_json::to_value(&p).unwrap();
    let pieces = &scan_json["pieces"];

    // Every serialized key is documented.
    let piece_doc = doc_table(doc, "### Scan field `pieces`", None);
    let survey_keys = keys(pieces);
    for k in &survey_keys {
        assert!(piece_doc.contains(k), "scan pieces.{k} is not documented");
    }
    let totals_doc = doc_table(doc, "### Scan field `pieces`", Some("totals"));
    for k in keys(&pieces["totals"]) {
        assert!(
            totals_doc.contains(&k),
            "scan pieces.totals.{k} is not documented"
        );
    }
    let mut piece_keys = BTreeSet::new();
    for sp in pieces["pieces"].as_array().unwrap() {
        piece_keys.extend(keys(sp));
    }
    for k in &piece_keys {
        assert!(
            piece_doc.contains(k),
            "scan piece field {k} is not documented"
        );
    }
    let lanes = &plan_json["lanes"];
    let lanes_doc = doc_table(doc, "### Plan field `lanes`", None);
    let mut lp_keys = BTreeSet::new();
    for lp in lanes["pieces"].as_array().unwrap() {
        lp_keys.extend(keys(lp));
    }
    let mut blob_keys = BTreeSet::new();
    let mut copy_keys = BTreeSet::new();
    for b in lanes["blobs"].as_array().unwrap() {
        blob_keys.extend(keys(b));
        for c in b["copies"].as_array().unwrap() {
            copy_keys.extend(keys(c));
        }
    }
    for k in keys(lanes).iter().chain(&lp_keys).chain(&blob_keys) {
        assert!(
            lanes_doc.contains(k),
            "plan lanes field {k} is not documented"
        );
    }
    let lane_totals_doc = doc_table(doc, "### Plan field `lanes`", Some("totals"));
    for k in keys(&lanes["totals"]) {
        assert!(
            lane_totals_doc.contains(&k),
            "plan lanes.totals.{k} is not documented"
        );
    }
    for (_, t) in lanes["totals"]["by_lane"].as_object().unwrap() {
        for k in keys(t) {
            assert!(
                lane_totals_doc.contains(&k),
                "by_lane field {k} is not documented"
            );
        }
    }
    let copies_doc = doc_table(doc, "### Plan field `lanes`", Some("copies"));
    for k in &copy_keys {
        assert!(
            copies_doc.contains(k),
            "plan blob copy field {k} is not documented"
        );
    }

    // Every documented field is serialized somewhere in this fixture (or is a field this
    // fixture cannot produce, listed here with why).
    let unproduced: BTreeSet<&str> = [
        "bytes_linked_outside", // needs a hard link from outside the roots
        "incomplete",           // needs an unreadable entry
        "evidence",             // checked below on the plan piece
        "git",                  // the repo is kept in this fixture
    ]
    .into_iter()
    .collect();
    let mut scan_blob_keys = BTreeSet::new();
    let mut scan_copy_keys = BTreeSet::new();
    for b in pieces["blobs"].as_array().unwrap() {
        scan_blob_keys.extend(keys(b));
        for c in b["copies"].as_array().unwrap() {
            scan_copy_keys.extend(keys(c));
        }
    }
    assert!(!scan_blob_keys.is_empty(), "the fixture has a blob");
    for k in &scan_blob_keys {
        assert!(
            piece_doc.contains(k),
            "scan blob field {k} is not documented"
        );
    }
    let scan_copies_doc = doc_table(doc, "### Scan field `pieces`", Some("copies"));
    for k in &scan_copy_keys {
        assert!(
            scan_copies_doc.contains(k),
            "scan blob copy field {k} is not documented"
        );
    }
    let all_scan: BTreeSet<String> = survey_keys
        .union(&piece_keys)
        .cloned()
        .chain(scan_blob_keys.iter().cloned())
        .collect();
    for k in &piece_doc {
        if !unproduced.contains(k.as_str()) && !all_scan.contains(k) {
            panic!("documented scan field {k} is never serialized");
        }
    }
    let all_plan: BTreeSet<String> = keys(lanes)
        .union(&lp_keys)
        .cloned()
        .chain(blob_keys.iter().cloned())
        .collect();
    for k in &lanes_doc {
        if !unproduced.contains(k.as_str()) && !all_plan.contains(k) {
            panic!("documented plan field {k} is never serialized");
        }
    }
    assert!(lp_keys.contains("evidence"));
}

#[test]
fn contract_digest_binds_the_plan_to_the_scan() {
    let (_tmp, root, _) = workspace();
    write_bytes(&root.join("a").join("x.bin"), 5000);
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    let l = lanes_of(&p);
    let survey = res.pieces.as_ref().unwrap();
    assert_eq!(
        l.scan_pieces_sha256,
        disk_cleaner_core::pieces::pieces_digest(survey)
    );
    assert_eq!(l.version, 1);
    // Without --lanes there is no survey and no lanes section.
    let plain = scan_plain(&root);
    assert!(plain.pieces.is_none());
    assert!(plan(&plain, &idle_opts()).lanes.is_none());
    let text = serde_json::to_string(&plan(&plain, &idle_opts())).unwrap();
    assert!(!text.contains("\"lanes\""));
}

/// A survey of `roots` (outer and retained inner roots, as the scan keeps them when the inner one
/// is another file system under `--one-file-system`) put in place of the scan's own survey.
fn with_survey(mut res: ScanResult, roots: &[PathBuf]) -> ScanResult {
    let s = disk_cleaner_core::pieces::survey(
        roots,
        &res.nodes,
        false,
        &RuleSet::builtin(),
        &PieceOptions::default(),
    );
    res.pieces = Some(s);
    res
}

/// PR review 3, F1: a scan root walked on its own inside a secret directory stays secret, in
/// either root order and with or without the enclosing root, and none of it is hashed. The real
/// mount case runs in CI (`mount_secret_root.rs`); here the outer walk skips the inner root
/// exactly as it does for a retained root.
#[test]
fn neg1_a_scan_root_inside_a_secret_directory_stays_secret() {
    for secret in ["secrets", ".aws"] {
        let (_tmp, root, _) = workspace();
        let root = fs::canonicalize(&root).unwrap();
        let inner = root.join(secret).join("mnt");
        write_bytes(&inner.join("a.bin"), 100 * KIB);
        write_bytes(&inner.join("b.bin"), 100 * KIB);
        // Outside the secret directory, two more copies: hashing does run.
        write_bytes(&root.join("loose").join("c.bin"), 100 * KIB);
        write_bytes(&root.join("loose").join("d.bin"), 100 * KIB);
        let base = lanes_scan(&root);
        let orders: [Vec<PathBuf>; 3] = [
            vec![root.clone(), inner.clone()],
            vec![inner.clone(), root.clone()],
            vec![inner.clone()],
        ];
        for roots in orders {
            let res = with_survey(base.clone(), &roots);
            let survey = res.pieces.as_ref().unwrap();
            let inner_pieces: Vec<_> = survey
                .pieces
                .iter()
                .filter(|p| Path::new(&p.path).starts_with(&inner))
                .collect();
            assert!(!inner_pieces.is_empty(), "{secret} {roots:?}");
            for p in &inner_pieces {
                assert!(p.secret.is_some(), "{secret} {roots:?}: {p:#?}");
            }
            for b in &survey.blobs {
                for c in &b.copies {
                    assert!(
                        !Path::new(&c.path).starts_with(root.join(secret)),
                        "{secret} {roots:?}: {} was hashed",
                        c.path
                    );
                }
            }
            if roots.len() == 2 {
                assert_eq!(
                    survey.blobs.len(),
                    1,
                    "{secret} {roots:?}: the loose copies"
                );
            }
            let p = plan(&res, &idle_opts());
            for f in [inner.join("a.bin"), inner.join("b.bin")] {
                let lp = the_piece(&res, lanes_of(&p), &f);
                assert_eq!(lp.lane, Lane::Keep, "{secret} {roots:?}: {}", f.display());
                assert!(lp.secret.is_some(), "{secret} {roots:?}: {}", f.display());
            }
        }
    }
}

/// PR review 3, F1: a retained inner root takes the node of the directory that encloses it, so
/// its pieces are judged with that node rather than as an ungrouped root.
#[test]
fn partition_a_retained_inner_root_inherits_its_enclosing_node() {
    let (_tmp, root, _) = workspace();
    let root = fs::canonicalize(&root).unwrap();
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("node_modules").join("x").join("index.js"), "x");
    let inner = proj.join("node_modules").join("mnt");
    write_bytes(&inner.join("a.bin"), 10 * KIB);
    let base = lanes_scan(&root);
    let nm = base
        .nodes
        .iter()
        .position(|n| Path::new(&n.path) == proj.join("node_modules"))
        .expect("node_modules is a node");
    for roots in [
        vec![root.clone(), inner.clone()],
        vec![inner.clone(), root.clone()],
    ] {
        let res = with_survey(base.clone(), &roots);
        let survey = res.pieces.as_ref().unwrap();
        let p = survey
            .pieces
            .iter()
            .find(|p| {
                p.members
                    .iter()
                    .any(|m| Path::new(&p.path).join(&m.0) == inner.join("a.bin"))
            })
            .expect("a.bin is in a piece");
        assert_eq!(p.node, Some(nm), "{roots:?}: {p:#?}");
        // Counted once: the outer walk does not enter the inner root.
        let n = survey
            .pieces
            .iter()
            .flat_map(|p| p.members.iter().map(move |m| Path::new(&p.path).join(&m.0)))
            .filter(|m| *m == inner.join("a.bin"))
            .count();
        assert_eq!(n, 1, "{roots:?}");
    }
}

/// Stream B probe (main triage USER_EXPERIENCE): secret words match whole name segments of a
/// secret-shaped name, never parts of ordinary source, document or branch names. A guard over a
/// corpus: every negative must stay non-secret and every positive secret.
#[test]
fn neg1_secret_words_match_secret_shaped_names_only() {
    let rules = RuleSet::builtin();
    let not_secret = [
        "tokenizer.rs",
        "tokenizer.js",
        "tokenize.py",
        "tokens.ts",
        "tokens.json",
        "token.rs",
        "token.go",
        "TokenService.java",
        "fix-token-refresh",
        "token-refresh.md",
        "secret_santa.md",
        "secretary.txt",
        "password_reset.py",
        "PasswordField.tsx",
        "passwordless.md",
        "credentials_test.go",
        "README.md",
        "Cargo.lock",
    ];
    for n in not_secret {
        assert_eq!(rules.secret_file(n, false), None, "{n} is not a secret");
    }
    let secret = [
        ".env",
        ".env.local",
        "api_token.txt",
        "access-token",
        "token",
        "token.json",
        ".token",
        "deploy.token",
        "github_token",
        "auth-token-cache.json",
        "client_secret.json",
        "clientSecret.json",
        "secrets.yaml",
        "secrets.py",
        "credentials",
        "credentials.ts",
        "db_password",
        "passwords.txt",
        ".htpasswd",
        "id_rsa",
        "server.key",
        "cert.pem",
    ];
    for n in secret {
        assert!(rules.secret_file(n, false).is_some(), "{n} is a secret");
    }
    // Inside an artifact directory words never apply; exact names still do.
    assert_eq!(rules.secret_file("api_token.txt", true), None);
    assert!(rules.secret_file(".env", true).is_some());
}

/// Stream B probe: a pushed, idle repository whose branches are named `fix-token-refresh`,
/// `secrets/rotate` and `password-reset`, with a tracked `src/tokenizer.rs`, is an ordinary git
/// lane repository, with loose and with packed refs. Git's bookkeeping names (refs, reflogs,
/// packed-refs) are branch names, not secrets.
#[cfg(unix)]
#[test]
fn neg1_branch_names_and_source_names_are_not_secrets() {
    for pack in [false, true] {
        let (_tmp, root, remotes) = workspace();
        let repo = pushed_repo(&root, &remotes, "app");
        commit_file(&repo, "src/tokenizer.rs", "fn main() {}\n");
        for b in ["fix-token-refresh", "secrets/rotate", "password-reset"] {
            git(&repo, &["branch", b]);
        }
        git(&repo, &["push", "-q", "origin", "--all"]);
        if pack {
            git(&repo, &["pack-refs", "--all"]);
        }
        let res = lanes_scan(&root);
        let p = plan(&res, &idle_opts());
        check_invariants(&res, &p);
        let l = lanes_of(&p);
        let lp = the_piece(&res, l, &repo.join("README.md"));
        assert_eq!(lp.kind, disk_cleaner_core::PieceKind::GitNode);
        assert!(lp.secret.is_none(), "pack {pack}: {lp:#?}");
        assert_eq!(lp.lane, Lane::Git, "pack {pack}: {}", lp.reason);
        assert_eq!(
            the_piece(&res, l, &repo.join("src").join("tokenizer.rs")).id,
            lp.id,
            "pack {pack}: tokenizer.rs is ordinary source"
        );
        if !pack {
            let r = repo.join(".git/refs/heads/secrets/rotate");
            assert!(r.exists());
            assert_eq!(the_piece(&res, l, &r).id, lp.id);
        }
    }
}

/// PR review 3, F1, the other two inherited contexts. A scan root inside a git directory: a
/// secret-named file there is never in a removing lane (the piece holding it is incomplete). A
/// scan root inside an artifact directory: exact secret names still apply, secret words do not,
/// exactly as when the enclosing walk reaches the same files.
#[cfg(unix)]
#[test]
fn neg1_a_scan_root_inside_a_git_or_artifact_directory_keeps_its_context() {
    let (_tmp, root, remotes) = workspace();
    let root = fs::canonicalize(&root).unwrap();
    let repo = pushed_repo(&root, &remotes, "app");
    let in_git = repo.join(".git").join("mnt");
    write(&in_git.join("id_rsa"), "k");
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("node_modules").join("x").join("index.js"), "x");
    let in_art = proj.join("node_modules").join("mnt");
    write(&in_art.join("id_rsa"), "k");
    write(&in_art.join("api_token.txt"), "t");
    let base = lanes_scan(&root);
    let orders: [Vec<PathBuf>; 4] = [
        vec![root.clone(), in_git.clone(), in_art.clone()],
        vec![in_art.clone(), in_git.clone(), root.clone()],
        vec![in_git.clone()],
        vec![in_art.clone()],
    ];
    for roots in orders {
        let res = with_survey(base.clone(), &roots);
        let p = plan(&res, &idle_opts());
        if roots.contains(&root) {
            // Byte totals compare with the scan, which walked the outer root.
            check_invariants(&res, &p);
        }
        let l = lanes_of(&p);
        if roots.contains(&in_git) {
            for lp in pieces_holding(&res, l, &in_git.join("id_rsa")) {
                assert!(!lp.lane.removes(), "{roots:?}: {lp:#?}");
            }
        }
        if roots.contains(&in_art) {
            let k = the_piece(&res, l, &in_art.join("id_rsa"));
            assert!(k.secret.is_some(), "{roots:?}: {k:#?}");
            assert_eq!(k.lane, Lane::Keep, "{roots:?}");
            let t = the_piece(&res, l, &in_art.join("api_token.txt"));
            assert!(
                t.secret.is_none(),
                "{roots:?}: words do not apply in artifacts"
            );
        }
    }
}

/// Advisor check on the secret words: a tracked file whose whole stem is a secret word in a code
/// file (`credentials.go`) is its own secret keep piece, and the repository stays in the git lane.
/// The git exemption is anchored to git's layout: a secret name elsewhere in the git directory,
/// even below a directory called `logs` or `refs` that is not a git top's, still makes the git
/// piece incomplete; a linked worktree named `secrets` does not.
#[cfg(unix)]
#[test]
fn neg12_git_bookkeeping_exemption_is_anchored_to_the_git_layout() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "app");
    commit_file(&repo, "aws/credentials.go", "package aws\n");
    git(&repo, &["push", "-q", "origin", "--all"]);
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let l = lanes_of(&p);
    let lp = the_piece(&res, l, &repo.join("README.md"));
    assert_eq!(lp.lane, Lane::Git, "{}", lp.reason);
    let c = the_piece(&res, l, &repo.join("aws").join("credentials.go"));
    assert_ne!(c.id, lp.id);
    assert!(c.secret.is_some());
    assert_eq!(c.lane, Lane::Keep);

    // Not a git top's `logs`: the exact rules still apply.
    let (_tmp2, root2, remotes2) = workspace();
    let repo2 = pushed_repo(&root2, &remotes2, "app");
    write(
        &repo2
            .join(".git")
            .join("info")
            .join("logs")
            .join("server.pem"),
        "k",
    );
    let res2 = lanes_scan(&root2);
    let survey2 = res2.pieces.as_ref().unwrap();
    let g = survey2
        .pieces
        .iter()
        .find(|p| p.kind == disk_cleaner_core::PieceKind::GitNode)
        .unwrap();
    assert!(
        g.incomplete.as_deref().unwrap_or("").contains("server.pem"),
        "{g:#?}"
    );

    // A linked worktree named `secrets` (its admin directory is .git/worktrees/secrets).
    let (_tmp3, root3, remotes3) = workspace();
    let repo3 = pushed_repo(&root3, &remotes3, "app");
    let wt = remotes3.join("secrets");
    git(
        &repo3,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "rotate",
        ],
    );
    assert!(repo3.join(".git/worktrees/secrets/HEAD").exists());
    let res3 = lanes_scan(&root3);
    let survey3 = res3.pieces.as_ref().unwrap();
    let g3 = survey3
        .pieces
        .iter()
        .find(|p| p.kind == disk_cleaner_core::PieceKind::GitNode)
        .unwrap();
    assert!(g3.secret.is_none(), "{g3:#?}");
    assert!(
        !g3.incomplete.as_deref().unwrap_or("").contains("secret"),
        "{g3:#?}"
    );
}

/// The exemption covers a linked worktree's name, not the files in its admin directory: a
/// secret-named file directly inside `.git/worktrees/<name>` makes the git piece incomplete.
#[cfg(unix)]
#[test]
fn neg12_a_secret_file_in_a_worktree_admin_directory_keeps_the_repo() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "app");
    let wt = remotes.join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "feature",
        ],
    );
    let admin = repo.join(".git").join("worktrees").join("feature");
    assert!(admin.join("HEAD").exists());
    write(&admin.join("id_rsa"), "k");
    let res = lanes_scan(&root);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let survey = res.pieces.as_ref().unwrap();
    let g = survey
        .pieces
        .iter()
        .find(|p| p.kind == disk_cleaner_core::PieceKind::GitNode)
        .unwrap();
    assert!(
        g.incomplete.as_deref().unwrap_or("").contains("id_rsa"),
        "{g:#?}"
    );
    for lp in pieces_holding(&res, lanes_of(&p), &admin.join("id_rsa")) {
        assert!(!lp.lane.removes(), "{lp:#?}");
    }
}

/// A scan root inside the git directory of a repository the scan does not cover, through the
/// real scan (no node for that repository): git-directory context comes from the directories
/// themselves, so nothing there is backed up and removed on its own. Also a root inside a bare
/// repository, and the git directory scanned as the root itself.
#[cfg(unix)]
#[test]
fn neg12_a_scan_root_inside_an_uncovered_git_directory_is_kept() {
    let (_tmp, root, remotes) = workspace();
    let repo = pushed_repo(&root, &remotes, "app");
    let mnt = repo.join(".git").join("mnt");
    write(&mnt.join("plain.txt"), "p");
    write(&mnt.join("id_rsa"), "k");
    let bare_objects = remotes.join("app.git").join("objects");
    assert!(bare_objects.is_dir());
    for scan_root in [mnt.clone(), bare_objects.clone(), repo.join(".git")] {
        let res = lanes_scan(&scan_root);
        let p = plan(&res, &idle_opts());
        // Includes: no removing piece other than a git-node piece lies inside a git directory.
        check_invariants(&res, &p);
        if scan_root == repo.join(".git") {
            continue;
        }
        let l = lanes_of(&p);
        let survey = res.pieces.as_ref().unwrap();
        assert!(!survey.pieces.is_empty(), "{}", scan_root.display());
        for lp in &l.pieces {
            assert!(!lp.lane.removes(), "{}: {lp:#?}", scan_root.display());
            let sp = &survey.pieces[lp.scan_piece];
            assert!(
                sp.incomplete
                    .as_deref()
                    .unwrap_or("")
                    .contains("git directory"),
                "{}: {sp:#?}",
                scan_root.display()
            );
        }
    }
}

/// Another scan root inside a repository's git directory (a mount under `.git` kept as its own
/// root under `--one-file-system`): the git piece is incomplete, in either root order, exactly as
/// when the walk meets that mount without entering it. Scanned alone, the same ordinary
/// directory is part of a whole git piece.
#[cfg(unix)]
#[test]
fn neg12_another_scan_root_inside_a_git_directory_makes_it_incomplete() {
    let (_tmp, root, remotes) = workspace();
    let root = fs::canonicalize(&root).unwrap();
    let repo = pushed_repo(&root, &remotes, "app");
    let mnt = repo.join(".git").join("mnt");
    write(&mnt.join("plain.txt"), "p");
    let base = lanes_scan(&root);
    let git_piece = |res: &ScanResult| {
        res.pieces
            .as_ref()
            .unwrap()
            .pieces
            .iter()
            .find(|p| p.kind == disk_cleaner_core::PieceKind::GitNode)
            .cloned()
            .unwrap()
    };
    let whole = git_piece(&base);
    assert!(whole.incomplete.is_none(), "{whole:#?}");
    for roots in [
        vec![root.clone(), mnt.clone()],
        vec![mnt.clone(), root.clone()],
    ] {
        let res = with_survey(base.clone(), &roots);
        let g = git_piece(&res);
        assert!(
            g.incomplete.as_deref().unwrap_or("").contains("mnt"),
            "{roots:?}: {g:#?}"
        );
        let p = plan(&res, &idle_opts());
        for lp in pieces_holding(&res, lanes_of(&p), &mnt.join("plain.txt")) {
            assert!(!lp.lane.removes(), "{roots:?}: {lp:#?}");
        }
    }
}

// ----------------------------------------------------------------------------------------------
// The class guard for context by location (PR review a2-3, F1-F3, and every earlier instance:
// CI3 F1/F2, CI4 F4/F5, PR3 F1, the .git root case). Layouts are composed from a context kind
// placed at an ancestor of the scan root, at the root itself, behind a symlink alias that is the
// root, below the root, and as a retained inner root; each is scanned with the real scan and
// `--lanes`, and checked against an oracle that reads only the disk.

/// A directory that gives what lies below it a context.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// No context: the control, which must give removing pieces.
    Plain,
    Secrets,
    Aws,
    /// A repository's `.git`.
    DotGit,
    /// A bare repository.
    Bare,
    /// A `.git` inside an opaque artifact (`node_modules/dep/.git`): never a git node.
    ArtifactGit,
    /// A directory named `.GIT`, with nothing in it: the name in another case (a git directory on
    /// a case-insensitive volume) is enough.
    UpperGit,
    /// A component that is not valid UTF-8 (Linux only: macOS file systems refuse such names).
    Invalid,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pos {
    /// The scan root is a directory inside the context directory.
    Ancestor,
    /// The scan root is the context directory.
    Root,
    /// The scan root is an ordinary-named symlink to the context directory.
    Alias,
    /// The context directory is below the scan root.
    Descendant,
}

/// Make the context directory of `kind` under `base` and return it.
#[cfg(unix)]
fn make_context(base: &Path, kind: Kind) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let d = match kind {
        Kind::Plain => base.join("plain"),
        Kind::Secrets => base.join("secrets"),
        Kind::Aws => base.join(".aws"),
        Kind::DotGit => {
            init_repo(&base.join("repo"));
            base.join("repo").join(".git")
        }
        Kind::Bare => {
            init_bare(&base.join("bare.git"));
            base.join("bare.git")
        }
        Kind::ArtifactGit => {
            let proj = base.join("proj");
            write(&proj.join("package.json"), "{}");
            let dep = proj.join("node_modules").join("dep");
            write(&dep.join("index.js"), "x");
            init_repo(&dep);
            dep.join(".git")
        }
        Kind::UpperGit => base.join("up").join(".GIT"),
        Kind::Invalid => base.join(std::ffi::OsStr::from_bytes(b"bad\xffdir")),
    };
    fs::create_dir_all(&d).unwrap();
    d
}

/// Files that tempt every lane: two equal copies (hashing), a file in a subdirectory, and a
/// subtree of empty directories (a piece with no file members).
#[cfg(unix)]
fn fill(dir: &Path) {
    write_bytes(&dir.join("a.bin"), 100 * KIB);
    write_bytes(&dir.join("b.bin"), 100 * KIB);
    write_bytes(&dir.join("sub").join("c.bin"), 100 * KIB);
    write(&dir.join("notes.txt"), "n");
    fs::create_dir_all(dir.join("hollow").join("deeper")).unwrap();
    // CT-A1: names that are not valid UTF-8, one per secret rule kind and an ordinary one, each
    // with the same contents as a.bin, so that reading one shows as a blob copy.
    if cfg!(target_os = "linux") {
        use std::os::unix::ffi::OsStrExt;
        for n in INVALID_NAMES {
            write_bytes(&dir.join(std::ffi::OsStr::from_bytes(n)), 100 * KIB);
        }
    }
}

/// The invalid-byte names `fill` writes: a prefix, a suffix, the `.env.*` pattern with
/// exceptions, two secret words, and an ordinary name (never hashed either).
#[cfg(unix)]
const INVALID_NAMES: [&[u8]; 6] = [
    b"id_rsa\xff",
    b"\xff.pem",
    b".env.\xff",
    b"api\xfftoken",
    b"\xff_secret.txt",
    b"plain\xff.bin",
];

/// The secret ones among [`INVALID_NAMES`].
#[cfg(unix)]
fn invalid_name_is_secret(n: &[u8]) -> bool {
    n != b"plain\xff.bin"
}

/// CT-A1 over one scanned root: every file below it (as spelled) named in [`INVALID_NAMES`] lies
/// in a survey piece; a secret one outside a git directory is in a piece marked secret, and in a
/// git directory in a piece that is never removed. (That none is hashed is checked over the
/// blobs in [`location_violations`].)
#[cfg(unix)]
fn invalid_name_violations(label: &str, res: &ScanResult, p: &Plan, root: &Path) -> Vec<String> {
    use std::os::unix::ffi::OsStrExt;
    let mut found: Vec<PathBuf> = Vec::new();
    // The scan records real paths (AC-16 of the rescue, goodqa#16529), so look under the root's.
    let mut stack = vec![fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() && INVALID_NAMES.contains(&e.file_name().as_bytes()) {
                found.push(e.path());
            }
        }
    }
    let survey = res.pieces.as_ref().unwrap();
    let l = lanes_of(p);
    let mut v = Vec::new();
    for f in &found {
        let lossy = f.to_string_lossy();
        let holder = survey.pieces.iter().enumerate().find(|(_, sp)| {
            sp.path == lossy
                || sp
                    .members
                    .iter()
                    .any(|m| Path::new(&sp.path).join(m.path()).to_string_lossy() == lossy)
        });
        let secret = invalid_name_is_secret(f.file_name().unwrap().as_bytes());
        // `f` is found under the root's real path; the root's spelling adds its context too.
        let in_git = oracle_in_gitdir(f.parent().unwrap()) || oracle_in_gitdir(root);
        match holder {
            None if in_git => {}
            None => v.push(format!("{label}: {lossy} is in no survey piece")),
            Some((i, sp)) => {
                let lane = l
                    .pieces
                    .iter()
                    .find(|lp| lp.scan_piece == i)
                    .map(|lp| lp.lane);
                if lane.map(|x| x.removes()).unwrap_or(false) {
                    v.push(format!(
                        "{label}: {lossy} is in removing piece {i} ({lane:?})"
                    ));
                }
                if secret && !in_git && sp.secret.is_none() {
                    v.push(format!("{label}: {lossy} is not marked secret (piece {i})"));
                }
            }
        }
    }
    v
}

/// Is any directory at or above `p` (as spelled, and as real) a secret directory by name?
#[cfg(unix)]
fn oracle_secret(p: &Path) -> bool {
    let rules = RuleSet::builtin();
    let real = p
        .parent()
        .and_then(|d| fs::canonicalize(d).ok())
        .unwrap_or_default();
    [p.parent().unwrap_or(p).to_path_buf(), real]
        .iter()
        .any(|q| {
            q.components().any(|c| {
                c.as_os_str()
                    .to_str()
                    .map(|n| rules.secret_dir(n).is_some())
                    .unwrap_or(false)
            })
        })
}

/// Is `dir`, or any directory above it on its real path, a git directory on disk (named `.git`
/// in any case, or holding HEAD, objects and refs), or named `.git` as spelled?
#[cfg(unix)]
fn oracle_in_gitdir(dir: &Path) -> bool {
    let spelled_git = dir
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case(".git"));
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    spelled_git
        || real.ancestors().any(|a| {
            let is = |n: &str, d: bool| {
                fs::symlink_metadata(a.join(n))
                    .map(|m| if d { m.is_dir() } else { m.is_file() })
                    .unwrap_or(false)
            };
            a.file_name()
                .map(|n| n.eq_ignore_ascii_case(".git"))
                .unwrap_or(false)
                || (is("HEAD", false) && is("objects", true) && is("refs", true))
        })
}

/// The three invariants, over every removing plan piece and every blob. Returns every violation
/// (so one run shows every layout that fails, not only the first).
#[cfg(unix)]
fn location_violations(label: &str, res: &ScanResult, p: &Plan) -> Vec<String> {
    let mut v = Vec::new();
    let survey = res.pieces.as_ref().unwrap();
    let l = lanes_of(p);
    for lp in l.pieces.iter().filter(|lp| lp.lane.removes()) {
        let sp = &survey.pieces[lp.scan_piece];
        let anchor = Path::new(&sp.path);
        // A removing piece identifies its files exactly: its path and every member read back.
        if sp.path.contains('\u{FFFD}') || fs::symlink_metadata(anchor).is_err() {
            v.push(format!(
                "{label}: removing piece {} has a path that does not read back: {}",
                lp.id, sp.path
            ));
            continue;
        }
        if sp.kind != disk_cleaner_core::PieceKind::GitNode && oracle_in_gitdir(anchor) {
            v.push(format!(
                "{label}: removing {:?} piece {} lies inside a git directory: {}",
                sp.kind, lp.id, sp.path
            ));
        }
        // The anchor itself, so a piece with no file members (empty directories only) counts.
        if oracle_secret(&anchor.join("_")) {
            v.push(format!(
                "{label}: removing {:?} piece {} is in a secret directory: {}",
                sp.kind, lp.id, sp.path
            ));
        }
        for m in &sp.members {
            if lp.moved_out.iter().any(|x| x == m.path()) {
                continue;
            }
            let full = anchor.join(m.path());
            if fs::symlink_metadata(&full).is_err() {
                v.push(format!(
                    "{label}: member {} does not read back",
                    full.display()
                ));
            }
            if oracle_secret(&full) {
                v.push(format!(
                    "{label}: {} is in a secret directory and in removing piece {} ({:?})",
                    full.display(),
                    lp.id,
                    lp.lane
                ));
            }
            if sp.kind != disk_cleaner_core::PieceKind::GitNode
                && oracle_in_gitdir(full.parent().unwrap())
            {
                v.push(format!(
                    "{label}: {} is inside a git directory and in removing piece {}",
                    full.display(),
                    lp.id
                ));
            }
        }
    }
    // The survey contract, whatever lane a piece gets: a piece anchored in a secret directory
    // (outside a git directory) is marked secret, empty or not.
    for (i, sp) in survey.pieces.iter().enumerate() {
        let anchor = Path::new(&sp.path);
        if sp.secret.is_none()
            && fs::symlink_metadata(anchor).is_ok()
            && oracle_secret(&anchor.join("_"))
            && !oracle_in_gitdir(anchor)
        {
            v.push(format!(
                "{label}: survey piece {i} is in a secret directory but not marked secret: {}",
                sp.path
            ));
        }
    }
    for b in &survey.blobs {
        for c in &b.copies {
            // CT-A1: a file reachable by a name that is not valid UTF-8 is never read.
            if c.path.contains('\u{FFFD}') {
                v.push(format!(
                    "{label}: {} has a name that is not valid UTF-8 and was hashed",
                    c.path
                ));
            }
            if oracle_secret(Path::new(&c.path)) {
                v.push(format!(
                    "{label}: {} is in a secret directory and was hashed",
                    c.path
                ));
            }
        }
    }
    v
}

/// The real scan with `--lanes` of `roots`, without the fixture-age wait (the caller waits once).
#[cfg(unix)]
fn scan_now(roots: &[PathBuf]) -> ScanResult {
    let mut opts = ScanOptions::new(roots.to_vec());
    opts.facts = network_everywhere();
    opts.git_jobs = 2;
    opts.lanes = Some(PieceOptions::default());
    scan(&opts).expect("scan")
}

#[cfg(unix)]
#[test]
fn guard_context_follows_location_over_composed_layouts() {
    let mut kinds = vec![
        Kind::Plain,
        Kind::Secrets,
        Kind::Aws,
        Kind::DotGit,
        Kind::Bare,
        Kind::ArtifactGit,
        Kind::UpperGit,
    ];
    if cfg!(target_os = "linux") {
        kinds.push(Kind::Invalid);
    }
    let positions = [Pos::Ancestor, Pos::Root, Pos::Alias, Pos::Descendant];
    let tmp = tempfile::tempdir().unwrap();
    let top = fs::canonicalize(tmp.path()).unwrap();
    // Build every layout first, then wait once so that every fixture predates the walks.
    let mut cases: Vec<(String, Kind, Pos, PathBuf, PathBuf)> = Vec::new();
    for &kind in &kinds {
        for &pos in &positions {
            let base = top.join(format!("{kind:?}-{pos:?}"));
            fs::create_dir_all(&base).unwrap();
            let ctx = make_context(&base, kind);
            let (root, inner) = match pos {
                Pos::Ancestor => {
                    let inner = ctx.join("inner");
                    fill(&inner);
                    (inner.clone(), inner)
                }
                Pos::Root => {
                    fill(&ctx);
                    (ctx.clone(), ctx.join("sub"))
                }
                Pos::Alias => {
                    fill(&ctx);
                    let alias = base.join("alias");
                    std::os::unix::fs::symlink(&ctx, &alias).unwrap();
                    (alias, ctx.join("sub"))
                }
                Pos::Descendant => {
                    fill(&ctx);
                    // A control file outside the context, so the scan has removable work.
                    write_bytes(&base.join("loose").join("z.bin"), 10 * KIB);
                    (base.clone(), ctx.join("sub"))
                }
            };
            cases.push((format!("{kind:?} at {pos:?}"), kind, pos, root, inner));
        }
    }
    // A secret directory holding only empty directories: its piece has no file member, so only
    // the directory's own context makes it secret.
    let base = top.join("EmptySecrets");
    fs::create_dir_all(base.join("secrets").join("hollow").join("deeper")).unwrap();
    write_bytes(&base.join("loose").join("z.bin"), 10 * KIB);
    cases.push((
        "empty Secrets below the root".to_string(),
        Kind::Secrets,
        Pos::Root,
        base.clone(),
        base.join("secrets"),
    ));
    // The spelled path alone carries the context: a root that is a symlink with a context name
    // (`secrets`, `.git`, a name that is not UTF-8) to an ordinary directory.
    let base = top.join("SpelledOnly");
    let target = base.join("target");
    fill(&target);
    let mut spelled: Vec<(&str, PathBuf)> = vec![
        ("secrets", base.join("secrets")),
        (".git", base.join("x").join(".git")),
    ];
    if cfg!(target_os = "linux") {
        use std::os::unix::ffi::OsStrExt;
        spelled.push((
            "invalid",
            base.join(std::ffi::OsStr::from_bytes(b"bad\xfflink")),
        ));
    }
    for (what, link) in spelled {
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        cases.push((
            format!("a root spelled {what}, real path plain"),
            Kind::Secrets,
            Pos::Root,
            link,
            target.join("sub"),
        ));
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut violations: Vec<String> = Vec::new();
    for (label, kind, pos, root, inner) in &cases {
        let res = scan_now(std::slice::from_ref(root));
        let p = plan(&res, &idle_opts());
        violations.extend(location_violations(label, &res, &p));
        violations.extend(invalid_name_violations(label, &res, &p, root));
        // Not vacuous: the ordinary files beside the invalid names are still hashed, wherever
        // their path is exact.
        if *kind == Kind::Plain {
            let hashed = res.pieces.as_ref().unwrap().blobs.len();
            assert!(hashed > 0, "{label}: nothing was hashed");
        }
        // Not vacuous: without context the same layout gives removing pieces.
        if *kind == Kind::Plain {
            let removing = lanes_of(&p)
                .pieces
                .iter()
                .filter(|lp| lp.lane.removes())
                .count();
            assert!(removing > 0, "{label}: the control removes nothing");
        }
        // A retained inner root inside the context, in both root orders (the outer walk does not
        // enter it, as for a mount under --one-file-system).
        if *pos == Pos::Descendant {
            for roots in [
                vec![root.clone(), inner.clone()],
                vec![inner.clone(), root.clone()],
            ] {
                let nested = with_survey(res.clone(), &roots);
                let np = plan(&nested, &idle_opts());
                let order = if roots[0] == *root {
                    "outer first"
                } else {
                    "inner first"
                };
                let nlabel = format!("{label}, nested root, {order}");
                violations.extend(location_violations(&nlabel, &nested, &np));
                violations.extend(invalid_name_violations(&nlabel, &nested, &np, root));
            }
        }
    }
    // One line per failing layout and invariant (the first example of each).
    let mut seen = BTreeSet::new();
    let summary: Vec<&String> = violations
        .iter()
        .filter(|v| {
            let key = v.split(": ").next().unwrap_or("").to_string()
                + v.split(" is ").nth(1).unwrap_or("").get(..20).unwrap_or("");
            seen.insert(key)
        })
        .collect();
    assert!(
        violations.is_empty(),
        "{} violations; by layout:\n{}",
        violations.len(),
        summary
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
