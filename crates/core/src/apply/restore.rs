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
    /// Interrupted manifest uploads whose rows a later acting run stored again in committed
    /// segments, so they are skipped without loss.
    pub abandoned_segments: Vec<String>,
}

/// `<sha256> <size>`, as a `.sum` sidecar holds it.
fn parse_sum(text: &str) -> Option<(String, u64)> {
    let mut it = text.split_whitespace();
    let sha = it.next()?;
    let len = it.next()?.parse::<u64>().ok()?;
    if it.next().is_some() || sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((sha.to_ascii_lowercase(), len))
}

/// What a remote manifest segment is (Codex plan review r8, F1).
enum SegState {
    /// Its sidecar is well formed and matches: committed history (its rows and its bytes).
    Committed(Vec<Envelope>, Vec<u8>),
    /// Its sidecar is well formed and does not match: damaged committed history (rejected).
    Damaged(String),
    /// No well-formed sidecar: the run that uploaded it stopped before committing it. Its bytes,
    /// judged by [`covered`] against the committed segments.
    Interrupted(Vec<u8>),
}

/// Whether committed history holds every row of an interrupted upload (A-4). The upload's bytes
/// must be the start of a committed segment's text at a line boundary: its complete lines
/// exactly and in order, and a cut last line as the start of the line that follows them there.
/// A later acting run stores the rows of an interrupted upload again in that same order, in
/// one segment, so a segment whose rows were stored again always passes. An upload cut inside
/// its first line has nothing to anchor a cut line to and is never covered; an empty one holds
/// no rows and always is. The one reader decides this from storage alone: nothing written
/// beside the upload (or missing from beside it) changes the answer.
pub(crate) fn covered(orphan: &[u8], committed: &[Vec<u8>]) -> bool {
    if orphan.is_empty() {
        return true;
    }
    if !orphan.contains(&b'\n') {
        return false;
    }
    committed.iter().any(|t| {
        let mut start = 0;
        while start < t.len() {
            if t[start..].starts_with(orphan) {
                return true;
            }
            match t[start..].iter().position(|&b| b == b'\n') {
                Some(i) => start += i + 1,
                None => break,
            }
        }
        false
    })
}

fn segment_state(rc: &Rclone, dir: &str, name: &str, names: &BTreeSet<String>) -> SegState {
    let read = |n: &str| rc.cat(&format!("{dir}/{n}"));
    let sum_name = format!("{name}.sum");
    let sidecar = if names.contains(&sum_name) {
        match read(&sum_name) {
            Ok(b) => parse_sum(&String::from_utf8_lossy(&b)),
            Err(e) => return SegState::Damaged(format!("its checksum file cannot be read: {e}")),
        }
    } else {
        None
    };
    let b = match read(name) {
        Ok(b) => b,
        Err(e) => return SegState::Damaged(e),
    };
    let Some((want_sha, want_len)) = sidecar else {
        return SegState::Interrupted(b);
    };
    if b.len() as u64 != want_len {
        return SegState::Damaged(format!(
            "it is {} bytes, its checksum file says {want_len}",
            b.len()
        ));
    }
    if hex::encode(Sha256::digest(&b)) != want_sha {
        return SegState::Damaged("its sha256 differs from its checksum file".into());
    }
    let text = match std::str::from_utf8(&b) {
        Ok(t) => t,
        Err(_) => return SegState::Damaged("it is not UTF-8".into()),
    };
    match manifest::parse_segment(text) {
        Ok(rows) => SegState::Committed(rows, b),
        Err(e) => SegState::Damaged(e),
    }
}

/// The manifest history this host's label holds in storage.
#[derive(Default)]
pub(crate) struct RemoteManifest {
    /// Rows of every committed segment, merged.
    pub rows: Vec<Envelope>,
    /// Every line of every committed segment, as stored.
    pub lines: BTreeSet<String>,
    /// Segments that are not committed history and whose rows committed history does not hold:
    /// damaged ones, and interrupted ones not covered (name, why).
    pub rejected: Vec<(String, String)>,
    /// Interrupted uploads whose rows committed history holds ([`covered`]): skipped.
    pub abandoned: Vec<String>,
}

const INTERRUPTED: &str = "its upload was interrupted before its checksum file was written, and no committed segment holds its rows; the next acting apply run stores them again from the local manifest";

