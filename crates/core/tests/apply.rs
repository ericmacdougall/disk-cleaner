//! Apply: backup lane, dedup, shards, verification, removal, manifest, resume and restore.
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
use disk_cleaner_core::apply::{apply, ApplyError, CrashPoint};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

fn ropts(fx: &Fx) -> RestoreOptions {
    RestoreOptions {
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

/// Two backup pieces: `a` holds x (shared content), `b` holds y (same content as x), a hard
/// link to y, and z (distinct).
fn dedup_fixture(fx: &Fx) -> (String, String) {
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

#[test]
fn dry_run_changes_nothing() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    let before = tree_digest(&fx.data);
    let rep = apply(&scan, &plan, &fx.opts(false)).unwrap();
    assert!(rep.dry_run);
    assert_eq!(tree_digest(&fx.data), before);
    assert!(!fx.manifest().exists(), "a dry run writes no manifest");
    assert_eq!(fx.log(), "", "a dry run never runs rclone");
    assert_eq!(rep.pieces.len(), 2);
    for p in &rep.pieces {
        assert!(p.outcome.starts_with("would"), "{p:?}");
        assert!(!p.reason.is_empty());
    }
    // Distinct content: 300000 shared + "alpha" + "zeta distinct".
    assert_eq!(rep.uploaded_bytes, 300_000 + 5 + 13);
}

#[test]
fn dedup_uploads_once_and_restore_reproduces() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    let ymeta = fs::metadata(fx.data.join("b/y.bin")).unwrap();
    let ysha = sha(&fx.data.join("b/y.bin"));
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert!(rep.shards_failed.is_empty(), "{rep:?}");
    assert_eq!(
        fx.shard_total(),
        300_000 + 5 + 13,
        "each distinct blob stored once"
    );
    assert!(
        !fx.data.join("a").exists() && !fx.data.join("b").exists(),
        "{rep:#?}"
    );
    // Restore the duplicate (and its hard-link sibling) from storage only.
    let log_before = fx.log().lines().count();
    let r = restore(&fx.data.join("b"), &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty() && r.conflicts.is_empty(), "{r:?}");
    assert_eq!(sha(&fx.data.join("b/y.bin")), ysha);
    assert_eq!(sha(&fx.data.join("b/y-link.bin")), ysha);
    let m = fs::metadata(fx.data.join("b/y.bin")).unwrap();
    assert_eq!(
        m.permissions().mode() & 0o7777,
        ymeta.permissions().mode() & 0o7777
    );
    use std::os::unix::fs::MetadataExt;
    assert_eq!(m.mtime(), ymeta.mtime());
    let new_calls: Vec<String> = fx
        .log()
        .lines()
        .skip(log_before)
        .map(|s| s.to_string())
        .collect();
    assert!(
        new_calls.iter().all(|l| l.starts_with("cat ")),
        "restore used only rclone cat: {new_calls:?}"
    );
    // Restoring again is a no-op (identical), never an overwrite.
    let r2 = restore(&fx.data.join("b"), &ropts(&fx)).unwrap();
    assert_eq!(r2.restored.len(), 0);
    assert_eq!(r2.identical.len(), 3);

    // A new piece whose content is already archived uploads nothing for it.
    let fx2_dir = fx.data.join("c");
    fs::create_dir_all(&fx2_dir).unwrap();
    fs::write(fx2_dir.join("again.bin"), vec![7u8; 300_000]).unwrap();
    let (scan2, plan2) = docs(&[&fx.data], &[P::tree(&fx2_dir, "backup")]);
    let total_before = fx.shard_total();
    let rep2 = apply(&scan2, &plan2, &fx.opts(true)).unwrap();
    assert_eq!(rep2.uploaded_bytes, 0, "{rep2:?}");
    assert_eq!(fx.shard_total(), total_before);
    assert!(!fx2_dir.exists());
}

#[test]
fn failed_shard_removes_nothing() {
    let fx = Fx::new();
    // Three pieces, shards of 1 MiB: each piece's single 700 KB file lands in its own shard
    // region; the first rcat (the first shard) fails.
    let mut pieces = Vec::new();
    for (k, fill) in [(0u8, 1u8), (1, 2), (2, 3)] {
        let d = fx.data.join(format!("p{k}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("f.bin"), vec![fill; 700_000]).unwrap();
        pieces.push(P::tree(&d, "backup"));
    }
    let (scan, plan) = docs(&[&fx.data], &pieces);
    fx.set_fake("if [ \"$cmd\" = rcat ] && [ \"$n\" = 1 ]; then cat > /dev/null; echo injected >&2; exit 1; fi");
    let mut o = fx.opts(true);
    o.jobs = 1;
    let before = tree_digest(&fx.data);
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.shards_failed.len(), 1, "{rep:#?}");
    let failed = &rep.shards_failed[0];
    let manifest = fs::read_to_string(fx.manifest()).unwrap();
    assert!(
        !manifest.contains(failed.as_str()),
        "no row names the failed shard"
    );
    // Every piece that had any byte in the failed shard is intact; the others proceeded.
    let kept: Vec<_> = rep.pieces.iter().filter(|p| p.outcome == "kept").collect();
    assert!(!kept.is_empty());
    for p in &kept {
        let rel = p
            .path
            .strip_prefix(&fx.data)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let mine: Vec<_> = before
            .iter()
            .filter(|(r, _)| r.starts_with(&rel))
            .cloned()
            .collect();
        let now: Vec<_> = tree_digest(&fx.data)
            .into_iter()
            .filter(|(r, _)| r.starts_with(&rel))
            .collect();
        assert_eq!(mine, now, "piece {rel} must be byte-identical");
    }
    assert!(
        rep.pieces.iter().any(|p| p.outcome == "removed"),
        "independent pieces proceed: {rep:#?}"
    );
}

#[test]
fn lying_hash_removes_nothing() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    fx.set_fake("if [ \"$cmd\" = lsjson ]; then \"$REAL\" \"$@\" | sed -E 's/\"(sha1|md5|sha256|SHA-1|MD5)\":\"[0-9a-f]+\"/\"\\1\":\"00\"/g'; exit ${PIPESTATUS[0]}; fi");
    let before = tree_digest(&fx.data);
    let rep = apply(&scan, &plan, &fx.opts(true));
    assert_eq!(tree_digest(&fx.data), before, "nothing removed: {rep:?}");
    if let Ok(r) = rep {
        assert!(!r.shards_failed.is_empty());
        let manifest = fs::read_to_string(fx.manifest()).unwrap_or_default();
        assert!(!manifest.contains("\"t\":\"blob\""));
        assert!(!manifest.contains("\"t\":\"file\""));
    }
}

#[test]
fn readback_corrupt_removes_nothing() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    // The listing is ignored (forced read-back) and the read-back is corrupted.
    fx.set_fake("if [ \"$cmd\" = cat ]; then \"$REAL\" \"$@\" | tr 'a-z' 'b-za'; exit 0; fi");
    let mut o = fx.opts(true);
    o.hooks.force_readback = true;
    let before = tree_digest(&fx.data);
    let rep = apply(&scan, &plan, &o).unwrap();
    assert!(!rep.shards_failed.is_empty());
    assert_eq!(tree_digest(&fx.data), before);
}

