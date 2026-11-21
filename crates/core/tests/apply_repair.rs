//! Apply repairs after plan review r6b and the acceptance review (goodqa#16529). One guard test
//! per defect class, each at its choke point:
//! - removal binds the unlink to the checked inode, and a failure past the quarantine move puts
//!   the member back (`fsops::remove_member`, r6b F1);
//! - the manifest's torn tail, also one that is not UTF-8, is repaired before an acting run
//!   appends (`manifest::open_for_append` and `Manifest::load`, r6b F2);
//! - a duplicate is backed up like the backup lane, so no removal relies on another copy
//!   staying (option A, r10 F1; the retention index of N4 is gone);
//! - an archived shard is reused only when its stored hash still matches
//!   (`shard::shard_still_verified`, I-1, NEG-8);
//! - noise whose evidence fails is backed up instead, under the backup lane's secret rule (AC-7,
//!   NEG-7);
//! - the tests the spec named but lacked (acceptance finding 4).
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

/// Codex plan review r7, F1: without an atomic no-replace rename there is no check-then-rename
/// fallback that could overwrite a file created in between. The member stays, byte for byte,
/// and a real apply keeps it with the reason.
#[test]
fn without_an_atomic_no_replace_rename_nothing_is_removed() {
    let fx = Fx::new();
    let f = fx.write("g/f.txt", b"original");
    let root = fsops::open_dir_abs(&fx.data.join("g")).unwrap();
    let id = fsops::lstat_rel(&root, "f.txt").unwrap();
    fsops::NOREPLACE_UNSUPPORTED.with(|c| c.set(true));
    let r = fsops::remove_member(&root, "f.txt", &id, false);
    fsops::NOREPLACE_UNSUPPORTED.with(|c| c.set(false));
    let err = r.expect_err("no removal without an atomic rename");
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
    assert_eq!(fs::read(&f).unwrap(), b"original");
    assert!(leftovers(&fx.data).is_empty(), "{:?}", leftovers(&fx.data));

    let fx = Fx::new();
    let f1 = fx.write("p/f1.txt", b"one");
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&fx.data.join("p"), "backup")]);
    fsops::NOREPLACE_UNSUPPORTED.with(|c| c.set(true));
    let rep = apply(&scan, &plan, &fx.opts(true));
    fsops::NOREPLACE_UNSUPPORTED.with(|c| c.set(false));
    let rep = rep.unwrap();
    assert_ne!(rep.pieces[0].outcome, "removed", "{rep:?}");
    assert_eq!(fs::read(&f1).unwrap(), b"one");
    let m = fs::read_to_string(fx.manifest()).unwrap();
    assert!(m.contains(fsops::NO_ATOMIC_RENAME), "{m}");
    assert!(leftovers(&fx.data).is_empty());
}

/// A file created at the quarantine name before the capture is never replaced: the capture
/// fails, the member stays, and the other file keeps its bytes.
#[test]
fn a_file_created_at_the_quarantine_name_is_never_replaced() {
    let fx = Fx::new();
    let f = fx.write("g/f.txt", b"original");
    let root = fsops::open_dir_abs(&fx.data.join("g")).unwrap();
    let id = fsops::lstat_rel(&root, "f.txt").unwrap();
    let q = fx.data.join("g").join(fsops::quarantine_name("f.txt"));
    let qq = q.clone();
    set_hook(move |stage, _| {
        if stage == RemoveStage::BeforeQuarantine {
            fs::write(&qq, b"SOMEONE-ELSES-FILE").unwrap();
        }
        Ok(())
    });
    let r = fsops::remove_member(&root, "f.txt", &id, false);
    clear_hook();
    assert!(r.is_err(), "{r:?}");
    assert_eq!(fs::read(&f).unwrap(), b"original");
    assert_eq!(fs::read(&q).unwrap(), b"SOMEONE-ELSES-FILE");
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

#[test]
fn a_failed_unlink_after_the_quarantine_move_puts_the_member_back() {
    // An error past the move (here the unlink) must never leave the file under its hidden
    // quarantine name while the manifest says it was kept at its own name.
    let fx = Fx::new();
    let f = fx.write("g/f.txt", b"original");
    let root = fsops::open_dir_abs(&fx.data.join("g")).unwrap();
    let id = fsops::lstat_rel(&root, "f.txt").unwrap();
    set_hook(|stage, _| {
        if stage == RemoveStage::Unlink {
            return Err(std::io::Error::from_raw_os_error(1)); // EPERM
        }
        Ok(())
    });
    let r = fsops::remove_member(&root, "f.txt", &id, false);
    clear_hook();
    assert!(r.is_err(), "{r:?}");
    assert_eq!(fs::read(&f).unwrap(), b"original");
    assert!(leftovers(&fx.data).is_empty(), "{:?}", leftovers(&fx.data));

    // Through a real apply: the piece is partial, the member is back where it was.
    let p1 = fx.write("p/f1.txt", b"one");
    let p2 = fx.write("p/f2.txt", b"two");
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&fx.data.join("p"), "backup")]);
    set_hook(|stage, rel| {
        if stage == RemoveStage::Unlink && rel == "f1.txt" {
            return Err(std::io::Error::from_raw_os_error(1));
        }
        Ok(())
    });
    let rep = apply(&scan, &plan, &fx.opts(true));
    clear_hook();
    let rep = rep.unwrap();
    assert_eq!(rep.pieces[0].outcome, "partial", "{rep:?}");
    assert_eq!(fs::read(&p1).unwrap(), b"one");
    assert!(!p2.exists(), "{rep:?}");
    assert!(leftovers(&fx.data).is_empty(), "{:?}", leftovers(&fx.data));
}

