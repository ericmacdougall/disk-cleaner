//! Enumerating a piece's members and re-checking it against the scan (docs/SCAN-FORMAT.md,
//! "Re-check"). This adapter implements the documented fingerprint until stream A's
//! `pieces::recheck` lands; it also accepts members that apply itself removed earlier
//! (`removed`), whose lines are synthesised from their recorded identity, so a derived sibling's
//! removal is not mistaken for drift (spec revision 3, F2).

use super::fsops::{self, Ident};
use super::input::{PieceKind, ScanPiece};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// One member: its `/`-separated path relative to the piece path, and its identity.
#[derive(Debug, Clone)]
pub struct Member {
    pub rel: String,
    pub id: Ident,
}

/// The members of a piece as found now, plus its directories (deepest last).
#[derive(Debug, Clone, Default)]
pub struct Enumerated {
    pub members: Vec<Member>,
    pub dirs: Vec<String>,
}

/// Why a piece cannot be acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift(pub String);

fn excluded(rel: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|e| {
        rel == e
            || (rel.len() > e.len()
                && rel.starts_with(e.as_str())
                && rel.as_bytes()[e.len()] == b'/')
    })
}

/// Enumerate the members of a piece (`kind`, `path`, `files`, `excludes`) as they are now.
/// A name that is not valid UTF-8, an unreadable directory or a symlinked ancestor is an error.
pub fn enumerate(
    kind: PieceKind,
    path: &Path,
    files: &[String],
    excludes: &[String],
) -> Result<Enumerated, Drift> {
    let root = fsops::open_dir_abs(path)
        .map_err(|e| Drift(format!("cannot open {}: {e}", path.display())))?;
    let mut out = Enumerated::default();
    match kind {
        PieceKind::Files => {
            for f in files {
                match fsops::lstat_rel(&root, f) {
                    Ok(id) if id.kind != 'd' => out.members.push(Member { rel: f.clone(), id }),
                    Ok(_) => return Err(Drift(format!("{f} is now a directory"))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(Drift(format!("{f}: {e}"))),
                }
            }
        }
        PieceKind::Tree | PieceKind::GitNode => {
            let mut stack = vec![String::new()];
            while let Some(dir) = stack.pop() {
                let full = if dir.is_empty() {
                    path.to_path_buf()
                } else {
                    path.join(&dir)
                };
                let rd = std::fs::read_dir(&full)
                    .map_err(|e| Drift(format!("cannot read {}: {e}", full.display())))?;
                let mut names = Vec::new();
                for ent in rd {
                    let ent =
                        ent.map_err(|e| Drift(format!("cannot read {}: {e}", full.display())))?;
                    let name = ent.file_name().into_string().map_err(|_| {
                        Drift(format!("a name in {} is not valid UTF-8", full.display()))
                    })?;
                    names.push(name);
                }
                names.sort();
                for name in names {
                    let rel = if dir.is_empty() {
                        name.clone()
                    } else {
                        format!("{dir}/{name}")
                    };
                    if kind == PieceKind::GitNode && rel == ".git" {
                        continue;
                    }
                    if excluded(&rel, excludes) {
                        continue;
                    }
                    let id =
                        fsops::lstat_rel(&root, &rel).map_err(|e| Drift(format!("{rel}: {e}")))?;
                    if id.kind == 'd' {
                        out.dirs.push(rel.clone());
                        stack.push(rel);
                    } else {
                        out.members.push(Member { rel, id });
                    }
                }
            }
        }
    }
    out.members
        .sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    out.dirs.sort_by_key(|d| d.matches('/').count());
    Ok(out)
}

/// One fingerprint line (docs/SCAN-FORMAT.md, "Re-check").
pub fn line(rel: &str, id: &Ident) -> String {
    let k = match id.kind {
        'f' => 'f',
        'l' => 'l',
        _ => 'o',
    };
    format!(
        "{rel}\0{k}\0{}\0{}.{:09}\0{}\0{}\n",
        id.len, id.mtime_s, id.mtime_ns, id.dev, id.ino
    )
}

/// The fingerprint of a member list (any order).
pub fn fingerprint<'a>(members: impl Iterator<Item = (&'a str, &'a Ident)>) -> String {
    let mut lines: Vec<String> = members.map(|(r, i)| line(r, i)).collect();
    lines.sort();
    let mut h = Sha256::new();
    for l in &lines {
        h.update(l.as_bytes());
    }
    hex::encode(h.finalize())
}

/// Re-check a scan piece: enumerate it, add the members apply removed itself (`removed`: rel ->
/// identity at removal), compare the fingerprint, and refuse any ctime at or after `walked_at`.
/// Returns the current enumeration on success.
pub fn recheck(sp: &ScanPiece, removed: &BTreeMap<String, Ident>) -> Result<Enumerated, Drift> {
    if let Some(why) = &sp.incomplete {
        return Err(Drift(format!("the scan could not judge it whole: {why}")));
    }
    let en = enumerate(sp.kind, &sp.path, &sp.files, &sp.excludes)?;
    for m in &en.members {
        if removed.contains_key(&m.rel) {
            return Err(Drift(format!(
                "{} reappeared after apply removed it",
                m.rel
            )));
        }
        if m.id.ctime_s >= sp.walked_at {
            return Err(Drift(format!("{} changed after the scan", m.rel)));
        }
    }
    let fp = fingerprint(
        en.members
            .iter()
            .map(|m| (m.rel.as_str(), &m.id))
            .chain(removed.iter().map(|(r, i)| (r.as_str(), i))),
    );
    if fp != sp.fingerprint {
        return Err(Drift(
            "its members differ from the scan (added, missing or changed)".to_string(),
        ));
    }
    Ok(en)
}
