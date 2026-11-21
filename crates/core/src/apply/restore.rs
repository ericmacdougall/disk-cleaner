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

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// A segment's `.sum` sidecar: `<sha256> <size>`, `name <segment>`, `end`, one per line. The
/// name binds the pair to its object name, so a pair copied over another name is damaged, and
/// the end line tells a sidecar cut mid-write (interrupted) from a whole one.
pub(crate) fn format_sidecar(name: &str, bytes: &[u8]) -> String {
    format!(
        "{} {}\nname {name}\nend\n",
        hex::encode(Sha256::digest(bytes)),
        bytes.len()
    )
}

fn parse_sidecar(text: &str) -> Option<(String, u64, String)> {
    let mut lines = text.strip_suffix('\n')?.split('\n');
    let mut first = lines.next()?.split(' ');
    let (sha, len) = (first.next()?, first.next()?.parse::<u64>().ok()?);
    let name = lines.next()?.strip_prefix("name ")?;
    if first.next().is_some() || !is_sha256(sha) || lines.next()? != "end" || lines.next().is_some()
    {
        return None;
    }
    Some((sha.to_string(), len, name.to_string()))
}

/// The two head slots (A-5). The head lists every committed segment of the label by name and
/// sha256, and every gap found earlier (`lost`). Writes alternate between the slots and carry a
/// generation, so a head cut mid-write leaves the other slot whole; the reader uses the
/// highest generation that parses.
pub(crate) const HEAD_SLOTS: [&str; 2] = ["head.0", "head.1"];

/// A parsed head, and also the writer's next head.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Head {
    /// `None`: no head that parses is stored.
    pub gen: Option<u64>,
    /// The id of the state directory that wrote it (A-5: one label, one state directory).
    pub state: Option<String>,
    pub segs: BTreeMap<String, String>,
    pub lost: Vec<String>,
}

/// A state directory id: 32 lowercase hex digits.
pub(crate) fn is_state_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub(crate) fn format_head(gen: u64, h: &Head) -> String {
    let mut t = format!("disk-cleaner manifest head\ngen {gen}\n");
    if let Some(s) = &h.state {
        t.push_str(&format!("state {s}\n"));
    }
    for (n, sha) in &h.segs {
        t.push_str(&format!("seg {n} {sha}\n"));
    }
    for l in &h.lost {
        let one: String = l
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        t.push_str(&format!("lost {one}\n"));
    }
    t.push_str("end\n");
    t
}