// ---- Class 2: the manifest's torn tail is repaired before an acting run appends (r6b F2) ----

#[test]
fn a_torn_tail_that_is_not_utf8_is_repaired_not_refused() {
    // A tail cut inside a multi-byte character, and NUL padding after a power loss: neither is
    // UTF-8, and neither may make the manifest unloadable.
    for (case, tail) in [
        (
            "cut inside a character",
            b"{\"v\":1,\"run\":\"c\",\"path\":\"caf\xc3".to_vec(),
        ),
        ("NUL padding", vec![0u8; 4096]),
    ] {
        let fx = Fx::new();
        let (a, b) = two_pieces(&fx);
        let db = tree_digest(&fx.data.join("b"));
        apply(&a.0, &a.1, &fx.opts(true)).unwrap();
        let mut bytes = fs::read(fx.manifest()).unwrap();
        bytes.extend_from_slice(&tail);
        fs::write(fx.manifest(), &bytes).unwrap();
        // A dry run reads it (and changes nothing).
        let dry = apply(&b.0, &b.1, &fx.opts(false));
        assert!(dry.is_ok(), "{case}: {dry:?}");
        assert_eq!(
            fs::read(fx.manifest()).unwrap(),
            bytes,
            "{case}: dry run wrote"
        );
        let rep = apply(&b.0, &b.1, &fx.opts(true)).unwrap();
        assert_eq!(rep.pieces[0].outcome, "removed", "{case}: {rep:?}");
        let m = Manifest::load(&fx.manifest(), "check").expect("the repaired manifest loads");
        assert!(!m.rows.is_empty(), "{case}");
        assert!(!fs::read(fx.manifest()).unwrap().contains(&0u8), "{case}");
        let r = restore(&fx.data.join("b"), &ropts(&fx)).unwrap();
        assert!(
            r.failed.is_empty() && r.conflicts.is_empty(),
            "{case}: {r:?}"
        );
        assert_eq!(tree_digest(&fx.data.join("b")), db, "{case}");
    }
}

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
    let m0 = Manifest::load(&fx.manifest(), "check").unwrap();
    let run_a = m0.rows[0].run.clone();
    let rows_a = m0.rows.iter().filter(|e| e.run == run_a).count();
    let mut bytes = fs::read(fx.manifest()).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    fs::write(fx.manifest(), &bytes).unwrap();
    let rep = apply(&b.0, &b.1, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
    let m = Manifest::load(&fx.manifest(), "check").expect("loads");
    assert_eq!(
        m.rows.iter().filter(|e| e.run == run_a).count(),
        rows_a,
        "every row of the first run is kept, its unterminated last row included"
    );
    assert!(m.rows.iter().any(|e| e.run != run_a));
    let side = fs::read_dir(&fx.state)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".torn-"))
        .count();
    assert_eq!(side, 0, "a whole row is not torn");
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

// ---- Class 3: a duplicate is backed up like the backup lane (option A, Codex plan review r10 F1) ----
//
// The retention index (N4, `protect_retained`) is gone: no removal relies on another copy
// staying. Its guards (r7 F3 a retained copy edited later, r8 F2 a retained copy changed during
// the run, a secret-named retained copy) are replaced by one rule: a duplicate's own bytes are
// uploaded and verified before it is removed, under the backup lane's secret rule (main's Q2).
// apply_namespaces.rs covers what later runs, labels, remotes and take-overs do to the copy.

