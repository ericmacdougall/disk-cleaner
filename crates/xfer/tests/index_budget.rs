//! The index share holds a backup's hot table (its uncommitted new chunks, allocated once) and
//! the committed chunk map (Revision 6.4):
//!
//! - AC-20: a full hot table is a pass boundary, not an exit. Readers wait for a commit to move
//!   chunks into the committed map, and the run completes with every file receipted.
//! - NEG-23: only a committed map that outgrows its share ends the run, with one clear error
//!   naming `--mem-budget` (exit 3); files committed before that keep their receipts.
//! - Dedup is unchanged: a chunk held in the hot table, uncommitted, when a second file needs it
//!   is still a duplicate, so a tiny hot table sends exactly the bytes a large one sends.
//! - The limits are said at start when the tree comes within 2x of them.
//!
//! Failing first: the tree before Revision 6.4 refuses the 600-file run with exit 3 (its
//! new-chunk set was the limit, not a pass boundary).

mod common;
use common::*;
use xfer::repo::{backup_map_fit, hot_room};

/// `n` small files with distinct contents: one new chunk each.
fn small_files(env: &Env, n: usize) -> std::path::PathBuf {
    let src = env.p("src");
    for i in 0..n {
        write(
            &src.join(format!("d{}/f{i:04}.bin", i % 7)),
            &rand_bytes(7000 + i as u64, 2000),
        );
    }
    src
}

#[test]
fn a_full_hot_table_is_a_pass_boundary_and_the_run_completes() {
    let env = Env::new();
    let src = small_files(&env, 600);
    let mut o = opts(&src, &env.p("receipts.jsonl"));
    // 40 KiB: a hot table of 112 new chunks at a time, a committed map of about 800
    o.budget.i = 40 << 10;
    assert_eq!(hot_room(o.budget.i), 112);
    assert!((700..900).contains(&backup_map_fit(o.budget.i)));
    let rep = xfer::backup::backup(env.local(), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert_eq!(
        receipts(&env.p("receipts.jsonl")).len(),
        manifest(&src).len()
    );
    assert_eq!(rep.new_chunks, 600);
    assert!(
        rep.index_waits > 0,
        "600 new chunks through a table of 112 must wait for commits"
    );
    assert!(rep.commits >= 600 / 112, "{} commits", rep.commits);
    // within 2x of the limits, the run said them at start
    let said = rep.limits.as_deref().expect("the limits were said");
    assert!(said.contains("--mem-budget"), "{said}");
    // and what it committed restores
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, Some(rep.run.clone())))
        .expect("restore");
    assert_eq!(rr.exit_code, 0, "{rr:#?}");
    assert_eq!(manifest(&src), manifest(&target));
}

#[test]
fn a_full_committed_map_stops_the_run_with_one_clear_error() {
    let env = Env::new();
    let src = small_files(&env, 600);
    let mut o = opts(&src, &env.p("receipts.jsonl"));
    // 24 KiB: a committed map of about 500 chunks, less what its packs take
    o.budget.i = 24 << 10;
    assert!(backup_map_fit(o.budget.i) < 550);
    let rep = xfer::backup::backup(env.local(), &o).expect("backup returns a report");
    assert_eq!(rep.exit_code, 3, "{rep:#?}");
    let errs: Vec<_> = rep
        .errors
        .iter()
        .filter(|e| e.contains("chunk map is full"))
        .collect();
    assert_eq!(errs.len(), 1, "{:#?}", rep.errors);
    assert!(errs[0].contains("--mem-budget"), "{}", errs[0]);
    let refused = rep
        .skipped
        .iter()
        .filter(|(_, why)| why.starts_with("not read:"))
        .count();
    assert!(refused > 0, "{rep:#?}");
    // whatever was receipted is real, and more than one pass of it
    let rs = receipts(&env.p("receipts.jsonl"));
    let m = manifest(&src);
    let files = rs
        .iter()
        .filter(|r| r.kind == xfer::meta::Kind::File)
        .count();
    assert!(files > hot_room(o.budget.i), "{files} files receipted");
    for r in &rs {
        let p = r.path.as_ref().expect("path");
        assert!(
            p.is_empty() || p == "." || m.contains_key(p.as_bytes()),
            "{p} is not in the source"
        );
    }
}

