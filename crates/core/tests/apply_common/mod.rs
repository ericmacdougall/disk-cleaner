//! Shared fixtures for the apply tests: a temp tree, a local-directory rclone remote, a
//! per-test fake rclone (failure injection without process-global environment), and scan/plan
//! documents built with the same fingerprint the scan writes.
#![allow(dead_code)]

use disk_cleaner_core::apply::fsops::Ident;
use disk_cleaner_core::apply::input::PieceKind;
use disk_cleaner_core::apply::recheck::{enumerate, fingerprint};
use disk_cleaner_core::apply::{ApplyOptions, PlanOracle};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub struct Fx {
    pub tmp: tempfile::TempDir,
    pub data: PathBuf,
    pub remote: PathBuf,
    pub state: PathBuf,
    pub fake: PathBuf,
    pub fake_dir: PathBuf,
}

pub fn real_rclone() -> PathBuf {
    for c in [
        "/usr/local/bin/rclone",
        "/usr/bin/rclone",
        "/opt/homebrew/bin/rclone",
    ] {
        if Path::new(c).exists() {
            return PathBuf::from(c);
        }
    }
    PathBuf::from("rclone")
}

impl Fx {
    pub fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let data = base.join("data");
        let remote = base.join("remote");
        let state = base.join("state");
        let fake_dir = base.join("fake");
        for d in [&data, &remote, &state, &fake_dir] {
            fs::create_dir_all(d).unwrap();
        }
        let fake = fake_dir.join("rclone");
        let fx = Fx {
            tmp,
            data,
            remote,
            state,
            fake,
            fake_dir,
        };
        fx.set_fake("");
        fx
    }

    /// Write the fake rclone. `extra` is bash run before delegating; it may `exit`.
    pub fn set_fake(&self, extra: &str) {
        let script = format!(
            "#!/bin/bash\nD={d}\necho \"$@\" >> $D/log\nREAL={real}\ncmd=\"$1\"\nif [ \"$cmd\" = rcat ]; then\n  exec 9>$D/lock; flock 9; n=$(( $(cat $D/rcat_count 2>/dev/null || echo 0) + 1 )); echo $n > $D/rcat_count; flock -u 9\nfi\n{extra}\nif [ \"$cmd\" = rcat ]; then s=$(date +%s.%N); \"$REAL\" \"$@\"; rc=$?; echo \"$s $(date +%s.%N)\" >> $D/intervals; exit $rc; fi\nexec \"$REAL\" \"$@\"\n",
            d = self.fake_dir.display(),
            real = real_rclone().display(),
        );
        fs::write(&self.fake, script).unwrap();
        fs::set_permissions(&self.fake, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn log(&self) -> String {
        fs::read_to_string(self.fake_dir.join("log")).unwrap_or_default()
    }

    pub fn manifest(&self) -> PathBuf {
        self.state.join("manifest.jsonl")
    }

    pub fn opts(&self, act: bool) -> ApplyOptions {
        let mut o = ApplyOptions::new(
            self.remote.to_string_lossy().to_string(),
            self.manifest(),
            self.state.join("lock"),
            now(),
        );
        o.remote_local = Some(self.remote.clone());
        o.rclone = self.fake.clone();
        o.act = act;
        o.jobs = 2;
        o.shard_bytes = 1 << 20;
        o.label = "testhost".into();
        o.home = Some(PathBuf::from("/nonexistent-home"));
        o.process = Box::new(disk_cleaner_core::ProcessCheck::none_running);
        o
    }

    pub fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = self.data.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }

    /// Every shard byte in the remote, concatenated (for canary greps).
    pub fn all_shard_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut stack = vec![self.remote.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    out.extend(fs::read(e.path()).unwrap());
                }
            }
        }
        out
    }

    pub fn shard_total(&self) -> u64 {
        let d = self.remote.join("shards");
        let mut total = 0;
        let mut stack = vec![d];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    total += e.metadata().unwrap().len();
                }
            }
        }
        total
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A piece specification for building documents.
#[derive(Clone)]
pub struct P {
    pub kind: PieceKind,
    pub path: PathBuf,
    pub files: Vec<String>,
    pub excludes: Vec<String>,
    pub lane: &'static str,
    pub reason: String,
    pub evidence: Option<Value>,
    pub git: Option<Value>,
    pub secret: Option<String>,
    pub scan_piece: Option<usize>,
}

impl P {
    pub fn tree(path: &Path, lane: &'static str) -> P {
        P {
            kind: PieceKind::Tree,
            path: path.to_path_buf(),
            files: vec![],
            excludes: vec![],
            lane,
            reason: format!("{lane} by test"),
            evidence: None,
            git: None,
            secret: None,
            scan_piece: None,
        }
    }
    pub fn files(dir: &Path, names: &[&str], lane: &'static str) -> P {
        P {
            kind: PieceKind::Files,
            files: names.iter().map(|s| s.to_string()).collect(),
            ..P::tree(dir, lane)
        }
    }
}

