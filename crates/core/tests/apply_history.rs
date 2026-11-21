//! Manifest history in storage with a gap (A-5, main's triage of segment deletion: CRITICAL).
//! The storage head lists every committed segment by name and sha256. A restore from storage
//! never succeeds silently with less history than was stored: a listed segment that is missing,
//! changed or replaced, or a missing head, refuses it (or, with `--accept-partial`, is listed).
//! The gap is permanent: a later acting run reports it and never drops it.
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
use disk_cleaner_core::apply::{apply, ApplyError, ApplyOptions, ApplyReport, CrashPoint};
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

/// One acting run that would back up and remove `data/<name>` (written first when `write`).
fn try_run(
    fx: &Fx,
    name: &str,
    write: bool,
    set: impl Fn(&mut ApplyOptions),
) -> Result<ApplyReport, ApplyError> {
    if write {
        fx.write(&format!("{name}/f.txt"), name.as_bytes());
    }
    let pieces = vec![P::tree(&fx.data.join(name), "backup")];
    let (scan, plan) = docs(&[&fx.data], &pieces);
    let mut o = fx.opts(true);
    set(&mut o);
    apply(&scan, &plan, &o)
}

/// An acting run that finds a gap does nothing (A-6): no removal, and no object written.
fn refused_to_act(fx: &Fx, name: &str, gap: &str) {
    let before = (
        tree_digest(&fx.data),
        fs::read_dir(dir(fx)).unwrap().count(),
    );
    match try_run(fx, name, false, |_| {}) {
        Err(ApplyError::HistoryGap(g)) => {
            let all = g.join("; ");
            assert!(all.contains(gap), "{all}");
            let e = ApplyError::HistoryGap(g).to_string();
            assert!(
                e.contains("--accept-history-gap") && e.contains("nothing was done"),
                "{e}"
            );
        }
        other => panic!("an acting run went ahead with a gap ({gap}): {other:?}"),
    }
    let after = (
        tree_digest(&fx.data),
        fs::read_dir(dir(fx)).unwrap().count(),
    );
    assert_eq!(before, after, "nothing removed, nothing written");
    assert!(fx.data.join(name).exists());
}

/// One acting run that backs up and removes `data/<name>` (written first when `write`).
fn run_one(fx: &Fx, name: &str, write: bool) -> ApplyReport {
    run_with(fx, name, write, |_| {})
}

