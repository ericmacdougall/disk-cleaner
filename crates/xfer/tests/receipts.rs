//! AC-12, NEG-15 and NEG-20: the deleter's checks. A receipt permits deletion only while the
//! path is exactly what was backed up, and only from an accepted engine build and version.
#![cfg(unix)]

mod common;
use common::*;
use std::path::Path;
use xfer::receipt::{deletable_group, Deletable, Policy, Receipt, Why};

fn policy() -> Policy {
    Policy {
        allow_unknown_build: true, // test builds carry no commit
        ..Policy::default()
    }
}

fn backup(src: &Path, rcp: &Path, env: &Env) -> Vec<Receipt> {
    let mut o = opts(src, rcp);
    o.state = Some(env.p("state"));
    let rep = xfer::backup::backup(env.local(), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    receipts(rcp)
}

fn by_path<'a>(rs: &'a [Receipt], p: &str) -> &'a Receipt {
    rs.iter()
        .find(|r| r.path.as_deref() == Some(p))
        .unwrap_or_else(|| panic!("no receipt for {p}"))
}

fn tree(src: &Path) {
    for n in ["keep", "touch", "replace", "swap", "rewrite"] {
        write(
            &src.join(format!("{n}.txt")),
            format!("{n} contents\n").as_bytes(),
        );
    }
    write(&src.join("g/a.txt"), b"linked\n");
    std::fs::hard_link(src.join("g/a.txt"), src.join("g/b.txt")).expect("link");
}

#[test]
fn still_matches_only_an_untouched_path() {
    let env = Env::new();
    let src = env.p("src");
    tree(&src);
    let rs = backup(&src, &env.p("r.jsonl"), &env);
    let yes = |p: &str| {
        by_path(&rs, p)
            .deletable(&src, &policy())
            .expect("deletable")
    };

    assert_eq!(yes("keep.txt"), Deletable::Yes);

    // same size, new mtime (a write after the read)
    std::fs::write(src.join("touch.txt"), b"TOUCH contents\n").expect("write");
    assert!(matches!(
        yes("touch.txt"),
        Deletable::No(Why::IdentityChanged(_))
    ));

    // replaced at the same path: a new inode
    write(&src.join("replace.tmp"), b"replace contents\n");
    std::fs::rename(src.join("replace.tmp"), src.join("replace.txt")).expect("rename");
    assert!(matches!(yes("replace.txt"), Deletable::No(_)));

    // a symlink substituted for the file
    std::fs::rename(src.join("swap.txt"), src.join("swap.orig")).expect("rename");
    std::os::unix::fs::symlink("swap.orig", src.join("swap.txt")).expect("symlink");
    assert!(matches!(yes("swap.txt"), Deletable::No(Why::KindChanged)));

    // gone
    std::fs::remove_file(src.join("keep.txt")).expect("rm");
    assert_eq!(yes("keep.txt"), Deletable::No(Why::Missing));
}

#[test]
fn a_hard_link_group_is_deletable_only_whole_and_unchanged() {
    let env = Env::new();
    let src = env.p("src");
    tree(&src);
    let rs = backup(&src, &env.p("r.jsonl"), &env);
    let a = by_path(&rs, "g/a.txt");
    let b = by_path(&rs, "g/b.txt");
    assert_eq!(
        deletable_group(&[a, b], &src, &policy()).expect("group"),
        Deletable::Yes
    );
    // one member alone does not cover every link
    assert!(matches!(
        deletable_group(&[a], &src, &policy()).expect("group"),
        Deletable::No(_)
    ));
    // a member removed outside the deleter changes the group
    std::fs::remove_file(src.join("g/b.txt")).expect("rm");
    assert!(matches!(
        deletable_group(&[a, b], &src, &policy()).expect("group"),
        Deletable::No(_)
    ));
}

#[test]
fn a_stale_receipt_is_refused_after_a_rerun_changed_the_file() {
    let env = Env::new();
    let src = env.p("src");
    tree(&src);
    let old = backup(&src, &env.p("r1.jsonl"), &env);
    std::fs::write(src.join("rewrite.txt"), b"a longer, different body\n").expect("write");
    let new = backup(&src, &env.p("r2.jsonl"), &env);
    let stale = by_path(&old, "rewrite.txt");
    assert!(matches!(
        stale.deletable(&src, &policy()).expect("deletable"),
        Deletable::No(Why::IdentityChanged(_))
    ));
    assert_eq!(
        by_path(&new, "rewrite.txt")
            .deletable(&src, &policy())
            .expect("deletable"),
        Deletable::Yes
    );
}

#[test]
fn policy_refusals() {
    let env = Env::new();
    let src = env.p("src");
    tree(&src);
    let rs = backup(&src, &env.p("r.jsonl"), &env);
    let r = by_path(&rs, "keep.txt").clone();

    let mut incomplete = r.clone();
    incomplete.meta_incomplete = true;
    assert_eq!(
        incomplete.deletable(&src, &policy()).expect("deletable"),
        Deletable::No(Why::MetaIncomplete)
    );

    let mut future = r.clone();
    future.v = 2;
    assert_eq!(
        future.deletable(&src, &policy()).expect("deletable"),
        Deletable::No(Why::UnknownVersion(2))
    );

    let deny = Policy {
        deny: vec!["disk-cleaner-xfer 0.1.0".into()],
        ..policy()
    };
    assert!(matches!(
        r.deletable(&src, &deny).expect("deletable"),
        Deletable::No(Why::DeniedEngine(_))
    ));

    // the default policy refuses a build without a recorded commit
    if r.engine.ends_with("+unknown") {
        assert_eq!(
            r.deletable(&src, &Policy::default()).expect("deletable"),
            Deletable::No(Why::UnknownBuild)
        );
    }
}
