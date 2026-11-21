//! AC-2 and AC-14: nothing is reported as backed up before it is verified.
//!
//! - NEG-1: a stat that drops, empties or corrupts the hash of a pack, of the `.idx` or of the
//!   `.files` object yields no receipt for the paths that depend on it, and exit 3.
//! - NEG-2: a failing `.files` put yields no receipt; a rerun adopts the verified packs (no pack
//!   is uploaded twice) and commits.
//! - AC-14a: a pack writer defect (cargo feature broken-pack-offset) yields zero receipts.
//! - AC-14b / NEG-12: a pack deleted or replaced in the bucket after it was indexed is uploaded
//!   again by the next run, with a warm cache and with an empty one, and the next run restores.
//!
//! Failing first: `--features broken-verify` (verified_put skips the comparison) and
//! `--features broken-commit-order` (receipts after the `.idx` alone) make these tests fail.

mod common;
use common::*;
use std::sync::Arc;
use xfer::transport::fault::{FaultConfig, Faulty, HashFault};
use xfer::transport::Transport;

/// Paths that carry file content in `tree` (the paths a missing pack must keep unreceipted).
fn content_paths(rs: &[xfer::receipt::Receipt]) -> Vec<String> {
    rs.iter()
        .filter(|r| r.blake3.is_some() && r.id.size > 0)
        .map(|r| r.path.clone().unwrap_or_default())
        .collect()
}

fn faulty(env: &Env, cfg: FaultConfig) -> Arc<Faulty> {
    Arc::new(Faulty::new(env.local(), cfg))
}

fn run_with(
    env: &Env,
    t: Arc<dyn Transport>,
) -> (xfer::backup::Report, Vec<xfer::receipt::Receipt>) {
    let src = env.p("src");
    if !src.exists() {
        sample_tree(&src);
    }
    let rcp = env.p("receipts.jsonl");
    let rep = xfer::backup::backup(t, &opts(&src, &rcp)).expect("backup returns a report");
    (rep, receipts(&rcp))
}

#[test]
fn a_bad_pack_hash_gives_no_receipt_for_its_files() {
    for fault in [HashFault::Drop, HashFault::Empty, HashFault::Corrupt] {
        let env = Env::new();
        let t = faulty(
            &env,
            FaultConfig {
                hash_fault: fault,
                hash_prefix: "v1/packs/".into(),
                ..Default::default()
            },
        );
        let (rep, rs) = run_with(&env, t);
        assert_eq!(rep.exit_code, 3, "{fault:?}: {rep:#?}");
        assert!(
            content_paths(&rs).is_empty(),
            "{fault:?}: files whose pack failed verification were receipted: {:?}",
            content_paths(&rs)
        );
    }
}

#[test]
fn a_bad_index_or_file_table_hash_gives_no_receipt_at_all() {
    for prefix in ["v1/index/", "v1/files/"] {
        for fault in [HashFault::Drop, HashFault::Empty, HashFault::Corrupt] {
            let env = Env::new();
            let t = faulty(
                &env,
                FaultConfig {
                    hash_fault: fault,
                    hash_prefix: prefix.into(),
                    ..Default::default()
                },
            );
            let (rep, rs) = run_with(&env, t);
            assert_eq!(rep.exit_code, 3, "{prefix} {fault:?}: {rep:#?}");
            assert!(rs.is_empty(), "{prefix} {fault:?}: {} receipts", rs.len());
        }
    }
}

#[test]
fn a_failed_index_put_gives_no_receipt_and_a_rerun_adopts_the_packs() {
    failed_commit_put("v1/index/", true);
}

#[test]
fn a_failed_file_table_put_gives_no_receipt_and_a_rerun_reuses_the_packs() {
    // the .idx was verified, so the rerun finds the packs through it, not as orphans
    failed_commit_put("v1/files/", false);
}

