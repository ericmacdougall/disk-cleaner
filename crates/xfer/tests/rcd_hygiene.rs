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

/// rcd's memory must not grow with the bytes uploaded. rclone keeps a stats group per rc call,
/// and a finished upload's accounting entry holds the buffer the object was read into, so without
/// the per-upload group and its deletion 24 uploads of 16 MiB leave rcd holding about 400 MiB.
#[cfg(target_os = "linux")]
#[test]
fn rcd_memory_does_not_grow_with_bytes_uploaded() {
    let Some(bin) = common::rclone() else { return };
    let remote = tempfile::tempdir().expect("tmp");
    let root = remote.path().to_string_lossy().into_owned();
    let t = RcloneRcd::start(&bin, &root, 16 << 20).expect("rcd starts");
    let kib = |field: &str| -> u64 {
        let s =
            std::fs::read_to_string(format!("/proc/{}/status", t.rclone_pid())).expect("status");
        s.lines()
            .find(|l| l.starts_with(field))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .expect("field")
    };
    let mut data = vec![0u8; 16 << 20];
    xfer::gen::Rng::new(5).fill(&mut data);
    let n = 24u32;
    for i in 0..n {
        data[..4].copy_from_slice(&i.to_le_bytes());
        verified_put(
            &t,
            &format!("v1/packs/{:02x}/p{i}.pack", i),
            &data,
            RetryPolicy::fast(),
            &|_| {},
        )
        .expect("upload");
    }
    let peak_mib = kib("VmHWM:") / 1024;
    let now_mib = kib("VmRSS:") / 1024;
    assert_eq!(t.list("v1/packs").expect("list").len(), n as usize);
    // The live set is one buffered upload; Go's heap target allows about twice that plus the
    // runtime. Retaining every upload would be n × 16 MiB = 384 MiB.
    assert!(
        peak_mib < 160,
        "rcd peak RSS {peak_mib} MiB (now {now_mib} MiB) after {n} uploads of 16 MiB"
    );
}

#[test]
fn rcd_refuses_old_or_missing_binary() {
    let r = RcloneRcd::start("/nonexistent/rclone", "/tmp/x", 32 << 20);
    assert!(matches!(r, Err(Error::Transport(_))));
}

/// Code lines (comments stripped) that would reach rclone's configuration: an rc `config/<method>`
/// call, its config file, its config environment variables, the `--config` flag or the `config`
/// and `obscure` subcommands.
fn config_hits(text: &str) -> Vec<String> {
    const BANNED: [&str; 5] = [
        "rclone.conf",
        "RCLONE_CONFIG",
        "\"--config",
        "\"config\"",
        "\"obscure\"",
    ];
    let mut hits = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let method = code.match_indices("\"config/").any(|(i, m)| {
            code[i + m.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
        });
        if method || BANNED.iter().any(|b| code.contains(b)) {
            hits.push(format!("{}: {}", n + 1, line.trim()));
        }
    }
    hits
}

#[test]
fn the_engine_source_never_reaches_rclone_config() {
    // the control: the scanner finds each forbidden form
    for bad in [
        r#"t.rc("config/dump", json!({}))"#,
        r#"let p = home.join(".config/rclone/rclone.conf");"#,
        r#"cmd.env("RCLONE_CONFIG_B2_KEY", k);"#,
        r#"cmd.args(["--config", path]);"#,
        r#"cmd.args(["config", "show"]);"#,
        r#"cmd.args(["obscure", s]);"#,
    ] {
        assert!(!config_hits(bad).is_empty(), "scanner missed {bad}");
    }
    // the allowlist's own refusal check names the bare prefix and is not a call
    assert!(config_hits(r#"if root.contains("config/") {"#).is_empty());

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack = vec![src];
    let (mut files, mut hits) = (0, Vec::new());
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read_dir") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                files += 1;
                let text = std::fs::read_to_string(&p).expect("read");
                for h in config_hits(&text) {
                    hits.push(format!("{}:{h}", p.display()));
                }
            }
        }
    }
    assert!(files >= 10, "scanned only {files} files");
    assert!(
        hits.is_empty(),
        "config access in src:\n{}",
        hits.join("\n")
    );
}
