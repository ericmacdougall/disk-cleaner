//! Manifest history in storage with a gap (A-5, main's triage of segment deletion: CRITICAL).
//! The storage head lists every committed segment by name and sha256. A restore from storage
//! never succeeds silently with less history than was stored: a listed segment that is missing,
//! changed or replaced, or a missing head, refuses it (or, with `--accept-partial`, is listed).
//! The gap is permanent: a later acting run reports it and never drops it.
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
use disk_cleaner_core::apply::{apply, ApplyError};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn ropts(fx: &Fx) -> RestoreOptions {
    RestoreOptions {
        accept_partial: false,
        remote: fx.remote.to_string_lossy().to_string(),
        rclone: fx.fake.clone(),
        git: "git".into(),
        manifest: fx.manifest(),
        from_remote: true,
        label: "testhost".into(),
        run: None,
        timeout: Duration::from_secs(120),
    }
}

fn dir(fx: &Fx) -> PathBuf {
    fx.remote.join("manifests/testhost")
}

/// Committed segments, oldest first (by modification order of creation).
fn committed(fx: &Fx) -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(dir(fx))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter(|p| Path::new(&format!("{}.sum", p.display())).exists())
        .map(|p| (fs::metadata(&p).unwrap().modified().unwrap(), p))
        .collect();
    v.sort();
    v.into_iter().map(|(_, p)| p).collect()
}

fn sum(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sum", p.display()))
}

/// One acting run that backs up and removes `data/<name>` (written first when `write`).
fn run_one(fx: &Fx, name: &str, write: bool) -> disk_cleaner_core::apply::ApplyReport {
    if write {
        fx.write(&format!("{name}/f.txt"), name.as_bytes());
    }
    let pieces = vec![P::tree(&fx.data.join(name), "backup")];
    let (scan, plan) = docs(&[&fx.data], &pieces);
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert!(
        !fx.data.join(name).exists(),
        "{name}: removal is not blocked: {rep:?}"
    );
    // Distinct modification times, so `committed` orders segments by run.
    std::thread::sleep(Duration::from_millis(20));
    rep
}

/// Three runs, three or more committed segments; returns the digest of the tree before them.
fn history(fx: &Fx) -> Vec<(String, String)> {
    for n in ["one", "two", "three"] {
        fx.write(&format!("{n}/f.txt"), n.as_bytes());
        fx.write(&format!("{n}/g.bin"), &vec![n.len() as u8; 70_000]);
    }
    let before = tree_digest(&fx.data);
    for n in ["one", "two", "three"] {
        let rep = run_one(fx, n, false);
        assert!(rep.history_gaps.is_empty(), "{n}: {rep:?}");
    }
    assert!(committed(fx).len() >= 3, "{:?}", committed(fx));
    before
}

fn refused(fx: &Fx, needle: &str) -> String {
    let _ = fs::remove_file(fx.manifest());
    match restore(&fx.data, &ropts(fx)) {
        Ok(r) => panic!("restore from storage succeeded with a gap ({needle}): {r:?}"),
        Err(ApplyError::Manifest(m)) => {
            assert!(m.contains(needle), "{m}");
            m
        }
        Err(e) => panic!("{e:?}"),
    }
}

fn complete(fx: &Fx, before: &[(String, String)]) {
    let _ = fs::remove_file(fx.manifest());
    let r = restore(&fx.data, &ropts(fx)).unwrap();
    assert!(
        r.failed.is_empty() && r.conflicts.is_empty() && r.rejected_segments.is_empty(),
        "{r:?}"
    );
    assert_eq!(tree_digest(&fx.data), before);
}

#[test]
fn a_segment_deleted_with_its_sidecar_is_named_whether_middle_or_last() {
    for which in ["middle", "last"] {
        let fx = Fx::new();
        let _ = history(&fx);
        let segs = committed(&fx);
        let gone = if which == "middle" {
            &segs[1]
        } else {
            segs.last().unwrap()
        };
        fs::remove_file(gone).unwrap();
        fs::remove_file(sum(gone)).unwrap();
        let name = gone.file_name().unwrap().to_string_lossy().to_string();
        let m = refused(&fx, &name);
        assert!(m.contains("missing from storage"), "{which}: {m}");
        // --accept-partial restores what is stored and names the gap.
        let mut o = ropts(&fx);
        o.accept_partial = true;
        let r = restore(&fx.data, &o).unwrap();
        assert!(
            r.rejected_segments.iter().any(|(n, _)| n == &name),
            "{which}: {r:?}"
        );
    }
}

