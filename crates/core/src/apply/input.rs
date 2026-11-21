//! Reading the scan and plan documents that `apply` executes (docs/SCAN-FORMAT.md, "Pieces and
//! lanes", schema 16). Only the fields apply needs are read; the whole document is validated
//! before anything acts (NEG-9).

use super::ApplyError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// The schema this module reads. Kept apart from [`crate::SCHEMA_VERSION`] on purpose: apply
/// refuses any other version, so a scan or plan schema change must be read here before apply
/// accepts it.
pub const LANES_SCHEMA: u64 = 16;

// A schema bump fails the build until someone has checked what apply reads and moved
// `LANES_SCHEMA` (main's ruling, 2026-11-21): never a silent exit 2 on every scan, never a
// silent acceptance of a changed field shape.
const _: () = assert!(
    LANES_SCHEMA == crate::SCHEMA_VERSION as u64,
    "SCHEMA_VERSION changed: check what apply reads (crates/core/src/apply/input.rs), then move LANES_SCHEMA"
);

/// Piece kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PieceKind {
    Tree,
    Files,
    GitNode,
}

/// A piece of the scan survey (`scan.pieces.pieces[i]`).
#[derive(Debug, Clone, Deserialize)]
pub struct ScanPiece {
    pub id: usize,
    pub kind: PieceKind,
    pub path: PathBuf,
    /// For a `files` piece: its member names (from `members`).
    #[serde(skip)]
    pub files: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub group: Option<PathBuf>,
    pub walked_at: i64,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub incomplete: Option<String>,
    /// The piece as core reads it (members with identity), for `pieces::recheck`.
    #[serde(skip, default = "empty_piece")]
    pub core: crate::Piece,
}

fn empty_piece() -> crate::Piece {
    crate::Piece {
        id: 0,
        kind: crate::PieceKind::Tree,
        path: String::new(),
        members: Vec::new(),
        excludes: Vec::new(),
        node: None,
        group: String::new(),
        entries: 0,
        bytes: 0,
        logical_bytes: 0,
        bytes_linked_outside: 0,
        newest_mtime: None,
        walked_at: 0,
        secret: None,
        incomplete: None,
    }
}

/// Lane of a plan piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Git,
    Noise,
    Backup,
    Keep,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Git => "git",
            Lane::Noise => "noise",
            Lane::Backup => "backup",
            Lane::Keep => "keep",
        }
    }
}

/// Noise evidence.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Evidence {
    Rule {
        rule: String,
        #[serde(default)]
        markers: Vec<String>,
        node: usize,
    },
    Duplicate {
        sha256: String,
        size: u64,
        retained_copy: PathBuf,
        #[serde(default)]
        retained_piece: Option<usize>,
    },
    Archive {
        sha256: String,
        size: u64,
        #[serde(default)]
        archive: Option<String>,
    },
}

/// The recovery method the plan chose for a git piece.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum Recovery {
    None,
    PushRecoveryRefs {
        remote: String,
        url: String,
        #[serde(default)]
        fallback: Option<String>,
    },
    BundleToBackup,
}

/// The `git` block of a git-lane piece.
#[derive(Debug, Clone, Deserialize)]
pub struct GitLane {
    pub action: String,
    #[serde(default)]
    pub preserve: bool,
    #[serde(default)]
    pub excludes_secret: Vec<String>,
    pub recovery: Recovery,
}

/// A plan piece (`plan.lanes.pieces[i]`).
#[derive(Debug, Clone, Deserialize)]
pub struct PlanPiece {
    pub id: usize,
    pub scan_piece: usize,
    #[serde(default)]
    pub derived: bool,
    pub kind: PieceKind,
    pub path: PathBuf,
    /// For a `files` piece: its member names (derived: `member`; otherwise the scan piece's
    /// members minus `moved_out`). Computed at parse.
    #[serde(skip)]
    pub files: Vec<String>,
    /// The scan piece's excludes plus, for a tree or git-node, `moved_out`. Computed at parse.
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub moved_out: Vec<String>,
    #[serde(default)]
    pub member: Option<String>,
    #[serde(default)]
    pub node: Option<usize>,
    #[serde(default)]
    pub group: Option<PathBuf>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub entries: u64,
    #[serde(default)]
    pub bytes: u64,
    pub lane: Lane,
    pub reason: String,
    #[serde(default)]
    pub evidence: Option<Evidence>,
    #[serde(default)]
    pub git: Option<GitLane>,
}