fn run_with(fx: &Fx, name: &str, write: bool, set: impl Fn(&mut ApplyOptions)) -> ApplyReport {
    let rep = try_run(fx, name, write, set).unwrap();
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

/// A gap stops every acting run (ASSUMPTION A-6, main's choice): it removes nothing and writes
/// nothing, and the error lists the gap and names the flag. With `accept_history_gap` the run
/// goes ahead, the report records the gap it went ahead with, and the gap stays in the head
/// (the laundering guard), so restore still refuses and the next run stops again.
#[test]
fn a_gap_stops_acting_runs_until_accepted_and_survives_them() {
    let fx = Fx::new();
    let _ = history(&fx);
    let last = committed(&fx).last().unwrap().clone();
    fs::remove_file(&last).unwrap();
    fs::remove_file(sum(&last)).unwrap();
    let name = last.file_name().unwrap().to_string_lossy().to_string();
    for n in ["four", "five"] {
        fx.write(&format!("{n}/f.txt"), n.as_bytes());
        refused_to_act(&fx, n, &name);
        let rep = run_with(&fx, n, false, |o| o.accept_history_gap = true);
        assert!(
            rep.history_gaps.iter().any(|g| g.contains(&name)),
            "{n}: {rep:?}"
        );
    }
    refused(&fx, &name);
    fx.write("nine/f.txt", b"nine");
    refused_to_act(&fx, "nine", &name);
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
    // With the local manifest gone too.
    refused(
        &fx,
        "storage head that lists the committed segments was missing",
    );
    fx.write("six/f.txt", b"six");
    refused_to_act(
        &fx,
        "six",
        "storage head that lists the committed segments was missing",
    );
    let rep = run_with(&fx, "six", false, |o| o.accept_history_gap = true);
    assert!(!rep.history_gaps.is_empty(), "{rep:?}");
    assert!(dir(&fx).join("head.0").exists() || dir(&fx).join("head.1").exists());
    refused(&fx, "an earlier apply run found a gap");
}

/// One label, one state directory (A-5). A second state directory using the label is refused
/// before anything is done; `take_over_label` hands the label over and the report says from
/// whom; then the first state directory is the one refused.
#[test]
fn a_second_state_directory_on_one_label_is_refused_until_it_takes_over() {
    let fx = Fx::new();
    let _ = history(&fx);
    let other = fx.state.parent().unwrap().join("state-two");
    fs::create_dir_all(&other).unwrap();
    let second = |o: &mut ApplyOptions| {
        o.state_dir = other.clone();
        o.manifest = other.join("manifest.jsonl");
    };
    fx.write("ten/f.txt", b"ten");
    let before = (
        tree_digest(&fx.data),
        fs::read_dir(dir(&fx)).unwrap().count(),
    );
    match try_run(&fx, "ten", false, second) {
        Err(e @ ApplyError::LabelInUse { .. }) => {
            assert!(e.to_string().contains("--take-over-label"), "{e}")
        }
        other => panic!("a second state directory went ahead: {other:?}"),
    }
    assert_eq!(
        before,
        (
            tree_digest(&fx.data),
            fs::read_dir(dir(&fx)).unwrap().count()
        )
    );
    let first_id = fs::read_to_string(fx.state.join("lock").join("state-id")).unwrap();
    let rep = run_with(&fx, "ten", false, |o| {
        second(o);
        o.take_over_label = true;
    });
    assert_eq!(rep.took_over_label_from.as_deref(), Some(first_id.trim()));
    fx.write("eleven/f.txt", b"eleven");
    assert!(matches!(
        try_run(&fx, "eleven", false, |_| {}),
        Err(ApplyError::LabelInUse { .. })
    ));
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

/// A manifest object that is listed but cannot be read (a storage outage) stops the run with a
/// storage error: nothing removed, nothing written, and no gap recorded, so the run after the
/// outage goes ahead with clean history and restore still finds every segment.
#[test]
fn a_manifest_read_that_fails_stops_the_run_and_records_no_gap() {
    let fx = Fx::new();
    let _ = history(&fx);
    fx.write("ten/f.txt", b"ten");
    for (kind, pat) in [
        ("segment", "*manifests/*.jsonl"),
        ("sidecar", "*manifests/*.sum"),
        ("head slot", "*manifests/*/head.?"),
    ] {
        fx.set_fake(&format!(
            "if [ \"$cmd\" = cat ]; then case \"$*\" in {pat}) echo outage >&2; exit 1;; esac; fi"
        ));
        let before = (
            tree_digest(&fx.data),
            fs::read_dir(dir(&fx)).unwrap().count(),
        );
        match try_run(&fx, "ten", false, |o| o.accept_history_gap = true) {
            Err(ApplyError::Storage(e)) => assert!(e.contains("cannot read"), "{kind}: {e}"),
            other => panic!("{kind}: a failed manifest read did not stop the run: {other:?}"),
        }
        let after = (
            tree_digest(&fx.data),
            fs::read_dir(dir(&fx)).unwrap().count(),
        );
        assert_eq!(before, after, "{kind}: nothing removed, nothing written");
        let _ = fs::remove_file(fx.manifest());
        assert!(
            matches!(restore(&fx.data, &ropts(&fx)), Err(ApplyError::Storage(_))),
            "{kind}: restore must stop on a failed read, never skip the segment"
        );
        fx.set_fake("");
    }
    let rep = run_one(&fx, "ten", false);
    assert!(
        rep.history_gaps.is_empty(),
        "an outage became history: {rep:?}"
    );
    let _ = fs::remove_file(fx.manifest());
    let r = restore(&fx.data, &ropts(&fx)).unwrap();
    assert!(
        r.failed.is_empty() && r.rejected_segments.is_empty(),
        "{r:?}"
    );
    assert_eq!(fs::read(fx.data.join("one/f.txt")).unwrap(), b"one");
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

// Retired with option A (main's ruling on Codex plan review r10 F1): the test
// `a_retained_copy_stays_protected_when_local_history_is_lost_or_taken_over` (r9b F1) pinned
// the retention index rebuilt from storage. No removal relies on a retained copy any more, so
// there is nothing to protect; apply_namespaces.rs pins the outcome it guarded (every removed
// duplicate restores from its own history alone after local state is lost, the label is taken
// over, or another label or remote removes the copy).

/// Codex plan review r9b, F2: every gap a run sees is durable in the next head. A segment
/// stored with its sidecar but not yet listed (a crash before the head) and then damaged is a
/// gap; runs that go ahead with it record it once (accepted runs do not grow the head), and
/// deleting the damaged objects afterwards does not make it go away.
#[test]
fn a_damaged_segment_the_head_does_not_list_yet_stays_a_gap() {
    let fx = Fx::new();
    let _ = history(&fx);
    fx.write("seven/f.txt", b"seven");
    match try_run(&fx, "seven", false, |o| {
        o.hooks.crash_at = Some(CrashPoint::AfterSidecarUpload)
    }) {
        Err(ApplyError::InjectedCrash(_)) => {}
        other => panic!("expected the injected crash: {other:?}"),
    }
    let seg = committed(&fx).last().unwrap().clone();
    let name = seg.file_name().unwrap().to_string_lossy().to_string();
    let heads = || -> Vec<String> {
        ["head.0", "head.1"]
            .iter()
            .filter_map(|s| fs::read_to_string(dir(&fx).join(s)).ok())
            .collect()
    };
    assert!(
        heads().iter().all(|h| !h.contains(&name)),
        "the head does not list the segment yet"
    );
    let mut b = fs::read(&seg).unwrap();
    b[0] ^= 1;
    fs::write(&seg, &b).unwrap();
    fx.write("eight/f.txt", b"eight");
    refused_to_act(&fx, "eight", &name);
    for n in ["eight", "nine"] {
        fx.write(&format!("{n}/f.txt"), n.as_bytes());
        let rep = run_with(&fx, n, false, |o| o.accept_history_gap = true);
        let named = rep
            .history_gaps
            .iter()
            .filter(|g| g.contains(&name))
            .count();
        assert_eq!(named, 1, "{n}: reported once: {rep:?}");
        for h in heads() {
            let lost = h
                .lines()
                .filter(|l| l.starts_with("lost ") && l.contains(&name))
                .count();
            assert!(lost <= 1, "{n}: the head grows: {h}");
        }
        assert!(
            heads().iter().any(|h| h
                .lines()
                .any(|l| l.starts_with("lost ") && l.contains(&name))),
            "{n}: the gap is recorded"
        );
    }
    fs::remove_file(&seg).unwrap();
    fs::remove_file(sum(&seg)).unwrap();
    fx.write("ten/f.txt", b"ten");
    refused_to_act(&fx, "ten", &name);
    refused(&fx, &name);
}

/// An interrupted upload is skipped only when its rows are stored elsewhere (A-4), and left
/// for the next run to store again only while the local manifest holds them. With the local
/// manifest lost, nothing holds them: that is a gap, and an acting run stops (A-6).
#[test]
fn an_interrupted_upload_whose_rows_only_the_lost_local_manifest_held_is_a_gap() {
    let needle = "neither committed history nor the local manifest";
    let fx = Fx::new();
    let _ = history(&fx);
    fx.write("seven/f.txt", b"seven");
    match try_run(&fx, "seven", false, |o| {
        o.hooks.crash_at = Some(CrashPoint::AfterSegmentUpload)
    }) {
        Err(ApplyError::InjectedCrash(_)) => {}
        other => panic!("expected the injected crash: {other:?}"),
    }
    fs::remove_file(fx.manifest()).unwrap();
    refused_to_act(&fx, "seven", needle);
    let rep = run_with(&fx, "seven", false, |o| o.accept_history_gap = true);
    assert!(
        rep.history_gaps.iter().any(|g| g.contains(needle)),
        "{rep:?}"
    );
    refused(&fx, needle);
}

/// The merged history is in commit order (r9b F1): segment names sort in the order they were
/// committed, also for runs within one second, where run ids (`<seconds>-<random>`) do not.
#[test]
fn segment_names_sort_in_commit_order_within_one_second() {
    let fx = Fx::new();
    let names = ["s1", "s2", "s3", "s4", "s5", "s6"];
    for n in names {
        fx.write(&format!("{n}/f.txt"), n.as_bytes());
        run_with(&fx, n, false, |o| o.now = 1_800_000_000);
    }
    let by_name: Vec<PathBuf> = {
        let mut v = committed(&fx);
        v.sort();
        v
    };
    assert_eq!(by_name, committed(&fx), "name order is commit order");
    // And the runs' own rows come out in run order when the local manifest is gone.
    let runs: Vec<String> = by_name
        .iter()
        .flat_map(|p| {
            fs::read_to_string(p)
                .unwrap()
                .lines()
                .filter_map(|l| {
                    serde_json::from_str::<serde_json::Value>(l)
                        .ok()
                        .and_then(|v| v["run"].as_str().map(str::to_string))
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let mut firsts: Vec<String> = Vec::new();
    for r in runs {
        if firsts.last() != Some(&r) {
            assert!(!firsts.contains(&r), "a run's rows are split: {firsts:?}");
            firsts.push(r);
        }
    }
    assert_eq!(firsts.len(), names.len(), "{firsts:?}");
}

/// Measurement, not a gate (main's condition for r10): an acting run reads the whole label
/// history before it acts. Builds `PD_HIST_SEGMENTS` x `PD_HIST_ROWS` committed rows (default
/// 100 x 1000) from real rows with fresh run ids, then times one acting run, counts its rclone
/// calls and prints the peak RSS. Run with `--ignored --nocapture` on Linux.
#[test]
#[ignore]
fn measure_an_acting_run_over_a_large_history() {
    use sha2::{Digest, Sha256};
    let env = |k: &str, d: usize| -> usize {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (segs, per) = (env("PD_HIST_SEGMENTS", 100), env("PD_HIST_ROWS", 1000));
    let fx = Fx::new();
    let _ = history(&fx);
    let template: Vec<String> = fs::read_to_string(committed(&fx)[0].clone())
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let run_of = |l: &str| -> String {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        v["run"].as_str().unwrap().to_string()
    };
    let mut rows = 0;
    for s in 0..segs {
        let mut text = String::new();
        let mut blk = 0;
        while text.lines().count() < per {
            for l in &template {
                let run = format!("1700000000-{s:08x}{blk:08x}");
                text.push_str(&l.replace(&run_of(l), &run));
                text.push('\n');
            }
            blk += 1;
        }
        rows += text.lines().count();
        let name = format!("o{:010}-1700000000-{s:016x}-00000001.jsonl", 1000 + s);
        let bytes = text.into_bytes();
        let sum = format!(
            "{} {}\nname {name}\nend\n",
            hex::encode(Sha256::digest(&bytes)),
            bytes.len()
        );
        fs::write(dir(&fx).join(&name), &bytes).unwrap();
        fs::write(dir(&fx).join(format!("{name}.sum")), sum).unwrap();
    }
    let calls = || {
        fs::read_to_string(fx.fake_dir.join("log"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    fx.write("big/f.txt", b"big");
    let before = calls();
    let t = std::time::Instant::now();
    let rep = run_one(&fx, "big", false);
    let secs = t.elapsed().as_secs_f64();
    let hwm = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "VmHWM unknown".into());
    eprintln!(
        "PD-HIST segments={segs} rows={rows} run_seconds={secs:.2} rclone_calls={} rows_from_storage={} {hwm}",
        calls() - before,
        rep.rows_from_storage
    );
    assert!(rep.history_gaps.is_empty(), "{rep:?}");
}