#[test]
fn readback_ok_when_no_hash() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    let mut o = fx.opts(true);
    o.hooks.force_readback = true;
    let rep = apply(&scan, &plan, &o).unwrap();
    assert!(rep.shards_failed.is_empty(), "{rep:?}");
    let manifest = fs::read_to_string(fx.manifest()).unwrap();
    assert!(manifest.contains("readback-sha256:"));
}

#[test]
fn touched_after_scan_is_skipped() {
    for case in ["mtime", "chmod", "added", "grown"] {
        let fx = Fx::new();
        let d = fx.data.join("p");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("f.txt"), b"hello").unwrap();
        let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
        std::thread::sleep(Duration::from_millis(1100));
        match case {
            "mtime" => fs::write(d.join("f.txt"), b"hellp").unwrap(),
            "chmod" => {
                fs::set_permissions(d.join("f.txt"), fs::Permissions::from_mode(0o600)).unwrap()
            }
            "added" => fs::write(d.join("new.txt"), b"n").unwrap(),
            _ => fs::write(d.join("f.txt"), b"hello world").unwrap(),
        }
        let before = tree_digest(&fx.data);
        let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
        assert_eq!(rep.pieces[0].outcome, "kept", "{case}: {rep:?}");
        assert_eq!(tree_digest(&fx.data), before, "{case}");
        assert!(
            !fs::read_to_string(fx.manifest())
                .unwrap_or_default()
                .contains("\"t\":\"file\""),
            "{case}"
        );
    }
}

