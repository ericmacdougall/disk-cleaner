//! The append-only manifest (DESIGN 7; spec I-4, revision 3 F1). One JSON object per line, each
//! with `v`, `run` and `t`. Every append is followed by fsync; a failure stops the run before any
//! unlink. A torn last line is ignored; a corrupt line elsewhere refuses the manifest.

use super::fsops::Ident;
use super::ApplyError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const MANIFEST_VERSION: u32 = 1;

/// One segment of a blob inside a shard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub shard: String,
    pub offset: u64,
    pub len: u64,
}

/// A manifest row.
#[allow(clippy::large_enum_variant)] // rows are written once, not stored in bulk hot paths
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum Row {
    /// A verified shard object in storage.
    Shard {
        remote: String,
        id: String,
        size: u64,
        /// `sha1:<hex>`, `md5:<hex>` or `readback-sha256:<hex>`: how it was verified.
        verified_by: String,
    },
    /// A blob whose every segment is in a verified shard.
    Blob {
        remote: String,
        sha256: String,
        size: u64,
        segments: Vec<Segment>,
    },
    /// A member whose content is accounted for (uploaded, in a backed-up git object store, or
    /// noise with evidence). Written before `remove-begin`.
    File {
        plan_piece: usize,
        scan_piece: usize,
        lane: String,
        path: PathBuf,
        rel: String,
        id: Ident,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_oid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_dir: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link_target: Option<String>,
        /// `dev:ino` for regular files with more than one name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link_group: Option<String>,
        reason: String,
        restore: String,
    },
    /// A directory of a removed piece (recreated by restore even when empty).
    Dir {
        plan_piece: usize,
        path: PathBuf,
        mode: u32,
    },
    /// Noise removed on a rule: not restorable from storage, regenerate instead.
    Regenerable {
        plan_piece: usize,
        path: PathBuf,
        rule: String,
        markers: Vec<String>,
    },
    /// A duplicate removed as noise, retained elsewhere.
    Retains {
        plan_piece: usize,
        path: PathBuf,
        retained: PathBuf,
        sha256: String,
        size: u64,
    },
    /// A git piece's recovery refs confirmed on the remote.
    Recovery {
        plan_piece: usize,
        path: PathBuf,
        url: String,
        refs: Vec<(String, String)>,
    },
    RemoveBegin {
        plan_piece: usize,
        scan_piece: usize,
        path: PathBuf,
        /// `lanes.scan_pieces_sha256` of the scan this removal was authorized by.
        #[serde(default)]
        scan: String,
        /// SHA-256 of the plan file this removal was authorized by.
        #[serde(default)]
        plan: String,
    },
    Removed {
        plan_piece: usize,
        path: PathBuf,
    },
    Kept {
        plan_piece: usize,
        path: PathBuf,
        reason: String,
    },
    RemoveDone {
        plan_piece: usize,
        path: PathBuf,
    },
}

/// An open removal: the run that began it and the plan digest that authorized it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Begun {
    pub run: String,
    pub plan: String,
}

/// A row with its envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub run: String,
    pub at: i64,
    #[serde(flatten)]
    pub row: Row,
}

/// The manifest file plus indexes over everything read or written.
pub struct Manifest {
    path: PathBuf,
    file: Option<File>,
    pub run: String,
    pub rows: Vec<Envelope>,
    /// Rows appended in this process not yet mirrored to storage.
    pub unmirrored: Vec<String>,
    /// Test seam: fail the append after this many successful appends.
    pub fail_after: Option<usize>,
    appended: usize,
}

/// The only way the manifest is opened for writing (r6b F2). Called by the first append of an
/// acting run, which holds the apply lock. A torn last line (what `load` ignores) would turn
/// into a corrupt middle line once more rows follow, so before appending:
///
/// - an unterminated last line that parses is a whole row missing its newline: the newline is
///   added;
/// - a last line that does not parse is copied to `<manifest>.torn-<now>[-n]` (0600, fsynced)
///   and cut off, and the manifest is fsynced.
///
/// A failure anywhere stops the run before any unlink and leaves the manifest as it was (the
/// copy is written before the cut). Dry runs never call this.
pub fn open_for_append(path: &Path, now: i64) -> Result<File, ApplyError> {
    use std::io::{Read, Seek, SeekFrom};
    let err = |e: std::io::Error| ApplyError::Manifest(format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(err)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).map_err(err)?;
    let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let start = body.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let last = &body[start..];
    let terminated = bytes.ends_with(b"\n");
    let parses = !last.iter().all(u8::is_ascii_whitespace)
        && serde_json::from_slice::<Envelope>(last).is_ok_and(|e| e.v == MANIFEST_VERSION);
    let blank = last.iter().all(u8::is_ascii_whitespace);
    if !bytes.is_empty() && !terminated && (parses || blank) {
        f.seek(SeekFrom::End(0)).map_err(err)?;
        f.write_all(b"\n").and_then(|_| f.sync_all()).map_err(err)?;
    } else if !bytes.is_empty() && !parses && !blank {
        let mut side = path.as_os_str().to_owned();
        side.push(format!(".torn-{now}"));
        let mut side = PathBuf::from(side);
        let mut n = 0;
        let mut out = loop {
            use std::os::unix::fs::OpenOptionsExt;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&side)
            {
                Ok(o) => break o,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                    n += 1;
                    let mut s = path.as_os_str().to_owned();
                    s.push(format!(".torn-{now}-{n}"));
                    side = PathBuf::from(s);
                }
                Err(e) => {
                    return Err(ApplyError::Manifest(format!(
                        "cannot save the torn last line of {} before repairing it: {e}",
                        path.display()
                    )))
                }
            }
        };
        out.write_all(&bytes[start..])
            .and_then(|_| out.sync_all())
            .map_err(|e| ApplyError::Manifest(format!("{}: {e}", side.display())))?;
        f.set_len(start as u64)
            .and_then(|_| f.sync_all())
            .map_err(err)?;
        eprintln!(
            "warning: cut a torn last line from {} (saved to {})",
            path.display(),
            side.display()
        );
    }
    drop(f);
    OpenOptions::new().append(true).open(path).map_err(err)
}