/// 400 small files over 40 contents and four 3 MiB files of one content: the duplicates of a
/// chunk still in the hot table are found there, so a tiny table sends what a large one does.
#[test]
fn a_tiny_hot_table_dedups_exactly_as_a_large_one() {
    let run = |i: u64| {
        let env = Env::new();
        let src = env.p("src");
        for n in 0..400u64 {
            write(
                &src.join(format!("s{}/f{n:03}.bin", n % 5)),
                &rand_bytes(900 + n % 40, 3000),
            );
        }
        let big = rand_bytes(77, 3 << 20);
        for n in 0..4 {
            write(&src.join(format!("big/b{n}.bin")), &big);
        }
        let mut o = opts(&src, &env.p("receipts.jsonl"));
        o.budget.i = i;
        let rep = xfer::backup::backup(env.local(), &o).expect("backup");
        assert_eq!(rep.exit_code, 0, "{rep:#?}");
        assert_eq!(
            receipts(&env.p("receipts.jsonl")).len(),
            manifest(&src).len()
        );
        rep
    };
    // 6 KiB: a hot table of 14 new chunks at a time, fewer than the tree's distinct chunks
    assert_eq!(hot_room(6 << 10), 14);
    let tiny = run(6 << 10);
    let large = run(budget().i);
    assert!(tiny.index_waits > 0, "the tiny table never filled");
    assert_eq!(large.index_waits, 0);
    // the ground truth any correct design gives (the design before Revision 6.4 included):
    // each distinct byte is sent once
    assert_eq!(tiny.new_chunk_bytes, 40 * 3000 + (3 << 20));
    assert_eq!(
        (tiny.new_chunks, tiny.new_chunk_bytes, tiny.dedup_chunks),
        (large.new_chunks, large.new_chunk_bytes, large.dedup_chunks)
    );
}

/// Run a backup through `cfg`'s faults in a thread and wait at most `bound` for it: a reader
/// left waiting for room fails the test instead of hanging it.
fn bounded_backup(
    env: &Env,
    src: &std::path::Path,
    i: u64,
    cfg: xfer::transport::fault::FaultConfig,
) -> xfer::backup::Report {
    let t: std::sync::Arc<dyn xfer::transport::Transport> =
        std::sync::Arc::new(xfer::transport::fault::Faulty::new(env.local(), cfg));
    let mut o = opts(src, &env.p("receipts.jsonl"));
    o.budget.i = i;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(xfer::backup::backup(t, &o).map_err(|e| e.to_string()));
    });
    rx.recv_timeout(std::time::Duration::from_secs(90))
        .expect("the backup hung: readers waiting for room were never woken")
        .expect("backup returns a report")
}

#[test]
fn readers_waiting_for_room_go_on_when_every_pack_upload_fails() {
    let env = Env::new();
    let src = small_files(&env, 200);
    let cfg = xfer::transport::fault::FaultConfig {
        fail_put_prefix: "v1/packs/".into(),
        ..Default::default()
    };
    // 6 KiB: 14 new chunks at a time, so readers wait for room again and again
    let rep = bounded_backup(&env, &src, 6 << 10, cfg);
    assert_eq!(rep.exit_code, 3, "{rep:#?}");
    assert!(rep.index_waits > 0, "readers never waited: {rep:#?}");
    assert!(
        rep.errors.iter().any(|e| e.starts_with("upload ")),
        "{:#?}",
        rep.errors
    );
    // every file was read (its pack failed, so it is not receipted), none silently dropped
    assert_eq!(rep.new_chunks, 200, "{rep:#?}");
    let files = receipts(&env.p("receipts.jsonl"))
        .iter()
        .filter(|r| r.kind == xfer::meta::Kind::File)
        .count();
    assert_eq!(files, 0);
}

#[test]
fn readers_waiting_for_room_stop_when_a_commit_fails() {
    let env = Env::new();
    let src = small_files(&env, 200);
    let cfg = xfer::transport::fault::FaultConfig {
        fail_put_prefix: "v1/index/".into(),
        ..Default::default()
    };
    let rep = bounded_backup(&env, &src, 6 << 10, cfg);
    assert_eq!(rep.exit_code, 3, "{rep:#?}");
    assert!(
        rep.errors.iter().any(|e| e.starts_with("commit ")),
        "{:#?}",
        rep.errors
    );
    // the readers stopped instead of waiting for a committer that had given up
    assert!(
        rep.skipped
            .iter()
            .any(|(_, why)| why.contains("stopped committing")),
        "{rep:#?}"
    );
}

#[test]
fn a_tree_far_inside_the_limits_hears_nothing_about_them() {
    let env = Env::new();
    let src = small_files(&env, 50);
    let o = opts(&src, &env.p("receipts.jsonl"));
    let rep = xfer::backup::backup(env.local(), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert_eq!(rep.limits, None);
}
