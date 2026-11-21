//! The rclone rcd transport against a real rclone, with a local directory as the remote:
//! authentication, the method allowlist, verified single-part uploads above rclone's default
//! streaming cutoff, list and bounded get, and that rclone and its private files are gone after
//! drop. (AC-10, NEG-7.)

mod common;

use std::time::{Duration, Instant};
use xfer::hash::HashKind;
use xfer::transport::rclone::RcloneRcd;
use xfer::transport::{verified_put, RetryPolicy, Transport};
use xfer::Error;

#[test]
fn rcd_round_trip_and_hygiene() {
    let Some(bin) = common::rclone() else { return };
    let remote = tempfile::tempdir().expect("tmp");
    let root = remote.path().to_string_lossy().into_owned();
    let t = RcloneRcd::start(&bin, &root, 32 << 20).expect("rcd starts");
    assert_eq!(
        t.hash_kind(),
        Some(HashKind::Sha1),
        "local remote reports sha1"
    );

    // No secret in argv: the only auth-related argument is the htpasswd path.
    let args = t.args().join(" ");
    assert!(args.contains("--rc-htpasswd"));
    assert!(
        !args.contains("--rc-user")
            && !args.contains("--rc-pass")
            && !args.contains("--rc-no-auth")
    );
    assert!(args.contains("127.0.0.1:"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let d = t.private_dir().to_path_buf();
        let dm = std::fs::metadata(&d).expect("dir").permissions().mode() & 0o777;
        let fm = std::fs::metadata(d.join("htpasswd"))
            .expect("htpasswd")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dm, 0o700);
        assert_eq!(fm, 0o600);
    }

    // Without credentials the rc refuses.
    let r = ureq::post(&format!("{}core/pid", t.base_url())).send_string("{}");
    assert!(
        matches!(r, Err(ureq::Error::Status(401, _))),
        "unauthenticated call must get 401"
    );

    // Methods outside the allowlist are refused before any request is made.
    assert!(matches!(
        t.rc("config/dump", serde_json::json!({})),
        Err(Error::Refused(_))
    ));

    // A 20 MiB object is far above rclone's default 100 KiB streaming cutoff; with the engine's
    // cutoff it must still arrive with a checksum the store reports.
    let mut big = vec![0u8; 20 << 20];
    xfer::gen::Rng::new(11).fill(&mut big);
    let v = verified_put(
        &t,
        "v1/packs/ab/big.pack",
        &big,
        RetryPolicy::fast(),
        &|_| {},
    )
    .expect("verified");
    assert_eq!(v.size, big.len() as u64);
    assert_eq!(v.hash, HashKind::Sha1.digest(&big));
    let small = b"hello index".to_vec();
    verified_put(
        &t,
        "v1/index/r1/000000.idx",
        &small,
        RetryPolicy::fast(),
        &|_| {},
    )
    .expect("small");

    let st = t
        .stat("v1/packs/ab/big.pack")
        .expect("stat")
        .expect("exists");
    assert_eq!(
        st.hash.as_deref(),
        Some(HashKind::Sha1.digest(&big).as_str())
    );
    assert!(t.stat("v1/packs/ab/missing.pack").expect("stat").is_none());

    let all = t.list("v1").expect("list");
    let keys: Vec<_> = all.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, vec!["v1/index/r1/000000.idx", "v1/packs/ab/big.pack"]);
    assert!(all.iter().all(|o| o.hash.is_some()));
    assert!(t
        .list("v1/nothing-here")
        .expect("missing prefix")
        .is_empty());

    assert_eq!(
        t.get("v1/index/r1/000000.idx", 1 << 20).expect("get"),
        small
    );
    assert_eq!(
        t.get("v1/packs/ab/big.pack", 64 << 20).expect("get big"),
        big
    );
    assert!(matches!(
        t.get("v1/packs/ab/big.pack", 1 << 20),
        Err(Error::Corrupt(_))
    ));

    // Bad object names are refused before reaching rclone.
    assert!(t.put("v1/packs/ab/../x", b"x").is_err());

    let pid = t.rclone_pid();
    let dir = t.private_dir().to_path_buf();
    drop(t);
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while common::alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!common::alive(pid), "rclone {pid} still running after drop");
    }
    assert!(!dir.exists(), "private dir left behind");
}

#[test]
fn rcd_refuses_old_or_missing_binary() {
    let r = RcloneRcd::start("/nonexistent/rclone", "/tmp/x", 32 << 20);
    assert!(matches!(r, Err(Error::Transport(_))));
}
