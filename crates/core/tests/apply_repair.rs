//! Apply repairs after plan review r6b and the acceptance review (goodqa#16529). One guard test
//! per defect class, each at its choke point:
//! - removal binds the unlink to the checked inode (`fsops::remove_member`, r6b F1);
//! - the manifest's torn tail is repaired before an acting run appends (r6b F2);
//! - the retention index keeps a retained copy from being removed without a backup (N4);
//! - an archived shard is reused only when its stored hash still matches (I-1, NEG-8);
//! - noise whose evidence fails is backed up instead (AC-7, NEG-7).
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::fsops::{self, RemoveStage, Unlinked, INJECTED_CRASH};
use disk_cleaner_core::apply::manifest::Manifest;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
use disk_cleaner_core::apply::{apply, ApplyError};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

fn set_hook(f: impl FnMut(RemoveStage, &str) -> std::io::Result<()> + 'static) {
    fsops::REMOVE_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

fn clear_hook() {
    fsops::REMOVE_HOOK.with(|h| *h.borrow_mut() = None);
}

/// Names in `dir` that are quarantine leftovers.
fn leftovers(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with(".pd-rm-") {
                out.push(e.path().display().to_string());
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            }
        }
    }
    out
}

/// Atomically put `bytes` at `target` (write a sibling, rename over).
fn swap_in(target: &Path, bytes: &[u8]) {
    let tmp = target.with_extension("swap-tmp");
    fs::write(&tmp, bytes).unwrap();
    fs::rename(&tmp, target).unwrap();
}

// ---- Class 1: removal binds the unlink to the checked inode (r6b F1) ----

#[test]
fn a_leaf_swapped_in_after_the_check_is_never_removed() {
    let fx = Fx::new();
    let f = fx.write("g/sub/f.txt", b"original");
    let root = fsops::open_dir_abs(&fx.data.join("g")).unwrap();
    let id = fsops::lstat_rel(&root, "sub/f.txt").unwrap();
    let target = f.clone();
    set_hook(move |stage, _| {
        if stage == RemoveStage::BeforeQuarantine {
            swap_in(&target, b"SWAPPED-IN-REPLACEMENT");
        }
        Ok(())
    });
    let r = fsops::remove_member(&root, "sub/f.txt", &id, false);
    clear_hook();
    assert!(matches!(r, Ok(Unlinked::Changed(_))), "{r:?}");
    assert_eq!(fs::read(&f).unwrap(), b"SWAPPED-IN-REPLACEMENT");
    assert!(leftovers(&fx.data).is_empty(), "{:?}", leftovers(&fx.data));
}

#[test]
fn a_swap_that_takes_both_names_leaves_both_files() {
    // The moved entry is replaced and the original name is taken again: nothing is removed,
    // and the quarantined replacement is reported where it is.
    let fx = Fx::new();
    let f = fx.write("g/f.txt", b"original");
    let root = fsops::open_dir_abs(&fx.data.join("g")).unwrap();
    let id = fsops::lstat_rel(&root, "f.txt").unwrap();
    let q = fx.data.join("g").join(fsops::quarantine_name("f.txt"));
    let (target, qq) = (f.clone(), q.clone());
    set_hook(move |stage, _| {
        if stage == RemoveStage::AfterQuarantine {
            swap_in(&qq, b"REPLACED-AT-QUARANTINE");
            fs::write(&target, b"NEW-AT-ORIGINAL-NAME").unwrap();
        }
        Ok(())
    });
    let r = fsops::remove_member(&root, "f.txt", &id, false);
    clear_hook();
    assert!(matches!(r, Ok(Unlinked::Displaced(_))), "{r:?}");
    assert_eq!(fs::read(&q).unwrap(), b"REPLACED-AT-QUARANTINE");
    assert_eq!(fs::read(&f).unwrap(), b"NEW-AT-ORIGINAL-NAME");
}

#[test]
fn a_swapped_member_survives_a_real_apply() {
    let fx = Fx::new();
    let f1 = fx.write("p/f1.txt", b"one");
    let f2 = fx.write("p/f2.txt", b"two");
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&fx.data.join("p"), "backup")]);
    let target = f1.clone();
    set_hook(move |stage, rel| {
        if stage == RemoveStage::BeforeQuarantine && rel == "f1.txt" {
            swap_in(&target, b"UNBACKED-REPLACEMENT");
        }
        Ok(())
    });
    let rep = apply(&scan, &plan, &fx.opts(true));
    clear_hook();
    let rep = rep.unwrap();
    assert_ne!(rep.pieces[0].outcome, "removed", "{rep:?}");
    assert_eq!(fs::read(&f1).unwrap(), b"UNBACKED-REPLACEMENT");
    assert!(!f2.exists(), "the unchanged member is removed: {rep:?}");
    assert!(leftovers(&fx.data).is_empty());
}

