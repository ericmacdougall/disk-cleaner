//! AC-1 and AC-6: backup then restore reproduces the tree exactly, from the bucket alone.

mod common;
use common::*;

#[test]
fn backup_then_restore_is_exact_from_the_bucket_alone() {
    let env = Env::new();
    let src = env.p("src");
    sample_tree(&src);
    let rcp = env.p("receipts.jsonl");
    let (t, counters) = env.store();
    let mut o = opts(&src, &rcp);
    o.state = Some(env.p("state"));
    let rep = xfer::backup::backup(t.clone(), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "report: {rep:#?}");
    assert!(rep.errors.is_empty() && rep.changed.is_empty() && rep.unreceipted.is_empty());
    let before = manifest(&src);
    let rs = receipts(&rcp);
    assert_eq!(rs.len(), before.len(), "one receipt per path");
    assert!(counters.snapshot().puts > 0);

    // restore with an empty state dir and a fresh transport (AC-6)
    std::fs::remove_dir_all(env.p("state")).ok();
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, None)).expect("restore");
    assert_eq!(rr.exit_code, 0, "restore: {rr:#?}");
    let after = manifest(&target);
    assert_eq!(before, after);
}

#[test]
fn restore_refuses_a_non_empty_target() {
    let env = Env::new();
    let src = env.p("src");
    write(&src.join("x"), b"x");
    let rep = xfer::backup::backup(env.local(), &opts(&src, &env.p("r"))).expect("backup");
    assert_eq!(rep.exit_code, 0);
    let target = env.p("t");
    write(&target.join("existing"), b"keep me");
    let r = xfer::restore::restore(env.local(), &restore_opts(&target, None));
    assert!(matches!(r, Err(xfer::Error::Refused(_))));
    assert_eq!(
        std::fs::read(target.join("existing")).expect("kept"),
        b"keep me"
    );
}
