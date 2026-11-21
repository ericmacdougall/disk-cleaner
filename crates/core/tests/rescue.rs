//! Stream A rescue (GoodQA goodqa#16529): one test per finding of the last two gates and the
//! click-test that was still open at df5d6df, and per defect the rescue's failure history found
//! (docs/SCAN-FORMAT.md, "Pieces and lanes"). Every test goes through the public `scan` with the
//! roots as a user gives them, and asserts what was assigned (lanes) and what was read (hashes,
//! git processes), never how the code got there. A test that loops over layouts collects every
//! violation before it fails, and each layout carries a positive control (a sibling that is still
//! backed up and hashed), so a blanket keep cannot pass it.
//!
//! Unix only: the fixtures use symbolic links and file modes.

#![cfg(unix)]

mod common;

use common::*;
use disk_cleaner_core::pieces::PieceOptions;
use disk_cleaner_core::{
    plan, scan, scan_from_json, Action, FactsConfig, Git, Lane, LanePiece, Plan, PlanOptions,
    ScanOptions, ScanResult,
};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const KIB: usize = 1024;

fn scan_roots_opts(roots: &[PathBuf], facts: FactsConfig, po: PieceOptions) -> ScanResult {
    // Status-change times at or after the survey's start count as "changed after the scan".
    std::thread::sleep(Duration::from_millis(1100));
    let mut opts = ScanOptions::new(roots.to_vec());
    opts.facts = facts;
    opts.git_jobs = 2;
    opts.lanes = Some(po);
    scan(&opts).expect("scan")
}

fn scan_roots_with(roots: &[PathBuf], facts: FactsConfig) -> ScanResult {
    scan_roots_opts(roots, facts, PieceOptions::default())
}

fn scan_roots(roots: &[PathBuf]) -> ScanResult {
    scan_roots_with(roots, network_everywhere())
}

fn small_pieces() -> PieceOptions {
    PieceOptions {
        max_piece_bytes: 64 * KIB as u64,
        ..PieceOptions::default()
    }
}

/// The plan as the command line makes it: from the scan file written and read back, never from
/// the scan held in memory, so a protection kept only in memory cannot pass. It must also equal
/// the plan made in memory.
fn idle_plan(res: &ScanResult) -> Plan {
    let opts = PlanOptions::new(7, far_future());
    let text = serde_json::to_string_pretty(res).expect("scan file");
    let back = scan_from_json(&text).expect("the scan file reads back");
    let from_file = plan(&back, &opts);
    let in_memory = plan(res, &opts);
    for (what, f, m) in [
        (
            "lanes",
            serde_json::to_value(&from_file.lanes).unwrap(),
            serde_json::to_value(&in_memory.lanes).unwrap(),
        ),
        (
            "items",
            serde_json::to_value(&from_file.items).unwrap(),
            serde_json::to_value(&in_memory.items).unwrap(),
        ),
    ] {
        assert!(
            f == m,
            "the plan's {what} from the scan file differ from those in memory"
        );
    }
    from_file
}

fn real(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Every (real path of a member, plan piece) pair of the plan.
fn members<'a>(res: &ScanResult, p: &'a Plan) -> Vec<(PathBuf, &'a LanePiece)> {
    let survey = res.pieces.as_ref().expect("pieces");
    let lanes = p.lanes.as_ref().expect("lanes");
    let mut out = Vec::new();
    for lp in &lanes.pieces {
        let sp = &survey.pieces[lp.scan_piece];
        let base = real(Path::new(&sp.path));
        let names: Vec<String> = match &lp.member {
            Some(m) => vec![m.clone()],
            None => sp
                .members
                .iter()
                .map(|m| m.path().to_string())
                .filter(|m| !lp.moved_out.contains(m))
                .collect(),
        };
        for n in names {
            out.push((base.join(n), lp));
        }
    }
    out
}

/// Members under `dir` (by real path), with their pieces.
fn pieces_under<'a>(res: &ScanResult, p: &'a Plan, dir: &Path) -> Vec<(PathBuf, &'a LanePiece)> {
    let dir = real(dir);
    members(res, p)
        .into_iter()
        .filter(|(m, _)| m.starts_with(&dir))
        .collect()
}