fn holds(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn dup_piece(dir: &Path, name: &str, retained: &Path) -> P {
    let mut piece = P::files(dir, &[name], "noise");
    piece.evidence = Some(serde_json::json!({
        "kind": "duplicate", "sha256": sha(retained),
        "size": fs::metadata(retained).unwrap().len(),
        "retained_copy": retained, "retained_piece": 0
    }));
    piece
}

#[test]
fn a_duplicate_is_backed_up_so_its_retained_copy_can_go_later() {
    let fx = Fx::new();
    let content = b"the same bytes in three places\n";
    let orig = fx.write("keep/orig.txt", content);
    let dup = fx.write("dups/copy.txt", content);
    let third = fx.write("other/third.txt", content);
    // A dry run says the duplicate is backed up, and counts its bytes in the upload bound.
    let pieces = [
        P::tree(&fx.data.join("keep"), "keep"),
        dup_piece(&fx.data.join("dups"), "copy.txt", &orig),
    ];
    let (scan, plan) = docs(&[&fx.data], &pieces);
    let mut o = fx.opts(false);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let dry = apply(&scan, &plan, &o).unwrap();
    assert!(dry.pieces[1].outcome.contains("back up"), "{dry:?}");
    assert!(dry.uploaded_bytes >= content.len() as u64, "{dry:?}");
    let mut o = fx.opts(true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.pieces[1].outcome, "removed", "{rep:?}");
    assert!(rep.pieces[1].reason.contains("backed up"), "{rep:?}");
    assert!(
        holds(&fx.all_shard_bytes(), content),
        "the duplicate's bytes are stored"
    );
    // A later plan removes the retained copy as a duplicate of third.txt: nothing protects it,
    // and nothing needs to.
    let (scan, plan) = docs(
        &[&fx.data],
        &[
            P::tree(&fx.data.join("other"), "keep"),
            dup_piece(&fx.data.join("keep"), "orig.txt", &third),
        ],
    );
    let mut o = fx.opts(true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.pieces[1].outcome, "removed", "{rep:?}");
    assert!(!orig.exists());
    // Third goes too, by hand. Both removed files come back from storage alone.
    fs::remove_file(&third).unwrap();
    for p in [&dup, &orig] {
        let r = restore(p, &ropts(&fx)).unwrap();
        assert!(
            r.failed.is_empty() && r.restored == vec![p.clone()],
            "{r:?}"
        );
        assert_eq!(fs::read(p).unwrap(), content);
    }
}

/// Codex plan review r8, F2 asked for the retained copy to be checked again just before
/// removal. Under option A the retained copy proves nothing, so changing or deleting it while
/// the manifest is uploaded changes nothing: the duplicate is removed on its own verified blob
/// and restores byte for byte.
#[test]
fn a_retained_copy_changed_or_deleted_during_the_run_does_not_matter() {
    for action in ["edit", "delete"] {
        let fx = Fx::new();
        let content = b"the same bytes in two places\n";
        let orig = fx.write("keep/orig.txt", content);
        let copy = fx.write("dups/copy.txt", content);
        let (scan, plan) = docs(
            &[&fx.data],
            &[
                P::tree(&fx.data.join("keep"), "keep"),
                dup_piece(&fx.data.join("dups"), "copy.txt", &orig),
            ],
        );
        let change = match action {
            "edit" => format!("printf 'edited during the upload' > '{}'", orig.display()),
            _ => format!("rm -f '{}'", orig.display()),
        };
        fx.set_fake(&format!(
            "if [ \"$cmd\" = rcat ]; then case \"$*\" in *manifests/*.jsonl) {change};; esac; fi"
        ));
        let mut o = fx.opts(true);
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        let rep = apply(&scan, &plan, &o).unwrap();
        assert_eq!(rep.pieces[1].outcome, "removed", "{action}: {rep:#?}");
        assert!(!copy.exists(), "{action}");
        fx.set_fake("");
        let r = restore(&copy, &ropts(&fx)).unwrap();
        assert!(r.failed.is_empty(), "{action}: {r:?}");
        assert_eq!(fs::read(&copy).unwrap(), content, "{action}");
    }
}

/// Main's Q2: a secret-named duplicate is treated exactly like the backup lane. Without
/// `--secrets upload` it is kept and none of its bytes are stored; with it, it is uploaded,
/// verified and removed, and restores.
#[test]
fn a_secret_named_duplicate_is_kept_unless_secrets_are_uploaded() {
    for upload in [false, true] {
        let fx = Fx::new();
        let content = b"bytes kept under a secret-like name\n";
        let orig = fx.write("keep/orig.txt", content);
        let dup = fx.write("dups/api_token_notes.txt", content);
        let (scan, plan) = docs(
            &[&fx.data],
            &[
                P::tree(&fx.data.join("keep"), "keep"),
                dup_piece(&fx.data.join("dups"), "api_token_notes.txt", &orig),
            ],
        );
        let mut o = fx.opts(true);
        o.secrets_upload = upload;
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        let rep = apply(&scan, &plan, &o).unwrap();
        if upload {
            assert_eq!(rep.pieces[1].outcome, "removed", "{rep:?}");
            assert!(holds(&fx.all_shard_bytes(), content));
            let r = restore(&dup, &ropts(&fx)).unwrap();
            assert!(r.failed.is_empty(), "{r:?}");
            assert_eq!(fs::read(&dup).unwrap(), content);
        } else {
            assert_eq!(rep.pieces[1].outcome, "kept", "{rep:?}");
            assert!(rep.pieces[1].reason.contains("secret name"), "{rep:?}");
            assert!(dup.exists());
            assert!(!holds(&fx.all_shard_bytes(), content), "never uploaded");
        }
    }
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

#[test]
fn an_intact_readback_only_shard_is_reused_after_reading_it_back() {
    // Decision (Revision 6): a shard whose backend reported no hash was verified by reading it
    // back; reuse reads it back again and compares sha256, rather than uploading it again.
    let fx = Fx::new();
    let content = vec![7u8; 200_000];
    fx.write("a/x.bin", &content);
    fx.write("b/y.bin", &content);
    let a = docs(&[&fx.data], &[P::tree(&fx.data.join("a"), "backup")]);
    let b = docs(&[&fx.data], &[P::tree(&fx.data.join("b"), "backup")]);
    let mut o = fx.opts(true);
    o.hooks.force_readback = true;
    apply(&a.0, &a.1, &o).unwrap();
    let manifest = fs::read_to_string(fx.manifest()).unwrap();
    assert!(manifest.contains("readback-sha256:"), "{manifest}");
    let rep = apply(&b.0, &b.1, &fx.opts(true)).unwrap();
    assert_eq!(rep.shards_uploaded, 0, "{rep:?}");
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
}

/// The read-back on reuse is never trusted when it fails or comes back short: the content is
/// uploaded again (main's ruling on the readback decision), and the second piece still restores.
#[test]
fn a_readback_that_fails_or_is_cut_short_means_upload_again() {
    for (case, fake) in [
        // Only shard reads break: a failed manifest read stops the run (apply_history).
        (
            "fails",
            "if [ \"$cmd\" = cat ]; then case \"$*\" in *manifests/*) ;; *) exit 1;; esac; fi",
        ),
        (
            "short",
            "if [ \"$cmd\" = cat ]; then case \"$*\" in *manifests/*) ;; *) \"$REAL\" \"$@\" | head -c 1000; exit 0;; esac; fi",
        ),
    ] {
        let fx = Fx::new();
        let content = vec![7u8; 200_000];
        fx.write("a/x.bin", &content);
        let y = fx.write("b/y.bin", &content);
        let a = docs(&[&fx.data], &[P::tree(&fx.data.join("a"), "backup")]);
        let b = docs(&[&fx.data], &[P::tree(&fx.data.join("b"), "backup")]);
        let mut o = fx.opts(true);
        o.hooks.force_readback = true;
        apply(&a.0, &a.1, &o).unwrap();
        // The reuse check follows the stored shard row (`readback-sha256:`); the new upload is
        // verified by the listing hash, so only the reuse read-back meets the broken `cat`.
        fx.set_fake(fake);
        let rep = apply(&b.0, &b.1, &fx.opts(true)).unwrap();
        fx.set_fake("");
        assert_eq!(rep.shards_uploaded, 1, "{case}: uploaded again: {rep:?}");
        assert_eq!(rep.pieces[0].outcome, "removed", "{case}: {rep:?}");
        let r = restore(&y, &ropts(&fx)).unwrap();
        assert!(r.failed.is_empty(), "{case}: {r:?}");
        assert_eq!(fs::read(&y).unwrap(), content, "{case}");
    }
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

#[test]
fn failed_noise_with_a_secret_like_name_is_kept_not_uploaded() {
    // The fallback to backup applies the backup lane's (wider) secret rule: a duplicate whose
    // proof fails and whose name is secret-like is kept, never uploaded by default.
    let fx = Fx::new();
    let content = b"FALLBACK-SECRET-CANARY-2b9c\n";
    let orig = fx.write("keep/orig.txt", content);
    let dup = fx.write("dups/my_token.txt", content);
    let want = sha(&orig);
    fs::write(&orig, b"changed after the plan").unwrap();
    let mut piece = P::files(&fx.data.join("dups"), &["my_token.txt"], "noise");
    piece.evidence = Some(serde_json::json!({
        "kind": "duplicate", "sha256": want, "size": content.len(),
        "retained_copy": orig, "retained_piece": 0
    }));
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::tree(&fx.data.join("keep"), "keep"), piece],
    );
    let mut o = fx.opts(true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.pieces[1].outcome, "kept", "{rep:?}");
    assert!(rep.pieces[1].reason.contains("secret name"), "{rep:?}");
    assert_eq!(fs::read(&dup).unwrap(), content);
    assert!(!fx
        .all_shard_bytes()
        .windows(content.len())
        .any(|w| w == content));
}

// ---- Named tests the spec promised (acceptance finding 4) ----

/// Spec "Upload once": when the piece holding the first copy is skipped, the mapped copy in
/// another piece is uploaded from itself after re-hashing, and restores.
#[test]
fn mapped_copy_uploaded_when_primary_skipped() {
    let fx = Fx::new();
    let content = b"MAPPED-COPY-CANARY-4d1e the same bytes in two pieces\n";
    fx.write("a/x.bin", content);
    let y = fx.write("b/y.bin", content);
    let (scan, plan) = docs(
        &[&fx.data],
        &[
            P::tree(&fx.data.join("a"), "backup"),
            P::tree(&fx.data.join("b"), "backup"),
        ],
    );
    // The first piece changes after the scan, so it is skipped.
    std::thread::sleep(Duration::from_millis(20));
    fx.write("a/new-after-scan.txt", b"new");
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "kept", "{rep:?}");
    assert_eq!(rep.pieces[1].outcome, "removed", "{rep:?}");
    assert!(fx.data.join("a/x.bin").exists());
    let shards = fx.all_shard_bytes();
    assert!(shards.windows(content.len()).any(|w| w == content));
    let r = restore(&y, &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read(&y).unwrap(), content);
}

