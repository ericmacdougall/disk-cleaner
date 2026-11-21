//! AC-3: duplicates are uploaded once: across files, inside a large file, across hard links and
//! across runs. Counted from the transport's puts.

mod common;
use common::*;
use std::sync::Arc;
use xfer::transport::fault::{FaultConfig, Faulty};

fn puts_under(t: &Faulty, prefix: &str) -> Vec<String> {
    t.put_keys
        .lock()
        .expect("keys")
        .iter()
        .filter(|k| k.starts_with(prefix))
        .cloned()
        .collect()
}

fn run(env: &Env, rcp: &str) -> (xfer::backup::Report, Arc<Faulty>) {
    let t = Arc::new(Faulty::new(env.local(), FaultConfig::default()));
    let mut o = opts(&env.p("src"), &env.p(rcp));
    o.state = Some(env.p("state"));
    let rep = xfer::backup::backup(t.clone(), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    (rep, t)
}

#[test]
fn an_identical_copy_adds_no_pack_bytes() {
    let env = Env::new();
    let data = rand_bytes(7, 8 << 20);
    write(&env.p("src/a.bin"), &data);
    let (r1, _) = run(&env, "r1.jsonl");
    assert!(r1.pack_bytes >= 8 << 20);
    write(&env.p("src/copy/b.bin"), &data);
    let (r2, t2) = run(&env, "r2.jsonl");
    assert_eq!(
        puts_under(&t2, "v1/packs/"),
        Vec::<String>::new(),
        "{r2:#?}"
    );
    assert_eq!(r2.pack_bytes, 0);
    assert_eq!(r2.new_chunk_bytes, 0);
}

#[test]
fn an_unchanged_rerun_adds_no_pack_one_index_and_one_snapshot() {
    let env = Env::new();
    sample_tree(&env.p("src"));
    run(&env, "r1.jsonl");
    let (r2, t2) = run(&env, "r2.jsonl");
    assert!(puts_under(&t2, "v1/packs/").is_empty(), "{r2:#?}");
    assert_eq!(puts_under(&t2, "v1/index/").len(), 1, "{r2:#?}");
    assert_eq!(puts_under(&t2, "v1/snapshots/").len(), 1);
    assert_eq!(r2.read_bytes, 0, "unchanged files are not read again");
    // every regular file took the fast path; hard-link members follow their group's file
    let links = receipts(&env.p("r2.jsonl"))
        .iter()
        .filter(|r| r.kind == xfer::meta::Kind::Hardlink)
        .count() as u64;
    assert_eq!(r2.reused_files + links, r2.files);
}

#[test]
fn repeated_regions_inside_a_large_file_are_stored_once() {
    let env = Env::new();
    // the repeat starts at an unaligned offset, so only content-defined boundaries find it
    let x = rand_bytes(11, 32 << 20);
    let y = rand_bytes(12, (777 << 10) + 13);
    let mut f = x.clone();
    f.extend_from_slice(&y);
    f.extend_from_slice(&x);
    write(&env.p("src/big.bin"), &f);
    let (r, _) = run(&env, "r.jsonl");
    // at most the unique bytes plus two maximal chunks that straddle the region boundaries
    let unique = (x.len() + y.len()) as u64;
    let bound = unique + 2 * u64::from(xfer::chunker::CDC_MAX);
    assert!(
        r.new_chunk_bytes <= bound,
        "{} new chunk bytes for {unique} bytes of unique content (bound {bound})",
        r.new_chunk_bytes
    );
    assert!(r.dedup_chunks > 0);
}

#[cfg(unix)]
#[test]
fn hard_links_are_read_and_stored_once() {
    let env = Env::new();
    let data = rand_bytes(21, 4 << 20);
    write(&env.p("src/a.bin"), &data);
    std::fs::hard_link(env.p("src/a.bin"), env.p("src/b.bin")).expect("link");
    std::fs::hard_link(env.p("src/a.bin"), env.p("src/c.bin")).expect("link");
    let (r, _) = run(&env, "r.jsonl");
    assert_eq!(r.read_bytes, 4 << 20, "{r:#?}");
    let rs = receipts(&env.p("r.jsonl"));
    assert_eq!(
        rs.iter()
            .filter(|r| r.kind == xfer::meta::Kind::Hardlink)
            .count(),
        2
    );
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, None)).expect("restore");
    assert_eq!(rr.exit_code, 0);
    assert_eq!(manifest(&env.p("src")), manifest(&target));
}