/// Members under `dir` that are in a removing lane, with the lane and reason.
fn removing_under(res: &ScanResult, p: &Plan, dir: &Path) -> Vec<String> {
    pieces_under(res, p, dir)
        .into_iter()
        .filter(|(_, lp)| lp.lane.removes())
        .map(|(m, lp)| format!("{} in {:?}: {}", m.display(), lp.lane, lp.reason))
        .collect()
}

/// Blob copies (hashed files) under `dir`, by real path.
fn hashed_under(res: &ScanResult, dir: &Path) -> Vec<String> {
    let dir = real(dir);
    let survey = res.pieces.as_ref().expect("pieces");
    survey
        .blobs
        .iter()
        .flat_map(|b| b.copies.iter())
        .filter(|c| {
            let p = Path::new(&c.path);
            let r = match (p.parent(), p.file_name()) {
                (Some(d), Some(n)) => real(d).join(n),
                _ => p.to_path_buf(),
            };
            r.starts_with(&dir)
        })
        .map(|c| c.path.clone())
        .collect()
}

/// Two byte-identical files big enough to be hashed, plus a small one.
fn content(dir: &Path) {
    write_bytes(&dir.join("x.bin"), 100 * KIB);
    write_bytes(&dir.join("y.bin"), 100 * KIB);
    write(&dir.join("notes.txt"), "notes\n");
}

/// `dir` holds members, none of them in a removing lane.
fn expect_kept(bad: &mut Vec<String>, label: &str, res: &ScanResult, p: &Plan, dir: &Path) {
    if pieces_under(res, p, dir).is_empty() {
        bad.push(format!(
            "{label}: vacuous, no member under {}",
            dir.display()
        ));
    }
    for r in removing_under(res, p, dir) {
        bad.push(format!("{label}: {r}"));
    }
}

/// `dir` holds members, none of them hashed, each in a secret `keep` piece.
fn expect_secret(bad: &mut Vec<String>, label: &str, res: &ScanResult, p: &Plan, dir: &Path) {
    let under = pieces_under(res, p, dir);
    if under.is_empty() {
        bad.push(format!(
            "{label}: vacuous, no member under {}",
            dir.display()
        ));
    }
    for (m, lp) in under {
        if lp.lane != Lane::Keep || lp.secret.is_none() {
            bad.push(format!(
                "{label}: {} is {:?} secret={:?}: {}",
                m.display(),
                lp.lane,
                lp.secret,
                lp.reason
            ));
        }
    }
    for h in hashed_under(res, dir) {
        bad.push(format!("{label}: {h} was hashed"));
    }
}

/// The positive control: `dir` (made with [`content`]) is backed up and its copies hashed.
fn expect_control(bad: &mut Vec<String>, label: &str, res: &ScanResult, p: &Plan, dir: &Path) {
    let under = pieces_under(res, p, dir);
    if under.is_empty() {
        bad.push(format!("{label}: control {} has no member", dir.display()));
    }
    for (m, lp) in under {
        if lp.lane != Lane::Backup && lp.lane != Lane::Noise {
            bad.push(format!(
                "{label}: control {} is {:?}: {}",
                m.display(),
                lp.lane,
                lp.reason
            ));
        }
    }
    if hashed_under(res, dir).len() < 2 {
        bad.push(format!("{label}: control {} was not hashed", dir.display()));
    }
}

fn verdict(bad: Vec<String>) {
    assert!(bad.is_empty(), "{} violations: {bad:#?}", bad.len());
}

// ----------------------------------------------------------------------------------------------
// CT-A3: a symlinked scan root is counted exactly as its target, and classified by what it is:
// the name a link gives it can add protection, never make anything removable.

/// Nodes as (path below the root, class), sorted.
fn classes(res: &ScanResult, root: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = res
        .nodes
        .iter()
        .map(|n| {
            let rel = Path::new(&n.path)
                .strip_prefix(root)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| format!("OUTSIDE {}", n.path));
            (rel, format!("{:?}", n.class))
        })
        .collect();
    out.sort();
    out
}

