//! AC-5a, AC-13 and NEG-6: a backup killed with SIGKILL (a real child process).
//!
//! - AC-13: every path receipted before the crash restores from the bucket alone, after the
//!   deleter removed it from the source and the state dir is gone; rebuilt receipts equal the
//!   streamed ones on content fields.
//! - AC-5a / NEG-6: a rerun completes without uploading any committed pack again, the repository
//!   deep-verifies (no partial object is referenced), and restore matches the source.
#![cfg(unix)]

mod common;
use common::*;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xfer::receipt::{Deletable, Policy, Receipt};
use xfer::transport::fault::{FaultConfig, Faulty};
use xfer::transport::Transport;

const BIN: &str = env!("CARGO_BIN_EXE_prunedisk-xfer");

fn tree(src: &Path) {
    for i in 0..40u64 {
        write(
            &src.join(format!("d{}/f{i:02}.bin", i % 4)),
            &rand_bytes(500 + i, 1 << 20),
        );
    }
    for i in 0..50u64 {
        write(
            &src.join(format!("small/s{i:02}.txt")),
            format!("small file {i}\n").as_bytes(),
        );
    }
}

/// Start a slow backup (each put sleeps) and kill it with SIGKILL once a receipt is on disk.
fn backup_then_kill(env: &Env) -> Vec<Receipt> {
    let rcp = env.p("receipts.jsonl");
    let mut child: Child = Command::new(BIN)
        .args(["backup", "--repo"])
        .arg(format!("local:{}?delay_ms=300", env.p("store").display()))
        .args([
            "--mem-budget",
            "640Mi",
            "--max-pack",
            "16Mi",
            "--max-streams",
            "2",
        ])
        .arg("--source")
        .arg(env.p("src"))
        .arg("--state")
        .arg(env.p("state"))
        .arg("--receipts")
        .arg(&rcp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backup");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let n = std::fs::read_to_string(&rcp)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        if n > 0 {
            break;
        }
        if let Some(st) = child.try_wait().expect("try_wait") {
            panic!("backup exited ({st}) before the test could kill it");
        }
        assert!(Instant::now() < deadline, "no receipt within 120 s");
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: plain kill(2) of our own child.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let st = child.wait().expect("wait");
    assert!(
        st.code().is_none(),
        "the child was killed, not exited: {st}"
    );
    receipts(&rcp)
}

fn snapshot_exists(env: &Env, run: &str) -> bool {
    env.p("store")
        .join(format!("v1/snapshots/{run}.json"))
        .exists()
}

fn policy() -> Policy {
    Policy {
        allow_unknown_build: true, // test builds carry no commit
        ..Policy::default()
    }
}

#[test]
fn receipted_paths_restore_after_a_crash_and_their_deletion() {
    let env = Env::new();
    let src = env.p("src");
    tree(&src);
    let before = manifest(&src);
    let rs = backup_then_kill(&env);
    assert!(!rs.is_empty());
    let run = rs[0].run.clone();
    assert!(
        !snapshot_exists(&env, &run),
        "the crash came too late: the snapshot was written"
    );

    // the deleter: each receipted regular file is checked, then removed from the source
    let mut deleted = Vec::new();
    for r in &rs {
        if r.kind != xfer::meta::Kind::File || r.link_group.is_some() {
            continue;
        }
        assert_eq!(
            r.deletable(&src, &policy()).expect("deletable"),
            Deletable::Yes
        );
        let rel = r.rel().expect("rel");
        std::fs::remove_file(src.join(String::from_utf8(rel.clone()).expect("utf8")))
            .expect("delete");
        deleted.push((rel, r.clone()));
    }
    assert!(!deleted.is_empty());
    std::fs::remove_dir_all(env.p("state")).expect("drop the state dir");

    // restore that run from the bucket alone
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, Some(run.clone())))
        .expect("restore");
    assert_eq!(rr.exit_code, 0, "{rr:#?}");
    let after = manifest(&target);
    for (rel, r) in &deleted {
        let m = after
            .get(rel)
            .unwrap_or_else(|| panic!("{rel:?} was not restored"));
        assert_eq!(Some(&m.content), r.blake3.as_ref(), "{rel:?} content");
        assert_eq!(m.mode, r.mode & 0o7777, "{rel:?} mode");
        assert_eq!(m.mtime_ns, r.id.mtime_ns, "{rel:?} mtime");
        assert_eq!(
            before.get(rel),
            Some(m),
            "{rel:?} differs from the original"
        );
    }

    // receipts rebuilt from the bucket equal the streamed ones on content fields
    let rebuilt = xfer::restore::rebuild_receipts(
        env.local(),
        &rs[0].repo,
        &run,
        &budget(),
        xfer::repo::DEFAULT_MAX_OBJECTS,
    )
    .expect("rebuild");
    let key = |r: &Receipt| {
        (
            r.path.clone(),
            r.path_hex.clone(),
            format!("{:?}", r.kind),
            r.blake3.clone(),
            r.mode,
            r.id.mtime_ns,
            r.id.size,
            r.link_group.clone(),
            r.idx.clone(),
            r.files.clone(),
        )
    };
    let mut a: Vec<_> = rs.iter().map(key).collect();
    let mut b: Vec<_> = rebuilt.iter().map(key).collect();
    a.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    b.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    assert_eq!(a, b);
}

#[test]
fn a_rerun_after_a_crash_reuploads_no_committed_pack() {
    let env = Env::new();
    let src = env.p("src");
    tree(&src);
    let rs = backup_then_kill(&env);
    assert!(!rs.is_empty());
    let store = xfer::transport::local::LocalDir::new(env.p("store"), xfer::hash::HashKind::Sha1)
        .expect("store");
    let packs_before: std::collections::HashSet<String> = store
        .list("v1/packs")
        .expect("list")
        .into_iter()
        .map(|o| o.key)
        .filter(|k| k.ends_with(".pack"))
        .collect();
    assert!(!packs_before.is_empty());

    let t = Arc::new(Faulty::new(env.local(), FaultConfig::default()));
    let mut o = opts(&src, &env.p("rerun.jsonl"));
    o.state = Some(env.p("state")); // the crashed run's state is not trusted, only checked
    let rep = xfer::backup::backup(t.clone(), &o).expect("rerun");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    let again: Vec<String> = t
        .put_keys
        .lock()
        .expect("keys")
        .iter()
        .filter(|k| packs_before.contains(*k))
        .cloned()
        .collect();
    assert!(
        again.is_empty(),
        "the rerun uploaded packs that were already stored: {again:?}"
    );

    let v = xfer::restore::verify(
        env.local(),
        Some(&rep.run),
        true,
        &budget(),
        xfer::repo::DEFAULT_MAX_OBJECTS,
    )
    .expect("verify");
    assert_eq!(v.exit_code, 0, "{v:#?}");
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, Some(rep.run.clone())))
        .expect("restore");
    assert_eq!(rr.exit_code, 0, "{rr:#?}");
    assert_eq!(manifest(&src), manifest(&target));
}
