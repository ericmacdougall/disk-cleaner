//! Apply on the real pipeline: `scan` with lanes, then `plan`, then `apply` with the production
//! oracle (core `plan()` run again on the scan). Proves that noise and git pieces act in
//! production, not only under a fixture oracle, and that a plan the scan does not support is kept.
#![cfg(unix)]

mod apply_common;
mod common;

use apply_common::Fx;
use common::*;
use disk_cleaner_core::apply::input::Lane;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
use disk_cleaner_core::apply::{apply, ApplyReport, CoreOracle};
use disk_cleaner_core::pieces::PieceOptions;
use disk_cleaner_core::{plan, scan, PlanOptions, ProcessCheck, ScanOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    proj: PathBuf,
    repo: PathBuf,
    scan: String,
    plan: String,
}

fn world() -> World {
    let (tmp, root, remotes) = workspace();
    let root = root.canonicalize().unwrap();
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("index.js"), "console.log(1)\n");
    write_bytes(&proj.join("node_modules").join("dep").join("x.js"), 9000);
    let repo = pushed_repo(&root, &remotes, "app");
    // Status-change times at or after the survey's start count as changed after the scan.
    std::thread::sleep(Duration::from_millis(1100));
    let mut so = ScanOptions::new(vec![root.clone()]);
    so.facts = network_everywhere();
    so.git_jobs = 2;
    so.lanes = Some(PieceOptions::default());
    let res = scan(&so).expect("scan");
    let p = plan(&res, &PlanOptions::new(7, far_future()));
    World {
        _tmp: tmp,
        root,
        proj,
        repo,
        scan: serde_json::to_string(&res).unwrap(),
        plan: serde_json::to_string(&p).unwrap(),
    }
}

fn core_oracle() -> Box<CoreOracle> {
    Box::new(CoreOracle {
        process: Box::new(ProcessCheck::none_running),
        ..CoreOracle::default()
    })
}

fn lane_of(rep: &ApplyReport, lane: Lane) -> Vec<(PathBuf, String, String)> {
    rep.pieces
        .iter()
        .filter(|p| p.lane == lane)
        .map(|p| (p.path.clone(), p.outcome.clone(), p.reason.clone()))
        .collect()
}

fn ropts(fx: &Fx) -> RestoreOptions {
    RestoreOptions {
        accept_partial: false,
        remote: fx.remote.to_string_lossy().to_string(),
        rclone: fx.fake.clone(),
        git: "git".into(),
        manifest: fx.manifest(),
        from_remote: false,
        label: "testhost".into(),
        run: None,
        timeout: Duration::from_secs(120),
    }
}

#[test]
fn real_plan_noise_and_git_act_with_the_core_oracle() {
    let w = world();
    let fx = Fx::new();
    let before_proj = tree_digest(&w.proj.join("index.js"));
    let before_repo = apply_common::tree_digest(&w.repo);
    let mut o = fx.opts(true);
    o.oracle = core_oracle();
    let rep = apply(&w.scan, &w.plan, &o).unwrap();
    let noise = lane_of(&rep, Lane::Noise);
    let git = lane_of(&rep, Lane::Git);
    assert!(!noise.is_empty() && !git.is_empty(), "{rep:#?}");
    for (path, out, why) in noise.iter().chain(git.iter()) {
        assert_eq!(out, "removed", "{}: {why}", path.display());
    }
    assert!(!w.proj.join("node_modules").exists());
    assert!(!w.repo.exists());
    // The git lane restores byte for byte; the noise lane says how to regenerate.
    let rr = restore(&w.root, &ropts(&fx)).unwrap();
    assert!(rr.failed.is_empty() && rr.conflicts.is_empty(), "{rr:?}");
    assert_eq!(apply_common::tree_digest(&w.repo), before_repo);
    assert_eq!(tree_digest(&w.proj.join("index.js")), before_proj);
    assert!(
        rr.regenerate
            .iter()
            .any(|(p, rule)| p.starts_with(w.proj.join("node_modules")) && rule == "node_modules"),
        "{rr:?}"
    );
}