#[test]
fn secret_is_never_uploaded_without_opt_in() {
    let fx = Fx::new();
    let canary = b"CANARY-7f3a9c-SECRET-VALUE";
    let d = fx.data.join("proj");
    fx.write("proj/readme.txt", b"public text");
    for n in [
        ".env",
        ".env.local",
        "id_ed25519",
        "x.pem",
        "credentials.json",
        ".npmrc",
    ] {
        fx.write(&format!("proj/{n}"), canary);
    }
    // The plan claims nothing is secret (secret:false by omission).
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
    let before = tree_digest(&fx.data);
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "kept", "{rep:?}");
    assert_eq!(tree_digest(&fx.data), before);
    let shards = fx.all_shard_bytes();
    assert!(
        !shards.windows(canary.len()).any(|w| w == canary),
        "canary uploaded"
    );
    // Positive control: with the opt-in the canary is uploaded and the piece removed.
    let mut o = fx.opts(true);
    o.secrets_upload = true;
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
    let shards = fx.all_shard_bytes();
    assert!(
        shards.windows(canary.len()).any(|w| w == canary),
        "control: uploaded with opt-in"
    );
}

#[test]
fn symlink_is_stored_as_link_never_followed() {
    let fx = Fx::new();
    let canary = b"CANARY-outside-target-9d2e";
    let outside = fx.tmp.path().canonicalize().unwrap().join("outside.txt");
    fs::write(&outside, canary).unwrap();
    let d = fx.data.join("p");
    fx.write("p/a.txt", b"a");
    std::os::unix::fs::symlink(&outside, d.join("notes")).unwrap();
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
    assert!(outside.exists(), "the link target is never touched");
    let shards = fx.all_shard_bytes();
    assert!(!shards.windows(canary.len()).any(|w| w == canary));
    let r = restore(&d, &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read_link(d.join("notes")).unwrap(), outside);
}