/// NEG-9: an unreadable file inside a piece keeps the whole piece.
#[test]
fn unreadable_file_keeps_piece() {
    let uid = std::process::Command::new("id").arg("-u").output().unwrap();
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        eprintln!("skipped: root reads mode-000 files");
        return;
    }
    let fx = Fx::new();
    let ok = fx.write("p/ok.txt", b"readable");
    let locked = fx.write("p/locked.txt", b"UNREADABLE-CANARY");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&fx.data.join("p"), "backup")]);
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "kept", "{rep:?}");
    assert!(ok.exists() && locked.exists());
    assert_eq!(fs::read(&locked).unwrap(), b"UNREADABLE-CANARY");
}

/// Main's ruling (with stream A's resolved-path contract): a plan piece whose path goes through
/// a symlinked folder is kept, and its reason names the link and what to do. A plan made from a
/// scan of a linked root never ends in a run that cleans nothing without saying why.
#[test]
fn a_piece_whose_path_goes_through_a_link_is_kept_with_a_reason_to_act_on() {
    let fx = Fx::new();
    fx.write("real/p/f.txt", b"behind a link");
    let link = fx.data.join("link");
    std::os::unix::fs::symlink(fx.data.join("real"), &link).unwrap();
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&fx.data.join("real/p"), "backup")]);
    let from = fx.data.join("real").to_string_lossy().to_string();
    let to = link.to_string_lossy().to_string();
    let scan = scan.replace(&from, &to);
    let plan = rebind(&scan, &plan.replace(&from, &to));
    assert!(plan.contains(&format!("{to}/p")), "{plan}");
    for act in [false, true] {
        let rep = apply(&scan, &plan, &fx.opts(act)).unwrap();
        let p = &rep.pieces[0];
        assert_eq!(p.outcome, "kept", "act {act}: {rep:?}");
        assert!(
            p.reason
                .contains(&format!("the path goes through a link at {to}"))
                && p.reason.contains("scan the real folder"),
            "act {act}: {}",
            p.reason
        );
    }
    assert_eq!(
        fs::read(fx.data.join("real/p/f.txt")).unwrap(),
        b"behind a link"
    );
}
