//! Restore a removed path byte for byte from storage (DESIGN 7; spec I-6). Works offline from
//! the manifest (local, or rebuilt from its mirrored segments) and the shards alone. Never
//! overwrites: every file is written to a temp name in its directory, re-hashed, fsynced and
//! published with `linkat`, which fails if the target exists. No symlink is followed on the way.

use super::fsops::{self, Published};
use super::manifest::{self, Envelope, Manifest, Row, Segment};
use super::storage::Rclone;
use super::ApplyError;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Options for a restore.
pub struct RestoreOptions {
    /// With `from_remote`: restore from the segments that pass their checks even when some do
    /// not (they are listed in the report). Without it any failing segment refuses the restore.
    pub accept_partial: bool,
    pub remote: String,
    pub rclone: PathBuf,
    pub git: PathBuf,
    /// Local manifest; ignored with `from_remote`.
    pub manifest: PathBuf,
    /// Rebuild the manifest from the mirrored segments in storage.
    pub from_remote: bool,
    pub label: String,
    /// Restore this run's generation instead of the newest.
    pub run: Option<String>,
    pub timeout: Duration,
}

/// What a restore did.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RestoreReport {
    pub restored: Vec<PathBuf>,
    pub identical: Vec<PathBuf>,
    pub conflicts: Vec<PathBuf>,
    pub regenerate: Vec<(PathBuf, String)>,
    pub failed: Vec<(PathBuf, String)>,
    /// Remote manifest segments that failed their checks (only with `accept_partial`).
    pub rejected_segments: Vec<(String, String)>,
}

/// One segment, checked against its `.sum` sidecar (`<sha256> <size>`), then parsed strictly.
fn verified_segment(rc: &Rclone, dir: &str, name: &str) -> Result<Vec<Envelope>, String> {
    let sum = rc
        .cat(&format!("{dir}/{name}.sum"))
        .map_err(|e| format!("its checksum file cannot be read: {e}"))?;
    let sum = String::from_utf8_lossy(&sum).to_string();
    let mut it = sum.split_whitespace();
    let (Some(want_sha), Some(want_len)) =
        (it.next(), it.next().and_then(|s| s.parse::<u64>().ok()))
    else {
        return Err("its checksum file is malformed".into());
    };
    let b = rc.cat(&format!("{dir}/{name}"))?;
    if b.len() as u64 != want_len {
        return Err(format!(
            "it is {} bytes, its checksum file says {want_len}",
            b.len()
        ));
    }
    if hex::encode(Sha256::digest(&b)) != want_sha {
        return Err("its sha256 differs from its checksum file".into());
    }
    let text = String::from_utf8(b).map_err(|_| "it is not UTF-8".to_string())?;
    manifest::parse_segment(&text)
}

/// Rows from verified segments, and the segments that failed their checks (name, why).
pub(crate) type RemoteRows = (Vec<Envelope>, Vec<(String, String)>);

/// The mirrored manifest rows in storage, every segment verified. Returns the rows and the
/// segments that failed, with why.
pub(crate) fn rows_from_remote(rc: &Rclone, label: &str) -> Result<RemoteRows, ApplyError> {
    let dir = format!("manifests/{label}");
    let mut names = rc.list(&dir).map_err(ApplyError::Storage)?;
    names.sort();
    let mut good = Vec::new();
    let mut rejected = Vec::new();
    for n in names.iter().filter(|n| n.ends_with(".jsonl")) {
        match verified_segment(rc, &dir, n) {
            Ok(rows) => good.push(rows),
            Err(why) => rejected.push((n.clone(), why)),
        }
    }
    Ok((manifest::merge_segments(good), rejected))
}