/// AC-7: rule noise needs its markers now, not only at scan time. With the marker gone the core
/// oracle still agrees (it re-derives from the scan file), so only apply's live check stands
/// between the piece and an unrecoverable removal: the piece must be backed up first, and
/// restore must bring the bytes back rather than say "regenerate".
#[test]
fn a_rule_noise_piece_whose_marker_is_gone_is_backed_up() {
    let w = world();
    let nm = w.proj.join("node_modules");
    let before = apply_common::tree_digest(&nm);
    std::fs::remove_file(w.proj.join("package.json")).unwrap();
    let fx = Fx::new();
    let mut o = fx.opts(true);
    o.oracle = core_oracle();
    let rep = apply(&w.scan, &w.plan, &o).unwrap();
    let mine: Vec<_> = rep
        .pieces
        .iter()
        .filter(|p| p.path.starts_with(&nm))
        .collect();
    assert!(!mine.is_empty(), "{rep:#?}");
    for p in &mine {
        assert!(
            p.reason.contains("marker"),
            "{}: {}",
            p.path.display(),
            p.reason
        );
        assert!(
            p.reason.contains("backed up"),
            "{}: {}",
            p.path.display(),
            p.reason
        );
        assert_eq!(p.outcome, "removed", "{}: {}", p.path.display(), p.reason);
    }
    assert!(fx.shard_total() > 0, "the node_modules bytes were archived");
    assert!(!nm.exists());
    let rr = restore(&nm, &ropts(&fx)).unwrap();
    assert!(rr.failed.is_empty() && rr.conflicts.is_empty(), "{rr:?}");
    assert!(
        !rr.regenerate.iter().any(|(p, _)| p.starts_with(&nm)),
        "{rr:?}"
    );
    assert_eq!(apply_common::tree_digest(&nm), before);
}

/// `tree_digest` for a single file or a directory.
fn tree_digest(p: &Path) -> String {
    if p.is_dir() {
        format!("{:?}", apply_common::tree_digest(p))
    } else {
        apply_common::sha(p)
    }
}

#[test]
fn a_plan_the_scan_does_not_support_is_kept_by_the_core_oracle() {
    let w = world();
    // Forge the plan: every backup piece becomes noise with a rule the scan never matched, and
    // the digest binding still holds (the lanes block is edited, the scan is not).
    let mut pv: serde_json::Value = serde_json::from_str(&w.plan).unwrap();
    let mut forged = 0;
    for lp in pv["lanes"]["pieces"].as_array_mut().unwrap() {
        if lp["lane"] == "backup" {
            lp["lane"] = "noise".into();
            lp["evidence"] = serde_json::json!({"kind": "rule", "rule": "node_modules",
                                                 "markers": ["package.json"], "node": 0});
            forged += 1;
        }
    }
    assert!(forged > 0, "the fixture has backup pieces to forge");
    let plan = pv.to_string();
    let fx = Fx::new();
    let mut o = fx.opts(true);
    o.oracle = core_oracle();
    let rep = apply(&w.scan, &plan, &o).unwrap();
    for (path, out, why) in lane_of(&rep, Lane::Noise) {
        if out == "removed" {
            // Only the genuine node_modules noise may go.
            assert!(
                path.starts_with(w.proj.join("node_modules")),
                "{}: {why}",
                path.display()
            );
        } else {
            assert_eq!(out, "kept");
        }
    }
    assert!(w.proj.join("index.js").exists());
    assert!(w.proj.join("package.json").exists());
}

#[test]
fn a_process_now_running_in_the_repo_keeps_it() {
    let w = world();
    let fx = Fx::new();
    let repo = w.repo.clone();
    let mut o = fx.opts(true);
    o.oracle = Box::new(CoreOracle {
        process: Box::new(move || ProcessCheck::Ok {
            cwds: vec![repo.clone()],
        }),
        ..CoreOracle::default()
    });
    let rep = apply(&w.scan, &w.plan, &o).unwrap();
    for (path, out, why) in lane_of(&rep, Lane::Git) {
        assert_eq!(out, "kept", "{}: {why}", path.display());
    }
    assert!(w.repo.join(".git/HEAD").exists());
}
