//! The memory budget bounds the engine plus rcd (AC-8's RSS cell, on trees larger than AC-8's
//! 300 MiB). On t2-dev, before the compression share, the rcd heap limit and the fixed mmap
//! threshold, the Mac profile (`--mem-budget 640Mi --max-pack 16Mi --max-streams 4`) measured
//! 820 MiB (engine 627 + rcd 242) on a 2 GiB W1 tree, and 1376 MiB with `--level 19`.
//!
//! The test backs up a generated W1 tree through a real rclone rcd (a local directory as the
//! remote) with nine compressors, as on a ten-core Mac, adaptive and at a fixed level 19, and
//! samples this process's RSS plus rcd's every 50 ms. Linux only (it reads /proc), and release
//! builds only: a debug build's zstd and BLAKE3 are too slow for a tree this size. Run with
//! `cargo test --release -p disk-cleaner-xfer --test mem_plan`.
//!
//! Failing first: `--features broken-mem-plan` (no compression share, no rcd heap limit, the
//! default glibc mmap threshold).

#![cfg(target_os = "linux")]

mod common;
use common::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use xfer::repo::{Budget, MIB};
use xfer::transport::rclone::RcloneRcd;

fn peak_during(rcd_pid: u32, f: impl FnOnce()) -> (u64, u64, u64) {
    let stop = AtomicBool::new(false);
    let (pe, pr, ps) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    std::thread::scope(|s| {
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let e = xfer::util::rss_of(std::process::id()).unwrap_or(0);
                let r = xfer::util::rss_of(rcd_pid).unwrap_or(0);
                pe.fetch_max(e, Ordering::Relaxed);
                pr.fetch_max(r, Ordering::Relaxed);
                ps.fetch_max(e + r, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        f();
        stop.store(true, Ordering::Relaxed);
    });
    (
        pe.into_inner() / MIB,
        pr.into_inner() / MIB,
        ps.into_inner() / MIB,
    )
}

#[test]
fn engine_plus_rcd_stay_inside_the_mac_budget() {
    if cfg!(debug_assertions) {
        eprintln!("skipping: release builds only (cargo test --release --test mem_plan)");
        return;
    }
    let Some(bin) = rclone() else { return };
    xfer::util::tune_allocator();
    let env = Env::new();
    let src = env.p("src");
    xfer::gen::generate(&src, xfer::gen::Workload::W1, 768 * MIB, 7).expect("gen");
    let total = 640 * MIB;
    let b = Budget::compute(total, 16 * MIB, 4).expect("mac budget");
    for level in [None, Some(19)] {
        let store = env.p(&format!("store-{}", level.unwrap_or(0)));
        std::fs::create_dir_all(&store).expect("store");
        let t =
            RcloneRcd::start_limited(&bin, &store.to_string_lossy(), b.p_max, Some(b.rcd_heap()))
                .expect("rcd");
        let pid = t.rclone_pid();
        let t: Arc<dyn xfer::transport::Transport> = Arc::new(t);
        let mut o = opts(&src, &env.p("r.jsonl"));
        o.budget = b;
        o.compressors = 9;
        o.readers = 4;
        o.level = level;
        o.policy = xfer::transport::RetryPolicy::default();
        let mut rep = None;
        let (e, r, sum) = peak_during(pid, || {
            rep = Some(xfer::backup::backup(t.clone(), &o).expect("backup"));
        });
        let rep = rep.expect("report");
        assert_eq!(rep.exit_code, 0, "{rep:#?}");
        assert!(
            rep.packs > 20,
            "the tree must span many packs: {}",
            rep.packs
        );
        eprintln!("level {level:?}: engine {e} MiB, rcd {r} MiB, sum {sum} MiB");
        assert!(
            sum <= total / MIB,
            "level {level:?}: engine {e} + rcd {r} = {sum} MiB peak, over --mem-budget {} MiB ({})",
            total / MIB,
            b.describe()
        );
    }
}
