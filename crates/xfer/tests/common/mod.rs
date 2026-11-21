#![allow(dead_code)]
//! Shared helpers for the integration tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use xfer::backup::Options;
use xfer::hash::HashKind;
use xfer::repo::{Budget, MIB};
use xfer::transport::local::LocalDir;
use xfer::transport::{Metered, RetryPolicy, Transport};

/// The rclone binary for tests that need one. `XFER_TEST_RCLONE` names it. When
/// `XFER_REQUIRE_RCLONE=1` (set by the t2-dev runner) a missing binary fails the test instead of
/// skipping it, so a mis-set environment can never pass silently.
pub fn rclone() -> Option<String> {
    match std::env::var("XFER_TEST_RCLONE") {
        Ok(b) if !b.is_empty() && std::path::Path::new(&b).exists() => Some(b),
        _ => {
            if std::env::var("XFER_REQUIRE_RCLONE").as_deref() == Ok("1") {
                panic!("XFER_REQUIRE_RCLONE=1 but XFER_TEST_RCLONE does not name an rclone binary");
            }
            eprintln!("skipping: XFER_TEST_RCLONE is not set");
            None
        }
    }
}

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

pub struct Env {
    pub dir: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Env {
        Env {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }
    pub fn p(&self, s: &str) -> PathBuf {
        self.dir.path().join(s)
    }
    /// A local "object store" with sha1 checksums, wrapped in request counters.
    pub fn store(&self) -> (Arc<dyn Transport>, Arc<xfer::transport::Counters>) {
        let l = LocalDir::new(self.p("store"), HashKind::Sha1).expect("store");
        let m = Metered::new(Arc::new(l));
        let c = m.counters.clone();
        (Arc::new(m), c)
    }
    pub fn local(&self) -> Arc<dyn Transport> {
        Arc::new(LocalDir::new(self.p("store"), HashKind::Sha1).expect("store"))
    }
}

pub fn budget() -> Budget {
    Budget::compute(640 * MIB, 16 * MIB, 4).expect("budget")
}

pub fn opts(src: &Path, receipts: &Path) -> Options {
    let mut o = Options::new("local:test", src, budget());
    o.receipts = Some(receipts.to_path_buf());
    o.policy = RetryPolicy::fast();
    o.commit_every = Duration::from_millis(300);
    o.epoch = Duration::from_millis(200);
    o.readers = 3;
    o.compressors = 2;
    o.root_label = "src".into();
    o
}

pub fn write(p: &Path, data: &[u8]) {
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, data).expect("write");
}

pub fn rand_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    xfer::gen::Rng::new(seed).fill(&mut v);
    v
}

/// A tree with every kind the engine handles.
pub fn sample_tree(root: &Path) {
    write(&root.join("a/small.txt"), b"hello world\n");
    write(&root.join("a/empty"), b"");
    write(&root.join("b/big.bin"), &rand_bytes(1, 6 << 20));
    write(&root.join("b/copy.bin"), &rand_bytes(1, 6 << 20));
    let mut text = Vec::new();
    for i in 0..20000 {
        text.extend_from_slice(format!("line {i} of a compressible log file\n").as_bytes());
    }
    write(&root.join("c/log.txt"), &text);
    std::fs::create_dir_all(root.join("d/empty-dir")).expect("mkdir");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        std::os::unix::fs::symlink("../a/small.txt", root.join("c/link")).expect("symlink");
        std::os::unix::fs::symlink("nowhere", root.join("c/dangling")).expect("symlink");
        std::fs::hard_link(root.join("a/small.txt"), root.join("d/hard.txt")).expect("hardlink");
        let name = std::ffi::OsStr::from_bytes(b"e/caf\xe9-latin1.txt");
        write(&root.join(name), b"non-utf8 name");
        std::fs::set_permissions(
            root.join("a/small.txt"),
            std::fs::Permissions::from_mode(0o640),
        )
        .expect("chmod");
        xfer::meta::set_mtime(&root.join("c/log.txt"), 1_600_000_000_123_456_789).expect("mtime");
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct M {
    pub kind: &'static str,
    pub mode: u32,
    pub mtime_ns: i64,
    pub content: String,
    pub link_group: Option<u64>,
}

/// Everything AC-1 compares, keyed by relative path. Hard-link groups are numbered by first
/// appearance so two trees compare equal when their grouping is the same.
pub fn manifest(root: &Path) -> BTreeMap<Vec<u8>, M> {
    let mut out = BTreeMap::new();
    let mut groups: BTreeMap<(u64, u64), u64> = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut ents: Vec<_> = std::fs::read_dir(&d)
            .expect("read_dir")
            .map(|e| e.expect("entry").path())
            .collect();
        ents.sort();
        for p in ents {
            let md = std::fs::symlink_metadata(&p).expect("lstat");
            let st = xfer::meta::stat_of(&md);
            #[cfg(unix)]
            let rel = {
                use std::os::unix::ffi::OsStrExt;
                p.strip_prefix(root)
                    .expect("rel")
                    .as_os_str()
                    .as_bytes()
                    .to_vec()
            };
            #[cfg(not(unix))]
            let rel = p
                .strip_prefix(root)
                .expect("rel")
                .to_string_lossy()
                .replace('\\', "/")
                .into_bytes();
            let (kind, content) = if md.file_type().is_symlink() {
                (
                    "symlink",
                    std::fs::read_link(&p)
                        .expect("readlink")
                        .to_string_lossy()
                        .into_owned(),
                )
            } else if md.is_dir() {
                stack.push(p.clone());
                ("dir", String::new())
            } else {
                let b = std::fs::read(&p).expect("read");
                ("file", blake3::hash(&b).to_hex().to_string())
            };
            let link_group = if kind == "file" && st.id.nlink > 1 {
                let n = groups.len() as u64;
                Some(*groups.entry((st.id.dev, st.id.ino)).or_insert(n))
            } else {
                None
            };
            let mtime_ns = if kind == "dir" { 0 } else { st.id.mtime_ns };
            out.insert(
                rel,
                M {
                    kind,
                    mode: if kind == "symlink" { 0 } else { st.mode },
                    mtime_ns,
                    content,
                    link_group,
                },
            );
        }
    }
    out
}

pub fn restore_opts(target: &Path, run: Option<String>) -> xfer::restore::RestoreOpts {
    xfer::restore::RestoreOpts {
        target: target.to_path_buf(),
        run,
        keep_setid: false,
        budget: budget(),
        max_objects: xfer::repo::DEFAULT_MAX_OBJECTS,
        policy: RetryPolicy::fast(),
    }
}

pub fn receipts(p: &Path) -> Vec<xfer::receipt::Receipt> {
    if !p.exists() {
        return Vec::new();
    }
    xfer::receipt::read_receipts(p).expect("receipts")
}

/// Concatenated put bytes cannot be inspected through Metered; LocalDir keeps every object, so
/// tests search the store directory for content instead.
pub fn store_contains(store: &Path, needle: &[u8]) -> bool {
    let mut stack = vec![store.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(b) = std::fs::read(&p) {
                if b.windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
                // packs may compress the needle: decode every segment and search raw bytes
                if let Ok(t) = xfer::pack::parse_pack(&b) {
                    for s in &t.segments {
                        if let Ok(raw) = xfer::pack::decode_segment(&b, s) {
                            if raw.windows(needle.len()).any(|w| w == needle) {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}
