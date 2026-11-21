//! AC-18 and NEG-3, NEG-4, NEG-11, NEG-18: the backup covers exactly the planned input.
//! Excluded or unlisted paths are never read, uploaded or receipted; paths that change,
//! cannot be read or are not regular files are reported and never treated as backed up.
#![cfg(unix)]

mod common;
use common::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use xfer::backup::Report;
use xfer::receipt::Receipt;
use xfer::transport::fault::{FaultConfig, Faulty};
use xfer::walk::Filter;

fn named(rs: &[Receipt], p: &str) -> bool {
    rs.iter().any(|r| r.path.as_deref() == Some(p))
}

fn run(env: &Env, o: &xfer::backup::Options) -> (Report, Vec<Receipt>) {
    let rep = xfer::backup::backup(env.local(), o).expect("backup");
    (rep, receipts(o.receipts.as_ref().expect("receipts path")))
}

#[test]
fn excluded_paths_and_their_hard_link_groups_are_never_uploaded() {
    let env = Env::new();
    let src = env.p("src");
    let secret = rand_bytes(61, 300 << 10);
    let linked = rand_bytes(62, 300 << 10);
    let kept = rand_bytes(63, 300 << 10);
    write(&src.join("keys/id_secret"), &secret);
    write(&src.join("data/a.bin"), &linked);
    std::fs::create_dir_all(src.join("cache")).expect("mkdir");
    std::fs::hard_link(src.join("data/a.bin"), src.join("cache/a-link.bin")).expect("link");
    write(&src.join("data/kept.bin"), &kept);
    std::os::unix::fs::symlink("../keys/id_secret", src.join("data/to-secret")).expect("link");

    let mut o = opts(&src, &env.p("r.jsonl"));
    o.filter = Filter::new(&["keys".into(), "cache/**".into()]).expect("filter");
    let (rep, rs) = run(&env, &o);
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    // the excluded file, both members of the group one excluded member belongs to, and the
    // symlink whose target is excluded: none receipted, none of their bytes in the store
    let receipted: Vec<&str> = [
        "keys/id_secret",
        "data/a.bin",
        "cache/a-link.bin",
        "data/to-secret",
    ]
    .into_iter()
    .filter(|p| named(&rs, p))
    .collect();
    assert!(receipted.is_empty(), "receipted: {receipted:?}");
    assert!(!store_contains(&env.p("store"), &secret[..4096]));
    assert!(!store_contains(&env.p("store"), &linked[..4096]));
    assert!(named(&rs, "data/kept.bin"));
    assert!(store_contains(&env.p("store"), &kept[..4096]));
}

#[test]
fn only_listed_paths_are_backed_up_and_unsafe_entries_are_refused() {
    let env = Env::new();
    let src = env.p("src");
    let listed = rand_bytes(71, 200 << 10);
    let unlisted = rand_bytes(72, 200 << 10);
    write(&src.join("a/listed.bin"), &listed);
    write(&src.join("a/unlisted.bin"), &unlisted);
    let mut o = opts(&src, &env.p("r.jsonl"));
    o.files_from = Some(vec![b"a/listed.bin".to_vec()]);
    let (rep, rs) = run(&env, &o);
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert!(named(&rs, "a/listed.bin"));
    assert!(!named(&rs, "a/unlisted.bin"));
    assert!(!store_contains(&env.p("store"), &unlisted[..4096]));

    // a listed path that has vanished, and one reached through a symlinked directory: both
    // skipped or reported changed, neither receipted, and the outside bytes never read
    let env = Env::new();
    let src = env.p("src");
    let outside = rand_bytes(73, 200 << 10);
    write(&env.p("outside/f.bin"), &outside);
    write(&src.join("a/listed.bin"), &listed);
    std::os::unix::fs::symlink(env.p("outside"), src.join("lnk")).expect("symlink");
    let mut o = opts(&src, &env.p("r.jsonl"));
    o.files_from = Some(vec![
        b"a/listed.bin".to_vec(),
        b"a/gone.bin".to_vec(),
        b"lnk/f.bin".to_vec(),
    ]);
    let (rep, rs) = run(&env, &o);
    assert_eq!(rep.exit_code, 2, "{rep:#?}");
    assert!(named(&rs, "a/listed.bin"));
    for p in ["a/gone.bin", "lnk/f.bin"] {
        assert!(!named(&rs, p), "{p} was receipted");
        assert!(
            rep.skipped.iter().any(|(s, why)| s == p && !why.is_empty())
                || rep.changed.iter().any(|c| c == p),
            "{p} neither skipped nor changed: {rep:#?}"
        );
    }
    assert!(!store_contains(&env.p("store"), &outside[..4096]));

    for bad in [&b"../outside"[..], b"/etc/passwd", b"a/../../x"] {
        let env = Env::new();
        write(&env.p("src/x"), b"x");
        let mut o = opts(&env.p("src"), &env.p("r.jsonl"));
        o.files_from = Some(vec![bad.to_vec()]);
        let r = xfer::backup::backup(env.local(), &o);
        assert!(
            matches!(r, Err(xfer::Error::Refused(_))),
            "{:?} was not refused: {r:?}",
            String::from_utf8_lossy(bad)
        );
        assert!(receipts(&env.p("r.jsonl")).is_empty());
    }
}

