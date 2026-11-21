//! Manifest publication (Codex plan review r8, F1). A segment is committed history once its
//! checksum sidecar is stored; an interrupted upload is retired only after its rows are stored
//! again in committed segments, and damaged committed history is still refused.
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
use disk_cleaner_core::apply::{apply, ApplyError, CrashPoint};
use std::fs;
use std::path::PathBuf;
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

/// Two backup pieces with shared and distinct content and a hard link.
fn fixture(fx: &Fx) -> (String, String) {
    let shared = vec![7u8; 300_000];
    fx.write("a/x.bin", &shared);
    fx.write("a/only-a.txt", b"alpha");
    let y = fx.write("b/y.bin", &shared);
    fs::hard_link(&y, fx.data.join("b/y-link.bin")).unwrap();
    fx.write("b/z.txt", b"zeta distinct");
    let pieces = vec![
        P::tree(&fx.data.join("a"), "backup"),
        P::tree(&fx.data.join("b"), "backup"),
    ];
    docs(&[&fx.data], &pieces)
}

fn segments(fx: &Fx) -> Vec<PathBuf> {
    let dir = fx.remote.join("manifests/testhost");
    let mut v: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| rd.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn one_actor(fx: &Fx) -> disk_cleaner_core::apply::ApplyOptions {
    let mut o = fx.opts(true);
    o.jobs = 1; // the call counter below needs one rclone call at a time
    o
}

/// The class guard: whichever rclone call of an acting run fails, one undisturbed retry leaves
/// a history in storage that restores everything that was removed, with the local manifest
/// gone and without `--accept-partial`.
#[test]
fn any_failed_storage_call_then_a_retry_restores_completely_from_storage() {
    let probe = Fx::new();
    let (scan, plan) = fixture(&probe);
    apply(&scan, &plan, &one_actor(&probe)).unwrap();
    let total = probe.log().lines().count();
    assert!(
        total >= 8,
        "an acting run makes several rclone calls: {total}"
    );
    for n in 1..=total {
        let fx = Fx::new();
        let (scan, plan) = fixture(&fx);
        let before = tree_digest(&fx.data);
        fx.set_fake(&format!(
            "c=$(( $(cat $D/calls 2>/dev/null || echo 0) + 1 )); echo $c > $D/calls; if [ $c -eq {n} ]; then echo injected failure >&2; exit 1; fi"
        ));
        let first = apply(&scan, &plan, &one_actor(&fx));
        // A run that reports success while storage holds an uncommitted segment says so.
        let uncommitted = segments(&fx).iter().any(|p| {
            p.extension().is_some_and(|e| e == "jsonl")
                && !PathBuf::from(format!("{}.sum", p.display())).exists()
        });
        if let (Ok(rep), true) = (&first, uncommitted) {
            assert!(rep.mirror_failed.is_some(), "call {n}: {rep:?}");
        }
        fx.set_fake("");
        let retry = apply(&scan, &plan, &one_actor(&fx));
        assert!(retry.is_ok(), "call {n}: first {first:?}, retry {retry:?}");
        let _ = fs::remove_file(fx.manifest());
        let r = restore(&fx.data, &ropts(&fx));
        let r = match r {
            Ok(r) => r,
            Err(e) => panic!("call {n}: restore from storage refused: {e:?} (first run {first:?})"),
        };
        assert!(
            r.failed.is_empty() && r.conflicts.is_empty() && r.rejected_segments.is_empty(),
            "call {n}: {r:?}"
        );
        assert_eq!(tree_digest(&fx.data), before, "call {n}: {first:?}");
    }
}

/// An interrupted upload whose rows are not stored again is never retired: restore from storage
/// still refuses, and names it. Two ways: the local manifest is gone before the retry, or the
/// retry cannot store manifest segments (so a retirement judged at the start, or against the
/// local manifest, would hide rows that storage does not hold).
#[test]
fn an_interrupted_segment_whose_rows_are_not_stored_again_is_still_refused() {
    for case in ["local manifest gone", "manifest uploads fail"] {
        let fx = Fx::new();
        let (scan, plan) = fixture(&fx);
        let mut o = fx.opts(true);
        o.hooks.crash_at = Some(CrashPoint::AfterSegmentUpload);
        assert!(
            matches!(apply(&scan, &plan, &o), Err(ApplyError::InjectedCrash(_))),
            "{case}"
        );
        let orphan: Vec<_> = segments(&fx)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        assert_eq!(orphan.len(), 1, "{case}: {:?}", segments(&fx));
        if case == "local manifest gone" {
            fs::remove_file(fx.manifest()).unwrap();
            apply(&scan, &plan, &fx.opts(true)).unwrap();
        } else {
            fx.set_fake(
                // Segments and their sidecars fail; a retirement record would still be stored.
                "if [ \"$cmd\" = rcat ]; then case \"$*\" in *manifests/*.jsonl|*manifests/*.jsonl.sum) echo refused >&2; exit 1;; esac; fi",
            );
            assert!(apply(&scan, &plan, &fx.opts(true)).is_err(), "{case}");
            fx.set_fake("");
            fs::remove_file(fx.manifest()).unwrap();
        }
        assert!(
            !segments(&fx)
                .iter()
                .any(|p| p.to_string_lossy().ends_with(".abandoned")),
            "{case}: {:?}",
            segments(&fx)
        );
        let e = restore(&fx.data, &ropts(&fx)).unwrap_err();
        assert!(format!("{e:?}").contains("interrupted"), "{case}: {e:?}");
    }
}

/// A retired upload is skipped only while it holds exactly the bytes it was retired with, and
/// a committed segment whose bytes no longer match its sidecar is refused as before.
#[test]
fn a_retired_segment_that_changes_and_damaged_committed_history_are_refused() {
    let fx = Fx::new();
    let (scan, plan) = fixture(&fx);
    let mut o = fx.opts(true);
    o.hooks.crash_at = Some(CrashPoint::AfterSegmentUpload);
    let _ = apply(&scan, &plan, &o);
    apply(&scan, &plan, &fx.opts(true)).unwrap();
    let all = segments(&fx);
    let retired: Vec<_> = all
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".abandoned"))
        .collect();
    assert_eq!(retired.len(), 1, "{all:?}");
    fs::remove_file(fx.manifest()).unwrap();
    // The history restores without --accept-partial, and says what it skipped.
    let r = restore(&fx.data, &ropts(&fx)).unwrap();
    assert_eq!(r.abandoned_segments.len(), 1, "{r:?}");
    assert!(
        r.failed.is_empty() && r.rejected_segments.is_empty(),
        "{r:?}"
    );
    // The retired upload changes: no longer bound to its record, so refused again.
    let orphan = PathBuf::from(retired[0].to_string_lossy().trim_end_matches(".abandoned"));
    let mut b = fs::read(&orphan).unwrap();
    b.extend_from_slice(b"{}\n");
    fs::write(&orphan, &b).unwrap();
    let e = restore(&fx.data, &ropts(&fx)).unwrap_err();
    assert!(format!("{e:?}").contains("interrupted"), "{e:?}");
    fs::write(&orphan, &b[..b.len() - 3]).unwrap();
    assert!(restore(&fx.data, &ropts(&fx)).is_ok());
    // A committed segment whose bytes differ from its sidecar is damaged history: refused.
    let committed = all
        .iter()
        .find(|p| {
            p.extension().is_some_and(|e| e == "jsonl")
                && all.contains(&PathBuf::from(format!("{}.sum", p.display())))
        })
        .unwrap();
    let mut c = fs::read(committed).unwrap();
    c[0] ^= 1;
    fs::write(committed, &c).unwrap();
    let e = restore(&fx.data, &ropts(&fx)).unwrap_err();
    assert!(format!("{e:?}").contains("sha256 differs"), "{e:?}");
}