/// An older segment copied over a newer one: as a pair (its sidecar names the other segment),
/// and with a sidecar made for the new name (only the head shows it is not the segment stored
/// there).
#[test]
fn a_segment_replaced_by_an_older_one_is_refused() {
    use sha2::{Digest, Sha256};
    for case in ["pair copied", "sidecar remade"] {
        let fx = Fx::new();
        let _ = history(&fx);
        let segs = committed(&fx);
        let name = segs[1].file_name().unwrap().to_string_lossy().to_string();
        let old = fs::read(&segs[0]).unwrap();
        fs::write(&segs[1], &old).unwrap();
        if case == "pair copied" {
            fs::copy(sum(&segs[0]), sum(&segs[1])).unwrap();
        } else {
            let s = format!(
                "{} {}\nname {name}\nend\n",
                hex::encode(Sha256::digest(&old)),
                old.len()
            );
            fs::write(sum(&segs[1]), s).unwrap();
        }
        let m = refused(&fx, &name);
        if case == "pair copied" {
            assert!(m.contains("belongs to another segment"), "{case}: {m}");
        } else {
            assert!(
                m.contains("differs from the copy the storage head lists"),
                "{m}"
            );
        }
    }
}

/// The laundering guard: a later acting run that finds a gap reports it (status 6 in the CLI),
/// removes what it was asked to, and keeps the gap in the head, so restore still refuses.
#[test]
fn a_gap_survives_later_runs_and_each_reports_it() {
    let fx = Fx::new();
    let _ = history(&fx);
    let last = committed(&fx).last().unwrap().clone();
    fs::remove_file(&last).unwrap();
    fs::remove_file(sum(&last)).unwrap();
    let name = last.file_name().unwrap().to_string_lossy().to_string();
    for n in ["four", "five"] {
        let rep = run_one(&fx, n, true);
        assert!(
            rep.history_gaps.iter().any(|g| g.contains(&name)),
            "{n}: {rep:?}"
        );
    }
    refused(&fx, &name);
}

/// A missing head while segments are stored is refused, and stays a gap once a later run
/// writes a new head.
#[test]
fn a_missing_head_is_refused_and_recorded() {
    let fx = Fx::new();
    let _ = history(&fx);
    for slot in ["head.0", "head.1"] {
        let _ = fs::remove_file(dir(&fx).join(slot));
    }
    // With the local manifest gone too, so the next run reads the head only when it mirrors.
    refused(
        &fx,
        "storage head that lists the committed segments was missing",
    );
    let rep = run_one(&fx, "six", true);
    assert!(!rep.history_gaps.is_empty(), "{rep:?}");
    assert!(dir(&fx).join("head.0").exists() || dir(&fx).join("head.1").exists());
    refused(&fx, "an earlier apply run found a gap");
}

/// A head that cannot be stored is a mirror failure, never skipped: until the head lists a
/// segment, its later loss would go unnoticed.
#[test]
fn a_head_that_cannot_be_stored_is_reported() {
    let fx = Fx::new();
    let _ = history(&fx);
    fx.set_fake(
        "if [ \"$cmd\" = rcat ]; then case \"$*\" in *manifests/*/head*) echo refused >&2; exit 1;; esac; fi",
    );
    fx.write("eight/f.txt", b"eight");
    let pieces = vec![P::tree(&fx.data.join("eight"), "backup")];
    let (scan, plan) = docs(&[&fx.data], &pieces);
    match apply(&scan, &plan, &fx.opts(true)) {
        Err(_) => {}
        Ok(rep) => assert!(rep.mirror_failed.is_some(), "{rep:?}"),
    }
    fx.set_fake("");
}

/// What is not a loss is not refused: the newer head slot rolled back or cut mid-write (the
/// older slot is whole), and a segment the head does not list yet.
#[test]
fn a_stale_or_cut_head_slot_alone_loses_nothing_and_restores_completely() {
    for case in [
        "newer slot deleted",
        "newer slot cut",
        "head older than a segment",
    ] {
        let fx = Fx::new();
        let before = history(&fx);
        let slots: Vec<PathBuf> = ["head.0", "head.1"]
            .iter()
            .map(|s| dir(&fx).join(s))
            .collect();
        assert!(slots.iter().all(|s| s.exists()), "{case}");
        let newer = slots
            .iter()
            .max_by_key(|s| fs::metadata(s).unwrap().modified().unwrap())
            .unwrap()
            .clone();
        let older = slots.iter().find(|s| **s != newer).unwrap().clone();
        match case {
            "newer slot deleted" => fs::remove_file(&newer).unwrap(),
            "newer slot cut" => {
                let b = fs::read(&newer).unwrap();
                fs::write(&newer, &b[..b.len() - 5]).unwrap();
            }
            _ => fs::copy(&older, &newer).map(|_| ()).unwrap(),
        }
        complete(&fx, &before);
        // A later run carries on from the older slot and nothing is reported as lost.
        fx.write("seven/f.txt", b"seven");
        let all = tree_digest(&fx.data);
        let rep = run_one(&fx, "seven", false);
        assert!(rep.history_gaps.is_empty(), "{case}: {rep:?}");
        complete(&fx, &all);
    }
}