#[test]
fn a_symlink_to_a_directory_is_stored_as_a_link_and_not_followed() {
    let env = Env::new();
    let outside = rand_bytes(81, 200 << 10);
    write(&env.p("elsewhere/private.bin"), &outside);
    let src = env.p("src");
    write(&src.join("a.txt"), b"a");
    std::os::unix::fs::symlink(env.p("elsewhere"), src.join("dirlink")).expect("symlink");
    let (rep, rs) = run(&env, &opts(&src, &env.p("r.jsonl")));
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    let link = rs
        .iter()
        .find(|r| r.path.as_deref() == Some("dirlink"))
        .expect("the link is receipted");
    assert_eq!(link.kind, xfer::meta::Kind::Symlink);
    assert!(!rs
        .iter()
        .any(|r| r.path.as_deref() == Some("dirlink/private.bin")));
    assert!(!store_contains(&env.p("store"), &outside[..4096]));
}

#[test]
fn unreadable_files_fifos_and_sockets_are_skipped_with_a_reason() {
    let env = Env::new();
    let src = env.p("src");
    write(&src.join("ok.txt"), b"fine");
    write(&src.join("locked.txt"), b"no access");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        src.join("locked.txt"),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod");
    let fifo = std::ffi::CString::new(src.join("pipe").to_str().expect("utf8")).expect("c");
    // SAFETY: mkfifo with a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let _sock = std::os::unix::net::UnixListener::bind(src.join("sock")).expect("socket");

    let (rep, rs) = run(&env, &opts(&src, &env.p("r.jsonl")));
    // a FIFO that were opened would block this test forever; it returns, so it was not
    let skipped: Vec<&str> = rep.skipped.iter().map(|(p, _)| p.as_str()).collect();
    let root = unsafe { libc::geteuid() } == 0;
    if !root {
        assert!(skipped.contains(&"locked.txt"), "{rep:#?}");
        assert!(!named(&rs, "locked.txt"));
    }
    for p in ["pipe", "sock"] {
        assert!(skipped.contains(&p), "{p} not skipped: {rep:#?}");
        assert!(!named(&rs, p));
    }
    assert!(rep.skipped.iter().all(|(_, why)| !why.is_empty()));
    assert!(named(&rs, "ok.txt"));
    assert_eq!(rep.exit_code, 2, "skips make a partial run");
    std::fs::set_permissions(
        src.join("locked.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod back");
}

/// Large files ahead of the target in the schedule, and a slow store, so the readers reach the
/// target only after the test has changed it.
fn slow_tree(src: &Path) {
    for i in 0..10u64 {
        write(
            &src.join(format!("big/b{i}.bin")),
            &rand_bytes(900 + i, 16 << 20),
        );
    }
}

fn slow_store(env: &Env) -> Arc<Faulty> {
    Arc::new(Faulty::new(
        env.local(),
        FaultConfig {
            put_delay: Some(Duration::from_millis(700)),
            slow_prefix: "v1/packs/".into(),
            ..Default::default()
        },
    ))
}

#[test]
fn a_symlink_swapped_into_a_middle_component_is_reported_changed() {
    let env = Env::new();
    let src = env.p("src");
    slow_tree(&src);
    write(&src.join("mid/x.txt"), b"original");
    write(
        &env.p("decoy/x.txt"),
        b"decoy content that must not be read",
    );
    let mut o = opts(&src, &env.p("r.jsonl"));
    o.readers = 2;
    o.streams = Some(1);
    let t = slow_store(&env);
    let h = std::thread::spawn(move || xfer::backup::backup(t, &o).expect("backup"));
    std::thread::sleep(Duration::from_millis(500));
    std::fs::rename(src.join("mid"), src.join("mid.orig")).expect("rename");
    std::os::unix::fs::symlink(env.p("decoy"), src.join("mid")).expect("symlink");
    let rep = h.join().expect("join");
    let rs = receipts(&env.p("r.jsonl"));
    assert!(
        rep.changed.iter().any(|p| p == "mid/x.txt"),
        "not reported changed: {rep:#?}"
    );
    assert!(!named(&rs, "mid/x.txt"));
    assert!(!store_contains(&env.p("store"), b"decoy content"));
}

#[test]
fn a_file_that_keeps_changing_is_reported_changed_and_not_receipted() {
    let env = Env::new();
    let src = env.p("src");
    slow_tree(&src);
    write(&src.join("hot.log"), &rand_bytes(99, 2 << 20));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        let p = src.join("hot.log");
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(p)
                .expect("open");
            while !stop.load(Ordering::Relaxed) {
                f.write_all(b"another line\n").expect("append");
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let mut o = opts(&src, &env.p("r.jsonl"));
    o.readers = 2;
    o.streams = Some(1);
    let rep = xfer::backup::backup(slow_store(&env), &o).expect("backup");
    stop.store(true, Ordering::Relaxed);
    writer.join().expect("writer");
    assert!(
        rep.changed.iter().any(|p| p == "hot.log"),
        "not reported changed: {:?}",
        rep.changed
    );
    assert!(!named(&receipts(&env.p("r.jsonl")), "hot.log"));
    assert_eq!(rep.exit_code, 2);
}

#[cfg(target_os = "linux")]
#[test]
fn one_file_system_stops_at_mount_points() {
    // /dev holds other mounts (/dev/shm, /dev/pts) on Linux and in containers
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let sub: Vec<&str> = mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter(|m| m.starts_with("/dev/") && m.matches('/').count() == 2)
        .collect();
    let under = |one: bool| -> Vec<String> {
        let f = Filter::new(&[]).expect("filter");
        let w = xfer::walk::walk(
            Path::new("/dev"),
            &xfer::walk::WalkOpts {
                filter: &f,
                files_from: None,
                one_file_system: one,
            },
        )
        .expect("walk");
        let rels = w.nodes.iter().map(|n| n.rel.clone());
        let rels = rels.chain(w.skipped.iter().map(|(r, _)| r.clone()));
        rels.map(|r| String::from_utf8_lossy(&r).into_owned())
            .filter(|r| sub.iter().any(|m| r.starts_with(&format!("{}/", &m[5..]))))
            .collect()
    };
    // the control: without the flag the walk does enter a mount below /dev
    if under(false).is_empty() {
        eprintln!("skipping: no populated mount below /dev here ({sub:?})");
        return;
    }
    assert_eq!(under(true), Vec::<String>::new(), "a mount was crossed");
    let env = Env::new();
    let mut o = opts(Path::new("/dev"), &env.p("r.jsonl"));
    o.one_file_system = true;
    let rep = xfer::backup::backup(env.local(), &o).expect("backup");
    let rs = receipts(&env.p("r.jsonl"));
    for m in &sub {
        let rel = m.trim_start_matches("/dev/");
        let inside = format!("{rel}/");
        assert!(
            !rs.iter()
                .any(|r| r.path.as_deref().is_some_and(|p| p.starts_with(&inside))),
            "{m} was crossed"
        );
        assert!(
            !rep.skipped.iter().any(|(p, _)| p.starts_with(&inside)),
            "{m} was walked"
        );
    }
    // a device is skipped with a reason, never receipted (NEG-4)
    assert!(!named(&rs, "null"));
    assert!(
        rep.skipped
            .iter()
            .any(|(p, why)| p == "null" && !why.is_empty()),
        "/dev/null not skipped: {:?}",
        rep.skipped
    );
}