#[test]
fn a_crash_after_the_quarantine_move_resumes_cleanly() {
    let fx = Fx::new();
    fx.write("p/f1.txt", b"one");
    fx.write("p/f2.txt", b"two");
    let digest = tree_digest(&fx.data.join("p"));
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&fx.data.join("p"), "backup")]);
    let mut fired = false;
    set_hook(move |stage, _| {
        if stage == RemoveStage::AfterQuarantine && !fired {
            fired = true;
            return Err(std::io::Error::other(INJECTED_CRASH));
        }
        Ok(())
    });
    let first = apply(&scan, &plan, &fx.opts(true));
    clear_hook();
    assert!(
        matches!(first, Err(ApplyError::InjectedCrash(_))),
        "{first:?}"
    );
    assert_eq!(
        leftovers(&fx.data).len(),
        1,
        "the crash left the moved entry"
    );
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
    assert!(!fx.data.join("p").exists(), "{rep:?}");
    assert!(leftovers(&fx.data).is_empty());
    let r = restore(&fx.data.join("p"), &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty() && r.conflicts.is_empty(), "{r:?}");
    assert_eq!(tree_digest(&fx.data.join("p")), digest);
}

// ---- Class 2: the manifest's torn tail is repaired before an acting run appends (r6b F2) ----

fn two_pieces(fx: &Fx) -> ((String, String), (String, String)) {
    fx.write("a/x.txt", b"alpha bytes");
    fx.write("b/y.txt", b"beta bytes");
    let a = docs(&[&fx.data], &[P::tree(&fx.data.join("a"), "backup")]);
    let b = docs(&[&fx.data], &[P::tree(&fx.data.join("b"), "backup")]);
    (a, b)
}

#[test]
fn a_torn_tail_is_repaired_and_later_runs_stay_loadable() {
    let fx = Fx::new();
    let (a, b) = two_pieces(&fx);
    let da = tree_digest(&fx.data.join("a"));
    let db = tree_digest(&fx.data.join("b"));
    apply(&a.0, &a.1, &fx.opts(true)).unwrap();
    let torn = b"{\"v\":1,\"run\":\"crashed\",\"at\":1,\"t\":\"rem";
    let mut bytes = fs::read(fx.manifest()).unwrap();
    bytes.extend_from_slice(torn);
    fs::write(fx.manifest(), &bytes).unwrap();

    let rep = apply(&b.0, &b.1, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
    // A third run must still load it: the torn bytes did not become a corrupt middle line.
    Manifest::load(&fx.manifest(), "check").expect("the repaired manifest loads");
    let text = fs::read_to_string(fx.manifest()).unwrap();
    assert!(!text.contains("\"crashed\""), "the torn tail was cut");
    assert!(text.ends_with('\n'));
    let side: Vec<PathBuf> = fs::read_dir(&fx.state)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().contains(".torn-"))
        .collect();
    assert_eq!(side.len(), 1, "{side:?}");
    assert_eq!(fs::read(&side[0]).unwrap(), torn);
    // Both pieces restore from the local manifest, and from the remote copies alone.
    let r = restore(&fx.data, &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty() && r.conflicts.is_empty(), "{r:?}");
    assert_eq!(tree_digest(&fx.data.join("a")), da);
    assert_eq!(tree_digest(&fx.data.join("b")), db);
    fs::remove_dir_all(fx.data.join("a")).unwrap();
    fs::remove_dir_all(fx.data.join("b")).unwrap();
    fs::remove_file(fx.manifest()).unwrap();
    let mut ro = ropts(&fx);
    ro.from_remote = true;
    let r = restore(&fx.data, &ro).unwrap();
    assert!(r.failed.is_empty() && r.conflicts.is_empty(), "{r:?}");
    assert_eq!(tree_digest(&fx.data.join("a")), da);
    assert_eq!(tree_digest(&fx.data.join("b")), db);
}