#[test]
fn large_file_segments_restore() {
    let fx = Fx::new();
    let d = fx.data.join("big");
    fs::create_dir_all(&d).unwrap();
    // 200 MiB of non-repeating data, 64 MiB shards: 4 segments.
    let mut data = vec![0u8; 200 << 20];
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for chunk in data.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    fs::write(d.join("model.bin"), &data).unwrap();
    let want = sha(&d.join("model.bin"));
    drop(data);
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
    let mut o = fx.opts(true);
    o.shard_bytes = 64 << 20;
    o.jobs = 4;
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(rep.shards_uploaded, 4, "{rep:?}");
    assert!(!d.exists());
    let r = restore(&d, &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(sha(&d.join("model.bin")), want);
}

#[test]
fn pool_runs_jobs_concurrently() {
    let fx = Fx::new();
    let d = fx.data.join("p");
    for k in 0..8u8 {
        fx.write(&format!("p/f{k}.bin"), &vec![k; 900_000]);
    }
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
    fx.set_fake("if [ \"$cmd\" = rcat ]; then sleep 0.4; fi");
    let mut o = fx.opts(true);
    o.jobs = 3;
    let rep = apply(&scan, &plan, &o).unwrap();
    assert!(rep.shards_failed.is_empty());
    let iv: Vec<(f64, f64)> = fs::read_to_string(fx.fake_dir.join("intervals"))
        .unwrap()
        .lines()
        .map(|l| {
            let mut it = l.split_whitespace().map(|x| x.parse::<f64>().unwrap());
            (it.next().unwrap(), it.next().unwrap())
        })
        .collect();
    // Peak overlap: more than one, never more than N.
    let mut peak = 0;
    for (s, _) in &iv {
        let live = iv.iter().filter(|(a, b)| a <= s && s < b).count();
        peak = peak.max(live);
    }
    assert!(peak >= 2, "uploads overlapped: {iv:?}");
    assert!(peak <= 3, "never more than N at once: {iv:?}");
}

#[test]
fn resume_after_crash_at_every_boundary() {
    for at in [
        CrashPoint::AfterBlobRows,
        CrashPoint::AfterFileRows,
        CrashPoint::AfterRemoveBegin,
        CrashPoint::AfterMirror,
        CrashPoint::MidUnlink,
    ] {
        let fx = Fx::new();
        let (scan, plan) = dedup_fixture(&fx);
        let digest_a = tree_digest(&fx.data.join("a"));
        let digest_b = tree_digest(&fx.data.join("b"));
        let mut o = fx.opts(true);
        o.hooks.crash_at = Some(at);
        match apply(&scan, &plan, &o) {
            Err(ApplyError::InjectedCrash(_)) => {}
            other => panic!("{at:?}: expected the injected crash, got {other:?}"),
        }
        let shards_after_crash = fx.shard_total();
        let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
        assert!(rep.shards_failed.is_empty(), "{at:?}");
        assert_eq!(
            fx.shard_total(),
            shards_after_crash,
            "{at:?}: verified shards are not uploaded again"
        );
        assert!(
            !fx.data.join("a").exists() && !fx.data.join("b").exists(),
            "{at:?}: {rep:#?}"
        );
        // Everything restores from the remote manifest alone.
        fs::remove_file(fx.manifest()).unwrap();
        let mut ro = ropts(&fx);
        ro.from_remote = true;
        let r = restore(&fx.data, &ro).unwrap();
        assert!(
            r.failed.is_empty() && r.conflicts.is_empty(),
            "{at:?}: {r:?}"
        );
        assert_eq!(tree_digest(&fx.data.join("a")), digest_a, "{at:?}");
        assert_eq!(tree_digest(&fx.data.join("b")), digest_b, "{at:?}");
    }
}

#[test]
fn manifest_failure_never_loses_an_entry() {
    // NEG-12: a failing append stops the run at once. Nothing is unlinked unless its `file` row
    // and its piece's `remove-begin` row were durable first, and a clean re-run converges.
    let mut saw_partial = false;
    for after in 0usize..8 {
        let fx = Fx::new();
        let (scan, plan) = dedup_fixture(&fx);
        let before = tree_digest(&fx.data);
        let mut o = fx.opts(true);
        o.hooks.manifest_fail_after = Some(after);
        let r = apply(&scan, &plan, &o);
        let failed = matches!(r, Err(ApplyError::Manifest(_)));
        assert!(failed || r.is_ok(), "after {after}: {r:?}");
        let rows: Vec<serde_json::Value> = fs::read_to_string(fx.manifest())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let has = |t: &str, p: &Path| {
            rows.iter()
                .any(|v| v["t"] == t && v["path"] == p.to_string_lossy().as_ref())
        };
        let now: Vec<String> = tree_digest(&fx.data).into_iter().map(|(p, _)| p).collect();
        for (rel, kind) in &before {
            if kind == "d" || now.contains(rel) {
                continue;
            }
            if failed {
                saw_partial = true;
            }
            let abs = fx.data.join(rel);
            assert!(
                has("file", &abs),
                "after {after}: {rel} removed without a durable file row"
            );
            assert!(
                has("remove-begin", abs.parent().unwrap()),
                "after {after}: {rel} removed before remove-begin"
            );
        }
        if failed {
            let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
            assert!(rep.shards_failed.is_empty(), "after {after}");
        }
        assert!(
            !fx.data.join("a").exists() && !fx.data.join("b").exists(),
            "after {after}: converges"
        );
        let r = restore(&fx.data, &ropts(&fx)).unwrap();
        assert!(
            r.failed.is_empty() && r.conflicts.is_empty(),
            "after {after}: {r:?}"
        );
        assert_eq!(
            tree_digest(&fx.data),
            before,
            "after {after}: restore reproduces"
        );
    }
    assert!(
        saw_partial,
        "some failure point lands between unlinks (the test is not vacuous)"
    );
}

#[test]
fn torn_last_line_ignored_corrupt_middle_refused() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    fs::create_dir_all(fx.state.clone()).unwrap();
    fs::write(fx.manifest(), "{\"v\":1,\"run\":\"x\",\"at\":1,\"t\":\"kept\",\"plan_piece\":9,\"path\":\"/x\",\"reason\":\"r\"}\n{\"v\":1,\"ru").unwrap();
    assert!(
        apply(&scan, &plan, &fx.opts(false)).is_ok(),
        "a torn last line is ignored"
    );
    fs::write(fx.manifest(), "garbage\n{\"v\":1,\"run\":\"x\",\"at\":1,\"t\":\"kept\",\"plan_piece\":9,\"path\":\"/x\",\"reason\":\"r\"}\n").unwrap();
    assert!(matches!(
        apply(&scan, &plan, &fx.opts(false)),
        Err(ApplyError::Manifest(_))
    ));
}