fn parse_head(text: &str) -> Option<Head> {
    let mut lines = text.strip_suffix('\n')?.split('\n');
    if lines.next()? != "disk-cleaner manifest head" {
        return None;
    }
    let gen = lines.next()?.strip_prefix("gen ")?.parse::<u64>().ok()?;
    let mut h = Head {
        gen: Some(gen),
        ..Default::default()
    };
    for l in lines.by_ref() {
        if l == "end" {
            return lines.next().is_none().then_some(h);
        }
        if let Some(rest) = l.strip_prefix("seg ") {
            let (n, sha) = rest.rsplit_once(' ')?;
            if !n.ends_with(".jsonl") || n.contains('/') || !is_sha256(sha) {
                return None;
            }
            h.segs.insert(n.to_string(), sha.to_string());
        } else if let Some(s) = l.strip_prefix("state ") {
            if h.state.is_some() || !is_state_id(s) {
                return None;
            }
            h.state = Some(s.to_string());
        } else {
            h.lost.push(l.strip_prefix("lost ")?.to_string());
        }
    }
    None
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

/// Read one listed manifest object. A failed read is a storage error that stops the caller,
/// never a damaged segment or a gap: a transient failure must not become permanent history.
fn read_listed(rc: &Rclone, dir: &str, name: &str) -> Result<Vec<u8>, ApplyError> {
    rc.cat(&format!("{dir}/{name}"))
        .map_err(|e| ApplyError::Storage(format!("cannot read {dir}/{name}: {e}")))
}

fn segment_state(
    rc: &Rclone,
    dir: &str,
    name: &str,
    names: &BTreeSet<String>,
) -> Result<SegState, ApplyError> {
    let sum_name = format!("{name}.sum");
    let sidecar = if names.contains(&sum_name) {
        parse_sidecar(&String::from_utf8_lossy(&read_listed(rc, dir, &sum_name)?))
    } else {
        None
    };
    let b = read_listed(rc, dir, name)?;
    let Some((want_sha, want_len, want_name)) = sidecar else {
        return Ok(SegState::Interrupted(b));
    };
    if want_name != name {
        return Ok(SegState::Damaged(format!(
            "its checksum file belongs to another segment, {want_name} (copied over it?)"
        )));
    }
    if b.len() as u64 != want_len {
        return Ok(SegState::Damaged(format!(
            "it is {} bytes, its checksum file says {want_len}",
            b.len()
        )));
    }
    if hex::encode(Sha256::digest(&b)) != want_sha {
        return Ok(SegState::Damaged(
            "its sha256 differs from its checksum file".into(),
        ));
    }
    let text = match std::str::from_utf8(&b) {
        Ok(t) => t,
        Err(_) => return Ok(SegState::Damaged("it is not UTF-8".into())),
    };
    Ok(match manifest::parse_segment(text) {
        Ok(rows) => SegState::Committed(rows, b),
        Err(e) => SegState::Damaged(e),
    })
}

/// The manifest history this host's label holds in storage.
#[derive(Default)]
pub(crate) struct RemoteManifest {
    /// Rows of every committed segment, merged.
    pub rows: Vec<Envelope>,
    /// Every line of every committed segment, as stored.
    pub lines: BTreeSet<String>,
    /// Segments that are not committed history and whose rows committed history does not hold,
    /// and history the head shows missing (name, why): every entry in `gaps`, plus interrupted
    /// uploads not covered (which the next acting run stores again).
    pub rejected: Vec<(String, String)>,
    /// History that no later run can bring back from the local manifest alone: damaged
    /// segments, segments the head lists that are missing, changed or not committed, gaps an
    /// earlier run recorded, and a missing head while segments exist.
    pub gaps: Vec<(String, String)>,
    /// Interrupted uploads whose rows committed history holds ([`covered`]): skipped.
    pub abandoned: Vec<String>,
    /// The head the next write must store (A-5): the stored head's entries, never dropped, plus
    /// every committed segment present, plus each new gap as a `lost` entry.
    pub next_head: Head,
    /// The commit order the next segment written gets: 1 + the highest present.
    pub next_order: u64,
}

const INTERRUPTED: &str = "its upload was interrupted before its checksum file was written, and no committed segment holds its rows; the next acting apply run stores them again from the local manifest";

/// The stored head: the slot with the highest generation that parses. The inner `Err` when
/// slots exist and none parses. A slot that cannot be read stops the caller (the outer `Err`):
/// a failed read never counts as a missing head, which would record a permanent gap.
fn read_head(
    rc: &Rclone,
    dir: &str,
    set: &BTreeSet<String>,
) -> Result<Result<Option<Head>, String>, ApplyError> {
    let mut best: Option<Head> = None;
    let mut present = 0;
    for slot in HEAD_SLOTS.iter().filter(|s| set.contains(**s)) {
        present += 1;
        let b = read_listed(rc, dir, slot)?;
        if let Some(h) = parse_head(&String::from_utf8_lossy(&b)) {
            if best.as_ref().is_none_or(|x| h.gen > x.gen) {
                best = Some(h);
            }
        }
    }
    Ok(match (best, present) {
        (Some(h), _) => Ok(Some(h)),
        (None, 0) => Ok(None),
        (None, _) => Err("no head slot parses".into()),
    })
}

/// The commit order of a segment named `o<10 digits>-<run>-<seq>.jsonl` (the writer gives each
/// new segment 1 + the highest order present). Run ids start with whole seconds, so they do not
/// order the runs of one second; names without the prefix (written before it) are order 0.
pub(crate) fn seg_order(name: &str) -> u64 {
    name.strip_prefix('o')
        .and_then(|r| r.split_once('-'))
        .filter(|(d, _)| d.len() == 10 && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|(d, _)| d.parse().ok())
        .unwrap_or(0)
}

/// The segment a `lost` line names: `<name>: <why>` where name is a segment file name.
fn lost_key(line: &str) -> Option<&str> {
    line.split_once(": ")
        .map(|(n, _)| n)
        .filter(|n| n.ends_with(".jsonl") && !n.contains(' ') && !n.contains('/'))
}

/// Whether every row of an interrupted upload is in `local` (the local manifest, one serialized
/// row per entry): each whole line is a local row, and a cut last line is the start of one.
fn held_locally(orphan: &[u8], local: &BTreeSet<String>) -> bool {
    let text = String::from_utf8_lossy(orphan);
    let (whole, tail) = match text.rfind('\n') {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => ("", &text[..]),
    };
    whole
        .lines()
        .filter(|l| !l.is_empty())
        .all(|l| local.contains(l))
        && (tail.is_empty() || local.iter().any(|l| l.starts_with(tail)))
}

const ORPHANED: &str = "its upload was interrupted and neither committed history nor the local manifest holds its rows (the local manifest was lost, replaced or taken over), so no run can store them again";

/// Every manifest segment in storage, classified; only committed segments give rows, in commit
/// order ([`seg_order`], then name). Two passes: committed history first, then each interrupted
/// upload judged against it, and against `local` when an acting run passes its local manifest.
/// Then the head: every segment it lists must be present, committed and byte for byte the one
/// listed. Every gap seen is made durable in `next_head`.
pub(crate) fn remote_manifest(
    rc: &Rclone,
    label: &str,
    local: Option<&BTreeSet<String>>,
) -> Result<RemoteManifest, ApplyError> {
    let dir = format!("manifests/{label}");
    let mut names = rc.list(&dir).map_err(ApplyError::Storage)?;
    names.sort();
    let set: BTreeSet<String> = names.iter().cloned().collect();
    let mut good = Vec::new();
    let mut texts = Vec::new();
    let mut committed: BTreeMap<String, String> = BTreeMap::new();
    let mut interrupted = Vec::new();
    let mut out = RemoteManifest::default();
    let segments: Vec<&String> = names.iter().filter(|n| n.ends_with(".jsonl")).collect();
    for n in &segments {
        match segment_state(rc, &dir, n, &set)? {
            SegState::Committed(rows, b) => {
                committed.insert(n.to_string(), hex::encode(Sha256::digest(&b)));
                good.push(rows);
                texts.push(b);
            }
            SegState::Damaged(why) => out.gaps.push((n.to_string(), why)),
            SegState::Interrupted(b) => interrupted.push((n.to_string(), b)),
        }
    }
    // The head (A-5).
    let (head, mut new_lost) = match read_head(rc, &dir, &set)? {
        Ok(Some(h)) => (h, Vec::new()),
        Ok(None) if segments.is_empty() => (Head::default(), Vec::new()),
        Ok(None) => (
            Head::default(),
            vec!["the storage head that lists the committed segments was missing while segments were stored, so segments missing before then cannot be named".to_string()],
        ),
        Err(_) if segments.is_empty() => (Head::default(), Vec::new()),
        Err(why) => (
            Head::default(),
            vec![format!("the storage head was damaged while segments were stored ({why}), so segments missing before then cannot be named")],
        ),
    };
    // Interrupted uploads. One that committed history holds is skipped (A-4). One the head
    // lists is a loss already named below. Otherwise its rows are stored nowhere else: the next
    // acting run stores them again while the local manifest holds them, and when it does not
    // (lost, replaced or taken over: `local` is what an acting run holds) they are a gap.
    let mut uncovered = Vec::new();
    for (n, b) in &interrupted {
        if head.segs.contains_key(n) {
            continue;
        }
        if covered(b, &texts) {
            out.abandoned.push(n.clone());
        } else if local.is_some_and(|l| !held_locally(b, l)) {
            out.gaps.push((n.clone(), ORPHANED.into()));
        } else {
            uncovered.push(n.clone());
        }
    }
    let damaged: BTreeSet<String> = out.gaps.iter().map(|(n, _)| n.clone()).collect();
    for (n, sha) in &head.segs {
        let why = match committed.get(n) {
            Some(have) if have == sha => continue,
            // Already named, with the reason.
            None if damaged.contains(n) => continue,
            Some(_) => "it differs from the copy the storage head lists (replaced?)",
            None if set.contains(n) => {
                "the storage head lists it as committed, but its checksum file is missing or cut"
            }
            None => "the storage head lists it, but it is missing from storage",
        };
        out.gaps.push((n.clone(), why.to_string()));
    }
    // What is seen now, by name, is reported once with today's reason; an earlier record of the
    // same segment is not repeated.
    let seen: BTreeSet<String> = out.gaps.iter().map(|(n, _)| n.clone()).collect();
    for l in &head.lost {
        if lost_key(l).is_some_and(|k| seen.contains(k)) {
            continue;
        }
        out.gaps.push((
            "history".into(),
            format!("an earlier apply run found a gap: {l}"),
        ));
    }
    for l in &new_lost {
        out.gaps.push(("history".into(), l.clone()));
    }
    let mut next = head.clone();
    for (n, sha) in &committed {
        next.segs.entry(n.clone()).or_insert_with(|| sha.clone());
    }
    next.lost.append(&mut new_lost);
    // The one place a gap becomes durable (Codex plan review r9b, F2): every gap seen is in the
    // next head. A segment the head lists stays listed, so its loss is found again at every
    // read; any other segment gap is recorded as a `lost` line keyed by its name, once.
    for (n, why) in &out.gaps {
        if n == "history" || head.segs.contains_key(n) {
            continue;
        }
        if !next.lost.iter().any(|l| lost_key(l) == Some(n.as_str())) {
            next.lost.push(format!("{n}: {why}"));
        }
    }
    out.next_head = next;
    out.rejected = out.gaps.clone();
    for n in uncovered {
        out.rejected.push((n, INTERRUPTED.into()));
    }
    out.next_order = segments.iter().map(|n| seg_order(n)).max().unwrap_or(0) + 1;
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
        let m = remote_manifest(&rc, &opts.label, None)?;
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
    use super::{
        covered, format_head, format_sidecar, held_locally, lost_key, parse_head, parse_sidecar,
        seg_order,
    };
    use std::collections::{BTreeMap, BTreeSet};

    /// Segment names order history (r9b F1): the order prefix, fixed width, and nothing else.
    #[test]
    fn segment_order_lost_keys_and_local_holding_parse_strictly() {
        assert_eq!(seg_order("o0000000012-1700000000-ab-00000003.jsonl"), 12);
        for n in [
            "1700000000-ab-00000003.jsonl",
            "o12-1700000000-ab-00000003.jsonl",
            "o00000000x2-r-1.jsonl",
            "x0000000012-r-1.jsonl",
        ] {
            assert_eq!(seg_order(n), 0, "{n}");
        }
        let a = "o0000000001-r-00000001.jsonl";
        let b = "o0000000002-r-00000002.jsonl";
        let mut names = [b, "1700000000-r-00000009.jsonl", a];
        names.sort();
        assert_eq!(names[1..], [a, b], "name order is commit order");
        assert_eq!(lost_key(&format!("{a}: its sha256 differs")), Some(a));
        assert_eq!(lost_key("the storage head was missing: x"), None);
        let local: BTreeSet<String> = ["{\"a\":1}", "{\"b\":2}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(held_locally(b"{\"a\":1}\n{\"b\":2}\n", &local));
        assert!(held_locally(b"{\"a\":1}\n{\"b\"", &local), "a cut last row");
        assert!(held_locally(b"", &local));
        assert!(!held_locally(b"{\"a\":1}\n{\"c\":3}\n", &local));
        assert!(!held_locally(b"{\"c\"", &local));
    }

    #[test]
    fn heads_and_sidecars_parse_only_whole() {
        let sha = "ab".repeat(32);
        let mut segs = BTreeMap::new();
        segs.insert("1-2-00000003.jsonl".to_string(), sha.clone());
        let head = super::Head {
            gen: None,
            state: Some("0f".repeat(16)),
            segs: segs.clone(),
            lost: vec!["a gap\nwith a newline".into()],
        };
        let t = format_head(7, &head);
        let h = parse_head(&t).unwrap();
        assert_eq!(
            (h.gen, h.state, h.segs, h.lost),
            (
                Some(7),
                Some("0f".repeat(16)),
                segs,
                vec!["a gap with a newline".into()]
            )
        );
        assert!(parse_head(&t.replace("state 0f", "state 0F")).is_none());
        assert!(parse_head(
            &t.replace("gen 7\n", "gen 7\nstate 00000000000000000000000000000000\n")
        )
        .is_none());
        // Cut anywhere, it does not parse: the reader falls back to the other slot.
        for cut in 1..t.len() {
            assert!(parse_head(&t[..cut]).is_none(), "{cut}");
        }
        assert!(parse_head(&t.replace("seg 1-2", "seg ../1-2")).is_none());
        assert!(parse_head(&t.replace("lost ", "other ")).is_none());
        let s = format_sidecar("x.jsonl", b"rows\n");
        assert_eq!(parse_sidecar(&s).unwrap().2, "x.jsonl");
        for cut in 1..s.len() {
            assert!(parse_sidecar(&s[..cut]).is_none(), "{cut}");
        }
        assert!(parse_sidecar(&s.to_uppercase()).is_none());
    }

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
