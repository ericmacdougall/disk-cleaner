//! AC-17: `verify --deep` re-proves a run from the bucket. It passes a good repository and fails,
//! naming the pack, when a pack byte is flipped or a pack is missing. A rotting store whose
//! recorded checksums still match (bit rot at rest) passes the shallow check and fails the deep
//! one, which is what makes the deep check worth its downloads.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use xfer::error::Result;
use xfer::hash::HashKind;
use xfer::transport::{RemoteObject, Transport};

const BIN: &str = env!("CARGO_BIN_EXE_prunedisk-xfer");

fn backed_up() -> (Env, String) {
    let env = Env::new();
    sample_tree(&env.p("src"));
    let rep =
        xfer::backup::backup(env.local(), &opts(&env.p("src"), &env.p("r.jsonl"))).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    (env, rep.run)
}

fn verify(t: Arc<dyn Transport>, run: &str, deep: bool) -> xfer::restore::VerifyReport {
    xfer::restore::verify(
        t,
        Some(run),
        deep,
        &budget(),
        xfer::repo::DEFAULT_MAX_OBJECTS,
    )
    .expect("verify")
}

/// The CLI's exit code and stdout for `verify --run RUN [--deep]` against the local store.
fn cli(store: &Path, run: &str, deep: bool) -> (i32, String) {
    let mut c = Command::new(BIN);
    c.args(["verify", "--repo"])
        .arg(format!("local:{}", store.display()))
        .args(["--mem-budget", "640Mi", "--max-pack", "16Mi", "--run", run]);
    if deep {
        c.arg("--deep");
    }
    let out = c.output().expect("run verify");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn packs(store: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![store.join("v1/packs")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read_dir").flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let key = p
                    .strip_prefix(store)
                    .expect("in store")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((key, p));
            }
        }
    }
    out.sort();
    assert!(!out.is_empty(), "no packs in the store");
    out
}

#[test]
fn a_good_repository_verifies_shallow_and_deep() {
    let (env, run) = backed_up();
    let shallow = verify(env.local(), &run, false);
    assert_eq!(shallow.exit_code, 0, "{shallow:#?}");
    let deep = verify(env.local(), &run, true);
    assert_eq!(deep.exit_code, 0, "{deep:#?}");
    assert!(deep.files >= 5 && deep.bytes > 12 << 20, "{deep:#?}");
    assert_eq!(cli(&env.p("store"), &run, true).0, 0);
}

#[test]
fn a_flipped_pack_byte_fails_naming_the_pack() {
    let (env, run) = backed_up();
    let (key, path) = packs(&env.p("store")).swap_remove(0);
    let mut b = std::fs::read(&path).expect("read pack");
    let mid = b.len() / 2;
    b[mid] ^= 0x40;
    std::fs::write(&path, &b).expect("write pack");
    let rep = verify(env.local(), &run, true);
    assert_eq!(rep.exit_code, 3, "{rep:#?}");
    assert!(rep.errors.iter().any(|e| e.contains(&key)), "{rep:#?}");
    let (code, out) = cli(&env.p("store"), &run, true);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains(&key), "{out}");
}

#[test]
fn a_missing_pack_fails_naming_the_pack() {
    let (env, run) = backed_up();
    let (key, path) = packs(&env.p("store")).swap_remove(0);
    std::fs::remove_file(&path).expect("rm pack");
    for deep in [false, true] {
        let rep = verify(env.local(), &run, deep);
        assert_eq!(rep.exit_code, 3, "{rep:#?}");
        assert!(rep.errors.iter().any(|e| e.contains(&key)), "{rep:#?}");
    }
    let (code, out) = cli(&env.p("store"), &run, true);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains(&key), "{out}");
}

/// A store whose recorded checksums are those of the good bytes while one pack's bytes have
/// rotted: list and stat report the original checksum, get returns flipped bytes.
struct Rotten {
    inner: Arc<dyn Transport>,
    key: String,
}

impl Transport for Rotten {
    fn hash_kind(&self) -> Option<HashKind> {
        self.inner.hash_kind()
    }
    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.inner.put(key, data)
    }
    fn stat(&self, key: &str) -> Result<Option<RemoteObject>> {
        self.inner.stat(key)
    }
    fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>> {
        self.inner.list(prefix)
    }
    fn get(&self, key: &str, max: u64) -> Result<Vec<u8>> {
        let mut b = self.inner.get(key, max)?;
        if key == self.key {
            let mid = b.len() / 2;
            b[mid] ^= 0x40;
        }
        Ok(b)
    }
    fn describe(&self) -> String {
        "rotten".into()
    }
}

#[test]
fn bit_rot_behind_a_matching_checksum_passes_shallow_and_fails_deep() {
    let (env, run) = backed_up();
    // the largest pack, so the flipped byte lands in chunk data
    let (key, _) = packs(&env.p("store"))
        .into_iter()
        .max_by_key(|(_, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .expect("a pack");
    let t: Arc<dyn Transport> = Arc::new(Rotten {
        inner: env.local(),
        key: key.clone(),
    });
    // the control: the shallow check trusts the store's checksum and cannot see the rot
    let shallow = verify(t.clone(), &run, false);
    assert_eq!(shallow.exit_code, 0, "{shallow:#?}");
    let deep = verify(t, &run, true);
    assert_eq!(deep.exit_code, 3, "{deep:#?}");
    assert!(deep.errors.iter().any(|e| e.contains(&key)), "{deep:#?}");
}