#[test]
fn hostile_plans_are_refused() {
    let fx = Fx::new();
    let d = fx.data.join("p");
    fx.write("p/f.txt", b"x");
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
    let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut v: serde_json::Value = serde_json::from_str(&plan).unwrap();
        f(&mut v);
        v.to_string()
    };
    let cases: Vec<(&str, String)> = vec![
        ("schema", mutate(&|v| v["schema_version"] = 15.into())),
        (
            "relative",
            mutate(&|v| v["lanes"]["pieces"][0]["path"] = "rel/p".into()),
        ),
        (
            "dotdot",
            mutate(&|v| {
                v["lanes"]["pieces"][0]["path"] = format!("{}/../p", fx.data.display()).into()
            }),
        ),
        (
            "outside roots",
            mutate(&|v| v["lanes"]["pieces"][0]["path"] = "/etc".into()),
        ),
        (
            "unknown lane",
            mutate(&|v| v["lanes"]["pieces"][0]["lane"] = "yolo".into()),
        ),
        (
            "scan mismatch",
            mutate(&|v| v["scan_generated_at"] = 1.into()),
        ),
        (
            "member escape",
            mutate(&|v| v["lanes"]["pieces"][0]["excludes"] = serde_json::json!(["../x"])),
        ),
    ];
    for (name, bad) in cases {
        let r = apply(&scan, &bad, &fx.opts(true));
        assert!(matches!(r, Err(ApplyError::Plan(_))), "{name}: {r:?}");
        assert!(d.join("f.txt").exists(), "{name}");
    }
    // Overlapping pieces.
    let (scan2, plan2) = docs(
        &[&fx.data],
        &[P::tree(&fx.data, "backup"), P::tree(&d, "backup")],
    );
    assert!(matches!(
        apply(&scan2, &plan2, &fx.opts(true)),
        Err(ApplyError::Plan(_))
    ));
    // The remote inside a removing piece.
    let mut o = fx.opts(true);
    o.remote_local = Some(d.join("store"));
    assert!(matches!(apply(&scan, &plan, &o), Err(ApplyError::Plan(_))));
    // The home directory itself.
    let mut o = fx.opts(true);
    o.home = Some(d.clone());
    assert!(matches!(apply(&scan, &plan, &o), Err(ApplyError::Plan(_))));
}

#[test]
fn symlinked_ancestor_keeps_piece() {
    let fx = Fx::new();
    let real = fx.data.join("real");
    fx.write("real/p/f.txt", b"x");
    let link = fx.data.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&real.join("p"), "backup")]);
    let v: serde_json::Value = serde_json::from_str(&plan).unwrap();
    let s: serde_json::Value = serde_json::from_str(&scan).unwrap();
    let swap = |mut v: serde_json::Value, ptr: &str| {
        *v.pointer_mut(ptr).unwrap() = serde_json::json!(link.join("p"));
        v.to_string()
    };
    let plan = swap(v, "/lanes/pieces/0/path");
    let scan = swap(s, "/pieces/pieces/0/path");
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "kept", "{rep:?}");
    assert!(real.join("p/f.txt").exists());
}

#[test]
fn fifo_keeps_piece() {
    let fx = Fx::new();
    let d = fx.data.join("p");
    fx.write("p/f.txt", b"x");
    let st = std::process::Command::new("mkfifo")
        .arg(d.join("pipe"))
        .status()
        .unwrap();
    assert!(st.success());
    let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "kept", "{rep:?}");
    assert!(d.join("f.txt").exists());
}

#[test]
fn excludes_stay_in_place() {
    let fx = Fx::new();
    let d = fx.data.join("p");
    fx.write("p/keep/inner.txt", b"other piece");
    fx.write("p/go.txt", b"go");
    let mut piece = P::tree(&d, "backup");
    piece.excludes = vec!["keep".into()];
    let mut other = P::tree(&d.join("keep"), "keep");
    other.reason = "kept by test".into();
    let (scan, plan) = docs(&[&fx.data], &[piece, other]);
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "removed", "{rep:?}");
    assert!(!d.join("go.txt").exists());
    assert_eq!(fs::read(d.join("keep/inner.txt")).unwrap(), b"other piece");
}

