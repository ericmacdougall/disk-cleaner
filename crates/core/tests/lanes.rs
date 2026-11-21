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

/// The invariants every survey and every plan must hold.
/// Is this member of a `git-node` piece one of git's own bookkeeping names (below `refs`, `logs`,
/// `worktrees` or `modules` of the git directory, or `packed-refs`)? Those are branch names, to
/// which name-based secret rules do not apply.
fn git_bookkeeping_member(sp: &disk_cleaner_core::pieces::Piece, parts: &[&str]) -> bool {
    if sp.kind != disk_cleaner_core::PieceKind::GitNode {
        return false;
    }
    let bare = !sp.members.iter().any(|m| m.path().starts_with(".git/"));
    let rest = match parts.iter().position(|p| *p == ".git") {
        Some(i) => &parts[i + 1..],
        None if bare => parts,
        None => return false,
    };
    match rest {
        [first, _, ..] if ["refs", "logs", "worktrees", "modules"].contains(first) => true,
        ["packed-refs"] => true,
        _ => false,
    }
}

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
            if git_bookkeeping_member(sp, &parts) {
                continue;
            }
            let (last, dirs) = parts.split_last().unwrap();
            assert!(
                rules.secret_file(last, in_artifact).is_none()
                    && dirs.iter().all(|d| rules.secret_dir(d).is_none()),
                "secret-named {} in removing piece {}",
                m.path(),
                lp.id
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
    }
    for s in &secrets {
        let lp = the_piece(&res, l, s);
        assert_eq!(lp.lane, Lane::Keep, "{}", s.display());
        assert!(lp.secret.is_some(), "{}", s.display());
        assert!(lp.reason.contains("stays local"), "{}", lp.reason);
    }
    for s in [loose.join(".env.example"), loose.join("plain.txt")] {
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
        let tmp = tempfile::tempdir().unwrap();
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
    check_partition(&alias, &res);
    let p = plan(&res, &idle_opts());
    check_invariants(&res, &p);
    let survey = res.pieces.as_ref().unwrap();
    assert!(!survey.pieces.is_empty());
    for sp in &survey.pieces {
        assert!(Path::new(&sp.path).starts_with(&alias), "{}", sp.path);
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
