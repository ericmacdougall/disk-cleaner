//! Enumerating a piece's members and re-checking it against the scan (docs/SCAN-FORMAT.md,
//! "Re-check") through core's `pieces::recheck`.

use super::fsops::{self, Ident};
use super::input::{PieceKind, ScanPiece};
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
                    // A git-node's members include its git directory (scan rev 5): the
                    // preserve obligation backs it up with the work tree.
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

/// Re-check a scan piece with core's `pieces::recheck` (docs/SCAN-FORMAT.md, "Re-check"),
/// leaving out the members apply itself removed earlier under the same scan and plan
/// (`removed`, keyed by relative path), so a derived sibling's removal is not drift (spec
/// revision 3, F2). A removed member that reappears is a non-member entry: core reports it.
pub fn recheck(sp: &ScanPiece, removed: &BTreeMap<String, Ident>) -> Result<(), Drift> {
    recheck_scoped(sp, removed, crate::RecheckScope::All)
}

/// [`recheck`] with an explicit scope (`WorkTree` only for a git-node after apply's own recovery
/// step wrote into its git directory).
pub fn recheck_scoped(
    sp: &ScanPiece,
    removed: &BTreeMap<String, Ident>,
    scope: crate::RecheckScope,
) -> Result<(), Drift> {
    if let Some(why) = &sp.core.incomplete {
        return Err(Drift(format!("the scan could not judge it whole: {why}")));
    }
    let mut piece = sp.core.clone();
    piece.members.retain(|m| !removed.contains_key(m.path()));
    crate::pieces::recheck(&piece, scope).map_err(|d| Drift(d.to_string()))
}