impl Manifest {
    /// Read `path` (absent is empty) for `run`. Does not create the file (dry run).
    pub fn load(path: &Path, run: &str) -> Result<Manifest, ApplyError> {
        let mut rows = Vec::new();
        if path.exists() {
            let f = File::open(path)
                .map_err(|e| ApplyError::Manifest(format!("{}: {e}", path.display())))?;
            let lines: Vec<String> = BufReader::new(f)
                .lines()
                .collect::<Result<_, _>>()
                .map_err(|e| ApplyError::Manifest(format!("{}: {e}", path.display())))?;
            let n = lines.len();
            for (i, l) in lines.iter().enumerate() {
                if l.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Envelope>(l) {
                    Ok(env) if env.v == MANIFEST_VERSION => rows.push(env),
                    Ok(env) => {
                        return Err(ApplyError::Manifest(format!(
                            "line {} has manifest version {}; this build reads {MANIFEST_VERSION}",
                            i + 1,
                            env.v
                        )))
                    }
                    Err(_) if i + 1 == n => {
                        eprintln!("warning: ignoring a torn last line in {}", path.display());
                    }
                    Err(err) => {
                        return Err(ApplyError::Manifest(format!(
                            "{} line {} is corrupt ({err}); refusing to act on a damaged manifest",
                            path.display(),
                            i + 1
                        )))
                    }
                }
            }
        }
        Ok(Manifest {
            path: path.to_path_buf(),
            file: None,
            run: run.to_string(),
            rows,
            unmirrored: Vec::new(),
            fail_after: None,
            appended: 0,
        })
    }

