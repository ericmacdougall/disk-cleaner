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
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|e| ApplyError::Manifest(format!("{}: {e}", self.path.display())))?;
            self.file = Some(f);
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

    /// Verified shards for `remote`: id -> size.
    pub fn shards(&self, remote: &str) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for e in &self.rows {
            if let Row::Shard {
                remote: r,
                id,
                size,
                ..
            } = &e.row
            {
                if r == remote {
                    out.insert(id.clone(), *size);
                }
            }
        }
        out
    }

    /// Members already removed under `scan_piece` (any run): rel path within the scan piece's
    /// directory -> identity at removal. A member counts only with both its `file` row and a
    /// `removed` row for the same path.
    pub fn removed_under(&self, scan_piece: usize, scan_path: &Path) -> BTreeMap<String, Ident> {
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
                if *sp == scan_piece && removed.contains(path) {
                    if let Ok(rel) = path.strip_prefix(scan_path) {
                        out.insert(rel.to_string_lossy().replace('\\', "/"), *id);
                    }
                }
            }
        }
        out
    }

    /// Plan pieces (path, plan piece id) with `remove-begin` and no later `remove-done`, each
    /// with the run that began it. Derived pieces share a path, so the id is part of the key.
    pub fn begun(&self) -> BTreeMap<(PathBuf, usize), String> {
        let mut s = BTreeMap::new();
        for e in &self.rows {
            match &e.row {
                Row::RemoveBegin {
                    path, plan_piece, ..
                } => {
                    s.insert((path.clone(), *plan_piece), e.run.clone());
                }
                Row::RemoveDone { path, plan_piece } => {
                    s.remove(&(path.clone(), *plan_piece));
                }
                _ => {}
            }
        }
        s
    }

    /// Plan pieces (path, plan piece id) whose latest begin was closed by `remove-done`.
    pub fn done(&self) -> BTreeSet<(PathBuf, usize)> {
        let mut s = BTreeSet::new();
        for e in &self.rows {
            match &e.row {
                Row::RemoveBegin {
                    path, plan_piece, ..
                } => {
                    s.remove(&(path.clone(), *plan_piece));
                }
                Row::RemoveDone { path, plan_piece } => {
                    s.insert((path.clone(), *plan_piece));
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

/// Parse manifest text (for `restore --from-remote`): rows of every segment, deduplicated.
pub fn parse_segments(texts: &[String]) -> Vec<Envelope> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for t in texts {
        for l in t.lines() {
            if let Ok(env) = serde_json::from_str::<Envelope>(l) {
                if seen.insert(l.to_string()) {
                    out.push(env);
                }
            }
        }
    }
    out
}
