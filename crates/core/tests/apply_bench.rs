//! AC-3: upload pool throughput against a real rclone remote. Ignored by default; it needs a
//! throwaway remote prefix and network. Run (t2-dev):
//!   PD_BENCH_REMOTE=b2-archive2:prunebackup/_pd-test-<id> PD_BENCH_RCLONE=/usr/local/bin/rclone \
//!   cargo test -p disk-cleaner-core --release --test apply_bench -- --ignored --nocapture
//! Each repetition uses fresh data (seeded), a fresh sub-prefix and a fresh manifest; the whole
//! prefix is purged at the end, also when the run fails. Prints one JSON line per run.
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::apply;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn fill(seed: u64, buf: &mut [u8]) {
    let mut x = seed | 1;
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
}

/// Purges the throwaway prefix when dropped, so a failed run leaves nothing behind.
struct Purge {
    rclone: PathBuf,
    remote: String,
}

impl Drop for Purge {
    fn drop(&mut self) {
        let ok = std::process::Command::new(&self.rclone)
            .args(["purge", &self.remote])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        println!("{{\"purged\":{ok}}}");
    }
}

#[test]
#[ignore]
fn ac3_pool_throughput() {
    let remote = std::env::var("PD_BENCH_REMOTE").expect("PD_BENCH_REMOTE");
    assert!(
        remote.contains("/_pd-test-"),
        "only a throwaway _pd-test- prefix may be used"
    );
    let rclone =
        PathBuf::from(std::env::var("PD_BENCH_RCLONE").unwrap_or_else(|_| "rclone".into()));
    let files: usize = std::env::var("PD_BENCH_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let file_bytes: usize = 64 << 20;
    let _purge = Purge {
        rclone: rclone.clone(),
        remote: remote.clone(),
    };
    let reps: usize = std::env::var("PD_BENCH_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let mut results = Vec::new();
    for rep in 0..reps {
        for jobs in [1usize, 4] {
            let fx = Fx::new();
            let d = fx.data.join("bench");
            fs::create_dir_all(&d).unwrap();
            let seed = 0x5eed_0000 + (rep as u64) * 16 + jobs as u64;
            let mut buf = vec![0u8; file_bytes];
            for k in 0..files {
                // Distinct per file: fill() forces the low bit, so vary higher bits only.
                fill((seed << 16 | k as u64) << 1, &mut buf);
                fs::write(d.join(format!("f{k:02}.bin")), &buf).unwrap();
            }
            drop(buf);
            let (scan, plan) = docs(&[&fx.data], &[P::tree(&d, "backup")]);
            let mut o = fx.opts(true);
            o.remote = format!("{remote}/r{rep}-j{jobs}");
            o.remote_local = None;
            o.rclone = rclone.clone();
            o.jobs = jobs;
            o.shard_bytes = 64 << 20;
            let t = Instant::now();
            let rep_out = apply(&scan, &plan, &o).unwrap();
            let secs = t.elapsed().as_secs_f64();
            assert!(rep_out.shards_failed.is_empty(), "{rep_out:?}");
            assert_eq!(
                rep_out.uploaded_bytes,
                (files * file_bytes) as u64,
                "every file is distinct content"
            );
            assert!(!d.exists(), "removed after verification");
            let manifest = fs::read_to_string(fx.manifest()).unwrap();
            let by_sha1 = manifest.matches("\"verified_by\":\"sha1:").count();
            let by_readback = manifest
                .matches("\"verified_by\":\"readback-sha256:")
                .count();
            let mb = rep_out.uploaded_bytes as f64 / 1e6;
            let line = format!(
                "{{\"rep\":{rep},\"jobs\":{jobs},\"seed\":{seed},\"bytes\":{},\"secs\":{secs:.2},\"mb_per_s\":{:.1},\"shards\":{},\"verified_sha1\":{by_sha1},\"verified_readback\":{by_readback}}}",
                rep_out.uploaded_bytes,
                mb / secs,
                rep_out.shards_uploaded
            );
            println!("{line}");
            results.push((jobs, mb / secs));
        }
    }
    for j in [1usize, 4] {
        let v: Vec<f64> = results.iter().filter(|r| r.0 == j).map(|r| r.1).collect();
        println!("{{\"jobs\":{j},\"mb_per_s_runs\":{v:?}}}");
    }
}