#[test]
fn second_apply_refused() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    fx.set_fake("if [ \"$cmd\" = rcat ]; then sleep 1; fi");
    let o = fx.opts(true);
    std::thread::scope(|s| {
        let h = s.spawn(|| apply(&scan, &plan, &o));
        std::thread::sleep(Duration::from_millis(300));
        let r2 = apply(&scan, &plan, &fx.opts(true));
        assert!(matches!(r2, Err(ApplyError::Locked(_))), "{r2:?}");
        h.join().unwrap().unwrap();
    });
    // The lock is released on exit: a later run proceeds.
    assert!(apply(&scan, &plan, &fx.opts(false)).is_ok());
}

#[test]
fn hung_rclone_is_killed_and_nothing_removed() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    fx.set_fake("if [ \"$cmd\" = rcat ]; then sleep 30; fi");
    let mut o = fx.opts(true);
    o.shard_timeout = Duration::from_secs(2);
    let before = tree_digest(&fx.data);
    let t = std::time::Instant::now();
    let rep = apply(&scan, &plan, &o).unwrap();
    assert!(t.elapsed() < Duration::from_secs(20));
    assert!(!rep.shards_failed.is_empty());
    assert_eq!(tree_digest(&fx.data), before);
}

#[test]
fn restore_never_overwrites() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    apply(&scan, &plan, &fx.opts(true)).unwrap();
    // A different file now sits at the target: refused, left as is.
    fx.write("a/only-a.txt", b"someone else's file");
    let r = restore(&fx.data.join("a"), &ropts(&fx)).unwrap();
    assert_eq!(r.conflicts, vec![fx.data.join("a/only-a.txt")]);
    assert_eq!(
        fs::read(fx.data.join("a/only-a.txt")).unwrap(),
        b"someone else's file"
    );
    // No temp file left behind.
    for e in fs::read_dir(fx.data.join("a")).unwrap() {
        let n = e.unwrap().file_name().to_string_lossy().to_string();
        assert!(!n.contains("pd-restore"), "{n}");
    }
}

#[test]
fn restore_refuses_symlinked_ancestor() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    apply(&scan, &plan, &fx.opts(true)).unwrap();
    let elsewhere = fx.tmp.path().canonicalize().unwrap().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, fx.data.join("a")).unwrap();
    let r = restore(&fx.data.join("a"), &ropts(&fx)).unwrap();
    assert!(!r.failed.is_empty(), "{r:?}");
    assert!(
        fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing written through the link"
    );
}

#[test]
fn restore_corrupt_shard_leaves_no_partial_file() {
    let fx = Fx::new();
    let (scan, plan) = dedup_fixture(&fx);
    apply(&scan, &plan, &fx.opts(true)).unwrap();
    fx.set_fake("if [ \"$cmd\" = cat ]; then \"$REAL\" \"$@\" | tr 'a-z' 'b-za'; exit 0; fi");
    let r = restore(&fx.data.join("a"), &ropts(&fx)).unwrap();
    assert!(
        r.failed.iter().any(|(p, _)| p.ends_with("only-a.txt")),
        "{r:?}"
    );
    assert!(!fx.data.join("a/only-a.txt").exists());
    for e in fs::read_dir(fx.data.join("a")).unwrap() {
        let n = e.unwrap().file_name().to_string_lossy().to_string();
        assert!(!n.contains("pd-restore"), "{n}");
    }
}