/// Compare a scan of `alias` with the scan `a` of its target.
fn same_as_target(
    bad: &mut Vec<String>,
    label: &str,
    a: &ScanResult,
    target: &Path,
    b: &ScanResult,
    alias: &Path,
) {
    let (ta, tb) = (
        serde_json::to_value(&a.totals).unwrap(),
        serde_json::to_value(&b.totals).unwrap(),
    );
    if ta != tb {
        bad.push(format!("{label}: totals differ: target {ta} alias {tb}"));
    }
    let (ca, cb) = (classes(a, target), classes(b, alias));
    if ca != cb {
        bad.push(format!(
            "{label}: classes differ: target {ca:?} alias {cb:?}"
        ));
    }
    let (pa, pb) = (idle_plan(a), idle_plan(b));
    let (la, lb) = (
        serde_json::to_value(&pa.lanes.as_ref().unwrap().totals.by_lane).unwrap(),
        serde_json::to_value(&pb.lanes.as_ref().unwrap().totals.by_lane).unwrap(),
    );
    if la != lb {
        bad.push(format!("{label}: lanes differ: target {la} alias {lb}"));
    }
    for (side, res, root, p) in [("target", a, target, &pa), ("alias", b, alias, &pb)] {
        for n in &res.nodes {
            if !Path::new(&n.path).starts_with(root) {
                bad.push(format!(
                    "{label} {side}: node {} lies outside the root",
                    n.path
                ));
            }
        }
        let survey = res.pieces.as_ref().unwrap();
        let sum: u64 = survey.pieces.iter().map(|p| p.bytes).sum();
        if sum + survey.totals.dir_overhead_bytes != res.totals.allocated_bytes {
            bad.push(format!(
                "{label} {side}: pieces {sum} plus overhead {} are not the scan's {}",
                survey.totals.dir_overhead_bytes, res.totals.allocated_bytes
            ));
        }
        if p.lanes.as_ref().unwrap().totals.scanned_bytes != res.totals.allocated_bytes {
            bad.push(format!("{label} {side}: lanes scanned_bytes differ"));
        }
    }
}

#[test]
fn cta3_a_symlinked_root_counts_and_classifies_exactly_as_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let base = real(tmp.path());
    let target = base.join("real");
    write_bytes(&target.join("a.bin"), 5 * KIB);
    write_bytes(&target.join("sub").join("b.bin"), 9 * KIB);
    write_bytes(&target.join("sub").join("deeper").join("c.bin"), 70 * KIB);
    // Maven's folder is `.m2/repository`: a rule that matches only through the root's spelling.
    write_bytes(&target.join("repository").join("r.bin"), 3 * KIB);
    let a = scan_roots(std::slice::from_ref(&target));
    let mut bad = Vec::new();
    // A name a source file could have, a name a cache rule matches, and a parent name a rule
    // needs: the link itself is never counted, and its name never classifies.
    for name in ["tool.py", ".cache", ".m2"] {
        let alias = base.join(name);
        symlink(&target, &alias).unwrap();
        let b = scan_roots(std::slice::from_ref(&alias));
        same_as_target(&mut bad, name, &a, &target, &b, &alias);
    }
    // The link on one device, its target on another (in the dc-rust container /tmp and the
    // cargo target dir differ): with --one-file-system the target's own folders are still walked.
    let far = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cta3-{}", std::process::id()));
    let _ = fs::remove_dir_all(&far);
    let far_target = far.join("real");
    write_bytes(&far_target.join("sub").join("b.bin"), 9 * KIB);
    let far_alias = base.join("far");
    symlink(&far_target, &far_alias).unwrap();
    let dev = |p: &Path| {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(p).unwrap().dev()
    };
    let cross = dev(&base) != dev(&far_target);
    println!("cta3: link and target on different devices: {cross}");
    let one_fs = |root: &Path| {
        std::thread::sleep(Duration::from_millis(1100));
        let mut opts = ScanOptions::new(vec![root.to_path_buf()]);
        opts.facts = network_everywhere();
        opts.one_file_system = true;
        opts.lanes = Some(PieceOptions::default());
        scan(&opts).expect("scan")
    };
    let (fa, fb) = (one_fs(&real(&far_target)), one_fs(&far_alias));
    same_as_target(&mut bad, "far", &fa, &real(&far_target), &fb, &far_alias);
    let _ = fs::remove_dir_all(&far);
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// PR review a3-3 F1: context of a root spelling that root de-duplication drops.

