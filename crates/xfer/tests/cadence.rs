//! Freed space first (AC-7's t50 cell, in miniature): on a slow link, receipts arrive while the
//! run is still uploading, not only at its end. The time and byte commit triggers are set out of
//! reach, so only the freed-space trigger (files worth 5% of the freeable bytes are ready) can
//! commit early.

mod common;
use common::*;
use std::sync::Arc;
use std::time::Duration;
use xfer::transport::fault::{FaultConfig, Faulty};

#[test]
fn receipts_arrive_while_packs_are_still_uploading() {
    let env = Env::new();
    let src = env.p("src");
    for i in 0..64u64 {
        write(
            &src.join(format!("f{i:02}.bin")),
            &rand_bytes(100 + i, 1 << 20),
        );
    }
    let slow = Faulty::new(
        env.local(),
        FaultConfig {
            put_delay: Some(Duration::from_millis(1500)),
            slow_prefix: "v1/packs/".into(),
            ..Default::default()
        },
    );
    let mut o = opts(&src, &env.p("receipts.jsonl"));
    o.streams = Some(1); // one pack at a time, so packs complete one after another
    o.commit_every = Duration::from_secs(600);
    o.commit_bytes = 1 << 40;
    let rep = xfer::backup::backup(Arc::new(slow), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert!(rep.packs >= 4, "expected several packs, got {}", rep.packs);
    assert!(
        rep.commits >= 3,
        "receipts were committed only {} time(s): nothing was freeable before the end",
        rep.commits
    );
    // Structural, not a wall-time ratio (which a slow CI runner moves): half the freeable bytes
    // are receipted before the final commit.
    let freed_before_last = rep
        .commit_trace
        .iter()
        .rev()
        .nth(1)
        .map_or(0, |&(_, freed, _)| freed);
    assert!(
        freed_before_last * 2 >= rep.freeable_bytes,
        "only {freed_before_last} of {} freeable bytes were receipted before the final commit",
        rep.freeable_bytes
    );
}

/// Large files read by parallel readers must not be interleaved across every pack: then each
/// file completes only with the last pack, and nothing is freeable until the end of the run.
#[test]
fn large_files_complete_one_after_another() {
    let env = Env::new();
    let src = env.p("src");
    for i in 0..8u64 {
        write(
            &src.join(format!("big{i}.bin")),
            &rand_bytes(200 + i, 8 << 20),
        );
    }
    let slow = Faulty::new(
        env.local(),
        FaultConfig {
            put_delay: Some(Duration::from_millis(1200)),
            slow_prefix: "v1/packs/".into(),
            ..Default::default()
        },
    );
    let mut o = opts(&src, &env.p("receipts.jsonl"));
    o.readers = 4;
    o.streams = Some(1);
    o.commit_every = Duration::from_secs(600);
    o.commit_bytes = 1 << 40;
    let rep = xfer::backup::backup(Arc::new(slow), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert!(rep.packs >= 4, "expected several packs, got {}", rep.packs);
    // Interleaved files complete only with the last pack, so everything is freed by the final
    // commit. Files read one after another free half the bytes before it. (A wall-time ratio is
    // not used: on a slow CI runner the debug build is CPU-bound and paces packs evenly, which
    // moves t50 towards 0.7 of wall without any interleaving.)
    let freed_before_last = rep
        .commit_trace
        .iter()
        .rev()
        .nth(1)
        .map_or(0, |&(_, freed, _)| freed);
    assert!(
        freed_before_last * 2 >= rep.freeable_bytes,
        "only {freed_before_last} of {} freeable bytes were receipted before the final commit {rep:#?}",
        rep.freeable_bytes
    );
}
