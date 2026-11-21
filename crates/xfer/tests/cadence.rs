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
    let t50 = rep.t50_ms.expect("t50");
    assert!(
        (t50 as f64) < 0.75 * rep.wall_ms as f64,
        "half the freeable bytes were receipted at {t50} ms of {} ms",
        rep.wall_ms
    );
}