#[test]
#[ignore = "red at df5d6df; enabled by the context-union slice (goodqa#16529)"]
fn a3f1_an_alias_root_with_a_secret_name_protects_in_every_order() {
    let mut bad = Vec::new();
    for name in ["secrets", ".aws"] {
        for nested in [false, true] {
            for alias_first in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let base = real(tmp.path());
                let plain = base.join("plain");
                let protected = if nested {
                    plain.join("sub")
                } else {
                    plain.clone()
                };
                content(&protected);
                content(&plain.join("other"));
                let ctl = base.join("ctl");
                content(&ctl.join("c"));
                let alias = base.join(name);
                symlink(&protected, &alias).unwrap();
                let roots = if alias_first {
                    vec![alias.clone(), plain.clone(), ctl.clone()]
                } else {
                    vec![plain.clone(), alias.clone(), ctl.clone()]
                };
                let label = format!("{name} nested={nested} alias_first={alias_first}");
                let res = scan_roots(&roots);
                let p = idle_plan(&res);
                expect_secret(&mut bad, &label, &res, &p, &protected);
                expect_control(&mut bad, &label, &res, &p, &ctl.join("c"));
            }
        }
    }
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// PR review a3-3 F2: a git directory git accepts by HEAD and commondir under an ordinary name.