/// A copy of a plan blob.
#[derive(Debug, Clone, Deserialize)]
pub struct BlobCopy {
    pub path: PathBuf,
    pub piece: usize,
}

/// A plan blob (content with more than one copy).
#[derive(Debug, Clone, Deserialize)]
pub struct PlanBlob {
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub copies: Vec<BlobCopy>,
    #[serde(default)]
    pub upload_path: Option<PathBuf>,
}

/// A plan item, as far as apply reads it (the noise cross-check).
#[derive(Debug, Clone, Deserialize)]
pub struct PlanItemRef {
    pub node: usize,
    pub path: PathBuf,
    pub action: String,
}

/// Everything apply needs from the two documents, validated.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// `lanes.scan_pieces_sha256`, checked against the scan.
    pub scan_digest: String,
    /// SHA-256 of the plan file text.
    pub plan_digest: String,
    pub scan_generated_at: i64,
    pub roots: Vec<PathBuf>,
    pub scan_pieces: Vec<ScanPiece>,
    pub pieces: Vec<PlanPiece>,
    pub blobs: Vec<PlanBlob>,
    pub items: Vec<PlanItemRef>,
    pub active_window_days: u64,
    pub active: Vec<PathBuf>,
}

fn bad(msg: impl Into<String>) -> ApplyError {
    ApplyError::Plan(msg.into())
}

fn field<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    Some(cur)
}

/// A path is acceptable as a piece path: absolute, normal components only.
fn plain_absolute(p: &Path) -> bool {
    p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && p.components().count() > 1
}