/// Every manifest segment in storage, classified; only committed segments give rows. Two
/// passes: committed history first, then each interrupted upload judged against it.
pub(crate) fn remote_manifest(rc: &Rclone, label: &str) -> Result<RemoteManifest, ApplyError> {
    let dir = format!("manifests/{label}");
    let mut names = rc.list(&dir).map_err(ApplyError::Storage)?;
    names.sort();
    let set: BTreeSet<String> = names.iter().cloned().collect();
    let mut good = Vec::new();
    let mut texts = Vec::new();
    let mut interrupted = Vec::new();
    let mut out = RemoteManifest::default();
    for n in names.iter().filter(|n| n.ends_with(".jsonl")) {
        match segment_state(rc, &dir, n, &set) {
            SegState::Committed(rows, b) => {
                good.push(rows);
                texts.push(b);
            }
            SegState::Damaged(why) => out.rejected.push((n.clone(), why)),
            SegState::Interrupted(b) => interrupted.push((n.clone(), b)),
        }
    }
    for (n, b) in interrupted {
        if covered(&b, &texts) {
            out.abandoned.push(n);
        } else {
            out.rejected.push((n, INTERRUPTED.into()));
        }
    }
    out.lines = texts
        .iter()
        .flat_map(|t| {
            String::from_utf8_lossy(t)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|l| !l.is_empty())
        .collect();
    out.rows = manifest::merge_segments(good);
    Ok(out)
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
    let mut abandoned_segments = Vec::new();
    let rows: Vec<Envelope> = if opts.from_remote {
        let m = remote_manifest(&rc, &opts.label)?;
        abandoned_segments = m.abandoned;
        let (rows, rejected) = (m.rows, m.rejected);
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
    // A HEAD-identical file restores from its git directory (I-1): when that directory is not on
    // disk and not part of this restore, its rows from the same run are restored first.
    let mut need: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for (run, row) in chosen.values() {
        if let Row::File {
            git_oid: Some(_),
            git_dir: Some(gd),
            ..
        } = row
        {
            if gd.symlink_metadata().is_err() {
                need.insert((run.clone(), gd.clone()));
            }
        }
    }
    for e in &rows {
        let needed = |path: &Path| {
            need.iter()
                .any(|(r, gd)| *r == e.run && path.starts_with(gd))
        };
        match &e.row {
            Row::File { path, .. } if needed(path) && !chosen.contains_key(path) => {
                chosen.insert(path.clone(), (e.run.clone(), e.row.clone()));
            }
            // Empty directories (refs/heads after pack-refs) make it a git directory too.
            Row::Dir { path, .. } if needed(path) => {
                dirs.insert(path.clone());
            }
            _ => {}
        }
    }
    // Duplicates removed as noise (N4): path -> (retained copy, sha256), per run.
    let mut retains: BTreeMap<(String, PathBuf), (PathBuf, String)> = BTreeMap::new();
    for e in &rows {
        if let Row::Retains {
            path,
            retained,
            sha256,
            ..
        } = &e.row
        {
            retains.insert(
                (e.run.clone(), path.clone()),
                (retained.clone(), sha256.clone()),
            );
        }
    }
    let regen_paths: Vec<PathBuf> = regen.iter().map(|(p, _)| p.clone()).collect();
    let mut rep = RestoreReport {
        regenerate: regen,
        rejected_segments,
        abandoned_segments,
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
        let _ = git_dir;
        if sha256.is_none() && link_target.is_none() {
            // Noise: rule noise is regenerated (listed above), never a failure.
            if regen_paths.iter().any(|r| path.starts_with(r)) {
                continue;
            }
            if let Some((retained, s)) = retains.get(&(run.clone(), path.clone())) {
                let res = restore_duplicate(
                    &rc,
                    &root,
                    path,
                    id.mode,
                    id.mtime_s,
                    id.mtime_ns,
                    retained,
                    s,
                    &blobs,
                );
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

/// A duplicate removed as noise: copied from its retained copy while that still has `sha256`
/// (re-hashed on the way), else from an archived blob with that sha256 (N4).
#[allow(clippy::too_many_arguments)]
fn restore_duplicate(
    rc: &Rclone,
    root: &rustix::fd::OwnedFd,
    path: &Path,
    mode: u32,
    ms: i64,
    mns: i64,
    retained: &Path,
    sha256: &str,
    blobs: &BTreeMap<String, (u64, Vec<Segment>)>,
) -> Result<One, String> {
    let rel = retained
        .strip_prefix("/")
        .map(|r| r.to_string_lossy().to_string())
        .unwrap_or_default();
    let from_copy = publish_bytes(root, path, mode, ms, mns, sha256, |w| {
        let (mut f, _) = fsops::open_file_rel(root, &rel).map_err(|e| e.to_string())?;
        std::io::copy(&mut f, w).map_err(|e| e.to_string())?;
        Ok(())
    });
    match from_copy {
        Ok(one) => Ok(one),
        Err(why) if blobs.contains_key(sha256) => {
            let _ = why;
            restore_one(rc, root, path, mode, ms, mns, Some(sha256), None, blobs)
        }
        Err(why) => Err(format!(
            "a duplicate of {}, which no longer has that content ({why}), and no archived copy exists",
            retained.display()
        )),
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

#[cfg(test)]
mod tests {
    use super::covered;

    fn seg(lines: &[&str]) -> Vec<u8> {
        let mut t = lines.join("\n");
        t.push('\n');
        t.into_bytes()
    }

    #[test]
    fn an_interrupted_upload_is_covered_only_by_its_rows_in_order() {
        let (r1, r2, r3, r40, r5) = (
            r#"{"r":"a","k":1}"#,
            r#"{"r":"a","k":2}"#,
            r#"{"r":"a","k":3}"#,
            r#"{"r":"a","k":40}"#,
            r#"{"r":"a","k":5}"#,
        );
        let committed = vec![seg(&[r1, r2]), seg(&[r3, r40, r5])];
        // Whole, at a line boundary, in order; and an empty upload holds no rows.
        assert!(covered(&seg(&[r3, r40]), &committed));
        assert!(covered(b"", &committed));
        // A cut last line: the start of the line that follows its complete lines there.
        assert!(covered(
            br#"{"r":"a","k":3}
{"r":"a","k":4"#,
            &committed
        ));
        // Cut a few bytes into a row stored nowhere, though it is the start of a stored row.
        assert!(!covered(
            br#"{"r":"a","k":1}
{"r":"a","k":4"#,
            &committed
        ));
        // Cut inside its first line: nothing to anchor the cut line to.
        assert!(!covered(br#"{"r":"a","k":4"#, &committed));
        // A row stored nowhere, rows out of order, rows across two segments, a mid-line start.
        assert!(!covered(&seg(&[r3, r#"{"r":"a","k":6}"#]), &committed));
        assert!(!covered(&seg(&[r2, r1]), &committed));
        assert!(!covered(&seg(&[r2, r3]), &committed));
        assert!(!covered(b"\"k\":2}\n", &committed));
        assert!(!covered(&seg(&[r1]), &[]));
    }
}