#[test]
fn derived_siblings_and_residual_converge() {
    // One scan piece (dir `s`) split by the plan into two derived backup files and the residual
    // tree; removing one derived file must not make the others drift (revision 3, F2).
    let fx = Fx::new();
    let s = fx.data.join("s");
    fx.write("s/d1.bin", b"one");
    fx.write("s/d2.bin", b"two");
    fx.write("s/rest/r.txt", b"rest");
    fx.write("s/rest/r2.txt", b"rest two");
    let mut residual = P::tree(&s, "backup");
    residual.excludes = vec!["d1.bin".into(), "d2.bin".into()];
    let mut d1 = P::files(&s, &["d1.bin"], "backup");
    d1.scan_piece = Some(0);
    let mut d2 = P::files(&s, &["d2.bin"], "backup");
    d2.scan_piece = Some(0);
    // The scan piece is the whole directory (no excludes): build it from an unsplit spec.
    let (scan, _) = docs(&[&fx.data], &[P::tree(&s, "backup")]);
    let (_, plan) = docs(&[&fx.data], &[residual.clone(), d1.clone(), d2.clone()]);
    let gen = serde_json::from_str::<serde_json::Value>(&scan).unwrap()["generated_at"].clone();
    let mut pv: serde_json::Value = serde_json::from_str(&plan).unwrap();
    pv["scan_generated_at"] = gen;
    let plan = pv.to_string();
    let before = tree_digest(&s);
    // Crash in the middle of the residual's removal, then converge.
    let mut o = fx.opts(true);
    o.hooks.crash_at = Some(CrashPoint::MidUnlink);
    let r = apply(&scan, &plan, &o);
    assert!(matches!(r, Err(ApplyError::InjectedCrash(_))), "{r:?}");
    // Files pieces go first; the crash lands halfway through the residual tree.
    assert!(!s.join("d1.bin").exists() && !s.join("d2.bin").exists());
    assert_eq!(
        ["r.txt", "r2.txt"]
            .iter()
            .filter(|n| s.join("rest").join(n).exists())
            .count(),
        1
    );
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert!(
        rep.pieces.iter().all(|p| p.outcome == "removed"),
        "{rep:#?}"
    );
    assert!(!s.exists());
    // A re-run of the finished plan changes nothing and reports the pieces as removed.
    let again = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert!(
        again.pieces.iter().all(|p| p.outcome == "removed"),
        "{again:#?}"
    );
    let r = restore(&s, &ropts(&fx)).unwrap();
    assert!(r.failed.is_empty() && r.conflicts.is_empty(), "{r:?}");
    assert_eq!(tree_digest(&s), before);
}

#[test]
fn group_turned_active_is_kept() {
    let fx = Fx::new();
    let d = fx.data.join("proj/p");
    fx.write("proj/p/f.txt", b"x");
    fx.write("proj/other.txt", b"y");
    let mut piece = P::tree(&d, "backup");
    piece.reason = "test".into();
    let (scan, plan) = docs(&[&fx.data], &[piece]);
    let mut v: serde_json::Value = serde_json::from_str(&plan).unwrap();
    v["lanes"]["pieces"][0]["group"] = serde_json::json!(fx.data.join("proj"));
    let plan = v.to_string();
    std::thread::sleep(Duration::from_millis(1100));
    fx.write("proj/other.txt", b"touched after the scan");
    let rep = apply(&scan, &plan, &fx.opts(true)).unwrap();
    assert_eq!(rep.pieces[0].outcome, "kept", "{rep:?}");
    assert!(d.join("f.txt").exists());
}

#[test]
fn unlink_guard_keeps_changed_and_swapped() {
    use disk_cleaner_core::apply::fsops;
    let fx = Fx::new();
    let p = fx.write("g/sub/f.txt", b"abc");
    let root = fsops::open_dir_abs(&fx.data.join("g")).unwrap();
    let id = fsops::lstat_rel(&root, "sub/f.txt").unwrap();
    // Changed after the baseline (ctime moves): kept.
    std::thread::sleep(Duration::from_millis(20));
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        fsops::unlink_guarded(&root, "sub/f.txt", &id, false).unwrap(),
        fsops::Unlinked::Changed(_)
    ));
    assert!(p.exists());
    // An intermediate directory swapped for a symlink to a sentinel directory: not followed.
    let sentinel = fx.tmp.path().canonicalize().unwrap().join("sentinel");
    fs::create_dir_all(&sentinel).unwrap();
    fs::write(sentinel.join("f.txt"), b"sentinel").unwrap();
    let id2 = fsops::lstat_rel(&root, "sub/f.txt").unwrap();
    fs::rename(fx.data.join("g/sub"), fx.data.join("g/sub.moved")).unwrap();
    std::os::unix::fs::symlink(&sentinel, fx.data.join("g/sub")).unwrap();
    let r = fsops::unlink_guarded(&root, "sub/f.txt", &id2, false);
    assert!(
        r.is_err() || matches!(r, Ok(fsops::Unlinked::Changed(_))),
        "{r:?}"
    );
    assert_eq!(fs::read(sentinel.join("f.txt")).unwrap(), b"sentinel");
    let _ = Path::new("/");
}