fn run_start(run: &str) -> i64 {
    run.split('-')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Restore `target` (a removed file or a directory that held removed entries).
pub fn restore(target: &Path, opts: &RestoreOptions) -> Result<RestoreReport, ApplyError> {
    let rc = Rclone {
        bin: opts.rclone.clone(),
        root: opts.remote.clone(),
        timeout: opts.timeout,
    };
    let mut rejected_segments = Vec::new();
    let rows: Vec<Envelope> = if opts.from_remote {
        let (rows, rejected) = rows_from_remote(&rc, &opts.label)?;
        if !rejected.is_empty() && !opts.accept_partial {
            let list: Vec<String> = rejected.iter().map(|(n, w)| format!("{n}: {w}")).collect();
            return Err(ApplyError::Manifest(format!(
                "{} manifest segment(s) in storage failed their checks, so the history may be incomplete ({}); pass --accept-partial to restore from the segments that pass",
                rejected.len(),
                list.join("; ")
            )));
        }
        rejected_segments = rejected;
        rows
    } else {
        Manifest::load(&opts.manifest, "restore")?.rows
    };
    // Pieces begun per run, and blobs by sha256 for this remote.
    let mut begun: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    let mut blobs: BTreeMap<String, (u64, Vec<Segment>)> = BTreeMap::new();
    for e in &rows {
        match &e.row {
            Row::RemoveBegin { path, .. } => {
                begun.insert((e.run.clone(), path.clone()));
            }
            Row::Blob {
                remote,
                sha256,
                size,
                segments,
            } if *remote == opts.remote => {
                blobs.insert(sha256.clone(), (*size, segments.clone()));
            }
            _ => {}
        }
    }
    let piece_of: BTreeMap<(String, usize), PathBuf> = rows
        .iter()
        .filter_map(|e| match &e.row {
            Row::RemoveBegin {
                plan_piece, path, ..
            } => Some(((e.run.clone(), *plan_piece), path.clone())),
            _ => None,
        })
        .collect();
    // Newest generation per path (or the named run).
    let mut chosen: BTreeMap<PathBuf, (String, Row)> = BTreeMap::new();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut regen: Vec<(PathBuf, String)> = Vec::new();
    for e in &rows {
        if let Some(r) = &opts.run {
            if &e.run != r {
                continue;
            }
        }
        match &e.row {
            Row::File {
                path, plan_piece, ..
            } if path == target || path.starts_with(target) => {
                let Some(ppath) = piece_of.get(&(e.run.clone(), *plan_piece)) else {
                    continue;
                };
                if !begun.contains(&(e.run.clone(), ppath.clone())) {
                    continue;
                }
                let newer = chosen
                    .get(path)
                    .is_none_or(|(r, _)| run_start(&e.run) >= run_start(r));
                if newer {
                    chosen.insert(path.clone(), (e.run.clone(), e.row.clone()));
                }
            }
            Row::Dir { path, .. } if path == target || path.starts_with(target) => {
                dirs.insert(path.clone());
            }
            Row::Regenerable { path, rule, .. }
                if path == target || path.starts_with(target) || target.starts_with(path) =>
            {
                regen.push((path.clone(), rule.clone()));
            }
            _ => {}
        }
    }
    let mut rep = RestoreReport {
        regenerate: regen,
        rejected_segments,
        ..Default::default()
    };
    let root = fsops::open_dir_abs(Path::new("/")).map_err(|e| ApplyError::Io(e.to_string()))?;
    for d in &dirs {
        let rel = d
            .strip_prefix("/")
            .unwrap_or(d)
            .to_string_lossy()
            .to_string();
        if let Err(e) = fsops::mkdirs_rel(&root, &rel, 0o755) {
            rep.failed
                .push((d.clone(), format!("cannot create directory: {e}")));
        }
    }
    let mut git_later = Vec::new();
    // Hard links (F8): names of one inode removed together are restored as one inode again. The
    // first name is restored from storage, the others are linked to it.
    let mut first_of_group: BTreeMap<(String, String), PathBuf> = BTreeMap::new();
    for (path, (run, row)) in &chosen {
        let Row::File {
            id,
            sha256,
            git_oid,
            git_dir,
            link_target,
            link_group,
            ..
        } = row
        else {
            continue;
        };
        if git_oid.is_some() {
            git_later.push((path.clone(), row.clone()));
            continue;
        }
        if let (Some(g), Some(s)) = (link_group, sha256) {
            let key = (run.clone(), g.clone());
            if let Some(first) = first_of_group.get(&key) {
                let res = link_to(&root, first, path, s);
                record(&mut rep, path, res);
                continue;
            }
        }
        let res = restore_one(
            &rc,
            &root,
            path,
            id.mode,
            id.mtime_s,
            id.mtime_ns,
            sha256.as_deref(),
            link_target.as_deref(),
            &blobs,
        );
        let _ = git_dir;
        let ok = matches!(res, Ok(One::Restored) | Ok(One::Identical));
        if let (true, Some(g)) = (ok, link_group) {
            first_of_group
                .entry((run.clone(), g.clone()))
                .or_insert_with(|| path.clone());
        }
        record(&mut rep, path, res);
    }
    for (path, row) in git_later {
        let Row::File {
            id,
            sha256,
            git_oid,
            git_dir,
            ..
        } = &row
        else {
            continue;
        };
        let res = restore_git_blob(
            opts,
            &root,
            &path,
            id.mode,
            id.mtime_s,
            id.mtime_ns,
            git_oid.as_deref().unwrap_or(""),
            git_dir.as_deref(),
            sha256.as_deref(),
        );
        record(&mut rep, &path, res);
    }
    Ok(rep)
}

enum One {
    Restored,
    Identical,
    Conflict,
}

/// Restore `path` as a hard link to `first` (already restored with content `sha256`).
fn link_to(
    root: &rustix::fd::OwnedFd,
    first: &Path,
    path: &Path,
    sha256: &str,
) -> Result<One, String> {
    let (fprel, fleaf) = split(first)?;
    let (prel, leaf) = split(path)?;
    let fdir = fsops::mkdirs_rel(root, &fprel, 0o755).map_err(|e| e.to_string())?;
    let dir = fsops::mkdirs_rel(root, &prel, 0o755).map_err(|e| e.to_string())?;
    match fsops::publish_link(&fdir, &fleaf, &dir, &leaf).map_err(|e| e.to_string())? {
        Published::Created => Ok(One::Restored),
        Published::Exists => match existing_sha(&dir, &leaf) {
            Some(s) if s == sha256 => Ok(One::Identical),
            _ => Ok(One::Conflict),
        },
    }
}

fn record(rep: &mut RestoreReport, path: &Path, res: Result<One, String>) {
    match res {
        Ok(One::Restored) => rep.restored.push(path.to_path_buf()),
        Ok(One::Identical) => rep.identical.push(path.to_path_buf()),
        Ok(One::Conflict) => rep.conflicts.push(path.to_path_buf()),
        Err(e) => rep.failed.push((path.to_path_buf(), e)),
    }
}

fn split(path: &Path) -> Result<(String, String), String> {
    let parent = path.parent().ok_or("no parent")?;
    let leaf = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("name is not UTF-8")?;
    let prel = parent
        .strip_prefix("/")
        .unwrap_or(parent)
        .to_string_lossy()
        .to_string();
    Ok((prel, leaf.to_string()))
}

fn existing_sha(dir: &rustix::fd::OwnedFd, leaf: &str) -> Option<String> {
    let (mut f, _) = fsops::open_file_rel(dir, leaf).ok()?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut HashWriter(&mut h)).ok()?;
    Some(hex::encode(h.finalize()))
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_bytes<F>(
    root: &rustix::fd::OwnedFd,
    path: &Path,
    mode: u32,
    ms: i64,
    mns: i64,
    sha256: &str,
    produce: F,
) -> Result<One, String>
where
    F: FnOnce(&mut dyn Write) -> Result<(), String>,
{
    let (prel, leaf) = split(path)?;
    let dir = fsops::mkdirs_rel(root, &prel, 0o755)
        .map_err(|e| format!("cannot open the target directory without following links: {e}"))?;
    let mut produce = Some(produce);
    let res = fsops::publish_file(&dir, &leaf, mode, |f| {
        let mut h = Sha256::new();
        struct Tee<'a, 'b>(&'a mut std::fs::File, &'b mut Sha256);
        impl Write for Tee<'_, '_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.1.update(b);
                self.0.write_all(b)?;
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }
        let p = produce.take().expect("called once");
        p(&mut Tee(f, &mut h)).map_err(std::io::Error::other)?;
        if hex::encode(h.finalize()) != sha256 {
            return Err(std::io::Error::other(
                "the restored bytes do not match the recorded sha256",
            ));
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    match res {
        Published::Created => {
            let _ = fsops::set_mtime(&dir, &leaf, ms, mns);
            Ok(One::Restored)
        }
        Published::Exists => {
            if existing_sha(&dir, &leaf).as_deref() == Some(sha256) {
                Ok(One::Identical)
            } else {
                Ok(One::Conflict)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn restore_one(
    rc: &Rclone,
    root: &rustix::fd::OwnedFd,
    path: &Path,
    mode: u32,
    ms: i64,
    mns: i64,
    sha256: Option<&str>,
    link_target: Option<&str>,
    blobs: &BTreeMap<String, (u64, Vec<Segment>)>,
) -> Result<One, String> {
    if let Some(t) = link_target {
        let (prel, leaf) = split(path)?;
        let dir = fsops::mkdirs_rel(root, &prel, 0o755).map_err(|e| e.to_string())?;
        return match fsops::publish_symlink(&dir, &leaf, t.as_bytes()).map_err(|e| e.to_string())? {
            Published::Created => Ok(One::Restored),
            Published::Exists => match fsops::readlink_rel(&dir, &leaf) {
                Ok(cur) if cur == t.as_bytes() => Ok(One::Identical),
                _ => Ok(One::Conflict),
            },
        };
    }
    let sha = sha256.ok_or("no content is recorded for this entry (noise: regenerate it)")?;
    let (size, segs) = blobs
        .get(sha)
        .ok_or("its blob is not in the manifest for this remote")?
        .clone();
    publish_bytes(root, path, mode, ms, mns, sha, move |w| {
        let mut n = 0u64;
        for s in &segs {
            let b = rc.cat_range(&s.shard, s.offset, s.len)?;
            w.write_all(&b).map_err(|e| e.to_string())?;
            n += b.len() as u64;
        }
        if n != size {
            return Err(format!("read {n} bytes, expected {size}"));
        }
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn restore_git_blob(
    opts: &RestoreOptions,
    root: &rustix::fd::OwnedFd,
    path: &Path,
    mode: u32,
    ms: i64,
    mns: i64,
    oid: &str,
    git_dir: Option<&Path>,
    sha256: Option<&str>,
) -> Result<One, String> {
    let gd = git_dir.ok_or("no git directory recorded")?;
    let sha = sha256.ok_or("no sha256 recorded")?;
    let out = super::gitlane::cat_blob(&opts.git, gd, oid)?;
    publish_bytes(root, path, mode, ms, mns, sha, move |w| {
        w.write_all(&out).map_err(|e| e.to_string())
    })
}
