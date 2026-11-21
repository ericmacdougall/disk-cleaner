//! Shared fixtures for the apply tests: a temp tree, a local-directory rclone remote, a
//! per-test fake rclone (failure injection without process-global environment), and scan/plan
//! documents built with the same fingerprint the scan writes.
#![allow(dead_code)]

use disk_cleaner_core::apply::input::PieceKind;
use disk_cleaner_core::apply::recheck::enumerate;
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
    // Fail loudly instead of reporting every shard as failed (CI installs rclone).
    let on_path = std::process::Command::new("rclone")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        on_path,
        "the apply tests need rclone (a local directory remote, no network); install it"
    );
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
    /// Plan only: members moved out to derived pieces.
    pub moved_out: Vec<String>,
    pub lane: &'static str,
    pub reason: String,
    pub evidence: Option<Value>,
    pub git: Option<Value>,
    pub secret: Option<String>,
    /// A derived plan piece of scan piece k (its one member is `files[0]`).
    pub scan_piece: Option<usize>,
}

impl P {
    pub fn tree(path: &Path, lane: &'static str) -> P {
        P {
            kind: PieceKind::Tree,
            path: path.to_path_buf(),
            files: vec![],
            excludes: vec![],
            moved_out: vec![],
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
    /// A git-node piece of the git lane with this `git` block; `excludes` stay in place.
    pub fn git(dir: &Path, excludes: &[&str], git: Value) -> P {
        P {
            kind: PieceKind::GitNode,
            excludes: excludes.iter().map(|s| s.to_string()).collect(),
            git: Some(git),
            ..P::tree(dir, "git")
        }
    }
    /// A derived files piece of one member, cut from scan piece `k`.
    pub fn derived(dir: &Path, name: &str, k: usize, lane: &'static str) -> P {
        P {
            scan_piece: Some(k),
            ..P::files(dir, &[name], lane)
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

fn core_kind(k: PieceKind) -> disk_cleaner_core::PieceKind {
    match k {
        PieceKind::Tree => disk_cleaner_core::PieceKind::Tree,
        PieceKind::Files => disk_cleaner_core::PieceKind::Files,
        PieceKind::GitNode => disk_cleaner_core::PieceKind::GitNode,
    }
}

/// A scan document (schema 16, rev 5) with one scan piece per spec, in order, enumerated now with
/// `walked_at` after every ctime.
pub fn scan_doc(roots: &[&Path], specs: &[P]) -> String {
    use disk_cleaner_core::pieces::{Member, PieceSurvey, PieceTotals};
    let mut max_ctime = 0i64;
    let mut pieces = Vec::new();
    for (k, p) in specs.iter().enumerate() {
        let en = enumerate(p.kind, &p.path, &p.files, &p.excludes).unwrap();
        let members: Vec<Member> = en
            .members
            .iter()
            .map(|m| {
                max_ctime = max_ctime.max(m.id.ctime_s);
                let kind = match m.id.kind {
                    'f' => "f",
                    'l' => "l",
                    _ => "o",
                };
                Member(
                    m.rel.clone(),
                    kind.to_string(),
                    m.id.len,
                    m.id.mtime_s,
                    m.id.mtime_ns as u32,
                    m.id.dev,
                    m.id.ino,
                    m.id.len,
                )
            })
            .collect();
        pieces.push(disk_cleaner_core::Piece {
            id: k,
            kind: core_kind(p.kind),
            path: p.path.to_string_lossy().to_string(),
            entries: members.len() as u64,
            members,
            excludes: p.excludes.clone(),
            node: None,
            group: p.path.to_string_lossy().to_string(),
            bytes: 0,
            logical_bytes: 0,
            bytes_linked_outside: 0,
            newest_mtime: None,
            walked_at: 0,
            secret: p.secret.clone(),
            incomplete: None,
        });
    }
    let walked = max_ctime.max(now()) + 1;
    for pc in pieces.iter_mut() {
        pc.walked_at = walked;
    }
    let survey = PieceSurvey {
        options: Default::default(),
        pieces,
        blobs: vec![],
        totals: PieceTotals::default(),
    };
    json!({
        "schema_version": 16, "generated_at": walked, "roots": roots, "pieces": survey,
    })
    .to_string()
}

/// A plan document bound to `scan` (its `generated_at` and `scan_pieces_sha256`). Non-derived
/// specs map to scan pieces 0, 1, ... in order; a derived spec names its scan piece.
pub fn plan_doc(scan: &str, pieces: &[P]) -> String {
    let sv: Value = serde_json::from_str(scan).unwrap();
    let survey: disk_cleaner_core::PieceSurvey =
        serde_json::from_value(sv["pieces"].clone()).unwrap();
    let digest = disk_cleaner_core::pieces::pieces_digest(&survey);
    let mut next = 0usize;
    let mut plan_pieces = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        let sp = match p.scan_piece {
            Some(k) => k,
            None => {
                next += 1;
                next - 1
            }
        };
        let scan_excludes = survey
            .pieces
            .get(sp)
            .map(|x| x.excludes.clone())
            .unwrap_or_default();
        let mut v = json!({
            "id": i, "scan_piece": sp, "kind": kind_str(p.kind), "path": p.path,
            "group": p.path, "excludes": scan_excludes, "entries": 0, "bytes": 0,
            "logical_bytes": 0, "lane": p.lane, "reason": p.reason, "state": "idle",
            "state_reason": "", "reclaimable_bytes": 0, "upload_bytes": 0,
        });
        if p.scan_piece.is_some() {
            v["derived"] = json!(true);
            v["member"] = json!(p.files[0]);
        }
        if !p.moved_out.is_empty() {
            v["moved_out"] = json!(p.moved_out);
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
    json!({
        "schema_version": 16, "generated_at": sv["generated_at"], "scan_generated_at": sv["generated_at"],
        "active_window_days": 7, "active": [], "items": [],
        "lanes": {"version": 1, "scan_pieces_sha256": digest, "pieces": plan_pieces, "blobs": [],
                  "totals": {"by_lane": {}, "scanned_bytes": 0, "dir_overhead_bytes": 0,
                             "duplicate_bytes": 0, "dedup_saved_bytes": 0, "secret_pieces": 0}},
    })
    .to_string()
}

/// Build (scan, plan) texts: one scan piece per non-derived spec (in order), and a plan over all
/// specs.
pub fn docs(roots: &[&Path], pieces: &[P]) -> (String, String) {
    let specs: Vec<P> = pieces
        .iter()
        .filter(|p| p.scan_piece.is_none())
        .cloned()
        .collect();
    let scan = scan_doc(roots, &specs);
    let plan = plan_doc(&scan, pieces);
    (scan, plan)
}

/// After a test edited the scan document, re-bind the plan to it (digest and generated_at).
pub fn rebind(scan: &str, plan: &str) -> String {
    let sv: Value = serde_json::from_str(scan).unwrap();
    let survey: disk_cleaner_core::PieceSurvey =
        serde_json::from_value(sv["pieces"].clone()).unwrap();
    let mut pv: Value = serde_json::from_str(plan).unwrap();
    pv["scan_generated_at"] = sv["generated_at"].clone();
    pv["lanes"]["scan_pieces_sha256"] = json!(disk_cleaner_core::pieces::pieces_digest(&survey));
    pv.to_string()
}

/// An oracle that re-derives exactly the given plan (tests of noise and git lanes swap only the
/// transport; the comparison in apply is unchanged).
pub struct FixtureOracle(pub BTreeMap<usize, String>);

impl PlanOracle for FixtureOracle {
    fn rederive(&self, _s: &str, _p: &str) -> Option<BTreeMap<usize, String>> {
        Some(self.0.clone())
    }
}

/// The oracle that agrees with every noise and git piece of `plan` (canonical forms computed from
/// apply's own parse of the two documents, as the production oracle does).
pub fn agreeing_oracle(scan: &str, plan: &str) -> FixtureOracle {
    let inputs = disk_cleaner_core::apply::input::parse(scan, plan, None, None).unwrap();
    FixtureOracle(
        inputs
            .pieces
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