#[test]
fn a_whole_last_row_without_its_newline_is_kept() {
    let fx = Fx::new();
    let (a, b) = two_pieces(&fx);
    apply(&a.0, &a.1, &fx.opts(true)).unwrap();
    let before = Manifest::load(&fx.manifest(), "check").unwrap().rows.len();
    let mut bytes = fs::read(fx.manifest()).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    fs::write(fx.manifest(), &bytes).unwrap();
    apply(&b.0, &b.1, &fx.opts(true)).unwrap();
    let m = Manifest::load(&fx.manifest(), "check").expect("loads");
    assert!(m.rows.len() > before, "the unterminated row was kept");
    let first = Manifest::load(&fx.manifest(), "check").unwrap();
    assert_eq!(first.rows.len(), m.rows.len());
}

#[test]
fn a_failed_repair_stops_before_anything_is_removed() {
    let fx = Fx::new();
    let (a, b) = two_pieces(&fx);
    apply(&a.0, &a.1, &fx.opts(true)).unwrap();
    let mut bytes = fs::read(fx.manifest()).unwrap();
    bytes.extend_from_slice(b"{\"v\":1,\"ru");
    fs::write(fx.manifest(), &bytes).unwrap();
    // The side copy cannot be written: the repair must fail and change nothing.
    fs::set_permissions(&fx.state, fs::Permissions::from_mode(0o500)).unwrap();
    let res = apply(&b.0, &b.1, &fx.opts(true));
    fs::set_permissions(&fx.state, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(res, Err(ApplyError::Manifest(_))), "{res:?}");
    assert!(fx.data.join("b/y.txt").exists(), "nothing removed");
    assert_eq!(
        fs::read(fx.manifest()).unwrap(),
        bytes,
        "manifest unchanged"
    );
}

// ---- Class 3: the retention index (N4) ----

#[test]
fn retained_copy_protected_in_later_run() {
    let fx = Fx::new();
    let content = b"the same bytes in three places\n";
    let orig = fx.write("keep/orig.txt", content);
    let dup = fx.write("dups/copy.txt", content);
    let third = fx.write("other/third.txt", content);
    // Run 1: copy.txt is removed as a duplicate of keep/orig.txt.
    let mut piece = P::files(&fx.data.join("dups"), &["copy.txt"], "noise");
    piece.evidence = Some(serde_json::json!({
        "kind": "duplicate", "sha256": sha(&orig), "size": content.len(),
        "retained_copy": orig, "retained_piece": 0
    }));
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::tree(&fx.data.join("keep"), "keep"), piece],
    );
    let mut o = fx.opts(true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.pieces[1].outcome, "removed", "{rep:?}");
    // Run 2: a later plan proposes removing keep/orig.txt as a duplicate of other/third.txt.
    let mut piece = P::files(&fx.data.join("keep"), &["orig.txt"], "noise");
    piece.evidence = Some(serde_json::json!({
        "kind": "duplicate", "sha256": sha(&third), "size": content.len(),
        "retained_copy": third, "retained_piece": 0
    }));
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::tree(&fx.data.join("other"), "keep"), piece],
    );
    let mut o = fx.opts(true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.pieces[1].outcome, "kept", "{rep:?}");
    assert!(rep.pieces[1].reason.contains("retained copy"), "{rep:?}");
    assert!(orig.exists());
    let r = restore(&dup, &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read(&dup).unwrap(), content);
}

// ---- Class 4: an archived shard is reused only when its stored hash matches (NEG-8) ----

fn flip_one_byte_in_every_shard(fx: &Fx) -> usize {
    let mut n = 0;
    let mut stack = vec![fx.remote.join("shards")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                let mut b = fs::read(e.path()).unwrap();
                let i = b.len() / 2;
                b[i] ^= 0xff;
                fs::write(e.path(), &b).unwrap();
                n += 1;
            }
        }
    }
    n
}

#[test]
fn a_same_size_altered_or_purged_shard_is_never_reused() {
    for (case, readback) in [
        ("altered, hash", false),
        ("altered, readback", true),
        ("purged", false),
    ] {
        let fx = Fx::new();
        let content = vec![5u8; 200_000];
        fx.write("a/x.bin", &content);
        fx.write("b/y.bin", &content);
        let a = docs(&[&fx.data], &[P::tree(&fx.data.join("a"), "backup")]);
        let b = docs(&[&fx.data], &[P::tree(&fx.data.join("b"), "backup")]);
        let mut o = fx.opts(true);
        o.hooks.force_readback = readback;
        let rep = apply(&a.0, &a.1, &o).unwrap();
        assert_eq!(rep.shards_uploaded, 1, "{case}: {rep:?}");
        if case == "purged" {
            fs::remove_dir_all(fx.remote.join("shards")).unwrap();
        } else {
            assert!(flip_one_byte_in_every_shard(&fx) > 0, "{case}");
        }
        let rep = apply(&b.0, &b.1, &fx.opts(true)).unwrap();
        assert_eq!(
            rep.shards_uploaded, 1,
            "{case}: the content was uploaded again: {rep:?}"
        );
        assert_eq!(rep.pieces[0].outcome, "removed", "{case}: {rep:?}");
        let r = restore(&fx.data.join("b"), &ropts(&fx)).unwrap();
        assert!(r.failed.is_empty(), "{case}: {r:?}");
        assert_eq!(
            fs::read(fx.data.join("b/y.bin")).unwrap(),
            content,
            "{case}"
        );
    }
}