    /// Append rows and fsync. Any failure is an error and nothing later may unlink.
    pub fn append(&mut self, now: i64, rows: Vec<Row>) -> Result<(), ApplyError> {
        if rows.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| ApplyError::Manifest(format!("{}: {e}", dir.display())))?;
            }
            self.file = Some(open_for_append(&self.path, now)?);
        }
        let mut text = String::new();
        let mut envs = Vec::new();
        for row in rows {
            let env = Envelope {
                v: MANIFEST_VERSION,
                run: self.run.clone(),
                at: now,
                row,
            };
            let line =
                serde_json::to_string(&env).map_err(|e| ApplyError::Manifest(e.to_string()))?;
            text.push_str(&line);
            text.push('\n');
            envs.push((env, line));
        }
        if let Some(limit) = self.fail_after {
            if self.appended >= limit {
                return Err(ApplyError::Manifest(
                    "injected append failure (no space left on device)".into(),
                ));
            }
        }
        let f = self.file.as_mut().expect("opened above");
        f.write_all(text.as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| ApplyError::Manifest(format!("{}: {e}", self.path.display())))?;
        self.appended += 1;
        // Only rows that reached the file are mirrored or counted.
        for (env, line) in envs {
            self.unmirrored.push(line);
            self.rows.push(env);
        }
        Ok(())
    }

    /// Verified blobs for `remote`, by sha256 (latest row wins).
    pub fn blobs(&self, remote: &str) -> BTreeMap<String, (u64, Vec<Segment>)> {
        let mut out = BTreeMap::new();
        for e in &self.rows {
            if let Row::Blob {
                remote: r,
                sha256,
                size,
                segments,
            } = &e.row
            {
                if r == remote {
                    out.insert(sha256.clone(), (*size, segments.clone()));
                }
            }
        }
        out
    }

    /// Verified shards for `remote`: id -> (size, how it was verified).
    pub fn shards(&self, remote: &str) -> BTreeMap<String, (u64, String)> {
        let mut out = BTreeMap::new();
        for e in &self.rows {
            if let Row::Shard {
                remote: r,
                id,
                size,
                verified_by,
            } = &e.row
            {
                if r == remote {
                    out.insert(id.clone(), (*size, verified_by.clone()));
                }
            }
        }
        out
    }

    /// Members already removed under `scan_piece` by runs authorized by the same scan
    /// (`scan`, its `scan_pieces_sha256`): rel path within the scan piece's directory -> identity
    /// at removal. A member counts only with its `file` row, a `removed` row for the same path,
    /// and a `remove-begin` of that run naming this scan.
    pub fn removed_under(
        &self,
        scan_piece: usize,
        scan_path: &Path,
        scan: &str,
    ) -> BTreeMap<String, Ident> {
        let runs: BTreeSet<&str> = self
            .rows
            .iter()
            .filter_map(|e| match &e.row {
                Row::RemoveBegin { scan: s, .. } if s == scan => Some(e.run.as_str()),
                _ => None,
            })
            .collect();
        let removed: BTreeSet<&PathBuf> = self
            .rows
            .iter()
            .filter_map(|e| match &e.row {
                Row::Removed { path, .. } => Some(path),
                _ => None,
            })
            .collect();
        let mut out = BTreeMap::new();
        for e in &self.rows {
            if let Row::File {
                scan_piece: sp,
                path,
                id,
                ..
            } = &e.row
            {
                if *sp == scan_piece && removed.contains(path) && runs.contains(e.run.as_str()) {
                    if let Ok(rel) = path.strip_prefix(scan_path) {
                        out.insert(rel.to_string_lossy().replace('\\', "/"), *id);
                    }
                }
            }
        }
        out
    }

    /// Plan pieces (path, plan piece id) with `remove-begin` and no later `remove-done`, each
    /// with the run that began it and the plan digest that authorized it. Derived pieces share a
    /// path, so the id is part of the key.
    pub fn begun(&self) -> BTreeMap<(PathBuf, usize), Begun> {
        let mut s = BTreeMap::new();
        for e in &self.rows {
            match &e.row {
                Row::RemoveBegin {
                    path,
                    plan_piece,
                    plan,
                    ..
                } => {
                    s.insert(
                        (path.clone(), *plan_piece),
                        Begun {
                            run: e.run.clone(),
                            plan: plan.clone(),
                        },
                    );
                }
                Row::RemoveDone { path, plan_piece } => {
                    s.remove(&(path.clone(), *plan_piece));
                }
                _ => {}
            }
        }
        s
    }

    /// Plan pieces (path, plan piece id) whose latest begin, under plan digest `plan`, was
    /// closed by `remove-done`.
    pub fn done(&self, plan: &str) -> BTreeSet<(PathBuf, usize)> {
        let mut s = BTreeSet::new();
        let mut last_plan: BTreeMap<(PathBuf, usize), String> = BTreeMap::new();
        for e in &self.rows {
            match &e.row {
                Row::RemoveBegin {
                    path,
                    plan_piece,
                    plan: p,
                    ..
                } => {
                    s.remove(&(path.clone(), *plan_piece));
                    last_plan.insert((path.clone(), *plan_piece), p.clone());
                }
                Row::RemoveDone { path, plan_piece } => {
                    let k = (path.clone(), *plan_piece);
                    if last_plan.get(&k).map(|p| p.as_str()) == Some(plan) {
                        s.insert(k);
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// The `file` rows `run` wrote for one plan piece, by path (for resume).
    pub fn file_rows_for(&self, plan_piece: usize, run: &str) -> BTreeMap<PathBuf, Row> {
        let mut out = BTreeMap::new();
        for e in &self.rows {
            if let Row::File {
                path,
                plan_piece: pp,
                ..
            } = &e.row
            {
                if *pp == plan_piece && e.run == run {
                    out.insert(path.clone(), e.row.clone());
                }
            }
        }
        out
    }

    /// Paths retained by earlier duplicate removals (protected in later runs, N4).
    pub fn retained_paths(&self) -> BTreeSet<PathBuf> {
        self.rows
            .iter()
            .filter_map(|e| match &e.row {
                Row::Retains { retained, .. } => Some(retained.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Parse one mirrored segment strictly: every non-empty line must be a row of this manifest
/// version. A segment is all or nothing (red-team A, Codex F6).
pub fn parse_segment(text: &str) -> Result<Vec<Envelope>, String> {
    let mut out = Vec::new();
    for (i, l) in text.lines().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        let env: Envelope =
            serde_json::from_str(l).map_err(|e| format!("line {} is not a row: {e}", i + 1))?;
        if env.v != MANIFEST_VERSION {
            return Err(format!(
                "line {} has manifest version {}; this build reads {MANIFEST_VERSION}",
                i + 1,
                env.v
            ));
        }
        out.push(env);
    }
    Ok(out)
}

/// Rows of several verified segments, de-duplicated by their exact text.
pub fn merge_segments(segments: Vec<Vec<Envelope>>) -> Vec<Envelope> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for seg in segments {
        for env in seg {
            if seen.insert(serde_json::to_string(&env).unwrap_or_default()) {
                out.push(env);
            }
        }
    }
    out
}