/// A relative member, exclude or file name: no `..`, no root, not empty.
fn plain_relative(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Parse and validate the scan and plan JSON texts. `home` is refused as a piece path, as is
/// any path outside the scan roots and any remote root inside a piece (`remote_local`).
pub fn parse(
    scan_text: &str,
    plan_text: &str,
    home: Option<&Path>,
    remote_local: Option<&Path>,
) -> Result<Inputs, ApplyError> {
    let scan: serde_json::Value =
        serde_json::from_str(scan_text).map_err(|e| bad(format!("scan file: {e}")))?;
    let plan: serde_json::Value =
        serde_json::from_str(plan_text).map_err(|e| bad(format!("plan file: {e}")))?;
    for (name, doc) in [("scan", &scan), ("plan", &plan)] {
        let v = doc.get("schema_version").and_then(|v| v.as_u64());
        if v != Some(LANES_SCHEMA) {
            return Err(bad(format!(
                "{name} file has schema_version {v:?}; apply reads {LANES_SCHEMA}"
            )));
        }
    }
    let scan_generated_at = scan
        .get("generated_at")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| bad("scan file has no generated_at"))?;
    let plan_scan_at = plan.get("scan_generated_at").and_then(|v| v.as_i64());
    if plan_scan_at != Some(scan_generated_at) {
        return Err(bad(
            "the plan was not made from this scan (scan_generated_at differs)",
        ));
    }
    let roots: Vec<PathBuf> = serde_json::from_value(
        scan.get("roots")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| bad(format!("scan roots: {e}")))?;
    let survey_v = field(&scan, &["pieces"])
        .cloned()
        .ok_or_else(|| bad("scan has no pieces survey (run scan --lanes)"))?;
    let survey: crate::PieceSurvey =
        serde_json::from_value(survey_v).map_err(|e| bad(format!("scan pieces: {e}")))?;
    let mut scan_pieces: Vec<ScanPiece> = serde_json::from_value(
        field(&scan, &["pieces", "pieces"])
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| bad(format!("scan pieces: {e}")))?;
    for (sp, core) in scan_pieces.iter_mut().zip(survey.pieces.iter()) {
        if sp.kind == PieceKind::Files {
            sp.files = core.members.iter().map(|m| m.path().to_string()).collect();
        }
        sp.core = core.clone();
    }
    let lanes = plan
        .get("lanes")
        .ok_or_else(|| bad("plan has no lanes (plan a scan made with --lanes)"))?;
    // The plan points at the scan's member lists: bind it to exactly this survey.
    let scan_pieces_sha256 = crate::pieces::pieces_digest(&survey);
    if lanes.get("scan_pieces_sha256").and_then(|v| v.as_str()) != Some(scan_pieces_sha256.as_str())
    {
        return Err(bad(
            "the plan was not made from this scan (lanes.scan_pieces_sha256 differs)",
        ));
    }
    let mut pieces: Vec<PlanPiece> = serde_json::from_value(
        lanes
            .get("pieces")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| bad(format!("plan lanes.pieces: {e}")))?;
    let blobs: Vec<PlanBlob> = match lanes.get("blobs") {
        Some(b) => {
            serde_json::from_value(b.clone()).map_err(|e| bad(format!("plan blobs: {e}")))?
        }
        None => Vec::new(),
    };
    let items: Vec<PlanItemRef> = match plan.get("items") {
        Some(b) => {
            serde_json::from_value(b.clone()).map_err(|e| bad(format!("plan items: {e}")))?
        }
        None => Vec::new(),
    };
    let active_window_days = plan
        .get("active_window_days")
        .and_then(|v| v.as_u64())
        .unwrap_or(crate::DEFAULT_ACTIVE_WINDOW_DAYS);
    let active: Vec<PathBuf> = match plan.get("active") {
        Some(a) => {
            serde_json::from_value(a.clone()).map_err(|e| bad(format!("plan active: {e}")))?
        }
        None => Vec::new(),
    };
    // Member sets (rev 5): a derived piece has one `member`; any other piece is its scan piece's
    // members minus `moved_out`.
    for p in pieces.iter_mut() {
        let sp = scan_pieces
            .get(p.scan_piece)
            .ok_or_else(|| bad(format!("plan piece {}: scan_piece out of range", p.id)))?;
        let names: std::collections::BTreeSet<&str> =
            sp.core.members.iter().map(|m| m.path()).collect();
        for m in p.moved_out.iter().chain(p.member.iter()) {
            if !names.contains(m.as_str()) {
                return Err(bad(format!(
                    "plan piece {}: {m:?} is not a member of scan piece {}",
                    p.id, p.scan_piece
                )));
            }
        }
        if p.path != sp.path {
            return Err(bad(format!(
                "plan piece {}: its path is not its scan piece's path",
                p.id
            )));
        }
        match (&p.member, p.kind) {
            (Some(m), PieceKind::Files) if p.derived => {
                p.files = vec![m.clone()];
                p.excludes.clear();
            }
            (Some(_), _) | (None, _) if p.derived => {
                return Err(bad(format!(
                    "plan piece {}: a derived piece is a files piece of one member",
                    p.id
                )))
            }
            (Some(_), _) => {
                return Err(bad(format!(
                    "plan piece {}: only a derived piece names a member",
                    p.id
                )))
            }
            (None, PieceKind::Files) => {
                p.files = sp
                    .files
                    .iter()
                    .filter(|f| !p.moved_out.contains(f))
                    .cloned()
                    .collect();
                p.excludes.clear();
            }
            (None, _) => {
                if p.kind != sp.kind {
                    return Err(bad(format!(
                        "plan piece {}: kind differs from its scan piece",
                        p.id
                    )));
                }
                let mut ex = sp.excludes.clone();
                ex.extend(p.moved_out.iter().cloned());
                p.excludes = ex;
            }
        }
    }
    let plan_digest = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(plan_text.as_bytes()))
    };
    let inputs = Inputs {
        scan_digest: scan_pieces_sha256,
        plan_digest,
        scan_generated_at,
        roots,
        scan_pieces,
        pieces,
        blobs,
        items,
        active_window_days,
        active,
    };
    validate(&inputs, home, remote_local)?;
    Ok(inputs)
}