fn kind_str(k: PieceKind) -> &'static str {
    match k {
        PieceKind::Tree => "tree",
        PieceKind::Files => "files",
        PieceKind::GitNode => "git-node",
    }
}

/// Build (scan, plan) texts. A piece with `scan_piece: Some(k)` is a derived plan piece of scan
/// piece k (it gets no scan piece of its own). Every scan piece is fingerprinted now, with
/// `walked_at` after every ctime.
pub fn docs(roots: &[&Path], pieces: &[P]) -> (String, String) {
    let mut scan_pieces = Vec::new();
    let mut plan_pieces = Vec::new();
    let mut max_ctime = 0i64;
    let mut fps = Vec::new();
    let mut scan_index: BTreeMap<usize, usize> = BTreeMap::new();
    for (i, p) in pieces.iter().enumerate() {
        if p.scan_piece.is_some() {
            continue;
        }
        let en = enumerate(p.kind, &p.path, &p.files, &p.excludes).unwrap();
        for m in &en.members {
            max_ctime = max_ctime.max(m.id.ctime_s);
        }
        let members: Vec<(String, Ident)> =
            en.members.iter().map(|m| (m.rel.clone(), m.id)).collect();
        fps.push(fingerprint(members.iter().map(|(r, i)| (r.as_str(), i))));
        scan_index.insert(i, scan_pieces.len());
        scan_pieces.push(i);
    }
    let walked = max_ctime.max(now()) + 1;
    let scan_json: Vec<Value> = scan_pieces
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let p = &pieces[i];
            let mut v = json!({
                "id": k, "kind": kind_str(p.kind), "path": p.path, "files": p.files,
                "excludes": p.excludes, "walked_at": walked, "fingerprint": fps[k],
            });
            if let Some(s) = &p.secret {
                v["secret"] = json!(s);
            }
            v
        })
        .collect();
    for (i, p) in pieces.iter().enumerate() {
        let sp = p.scan_piece.unwrap_or_else(|| scan_index[&i]);
        let mut v = json!({
            "id": i, "scan_piece": sp, "kind": kind_str(p.kind), "path": p.path,
            "files": p.files, "excludes": p.excludes, "lane": p.lane, "reason": p.reason,
            "entries": 0, "bytes": 0,
        });
        if p.scan_piece.is_some() {
            v["derived"] = json!(true);
        }
        if let Some(e) = &p.evidence {
            v["evidence"] = e.clone();
        }
        if let Some(g) = &p.git {
            v["git"] = g.clone();
        }
        if let Some(s) = &p.secret {
            v["secret"] = json!(s);
        }
        plan_pieces.push(v);
    }
    let scan = json!({
        "schema_version": 16, "generated_at": walked, "roots": roots,
        "pieces": {"pieces": scan_json, "blobs": []},
    });
    let plan = json!({
        "schema_version": 16, "generated_at": walked, "scan_generated_at": walked,
        "active_window_days": 7, "active": [], "items": [],
        "lanes": {"version": 1, "pieces": plan_pieces, "blobs": []},
    });
    (scan.to_string(), plan.to_string())
}

/// An oracle that re-derives exactly the given plan (tests of noise and git lanes swap only the
/// transport; the comparison in apply is unchanged).
pub struct FixtureOracle(pub BTreeMap<usize, String>);

impl PlanOracle for FixtureOracle {
    fn rederive(&self, _s: &str, _p: &str) -> Option<BTreeMap<usize, String>> {
        Some(self.0.clone())
    }
}

/// The oracle that agrees with every noise and git piece of `plan`.
pub fn agreeing_oracle(plan: &str) -> FixtureOracle {
    let v: Value = serde_json::from_str(plan).unwrap();
    let pieces: Vec<disk_cleaner_core::apply::input::PlanPiece> =
        serde_json::from_value(v["lanes"]["pieces"].clone()).unwrap();
    FixtureOracle(
        pieces
            .iter()
            .map(|p| (p.id, disk_cleaner_core::apply::canonical(p)))
            .collect(),
    )
}

/// sha256 of a file, hex.
pub fn sha(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(fs::read(path).unwrap()))
}

/// A digest of a tree: relative path, kind, mode, content or link target.
pub fn tree_digest(dir: &Path) -> Vec<(String, String)> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd {
            let e = e.unwrap();
            let p = e.path();
            let md = p.symlink_metadata().unwrap();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().to_string();
            if md.file_type().is_symlink() {
                out.push((rel, format!("l:{}", fs::read_link(&p).unwrap().display())));
            } else if md.is_dir() {
                out.push((rel, "d".into()));
                stack.push(p);
            } else {
                out.push((rel, format!("f:{:o}:{}", md.mode() & 0o7777, sha(&p))));
            }
        }
    }
    out.sort();
    out
}

/// Bind a plan built separately to `scan` (same `scan_generated_at`), for derived-piece plans.
pub fn bind(scan: &str, plan: &str) -> String {
    let gen = serde_json::from_str::<Value>(scan).unwrap()["generated_at"].clone();
    let mut pv: Value = serde_json::from_str(plan).unwrap();
    pv["scan_generated_at"] = gen;
    pv.to_string()
}