#[test]
fn an_intact_shard_is_reused_without_upload() {
    let fx = Fx::new();
    let content = vec![6u8; 200_000];
    fx.write("a/x.bin", &content);
    fx.write("b/y.bin", &content);
    let a = docs(&[&fx.data], &[P::tree(&fx.data.join("a"), "backup")]);
    let b = docs(&[&fx.data], &[P::tree(&fx.data.join("b"), "backup")]);
    apply(&a.0, &a.1, &fx.opts(true)).unwrap();
    let rep = apply(&b.0, &b.1, &fx.opts(true)).unwrap();
    assert_eq!(rep.shards_uploaded, 0, "{rep:?}");
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
}

// ---- Class 5: noise whose evidence fails is backed up instead (AC-7, NEG-7) ----

#[test]
fn a_duplicate_whose_proof_fails_is_backed_up_instead() {
    for case in [
        "retained changed",
        "retained is a symlink",
        "retained in a removing piece",
    ] {
        let fx = Fx::new();
        let content = b"duplicate content for the fallback\n";
        let orig = fx.write("keep/orig.txt", content);
        let dup = fx.write("dups/copy.txt", content);
        let want_sha = sha(&orig);
        let mut retained_lane = "keep";
        let mut retained = orig.clone();
        match case {
            "retained changed" => {
                fs::write(&orig, b"edited after the plan").unwrap();
            }
            "retained is a symlink" => {
                let real = fx.write("elsewhere/real.txt", content);
                let link = fx.data.join("keep/link.txt");
                std::os::unix::fs::symlink(&real, &link).unwrap();
                retained = link;
            }
            _ => retained_lane = "backup",
        }
        let mut piece = P::files(&fx.data.join("dups"), &["copy.txt"], "noise");
        piece.evidence = Some(serde_json::json!({
            "kind": "duplicate", "sha256": want_sha, "size": content.len(),
            "retained_copy": retained, "retained_piece": 0
        }));
        let (scan, plan) = docs(
            &[&fx.data],
            &[P::tree(&fx.data.join("keep"), retained_lane), piece],
        );
        let mut o = fx.opts(true);
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        let rep = apply(&scan, &plan, &o).unwrap();
        assert_eq!(rep.pieces[1].outcome, "removed", "{case}: {rep:?}");
        assert!(
            rep.pieces[1].reason.contains("backed up"),
            "{case}: {rep:?}"
        );
        assert!(fx.shard_total() > 0, "{case}: the duplicate was archived");
        // It comes back from the archive, whatever happened to the retained copy.
        let _ = fs::remove_file(&orig);
        let r = restore(&dup, &ropts(&fx)).unwrap();
        assert!(r.failed.is_empty(), "{case}: {r:?}");
        assert_eq!(fs::read(&dup).unwrap(), content, "{case}");
    }
}

#[test]
fn a_dry_run_says_failed_noise_would_be_backed_up() {
    let fx = Fx::new();
    let content = b"dry run fallback\n";
    let orig = fx.write("keep/orig.txt", content);
    fx.write("dups/copy.txt", content);
    let want = sha(&orig);
    fs::write(&orig, b"changed").unwrap();
    let mut piece = P::files(&fx.data.join("dups"), &["copy.txt"], "noise");
    piece.evidence = Some(serde_json::json!({
        "kind": "duplicate", "sha256": want, "size": content.len(),
        "retained_copy": orig, "retained_piece": 0
    }));
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::tree(&fx.data.join("keep"), "keep"), piece],
    );
    let mut o = fx.opts(false);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert!(rep.pieces[1].outcome.contains("back up"), "{rep:?}");
    assert!(rep.uploaded_bytes > 0, "{rep:?}");
}
