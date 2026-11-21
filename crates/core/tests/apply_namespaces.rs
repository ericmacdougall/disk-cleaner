//! Codex plan review r10, F1, and main's ruling on it (option A): a file is removed as a
//! duplicate only when its content is in verified storage under the history that records the
//! removal. So a removed duplicate never depends on another file staying, whatever later runs,
//! labels, remotes or the user do to that file, and it restores from its own history alone.
//!
//! Each case removes duplicate D (its retained copy R kept) under remote X and label `hosta`,
//! loses every piece of local state, and then changes R the way a later actor could. Afterwards
//! every removed path restores byte for byte through the route that removed it (its remote and
//! label), from storage alone with no state directory, and its content is in that history's
//! shards.
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions, RestoreReport};
use disk_cleaner_core::apply::{apply, ApplyOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a removal's history lives: a remote and a label.
#[derive(Clone)]
struct Route {
    remote: PathBuf,
    label: &'static str,
}

// --- API seam: the restore and state options of this build ---

/// Point an acting run at `route`, with its local state in `state`.
fn point_at(o: &mut ApplyOptions, route: &Route, state: &Path) {
    o.remote = route.remote.to_string_lossy().to_string();
    o.remote_local = Some(route.remote.clone());
    o.label = route.label.to_string();
    o.state_dir = state.join("lock");
    o.manifest = state.join("manifest.jsonl");
}

/// Restore `path` through `route` from storage alone, with no local state at all.
fn restore_from_storage(fx: &Fx, path: &Path, route: &Route) -> RestoreReport {
    let opts = RestoreOptions {
        accept_partial: false,
        remote: route.remote.to_string_lossy().to_string(),
        rclone: fx.fake.clone(),
        git: "git".into(),
        manifest: fx.tmp.path().join("state-none/manifest.jsonl"),
        from_remote: true,
        label: route.label.to_string(),
        run: None,
        timeout: Duration::from_secs(120),
    };
    restore(path, &opts).unwrap()
}

// --- end of the API seam ---

/// Lose every piece of local state: logs, manifests, lock, state id, caches.
fn lose_local_state(fx: &Fx) {
    for e in fs::read_dir(fx.tmp.path().canonicalize().unwrap()).unwrap() {
        let p = e.unwrap().path();
        if p.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("state")
        {
            fs::remove_dir_all(&p).unwrap();
        }
    }
}

fn state(fx: &Fx, name: &str) -> PathBuf {
    let s = fx.tmp.path().canonicalize().unwrap().join(name);
    fs::create_dir_all(&s).unwrap();
    s
}

/// A duplicate-noise piece of one file `name` in `dir`, whose retained copy `retained` lies in
/// kept piece `retained_piece`.
fn dup(dir: &Path, name: &str, retained: &Path, retained_piece: usize) -> P {
    let mut p = P::files(dir, &[name], "noise");
    p.evidence = Some(serde_json::json!({
        "kind": "duplicate",
        "sha256": sha(retained),
        "size": fs::metadata(retained).unwrap().len(),
        "retained_copy": retained,
        "retained_piece": retained_piece,
    }));
    p
}

/// One acting run of `pieces` through `route`; every piece not in the keep lane must be removed.
fn act(fx: &Fx, route: &Route, st: &Path, pieces: Vec<P>, take_over: bool) {
    let (scan, plan) = docs(&[&fx.data], &pieces);
    let mut o = fx.opts(true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    point_at(&mut o, route, st);
    o.take_over_label = take_over;
    let rep = apply(&scan, &plan, &o).unwrap();
    for (k, p) in pieces.iter().enumerate() {
        if p.lane != "keep" {
            assert_eq!(
                rep.pieces[k].outcome, "removed",
                "piece {k} under {}: {rep:?}",
                route.label
            );
        }
    }
}

/// Every byte stored under `route`'s shards.
fn shard_bytes(route: &Route) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stack = vec![route.remote.join("shards").join(route.label)];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.extend(fs::read(e.path()).unwrap());
            }
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[derive(Clone, Copy, Debug)]
enum Later {
    /// The user deletes R by hand.
    HandDeletes,
    /// A run under another label on the same remote removes R (backup lane).
    OtherLabelRemoves,
    /// A run under the same label on another remote removes R (backup lane).
    OtherRemoteRemoves,
    /// Under label `hostb`, R is removed as a duplicate of T; then T is removed under `hostc`.
    ChainAcrossLabels,
    /// Under label `hostb`, R is removed as a duplicate of T; then T is removed under `hostb`.
    ChainInOneLabel,
    /// A new state directory takes label `hosta` over on remote X and removes R (backup lane).
    TakeOverRemoves,
}

fn run_case(later: Later) {
    let fx = Fx::new();
    let x = Route {
        remote: fx.remote.clone(),
        label: "hosta",
    };
    let other_remote = fx.tmp.path().canonicalize().unwrap().join("remote-y");
    fs::create_dir_all(&other_remote).unwrap();
    let on_x = |label| Route {
        remote: fx.remote.clone(),
        label,
    };
    let y = Route {
        remote: other_remote,
        label: "hosta",
    };
    let content = format!("the same bytes in three places ({later:?})\n").into_bytes();
    let r = fx.write("keep/r.txt", &content);
    let d = fx.write("dups/d.txt", &content);
    let t = fx.write("other/t.txt", &content);

    // D is removed as a duplicate of R under X/hosta; then every piece of local state goes.
    let st = state(&fx, "state-1");
    act(
        &fx,
        &x,
        &st,
        vec![
            P::tree(&fx.data.join("keep"), "keep"),
            dup(&fx.data.join("dups"), "d.txt", &r, 0),
        ],
        false,
    );
    assert!(!d.exists());
    lose_local_state(&fx);

    // A later actor changes R.
    let mut removed: Vec<(PathBuf, Route)> = vec![(d.clone(), x.clone())];
    let keep_r = || vec![P::tree(&fx.data.join("keep"), "backup")];
    let r_as_dup_of_t = || {
        vec![
            P::tree(&fx.data.join("other"), "keep"),
            dup(&fx.data.join("keep"), "r.txt", &t, 0),
        ]
    };
    let remove_t = || vec![P::tree(&fx.data.join("other"), "backup")];
    let st = state(&fx, "state-2");
    match later {
        Later::HandDeletes => fs::remove_file(&r).unwrap(),
        Later::OtherLabelRemoves => {
            act(&fx, &on_x("hostb"), &st, keep_r(), false);
            removed.push((r.clone(), on_x("hostb")));
        }
        Later::OtherRemoteRemoves => {
            act(&fx, &y, &st, keep_r(), false);
            removed.push((r.clone(), y.clone()));
        }
        Later::ChainAcrossLabels => {
            act(&fx, &on_x("hostb"), &st, r_as_dup_of_t(), false);
            let st3 = state(&fx, "state-3");
            act(&fx, &on_x("hostc"), &st3, remove_t(), false);
            removed.push((r.clone(), on_x("hostb")));
            removed.push((t.clone(), on_x("hostc")));
        }
        Later::ChainInOneLabel => {
            act(&fx, &on_x("hostb"), &st, r_as_dup_of_t(), false);
            act(&fx, &on_x("hostb"), &st, remove_t(), false);
            removed.push((r.clone(), on_x("hostb")));
            removed.push((t.clone(), on_x("hostb")));
        }
        Later::TakeOverRemoves => {
            act(&fx, &x, &st, keep_r(), true);
            removed.push((r.clone(), x.clone()));
        }
    }
    assert!(!r.exists(), "the retained copy is gone");
    lose_local_state(&fx);

    // Every removed path restores through its own route, from storage alone, in removal order
    // (so no restore can lean on a file an earlier restore put back). Every path is tried and
    // every failure reported, so a chain shows each link.
    let mut failures = Vec::new();
    for (path, route) in &removed {
        let what = format!("{} through {}", path.display(), route.label);
        let rep = restore_from_storage(&fx, path, route);
        if !(rep.failed.is_empty()
            && rep.conflicts.is_empty()
            && rep.restored == vec![path.clone()])
        {
            failures.push(format!("{what}: {rep:?}"));
        } else if fs::read(path).ok().as_deref() != Some(&content[..]) {
            failures.push(format!("{what}: restored with other bytes"));
        }
        if !contains(&shard_bytes(route), &content) {
            failures.push(format!(
                "{what}: its content is not in the shards of the history that removed it"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_removed_duplicate_restores_after_the_user_deletes_its_retained_copy() {
    run_case(Later::HandDeletes);
}

#[test]
fn a_removed_duplicate_restores_after_another_label_removes_its_retained_copy() {
    run_case(Later::OtherLabelRemoves);
}

#[test]
fn a_removed_duplicate_restores_after_another_remote_removes_its_retained_copy() {
    run_case(Later::OtherRemoteRemoves);
}

#[test]
fn a_chain_of_duplicates_across_labels_restores_link_by_link() {
    run_case(Later::ChainAcrossLabels);
}

#[test]
fn a_chain_of_duplicates_in_one_label_restores_link_by_link() {
    run_case(Later::ChainInOneLabel);
}

#[test]
fn a_removed_duplicate_restores_after_a_take_over_removes_its_retained_copy() {
    run_case(Later::TakeOverRemoves);
}
