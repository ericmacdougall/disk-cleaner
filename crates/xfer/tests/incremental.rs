//! AC-4: a changed large file re-uploads only its changed chunks.

mod common;
use common::*;
use std::sync::Arc;
use xfer::transport::fault::{FaultConfig, Faulty};

#[test]
fn a_mid_file_edit_resends_at_most_three_chunks() {
    let env = Env::new();
    let src = env.p("src");
    let mut data = rand_bytes(31, 64 << 20);
    write(&src.join("disk.img"), &data);
    let mut o = opts(&src, &env.p("r1.jsonl"));
    o.state = Some(env.p("state"));
    let r1 = xfer::backup::backup(env.local(), &o).expect("backup 1");
    assert_eq!(r1.exit_code, 0);

    // rewrite 1 MiB in the middle
    let edit = rand_bytes(32, 1 << 20);
    data[32 << 20..33 << 20].copy_from_slice(&edit);
    write(&src.join("disk.img"), &data);

    let t = Arc::new(Faulty::new(env.local(), FaultConfig::default()));
    let mut o2 = opts(&src, &env.p("r2.jsonl"));
    o2.state = Some(env.p("state"));
    let r2 = xfer::backup::backup(t, &o2).expect("backup 2");
    assert_eq!(r2.exit_code, 0, "{r2:#?}");
    assert!(r2.new_chunks <= 3, "{} new chunks", r2.new_chunks);
    assert!(
        r2.new_chunk_bytes <= 12 << 20,
        "{} new chunk bytes",
        r2.new_chunk_bytes
    );

    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, Some(r2.run.clone())))
        .expect("restore");
    assert_eq!(rr.exit_code, 0);
    assert_eq!(std::fs::read(target.join("disk.img")).expect("read"), data);
}

/// An insertion shifts every later byte; content-defined boundaries resynchronise, so only the
/// chunks around the insertion are new (fixed-size blocks would make every later block new).
#[test]
fn an_insertion_resends_only_the_chunks_around_it() {
    let env = Env::new();
    let src = env.p("src");
    let data = rand_bytes(41, 64 << 20);
    write(&src.join("disk.img"), &data);
    let mut o = opts(&src, &env.p("r1.jsonl"));
    o.state = Some(env.p("state"));
    assert_eq!(
        xfer::backup::backup(env.local(), &o)
            .expect("backup 1")
            .exit_code,
        0
    );

    let mut shifted = data[..20 << 20].to_vec();
    shifted.extend_from_slice(&rand_bytes(42, 1000));
    shifted.extend_from_slice(&data[20 << 20..]);
    write(&src.join("disk.img"), &shifted);
    let mut o2 = opts(&src, &env.p("r2.jsonl"));
    o2.state = Some(env.p("state"));
    let r2 = xfer::backup::backup(env.local(), &o2).expect("backup 2");
    assert_eq!(r2.exit_code, 0, "{r2:#?}");
    assert!(
        r2.new_chunks <= 3,
        "{} new chunks after a 1000-byte insertion",
        r2.new_chunks
    );
    assert!(
        r2.new_chunk_bytes <= 12 << 20,
        "{} new chunk bytes",
        r2.new_chunk_bytes
    );
}
