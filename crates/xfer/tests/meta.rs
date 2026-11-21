//! AC-16: extended metadata on unix. A deleter removes the source after backup, so metadata that
//! is not captured is lost for good: what round-trips must round-trip byte-equal, and what cannot
//! be captured must mark the receipt `meta_incomplete`, which `deletable` refuses.
#![cfg(unix)]

mod common;
use common::*;
use std::path::Path;
use xfer::receipt::{Deletable, Policy, Receipt, Why};

fn policy() -> Policy {
    Policy {
        allow_unknown_build: true,
        ..Policy::default()
    }
}

fn backup_and_restore(env: &Env) -> (Vec<Receipt>, std::path::PathBuf) {
    let rep =
        xfer::backup::backup(env.local(), &opts(&env.p("src"), &env.p("r.jsonl"))).expect("backup");
    assert!(rep.exit_code == 0 || rep.exit_code == 2, "{rep:#?}");
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, Some(rep.run)))
        .expect("restore");
    assert_eq!(rr.exit_code, 0, "{rr:#?}");
    (receipts(&env.p("r.jsonl")), target)
}

fn by_path<'a>(rs: &'a [Receipt], p: &str) -> &'a Receipt {
    rs.iter()
        .find(|r| r.path.as_deref() == Some(p))
        .unwrap_or_else(|| panic!("no receipt for {p}"))
}

/// Set an xattr, or None when this filesystem does not take one of this size.
fn try_set(p: &Path, name: &str, v: &[u8]) -> Option<()> {
    match xattr::set(p, name, v) {
        Ok(()) => Some(()),
        Err(e) => {
            eprintln!(
                "this filesystem refused xattr {name} ({} bytes): {e}",
                v.len()
            );
            None
        }
    }
}

#[test]
fn a_small_xattr_round_trips_byte_equal() {
    let env = Env::new();
    write(&env.p("src/tagged.txt"), b"has an xattr\n");
    write(&env.p("src/plain.txt"), b"has none\n");
    let value = rand_bytes(31, 1024);
    if try_set(&env.p("src/tagged.txt"), "user.prunedisk.test", &value).is_none() {
        // CI's ext4 and APFS take user xattrs; only an exotic dev filesystem gets here
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must run the xattr round trip"
        );
        return;
    }
    let (rs, target) = backup_and_restore(&env);
    let r = by_path(&rs, "tagged.txt");
    assert!(!r.meta_incomplete);
    assert_eq!(
        r.deletable(&env.p("src"), &policy()).expect("check"),
        Deletable::Yes
    );
    let got = xattr::get(target.join("tagged.txt"), "user.prunedisk.test").expect("get");
    assert_eq!(
        got.as_deref(),
        Some(&value[..]),
        "the xattr did not round-trip"
    );
    let none: Vec<_> = xattr::list(target.join("plain.txt"))
        .expect("list")
        .collect();
    assert!(
        none.iter()
            .all(|n| !n.to_string_lossy().starts_with("user.")),
        "{none:?}"
    );
}

#[test]
fn xattrs_over_the_cap_mark_the_receipt_incomplete() {
    // one 100 KiB value where the filesystem allows it (APFS); else several values that together
    // pass the 64 KiB cap (Linux limits one value to 64 KiB and ext4 all of them to one block,
    // so on Linux the test falls back to tmpfs under /dev/shm, which holds them)
    let oversize = |p: &Path| {
        try_set(p, "user.prunedisk.big", &rand_bytes(32, 100 << 10)).or_else(|| {
            (0..20).try_for_each(|i| {
                try_set(
                    p,
                    &format!("user.prunedisk.p{i:02}"),
                    &rand_bytes(40 + i, 4000),
                )
            })
        })
    };
    let mut dirs = vec![Env::new()];
    if cfg!(target_os = "linux") {
        if let Ok(d) = tempfile::tempdir_in("/dev/shm") {
            dirs.push(Env { dir: d });
        }
    }
    let found = dirs.into_iter().find(|env| {
        let p = env.p("src/big-xattr.txt");
        write(&p, b"oversized metadata\n");
        oversize(&p).is_some()
    });
    let Some(env) = found else {
        #[cfg(target_os = "macos")]
        panic!("APFS must hold a 100 KiB xattr");
        #[cfg(not(target_os = "macos"))]
        {
            eprintln!("skipping: no filesystem here holds over 64 KiB of xattrs per file");
            return;
        }
    };
    {}
    write(&env.p("src/ok.txt"), b"fine\n");
    let rep =
        xfer::backup::backup(env.local(), &opts(&env.p("src"), &env.p("r.jsonl"))).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    let rs = receipts(&env.p("r.jsonl"));
    let r = by_path(&rs, "big-xattr.txt");
    assert!(r.meta_incomplete, "{r:?}");
    assert_eq!(
        r.deletable(&env.p("src"), &policy()).expect("check"),
        Deletable::No(Why::MetaIncomplete)
    );
    assert!(!by_path(&rs, "ok.txt").meta_incomplete);
}

#[cfg(target_os = "macos")]
#[test]
fn a_macos_acl_marks_the_receipt_incomplete() {
    let env = Env::new();
    let p = env.p("src/acl.txt");
    write(&p, b"an ACL entry\n");
    let st = std::process::Command::new("chmod")
        .args(["+a", "everyone deny delete"])
        .arg(&p)
        .status()
        .expect("chmod +a");
    assert!(st.success());
    let rep =
        xfer::backup::backup(env.local(), &opts(&env.p("src"), &env.p("r.jsonl"))).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    let rs = receipts(&env.p("r.jsonl"));
    let r = by_path(&rs, "acl.txt");
    assert!(r.meta_incomplete, "{r:?}");
    assert_eq!(
        r.deletable(&env.p("src"), &policy()).expect("check"),
        Deletable::No(Why::MetaIncomplete)
    );
    let _ = std::process::Command::new("chmod")
        .args(["-N"])
        .arg(&p)
        .status();
}

#[cfg(target_os = "macos")]
#[test]
fn macos_hidden_flag_round_trips() {
    use std::os::macos::fs::MetadataExt;
    let env = Env::new();
    let p = env.p("src/hidden.txt");
    write(&p, b"hidden in Finder\n");
    let st = std::process::Command::new("chflags")
        .arg("hidden")
        .arg(&p)
        .status()
        .expect("chflags");
    assert!(st.success());
    let (_, target) = backup_and_restore(&env);
    let flags = std::fs::symlink_metadata(target.join("hidden.txt"))
        .expect("lstat")
        .st_flags();
    assert_ne!(flags & libc::UF_HIDDEN, 0, "UF_HIDDEN lost: {flags:#x}");
}