fn validate(
    i: &Inputs,
    home: Option<&Path>,
    remote_local: Option<&Path>,
) -> Result<(), ApplyError> {
    for (k, sp) in i.scan_pieces.iter().enumerate() {
        if sp.id != k {
            return Err(bad(format!("scan piece {k} has id {}", sp.id)));
        }
    }
    let mut removing: Vec<(&Path, usize)> = Vec::new();
    for (k, p) in i.pieces.iter().enumerate() {
        if p.id != k {
            return Err(bad(format!("plan piece {k} has id {}", p.id)));
        }
        let sp = i.scan_pieces.get(p.scan_piece).ok_or_else(|| {
            bad(format!(
                "plan piece {k}: scan_piece {} out of range",
                p.scan_piece
            ))
        })?;
        if !plain_absolute(&p.path) {
            return Err(bad(format!(
                "plan piece {k}: path {} is not a plain absolute path",
                p.path.display()
            )));
        }
        if home.is_some_and(|h| h == p.path) {
            return Err(bad(format!(
                "plan piece {k}: the home directory itself is a piece"
            )));
        }
        if !i.roots.iter().any(|r| p.path.starts_with(r)) {
            return Err(bad(format!(
                "plan piece {k}: {} is outside the scan roots",
                p.path.display()
            )));
        }
        if !p.path.starts_with(&sp.path) {
            return Err(bad(format!(
                "plan piece {k}: path is not inside its scan piece"
            )));
        }
        for s in p.files.iter().chain(p.excludes.iter()) {
            if !plain_relative(s) {
                return Err(bad(format!(
                    "plan piece {k}: member or exclude {s:?} is not a plain relative path"
                )));
            }
        }
        if p.kind == PieceKind::Files && p.files.is_empty() {
            return Err(bad(format!("plan piece {k}: a files piece lists no files")));
        }
        if p.files.iter().any(|f| f.contains('/')) {
            return Err(bad(format!(
                "plan piece {k}: a files member must be a direct child"
            )));
        }
        match (p.lane, &p.evidence, &p.git) {
            (Lane::Noise, None, _) => {
                return Err(bad(format!("plan piece {k}: noise without evidence")))
            }
            (Lane::Git, _, None) => {
                return Err(bad(format!("plan piece {k}: git lane without a git block")))
            }
            (Lane::Git, _, _) if p.kind != PieceKind::GitNode => {
                return Err(bad(format!(
                    "plan piece {k}: git lane on a piece that is not a git node"
                )))
            }
            _ => {}
        }
        if p.lane != Lane::Keep {
            removing.push((&p.path, k));
            if let Some(r) = remote_local {
                if r.starts_with(&p.path) || p.path.starts_with(r) {
                    return Err(bad(format!(
                        "the backup remote {} overlaps the removing piece {}",
                        r.display(),
                        p.path.display()
                    )));
                }
            }
        }
    }
    // Overlap: two removing pieces may share a directory only through `files` pieces (siblings in
    // one directory) or when one excludes the other.
    let by_path: BTreeMap<usize, &PlanPiece> = i.pieces.iter().map(|p| (p.id, p)).collect();
    for (a_path, a) in &removing {
        for (b_path, b) in &removing {
            if a == b || !b_path.starts_with(a_path) {
                continue;
            }
            let pa = by_path[a];
            let pb = by_path[b];
            let ok = match pa.kind {
                PieceKind::Files => {
                    // A files piece owns only its listed names: b may sit beside them or below a
                    // directory that is not one of them.
                    let rel = b_path.strip_prefix(a_path).ok();
                    let first = rel
                        .and_then(|r| r.components().next())
                        .map(|c| c.as_os_str().to_string_lossy().to_string());
                    match first {
                        None => match pb.kind {
                            PieceKind::Files => pa.files.iter().all(|f| !pb.files.contains(f)),
                            // A residual tree beside derived files pieces: it excludes each name.
                            PieceKind::Tree => pa.files.iter().all(|f| pb.excludes.contains(f)),
                            PieceKind::GitNode => false,
                        },
                        Some(f) => !pa.files.contains(&f),
                    }
                }
                _ => {
                    let rel = b_path
                        .strip_prefix(a_path)
                        .ok()
                        .map(|r| r.to_string_lossy().replace('\\', "/"));
                    match rel.as_deref() {
                        Some("") => {
                            pa.kind == PieceKind::Tree
                                && pb.kind == PieceKind::Files
                                && pb.files.iter().all(|f| pa.excludes.contains(f))
                        }
                        Some(r) => pa
                            .excludes
                            .iter()
                            .any(|e| r == e || r.starts_with(&format!("{e}/"))),
                        None => false,
                    }
                }
            };
            if !ok {
                return Err(bad(format!(
                    "plan pieces {a} and {b} overlap ({} and {})",
                    a_path.display(),
                    b_path.display()
                )));
            }
        }
    }
    Ok(())
}