fn borrower(dir: &Path, provider: &Path, head_symlink: bool) {
    fs::create_dir_all(dir).unwrap();
    if head_symlink {
        // The legacy symbolic-link form of a symbolic ref, which git still accepts.
        symlink("refs/heads/main", dir.join("HEAD")).unwrap();
    } else {
        fs::copy(provider.join(".git").join("HEAD"), dir.join("HEAD")).unwrap();
    }
    write(
        &dir.join("commondir"),
        &format!("{}\n", provider.join(".git").display()),
    );
    write_bytes(&dir.join("index"), 70 * KIB);
    // The oracle: git itself accepts it as a git directory.
    let out = git_command()
        .arg("--git-dir")
        .arg(dir)
        .args(["rev-parse", "--git-dir"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fixture: git accepts {} as a git directory: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "red at df5d6df; enabled by the git-detector slice (goodqa#16529)"]
fn a3f2_a_git_directory_by_head_and_commondir_is_never_removed() {
    let mut bad = Vec::new();
    for head_symlink in [false, true] {
        for place in ["below", "own-root", "alias-root", "below-second-root"] {
            let (tmp, root, remotes) = workspace();
            let root = real(&root);
            let a = pushed_repo(&root, &remotes, "a");
            let m = root.join("work").join("m");
            borrower(&m, &a, head_symlink);
            let ctl = real(tmp.path()).join("ctl");
            content(&ctl.join("c"));
            let roots = match place {
                "below" => vec![root.clone(), ctl.clone()],
                "own-root" => vec![m.clone(), ctl.clone()],
                "alias-root" => {
                    let alias = real(tmp.path()).join("alias");
                    symlink(&m, &alias).unwrap();
                    vec![alias, ctl.clone()]
                }
                _ => vec![ctl.clone(), root.join("work")],
            };
            let label = format!("head_symlink={head_symlink} {place}");
            let res = scan_roots(&roots);
            let p = idle_plan(&res);
            expect_kept(&mut bad, &label, &res, &p, &m);
            expect_control(&mut bad, &label, &res, &p, &ctl.join("c"));
        }
    }
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// PR review a3-3 F3, N2 and N3: what git data refers to is never removed, whatever holds it, and
// whatever else was found about its holder first.

#[test]
#[ignore = "red at df5d6df; enabled by the dependency-pin slice (goodqa#16529)"]
fn a3f3_a_directory_that_git_data_refers_to_is_kept() {
    let mut bad = Vec::new();
    for (holder, state_file_first) in [
        ("artifact", false),
        ("artifact", true),
        ("loose-code", false),
        ("no-node", false),
    ] {
        let (_tmp, root, remotes) = workspace();
        let root = real(&root);
        let b = pushed_repo(&root, &remotes, "b");
        let data = match holder {
            "artifact" => {
                let js = root.join("js");
                write(&js.join("package.json"), "{}\n");
                let nm = js.join("node_modules");
                if state_file_first {
                    // A backup-answerable ask found first: the dependency must still count.
                    write(&nm.join("aaa").join("prod.tfstate"), "{}\n");
                }
                nm.join("store")
            }
            "loose-code" => {
                let lib = root.join("lib");
                write(&lib.join("main.py"), "print(1)\n");
                lib.join("store")
            }
            _ => root.join("plain").join("store"),
        };
        content(&data);
        let ctl = root.join("ctl");
        content(&ctl);
        write(
            &b.join(".git")
                .join("objects")
                .join("info")
                .join("alternates"),
            &format!("{}\n", data.display()),
        );
        let label = format!("{holder} state_file_first={state_file_first}");
        let res = scan_roots(std::slice::from_ref(&root));
        let p = idle_plan(&res);
        expect_kept(&mut bad, &label, &res, &p, &data);
        expect_control(&mut bad, &label, &res, &p, &ctl);
    }
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// N1: a symbolic link whose own name is a secret name protects what it names.

#[test]
#[ignore = "red at df5d6df; enabled by the context-union slice (goodqa#16529)"]
fn n1_a_secret_named_symlink_protects_what_it_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = real(tmp.path()).join("root");
    let vault = root.join("vault");
    content(&vault.join("aws"));
    write(&vault.join("settings.txt"), "k=v\n");
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    symlink("../vault/aws", home.join(".aws")).unwrap();
    symlink("../vault/settings.txt", home.join(".env")).unwrap();
    let ctl = root.join("ctl");
    content(&ctl);
    let res = scan_roots(std::slice::from_ref(&root));
    let p = idle_plan(&res);
    let mut bad = Vec::new();
    expect_secret(&mut bad, ".aws", &res, &p, &vault.join("aws"));
    expect_secret(&mut bad, ".env", &res, &p, &vault.join("settings.txt"));
    expect_control(&mut bad, "control", &res, &p, &ctl);
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// N4: the work tree of a repository the scan does not own goes with its git directory.

#[test]
#[ignore = "red at df5d6df; enabled by the git-detector slice (goodqa#16529)"]
fn n4_the_work_tree_of_a_repository_the_scan_does_not_own_is_kept() {
    let (_tmp, root, _) = workspace();
    let root = real(&root);
    // A dependency checkout inside an artifact: nothing inside an artifact becomes a node. Its
    // work tree is too big to share a piece with its git directory.
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}\n");
    let dep = proj.join("node_modules").join("dep");
    init_repo(&dep);
    content(&dep);
    let sibling = proj.join("node_modules").join("plain");
    content(&sibling);
    // A git directory spelled `.GIT`: git on a case-insensitive file system uses it.
    let odd = root.join("loose").join("x");
    let gd = odd.join(".GIT");
    fs::create_dir_all(&gd).unwrap();
    git(&gd, &["init", "-q", "--bare", "-b", "main"]);
    content(&odd);
    let ctl = root.join("ctl");
    content(&ctl);
    let res = scan_roots_opts(
        std::slice::from_ref(&root),
        network_everywhere(),
        small_pieces(),
    );
    let p = idle_plan(&res);
    let mut bad = Vec::new();
    expect_kept(&mut bad, "dependency checkout", &res, &p, &dep);
    expect_kept(&mut bad, ".GIT", &res, &p, &odd);
    // Not over-held: the sibling package and the control are still removed.
    expect_control(&mut bad, "sibling package", &res, &p, &sibling);
    expect_control(&mut bad, "control", &res, &p, &ctl);
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// N5: a scan root inside a repository the scan does not cover.

#[test]
#[ignore = "red at df5d6df; enabled by the uncovered-repository slice (goodqa#16529)"]
fn n5_a_root_inside_a_repository_the_scan_does_not_cover_keeps_its_files() {
    let (_tmp, root, _) = workspace();
    let root = real(&root);
    let big = root.join("big");
    init_repo(&big);
    write(&big.join(".gitignore"), "node_modules/\n");
    let sub = big.join("sub");
    content(&sub.join("docs"));
    write(&sub.join("package.json"), "{}\n");
    write_bytes(&sub.join("node_modules").join("x").join("i.js"), 9 * KIB);
    let ctl = root.join("ctl");
    content(&ctl);
    let res = scan_roots(&[sub.clone(), ctl.clone()]);
    let mut bad = Vec::new();
    if res.enclosing_repos.is_empty() {
        bad.push("fixture: sub is inside big".to_string());
    }
    let p = idle_plan(&res);
    expect_kept(&mut bad, "docs", &res, &p, &sub.join("docs"));
    expect_control(&mut bad, "control", &res, &p, &ctl);
    // Positive control: git verified the ignored artifact, so it is still noise.
    let nm = pieces_under(&res, &p, &sub.join("node_modules"));
    if nm.is_empty() {
        bad.push("node_modules has no member".to_string());
    }
    for (m, lp) in nm {
        if lp.lane != Lane::Noise {
            bad.push(format!("{} is {:?}: {}", m.display(), lp.lane, lp.reason));
        }
    }
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// N6: no git process runs for a repository in a secret place.

#[test]
#[ignore = "red at df5d6df; enabled by the secret-place slice (goodqa#16529)"]
fn n6_git_never_runs_for_a_repository_in_a_secret_place() {
    let tmp = tempfile::tempdir().unwrap();
    let base = real(tmp.path());
    let root = base.join("root");
    let pass = root.join(".password-store");
    init_repo(&pass);
    let inside = root.join("secrets").join("proj");
    init_repo(&inside);
    let ok = root.join("ok");
    init_repo(&ok);
    // A repository that is secret only through the spelling of a second root.
    let plain = base.join("plain");
    let r2 = plain.join("r2");
    init_repo(&r2);
    let alias = base.join(".aws");
    symlink(&plain, &alias).unwrap();

    let log = base.join("git.log");
    let real_git = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let wrapper = script(
        &base,
        "git-recorder",
        &format!(
            "#!/bin/sh\nprintf '%s %s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real_git
        ),
    );
    let facts = network_everywhere().with_git(Git::new(&wrapper, Duration::from_secs(60)));
    let res = scan_roots_with(&[root.clone(), plain.clone(), alias.clone()], facts);
    let ran = fs::read_to_string(&log).unwrap_or_default();
    let mut bad = Vec::new();
    if !ran
        .lines()
        .any(|l| l.starts_with(&ok.display().to_string()))
    {
        bad.push(format!(
            "positive control: git did not run for {}",
            ok.display()
        ));
    }
    let p = idle_plan(&res);
    for repo in [&pass, &inside, &r2] {
        let shown = repo.display().to_string();
        for l in ran.lines().filter(|l| l.contains(&shown)) {
            bad.push(format!("git ran for {shown}: {l}"));
        }
        expect_kept(&mut bad, &shown, &res, &p, repo);
        match p.items.iter().find(|i| Path::new(&i.path) == *repo) {
            Some(it) if it.action != Action::Ask => {
                bad.push(format!("{shown}: item is {:?}: {}", it.action, it.reason))
            }
            Some(_) => {}
            // The alias spelling may report the repository under another path.
            None if *repo == r2 => {}
            None => bad.push(format!("{shown}: no item")),
        }
    }
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// D2 (ASSUMPTION, main's ruling, not user-confirmed): a git-data reference that cannot be read
// keeps every piece of the scan, says which and what to do, and the keep lifts once it can be.

#[test]
#[ignore = "red at df5d6df; enabled by the D2 slice (goodqa#16529)"]
fn d2_an_unreadable_git_reference_keeps_everything_until_it_is_readable() {
    let (_tmp, root, remotes) = workspace();
    let root = real(&root);
    let b = pushed_repo(&root, &remotes, "b");
    let alt = b
        .join(".git")
        .join("objects")
        .join("info")
        .join("alternates");
    write(&alt, &format!("{}\n", root.join("lib").display()));
    content(&root.join("lib"));
    content(&root.join("loose"));
    fs::set_permissions(&alt, fs::Permissions::from_mode(0o000)).unwrap();
    // A privileged run reads a mode-000 file and would prove nothing: fail, never pass.
    assert!(
        fs::read(&alt).is_err(),
        "d2 must run unprivileged: a mode-000 file was readable"
    );
    println!("d2: ran unprivileged, the alternates file is unreadable");
    let mut bad = Vec::new();
    let res = scan_roots(std::slice::from_ref(&root));
    let p = idle_plan(&res);
    let l = p.lanes.as_ref().unwrap();
    for lp in l.pieces.iter().filter(|lp| lp.lane.removes()) {
        bad.push(format!(
            "unreadable: {} is {:?}: {}",
            lp.path, lp.lane, lp.reason
        ));
    }
    match pieces_under(&res, &p, &root.join("loose")).first() {
        None => bad.push("loose has no member".to_string()),
        Some((_, lp)) => {
            if !lp.reason.contains(&alt.display().to_string()) {
                bad.push(format!("does not name the reference: {}", lp.reason));
            }
            if !(lp.reason.contains("readable") && lp.reason.contains("rescan")) {
                bad.push(format!("does not say what to do: {}", lp.reason));
            }
        }
    }
    fs::set_permissions(&alt, fs::Permissions::from_mode(0o644)).unwrap();
    let res = scan_roots(std::slice::from_ref(&root));
    let p = idle_plan(&res);
    // Once readable: the referred-to directory is kept, the rest is backed up again.
    expect_kept(&mut bad, "readable", &res, &p, &root.join("lib"));
    expect_control(&mut bad, "readable", &res, &p, &root.join("loose"));
    verdict(bad);
}

// ----------------------------------------------------------------------------------------------
// AC-21: a plan item that removes holds no kept piece under its path except a secret piece, which
// stays in place (lane rule 1). NEG-30: a secret-named entry inside an artifact does not make the
// artifact ask; the item stays `remove-regenerable`, exactly as at df5d6df.

/// AC-21 on one plan: every `keep` piece under a removing item's path is a secret piece.
fn items_invariant(bad: &mut Vec<String>, label: &str, res: &ScanResult, p: &Plan) {
    let all = members(res, p);
    for it in p.items.iter().filter(|i| i.action.removes()) {
        let at = real(Path::new(&it.path));
        for (m, lp) in all.iter().filter(|(m, _)| m.starts_with(&at)) {
            if lp.lane == Lane::Keep && lp.secret.is_none() {
                bad.push(format!(
                    "{label}: item {} is {:?} but {} is kept: {}",
                    it.path,
                    it.action,
                    m.display(),
                    lp.reason
                ));
            }
        }
    }
}

#[test]
fn artifact_secret_a_secret_entry_stays_in_place_and_the_artifact_is_still_noise() {
    let tmp = tempfile::tempdir().unwrap();
    let root = real(tmp.path()).join("root");
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}\n");
    write(&proj.join("requirements.txt"), "certifi\n");
    let nm = proj.join("node_modules");
    let pkg = nm.join("pkg");
    write(&pkg.join("package.json"), "{\"name\": \"pkg\"}\n");
    write_bytes(&pkg.join("index.js"), 9 * KIB);
    write(
        &pkg.join(".npmrc"),
        "registry=https://registry.example.invalid/\n",
    );
    write(&pkg.join("test").join("fixture.pem"), "not a key\n");
    let venv = proj.join("venv");
    write(&venv.join("pyvenv.cfg"), "home = /usr/bin\n");
    let site = venv.join("lib").join("python3.11").join("site-packages");
    write(&site.join("pip").join("__init__.py"), "\n");
    let certifi = site.join("pip").join("_vendor").join("certifi");
    write(&certifi.join("cacert.pem"), "not a bundle\n");
    write_bytes(&certifi.join("core.py"), 9 * KIB);
    let ctl = root.join("ctl");
    content(&ctl);
    let res = scan_roots(std::slice::from_ref(&root));
    let p = idle_plan(&res);
    let mut bad = Vec::new();
    expect_control(&mut bad, "control", &res, &p, &ctl);
    items_invariant(&mut bad, "items", &res, &p);
    let secrets = [
        pkg.join(".npmrc"),
        pkg.join("test").join("fixture.pem"),
        certifi.join("cacert.pem"),
    ];
    for (label, dir) in [("node_modules", &nm), ("venv", &venv)] {
        match p
            .items
            .iter()
            .find(|i| real(Path::new(&i.path)) == real(dir))
        {
            Some(it) if it.action != Action::RemoveRegenerable => {
                bad.push(format!("{label}: item is {:?}: {}", it.action, it.reason))
            }
            Some(_) => {}
            None => bad.push(format!("{label}: no item")),
        }
        let under = pieces_under(&res, &p, dir);
        if under.len() < 2 {
            bad.push(format!("{label}: vacuous, {} members", under.len()));
        }
        for (m, lp) in under {
            let secret = secrets.iter().any(|s| real(s) == m);
            let ok = if secret {
                lp.lane == Lane::Keep && lp.secret.is_some()
            } else {
                lp.lane == Lane::Noise
            };
            if !ok {
                bad.push(format!(
                    "{label}: {} is {:?} secret={:?}: {}",
                    m.display(),
                    lp.lane,
                    lp.secret,
                    lp.reason
                ));
            }
        }
    }
    for s in &secrets {
        if !members(&res, &p).iter().any(|(m, _)| m == &real(s)) {
            bad.push(format!("{} has no piece", s.display()));
        }
    }
    verdict(bad);
}