fn failed_commit_put(prefix: &str, orphans: bool) {
    let env = Env::new();
    let t = faulty(
        &env,
        FaultConfig {
            fail_put_prefix: prefix.into(),
            ..Default::default()
        },
    );
    let (rep, rs) = run_with(&env, t.clone());
    assert_eq!(rep.exit_code, 3, "{rep:#?}");
    assert!(
        rs.is_empty(),
        "{} receipts without a verified .files",
        rs.len()
    );
    let packs_first = t
        .put_keys
        .lock()
        .expect("keys")
        .iter()
        .filter(|k| k.starts_with("v1/packs/"))
        .count();
    assert!(packs_first > 0);

    // rerun on a healthy transport: the verified packs are adopted, not uploaded again
    std::fs::remove_file(env.p("receipts.jsonl")).ok();
    let again = faulty(&env, FaultConfig::default());
    let (rep2, rs2) = run_with(&env, again.clone());
    assert_eq!(rep2.exit_code, 0, "{rep2:#?}");
    if orphans {
        // no .idx was committed, so every pack is an orphan the rerun must adopt
        assert!(rep2.adopted_packs > 0, "{prefix}: {rep2:#?}");
    } else {
        // the committed .idx covers its packs; only a pack sealed after it (a slow runner can
        // have one in flight when the .files put fails) is an orphan to adopt
        assert!(
            (rep2.adopted_packs as usize) < packs_first,
            "{prefix}: all {packs_first} packs adopted although an .idx was committed {rep2:#?}"
        );
    }
    let packs_again = again
        .put_keys
        .lock()
        .expect("keys")
        .iter()
        .filter(|k| k.starts_with("v1/packs/"))
        .count();
    assert_eq!(
        packs_again, 0,
        "the rerun uploaded packs that were already verified"
    );
    assert_eq!(rs2.len(), manifest(&env.p("src")).len());
}

#[test]
fn a_pack_writer_defect_yields_zero_receipts() {
    let env = Env::new();
    let (rep, rs) = run_with(&env, env.local());
    if cfg!(feature = "broken-pack-offset") {
        assert!(content_paths(&rs).is_empty(), "{:?}", content_paths(&rs));
        assert_eq!(rep.exit_code, 3);
        assert!(
            rep.errors.iter().any(|e| e.contains("decode check")),
            "{:?}",
            rep.errors
        );
    } else {
        assert_eq!(rep.exit_code, 0, "{rep:#?}");
        assert!(!content_paths(&rs).is_empty());
    }
}

/// Delete or replace one indexed pack in the store, then back up again.
fn lose_a_pack(replace: bool, warm_cache: bool) {
    let env = Env::new();
    let src = env.p("src");
    sample_tree(&src);
    let rcp = env.p("r1.jsonl");
    let mut o = opts(&src, &rcp);
    o.state = Some(env.p("state"));
    let rep = xfer::backup::backup(env.local(), &o).expect("backup 1");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");

    // the pack holding b/big.bin's bytes
    let store = xfer::transport::local::LocalDir::new(env.p("store"), xfer::hash::HashKind::Sha1)
        .expect("store");
    let packs = store.list("v1/packs").expect("list");
    let big = rand_bytes(1, 6 << 20);
    let victim = packs
        .iter()
        .find(|p| {
            let b = std::fs::read(store.path_of(&p.key)).expect("read pack");
            let t = xfer::pack::parse_pack(&b).expect("parse");
            t.segments.iter().any(|s| {
                xfer::pack::decode_segment(&b, s)
                    .map(|raw| raw.windows(4096).any(|x| x == &big[..4096]))
                    .unwrap_or(false)
            })
        })
        .expect("a pack holds big.bin");
    let path = store.path_of(&victim.key);
    if replace {
        std::fs::write(&path, b"replaced by someone else").expect("replace");
    } else {
        std::fs::remove_file(&path).expect("delete");
    }
    if !warm_cache {
        std::fs::remove_dir_all(env.p("state")).ok();
    }

    let rcp2 = env.p("r2.jsonl");
    let mut o2 = opts(&src, &rcp2);
    o2.state = Some(env.p("state"));
    let t2 = faulty(&env, FaultConfig::default());
    let rep2 = xfer::backup::backup(t2.clone(), &o2).expect("backup 2");
    assert_eq!(
        rep2.exit_code, 0,
        "replace={replace} warm={warm_cache}: {rep2:#?}"
    );
    let reput = t2
        .put_keys
        .lock()
        .expect("keys")
        .iter()
        .filter(|k| k.starts_with("v1/packs/"))
        .count();
    assert!(
        reput > 0,
        "replace={replace} warm={warm_cache}: the lost chunks were not uploaded again"
    );
    let rs2 = receipts(&rcp2);
    assert!(rs2.iter().any(|r| r.path.as_deref() == Some("b/big.bin")));

    // the new run restores exactly from the bucket alone
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, Some(rep2.run.clone())))
        .expect("restore");
    assert_eq!(rr.exit_code, 0, "{rr:#?}");
    assert_eq!(manifest(&src), manifest(&target));
}

#[test]
fn a_deleted_pack_is_uploaded_again_with_a_warm_cache() {
    lose_a_pack(false, true);
}

#[test]
fn a_deleted_pack_is_uploaded_again_with_an_empty_cache() {
    lose_a_pack(false, false);
}

#[test]
fn a_replaced_pack_is_uploaded_again_with_a_warm_cache() {
    lose_a_pack(true, true);
}

#[test]
fn a_replaced_pack_is_uploaded_again_with_an_empty_cache() {
    lose_a_pack(true, false);
}
