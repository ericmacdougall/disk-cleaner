//! Lanes: what the plan does with each piece of the scan's survey (docs/SCAN-FORMAT.md, "Plan
//! field `lanes`"). Pure: reads the scan and the plan's items and touches nothing on disk except
//! to resolve `--active` paths and process working directories exactly as the items do.
//!
//! Every piece gets exactly one lane, the first rule that applies:
//! secret, incomplete, cannot act (process check), active or unknown group, inside a kept
//! repository, git node, noise by rule, backup. Then a backup copy of a blob that also has a copy
//! in a durable `keep` piece is cut out as noise by duplicate, and upload-once and reclaimable
//! bytes are computed on the result.

use crate::model::{Class, ScanResult};
use crate::pieces::{pieces_digest, Member, Piece, PieceKind, PieceSurvey};
use crate::plan::{Action, PlanItem, PlanOptions, ProjectState};
use crate::procs::ProcessCheck;
use crate::rules::RuleSet;
use crate::words::{agree, count};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// The version of the `lanes` section.
pub const LANES_VERSION: u32 = 1;

/// What happens to a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    /// A git node saved by its preserve obligation (and recovery), then removed.
    Git,
    /// Positively shown regenerable, or a copy of content that stays on disk: removed.
    Noise,
    /// Backed up, then removed.
    Backup,
    /// Left alone.
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

    /// Every lane but `keep` ends with something removed.
    pub fn removes(self) -> bool {
        self != Lane::Keep
    }

    pub const ALL: [Lane; 4] = [Lane::Git, Lane::Noise, Lane::Backup, Lane::Keep];
}

/// Why a piece is `noise`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Evidence {
    /// An artifact node whose rule matched with its markers and whose item removes it.
    Rule {
        rule: String,
        markers: Vec<String>,
        node: usize,
    },
    /// Byte-identical content that stays on disk in a durable `keep` piece.
    Duplicate {
        sha256: String,
        size: u64,
        retained_copy: String,
        retained_piece: usize,
    },
}

/// How a git piece's unsaved work is saved before removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Recovery {
    /// Nothing to recover (`backup-then-remove`).
    None,
    /// Push recovery refs to this remote, but only once apply has established it is private;
    /// otherwise the fallback.
    PushRecoveryRefs {
        remote: String,
        url: String,
        fallback: String,
        decided_at: String,
        note: String,
    },
    /// Bundle the unsaved work into the backup.
    BundleToBackup,
}

/// The `git` field of a `git` piece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitLane {
    pub action: Action,
    pub preserve: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes_secret: Vec<String>,
    pub recovery: Recovery,
}

/// One plan piece.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanePiece {
    pub id: usize,
    pub scan_piece: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub derived: bool,
    pub kind: PieceKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<usize>,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moved_out: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    pub entries: u64,
    pub bytes: u64,
    pub logical_bytes: u64,
    pub lane: Lane,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitLane>,
    pub state: ProjectState,
    pub state_reason: String,
    pub reclaimable_bytes: u64,
    pub upload_bytes: u64,
}

/// One copy of a plan blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneCopy {
    pub path: String,
    pub piece: usize,
    pub lane: Lane,
    pub inode: u32,
    pub bytes: u64,
}

/// One plan blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneBlob {
    pub sha256: String,
    pub size: u64,
    pub bytes: u64,
    pub copies: Vec<LaneCopy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_path: Option<String>,
    pub saved_bytes: u64,
}

/// Totals of one lane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneTotal {
    pub pieces: u64,
    pub entries: u64,
    pub bytes: u64,
    pub reclaimable_bytes: u64,
    pub upload_bytes: u64,
}

/// Totals of the `lanes` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneTotals {
    pub by_lane: BTreeMap<Lane, LaneTotal>,
    pub scanned_bytes: u64,
    pub dir_overhead_bytes: u64,
    pub duplicate_bytes: u64,
    pub dedup_saved_bytes: u64,
    pub secret_pieces: u64,
}

/// The plan field `lanes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lanes {
    pub version: u32,
    pub scan_pieces_sha256: String,
    pub pieces: Vec<LanePiece>,
    pub blobs: Vec<LaneBlob>,
    pub totals: LaneTotals,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// How a piece was kept, which decides whether a copy in it can make another copy noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// Kept for a reason that may change soon (secret, incomplete, processes, activity).
    Fleeting,
    /// Rule 5 or 6: inside a kept repository, or a git node that is not removed.
    Durable,
    /// Not kept.
    Removed,
}

/// Build the lanes of a plan. `node_state` is the judgement of every node (as the items use),
/// `items` the plan's items after every veto.
pub(crate) fn build(
    scan: &ScanResult,
    survey: &PieceSurvey,
    items: &[PlanItem],
    node_state: &[(ProjectState, String)],
    opts: &PlanOptions,
) -> Lanes {
    let nn = scan.nodes.len();
    let mut item_of: Vec<Option<&PlanItem>> = vec![None; nn];
    for it in items {
        if it.node < nn {
            item_of[it.node] = Some(it);
        }
    }
    let parent = |i: usize| scan.nodes[i].parent.filter(|p| *p < nn && *p != i);
    // Node depth, guarded against cycles (the scan loader rejects them).
    let depth: Vec<usize> = (0..nn)
        .map(|i| {
            let mut d = 0usize;
            let mut cur = parent(i);
            while let Some(c) = cur {
                d += 1;
                if d > nn {
                    break;
                }
                cur = parent(c);
            }
            d
        })
        .collect();

    // The judgement of each piece's group.
    let group_state = group_states(survey, opts);
    let state_of = |p: &Piece| -> (ProjectState, String) {
        match p.node.filter(|n| *n < nn) {
            Some(n) => node_state[n].clone(),
            None => group_state
                .get(&p.group)
                .cloned()
                .unwrap_or((ProjectState::Unknown, "no activity evidence".to_string())),
        }
    };

    // The `git-node` piece of each git node.
    let mut git_piece: Vec<Option<usize>> = vec![None; nn];
    for p in &survey.pieces {
        if p.kind == PieceKind::GitNode {
            if let Some(n) = p.node.filter(|n| *n < nn) {
                git_piece[n] = Some(p.id);
            }
        }
    }

    let process_block: Option<String> = match &opts.process {
        ProcessCheck::Unknown { reason } => Some(format!(
            "running processes could not be inspected ({reason}), so one may be using it; kept"
        )),
        ProcessCheck::Partial { unreadable, .. } if !opts.trust_partial_process_check => {
            let n = *unreadable as u64;
            Some(format!(
                "{} could not be inspected, so {} may be using it \
                 (--trust-partial-process-check accepts that risk); kept",
                count(n, "running process", "running processes"),
                agree(n, "it", "one of them")
            ))
        }
        _ => None,
    };

    let rules = RuleSet::builtin();
    let np = survey.pieces.len();
    let mut decided: Vec<Option<Decision>> = vec![None; np];
    // Git-node pieces first, outermost first, so rule 5 can look at the enclosing repository.
    let mut order: Vec<usize> = (0..np).collect();
    order.sort_by_key(|&i| {
        let p = &survey.pieces[i];
        let git = p.kind == PieceKind::GitNode;
        let d = p.node.filter(|n| *n < nn).map(|n| depth[n]).unwrap_or(0);
        (!git, d, i)
    });
    for &i in &order {
        let p = &survey.pieces[i];
        let (state, state_reason) = state_of(p);
        let d = decide(DecideIn {
            p,
            state,
            state_reason: &state_reason,
            process_block: process_block.as_deref(),
            scan,
            item_of: &item_of,
            git_piece: &git_piece,
            decided: &decided,
            parent: &parent,
            rules: &rules,
            survey,
        });
        decided[i] = Some(Decision {
            state,
            state_reason,
            ..d
        });
    }
    let decided: Vec<Decision> = decided
        .into_iter()
        .map(|d| d.expect("every piece decided"))
        .collect();

    // Plan pieces: one per scan piece, same ids.
    let mut pieces: Vec<LanePiece> = survey
        .pieces
        .iter()
        .zip(&decided)
        .map(|(p, d)| LanePiece {
            id: p.id,
            scan_piece: p.id,
            derived: false,
            kind: p.kind,
            path: p.path.clone(),
            node: p.node,
            group: p.group.clone(),
            secret: p.secret.clone(),
            moved_out: Vec::new(),
            member: None,
            excludes: p.excludes.clone(),
            entries: p.entries,
            bytes: p.bytes,
            logical_bytes: p.logical_bytes,
            lane: d.lane,
            reason: d.reason.clone(),
            evidence: d.evidence.clone(),
            git: d.git.clone(),
            state: d.state,
            state_reason: d.state_reason.clone(),
            reclaimable_bytes: 0,
            upload_bytes: 0,
        })
        .collect();
    let why: Vec<Why> = decided.iter().map(|d| d.why).collect();

    // Plan blob copies start in their scan pieces.
    let mut blobs: Vec<LaneBlob> = survey
        .blobs
        .iter()
        .map(|b| LaneBlob {
            sha256: b.sha256.clone(),
            size: b.size,
            bytes: b.bytes,
            copies: b
                .copies
                .iter()
                .map(|c| LaneCopy {
                    path: c.path.clone(),
                    piece: c.piece,
                    lane: pieces.get(c.piece).map(|p| p.lane).unwrap_or(Lane::Keep),
                    inode: c.inode,
                    bytes: c.bytes,
                })
                .collect(),
            upload_path: None,
            saved_bytes: 0,
        })
        .collect();

    // Noise by duplicate.
    for blob in blobs.iter_mut() {
        let retained = blob
            .copies
            .iter()
            .find(|c| c.piece < np && why[c.piece] == Why::Durable)
            .map(|c| (c.path.clone(), c.piece));
        let (retained_copy, retained_piece) = match retained {
            Some(r) => r,
            None => continue,
        };
        let mut seen_inode: HashSet<u32> = HashSet::new();
        for k in 0..blob.copies.len() {
            let c = blob.copies[k].clone();
            let first_name = seen_inode.insert(c.inode);
            if c.piece >= np || pieces[c.piece].lane != Lane::Backup || pieces[c.piece].derived {
                continue;
            }
            if c.path == retained_copy {
                continue;
            }
            let src = &survey.pieces[c.piece];
            let rel = match rel_path(&src.path, &c.path) {
                Some(r) => r,
                None => continue,
            };
            let m: &Member = match find_member(src, &rel) {
                Some(m) => m,
                None => continue,
            };
            // The first name of an inode in walk order owns its bytes (SCAN-FORMAT, "Hard links").
            let logical = if first_name { m.length() } else { 0 };
            let id = pieces.len();
            {
                let parent_piece = &mut pieces[c.piece];
                parent_piece.moved_out.push(rel.clone());
                parent_piece.entries = parent_piece.entries.saturating_sub(1);
                parent_piece.bytes = parent_piece.bytes.saturating_sub(m.bytes());
                parent_piece.logical_bytes = parent_piece.logical_bytes.saturating_sub(logical);
            }
            let (state, state_reason) =
                (pieces[c.piece].state, pieces[c.piece].state_reason.clone());
            pieces.push(LanePiece {
                id,
                scan_piece: c.piece,
                derived: true,
                kind: PieceKind::Files,
                path: src.path.clone(),
                node: src.node,
                group: src.group.clone(),
                secret: None,
                moved_out: Vec::new(),
                member: Some(rel),
                excludes: Vec::new(),
                entries: 1,
                bytes: m.bytes(),
                logical_bytes: logical,
                lane: Lane::Noise,
                reason: format!(
                    "a byte-identical copy ({} bytes, sha256 {}) stays on disk at {} (piece {}), \
                     which is kept",
                    blob.size,
                    short(&blob.sha256),
                    retained_copy,
                    retained_piece
                ),
                evidence: Some(Evidence::Duplicate {
                    sha256: blob.sha256.clone(),
                    size: blob.size,
                    retained_copy: retained_copy.clone(),
                    retained_piece,
                }),
                git: None,
                state,
                state_reason,
                reclaimable_bytes: 0,
                upload_bytes: 0,
            });
            blob.copies[k].piece = id;
            blob.copies[k].lane = Lane::Noise;
        }
    }
    for p in pieces.iter_mut() {
        p.moved_out.sort();
    }

    // Upload once, after the noise cut.
    let mut mapped_bytes: Vec<u64> = vec![0; pieces.len()];
    let mut dedup_saved = 0u64;
    for blob in blobs.iter_mut() {
        let first = blob.copies.iter().find(|c| c.lane == Lane::Backup).cloned();
        let first = match first {
            Some(f) => f,
            None => continue,
        };
        blob.upload_path = Some(first.path.clone());
        let mut saved = 0u64;
        for c in &blob.copies {
            if c.lane == Lane::Backup && c.inode != first.inode {
                saved = saved.saturating_add(c.bytes);
                mapped_bytes[c.piece] = mapped_bytes[c.piece].saturating_add(c.bytes);
            }
        }
        blob.saved_bytes = saved;
        dedup_saved = dedup_saved.saturating_add(saved);
    }

    // Bytes of removing pieces whose inode also has a name in a `keep` piece, from the members'
    // device and inode (not from blobs: an unhashed inode still has its kept name).
    let mut kept_alias: Vec<u64> = vec![0; pieces.len()];
    {
        // The plan piece of each member: its derived piece when moved out, else its scan piece.
        let mut moved: HashMap<(usize, &str), usize> = HashMap::new();
        for lp in pieces.iter().filter(|p| p.derived) {
            if let Some(m) = lp.member.as_deref() {
                moved.insert((lp.scan_piece, m), lp.id);
            }
        }
        let holder =
            |sp: usize, m: &Member| -> usize { moved.get(&(sp, m.path())).copied().unwrap_or(sp) };
        let mut kept: HashSet<(u64, u64)> = HashSet::new();
        for sp in &survey.pieces {
            for m in &sp.members {
                if m.6 != 0 && pieces[holder(sp.id, m)].lane == Lane::Keep {
                    kept.insert((m.5, m.6));
                }
            }
        }
        if !kept.is_empty() {
            for sp in &survey.pieces {
                for m in &sp.members {
                    let id = holder(sp.id, m);
                    if m.bytes() > 0 && pieces[id].lane.removes() && kept.contains(&(m.5, m.6)) {
                        kept_alias[id] = kept_alias[id].saturating_add(m.bytes());
                    }
                }
            }
        }
    }

    for p in pieces.iter_mut() {
        let linked_outside = if p.derived {
            0
        } else {
            survey.pieces[p.scan_piece].bytes_linked_outside
        };
        p.reclaimable_bytes = if p.lane.removes() {
            p.bytes
                .saturating_sub(linked_outside)
                .saturating_sub(kept_alias[p.id])
        } else {
            0
        };
        p.upload_bytes = match p.lane {
            Lane::Backup => p.bytes.saturating_sub(mapped_bytes[p.id]),
            Lane::Git => p.bytes,
            _ => 0,
        };
    }

    let mut totals = LaneTotals {
        dir_overhead_bytes: survey.totals.dir_overhead_bytes,
        scanned_bytes: survey
            .totals
            .bytes
            .saturating_add(survey.totals.dir_overhead_bytes),
        duplicate_bytes: survey.totals.duplicate_bytes,
        dedup_saved_bytes: dedup_saved,
        ..LaneTotals::default()
    };
    for lane in Lane::ALL {
        totals.by_lane.insert(lane, LaneTotal::default());
    }
    for p in &pieces {
        let t = totals.by_lane.entry(p.lane).or_default();
        t.pieces += 1;
        t.entries = t.entries.saturating_add(p.entries);
        t.bytes = t.bytes.saturating_add(p.bytes);
        t.reclaimable_bytes = t.reclaimable_bytes.saturating_add(p.reclaimable_bytes);
        t.upload_bytes = t.upload_bytes.saturating_add(p.upload_bytes);
        if p.secret.is_some() {
            totals.secret_pieces += 1;
        }
    }

    Lanes {
        version: LANES_VERSION,
        scan_pieces_sha256: pieces_digest(survey),
        pieces,
        blobs,
        totals,
    }
}

#[derive(Debug, Clone)]
struct Decision {
    lane: Lane,
    reason: String,
    evidence: Option<Evidence>,
    git: Option<GitLane>,
    why: Why,
    state: ProjectState,
    state_reason: String,
}

struct DecideIn<'a, F: Fn(usize) -> Option<usize>> {
    p: &'a Piece,
    state: ProjectState,
    state_reason: &'a str,
    process_block: Option<&'a str>,
    scan: &'a ScanResult,
    item_of: &'a [Option<&'a PlanItem>],
    git_piece: &'a [Option<usize>],
    decided: &'a [Option<Decision>],
    parent: &'a F,
    rules: &'a RuleSet,
    survey: &'a PieceSurvey,
}

fn keep(reason: String, why: Why) -> Decision {
    Decision {
        lane: Lane::Keep,
        reason,
        evidence: None,
        git: None,
        why,
        state: ProjectState::Unknown,
        state_reason: String::new(),
    }
}

/// The lane rules, in order (SCAN-FORMAT.md, "Lanes, in order").
fn decide<F: Fn(usize) -> Option<usize>>(i: DecideIn<'_, F>) -> Decision {
    let p = i.p;
    let nn = i.scan.nodes.len();
    let node = p.node.filter(|n| *n < nn);
    // 1. Secret.
    if let Some(kind) = &p.secret {
        return keep(
            format!("secret-named file ({kind}), stays local"),
            Why::Fleeting,
        );
    }
    // 2. Incomplete.
    if let Some(why) = &p.incomplete {
        return keep(
            format!("it cannot be judged whole: {why}; kept"),
            Why::Fleeting,
        );
    }
    // 3. Cannot act.
    if let Some(why) = i.process_block {
        return keep(why.to_string(), Why::Fleeting);
    }
    // 4. Active or unknown group.
    match i.state {
        ProjectState::Active => {
            return keep(
                format!("its group {} is active: {}; kept", p.group, i.state_reason),
                Why::Fleeting,
            )
        }
        ProjectState::Unknown => {
            return keep(
                format!(
                    "the activity of its group {} is unknown: {}; kept",
                    p.group, i.state_reason
                ),
                Why::Fleeting,
            )
        }
        ProjectState::Idle => {}
    }
    // 5. Inside a kept repository: the nearest enclosing git node other than the piece's own.
    if let Some(n) = node {
        let own_git = p.kind == PieceKind::GitNode;
        let mut cur = if own_git { (i.parent)(n) } else { Some(n) };
        let mut steps = 0usize;
        while let Some(c) = cur {
            steps += 1;
            if steps > nn {
                break;
            }
            if i.scan.nodes[c].class.is_git() {
                if let Some(gp) = i.git_piece[c] {
                    if let Some(d) = &i.decided[gp] {
                        if d.lane == Lane::Keep {
                            return keep(
                                format!(
                                    "inside the kept repository {}; kept",
                                    i.scan.nodes[c].path
                                ),
                                Why::Durable,
                            );
                        }
                    }
                }
            }
            cur = (i.parent)(c);
        }
    }
    // 6. A git node.
    if p.kind == PieceKind::GitNode {
        let item = node.and_then(|n| i.item_of[n]);
        return match item {
            Some(it)
                if matches!(
                    it.action,
                    Action::BackupThenRemove | Action::RecoverBackupThenRemove
                ) =>
            {
                let recovery = if it.action == Action::BackupThenRemove {
                    Recovery::None
                } else {
                    recovery_for(i.scan, node.expect("a git-node piece has a node"))
                };
                Decision {
                    lane: Lane::Git,
                    reason: format!("a git repository the plan removes: {}", it.reason),
                    evidence: None,
                    git: Some(GitLane {
                        action: it.action,
                        preserve: true,
                        excludes_secret: secret_paths_under(i.survey, &p.path),
                        recovery,
                    }),
                    why: Why::Removed,
                    state: ProjectState::Unknown,
                    state_reason: String::new(),
                }
            }
            Some(it) => keep(
                format!(
                    "its repository is not removed ({}): {}",
                    it.action.as_str(),
                    it.reason
                ),
                Why::Durable,
            ),
            None => keep(
                "a git repository with no plan item; kept".to_string(),
                Why::Durable,
            ),
        };
    }
    // 7. Noise by rule.
    if let Some(n) = node {
        let nd = &i.scan.nodes[n];
        if nd.class.is_artifact() {
            if let Some(it) = i.item_of[n] {
                if it.action == Action::RemoveRegenerable {
                    let rule = nd.rule.clone().unwrap_or_default();
                    let markers = i
                        .rules
                        .rules()
                        .iter()
                        .find(|r| r.name == rule)
                        .map(|r| r.markers.clone())
                        .unwrap_or_default();
                    let class = if nd.class == Class::Cache {
                        "cache"
                    } else {
                        "regenerable"
                    };
                    return Decision {
                        lane: Lane::Noise,
                        reason: format!(
                            "a {class} directory (rule {rule}) that can be regenerated: {}",
                            it.reason
                        ),
                        evidence: Some(Evidence::Rule {
                            rule,
                            markers,
                            node: n,
                        }),
                        git: None,
                        why: Why::Removed,
                        state: ProjectState::Unknown,
                        state_reason: String::new(),
                    };
                }
            }
        }
    }
    // 8. Backup.
    Decision {
        lane: Lane::Backup,
        reason: "no rule shows it can be regenerated, so it is backed up before it is removed"
            .to_string(),
        evidence: None,
        git: None,
        why: Why::Removed,
        state: ProjectState::Unknown,
        state_reason: String::new(),
    }
}

/// Judge every group of pieces outside every node: newest member time over all its pieces, an
/// `--active` path covering or inside it, a running process inside it.
fn group_states(
    survey: &PieceSurvey,
    opts: &PlanOptions,
) -> HashMap<String, (ProjectState, String)> {
    let mut newest: HashMap<&str, Option<i64>> = HashMap::new();
    for p in survey.pieces.iter().filter(|p| p.node.is_none()) {
        let slot = newest.entry(p.group.as_str()).or_insert(None);
        *slot = match (*slot, p.newest_mtime) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, None) => a,
            (None, b) => b,
        };
    }
    newest
        .into_iter()
        .map(|(g, t)| (g.to_string(), judge_group(g, t, opts)))
        .collect()
}

fn judge_group(group: &str, last: Option<i64>, opts: &PlanOptions) -> (ProjectState, String) {
    let path = Path::new(group);
    if let Some(a) = crate::plan::under_active(path, &opts.active) {
        return (
            ProjectState::Active,
            format!("covered by the active path {a}"),
        );
    }
    let real = crate::plan::canon(path);
    if let Some(c) = opts
        .process
        .inside(path)
        .or_else(|| opts.process.inside(&real))
    {
        return (
            ProjectState::Active,
            format!(
                "in use: {} (working directory of a running process)",
                c.display()
            ),
        );
    }
    let window = opts.active_window_days;
    match last {
        Some(t) => {
            let age = opts.now.saturating_sub(t).max(0);
            let d = (age / 86_400) as u64;
            let window_secs = i64::try_from(window)
                .unwrap_or(i64::MAX)
                .saturating_mul(86_400);
            if age <= window_secs {
                (
                    ProjectState::Active,
                    format!(
                        "newest file {} ago, within the {window}-day active window",
                        count(d, "day", "days")
                    ),
                )
            } else {
                (
                    ProjectState::Idle,
                    format!(
                        "newest file {} ago, outside the {window}-day active window",
                        count(d, "day", "days")
                    ),
                )
            }
        }
        None => (
            ProjectState::Unknown,
            "no activity evidence (no files found)".to_string(),
        ),
    }
}

/// The recovery of a `recover-backup-then-remove` git node: a pushable remote (`origin` first)
/// with the decision deferred to apply, or a bundle.
fn recovery_for(scan: &ScanResult, node: usize) -> Recovery {
    let remotes = match scan.nodes[node].repo.as_ref() {
        Some(r) => &r.remotes,
        None => return Recovery::BundleToBackup,
    };
    let mut ok: Vec<(&str, String)> = remotes
        .iter()
        .filter_map(|r| pushable_url(&r.url).map(|u| (r.name.as_str(), u)))
        .collect();
    ok.sort_by(|a, b| (a.0 != "origin", a.0).cmp(&(b.0 != "origin", b.0)));
    match ok.into_iter().next() {
        Some((name, url)) => Recovery::PushRecoveryRefs {
            remote: name.to_string(),
            url,
            fallback: "bundle-to-backup".to_string(),
            decided_at: "apply".to_string(),
            note: "the plan does not know whether this remote is private: apply pushes recovery \
                   refs only after it establishes that it is, and otherwise bundles the unsaved \
                   work into the backup"
                .to_string(),
        },
        None => Recovery::BundleToBackup,
    }
}

/// A remote URL that recovery refs could be pushed to, without userinfo, query or fragment:
/// `https://`, `ssh://` or scp-like `user@host:path`. Anything else is `None`.
pub fn pushable_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    for scheme in ["https://", "ssh://"] {
        if lower.starts_with(scheme) {
            let rest = &url[scheme.len()..];
            let rest = rest.split(['?', '#']).next().unwrap_or("");
            let (authority, path) = match rest.find('/') {
                Some(k) => (&rest[..k], &rest[k..]),
                None => (rest, ""),
            };
            let host = match authority.rfind('@') {
                Some(k) => &authority[k + 1..],
                None => authority,
            };
            if host.is_empty() {
                return None;
            }
            // Keep a plain user name for ssh (it carries no secret); drop any userinfo for https.
            let user = if scheme == "ssh://" {
                authority
                    .rfind('@')
                    .map(|k| &authority[..k])
                    .filter(|u| !u.contains(':'))
                    .map(|u| format!("{u}@"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            return Some(format!("{}{user}{host}{path}", &url[..scheme.len()]));
        }
    }
    if url.contains("://") {
        return None;
    }
    // scp-like: user@host:path, with no slash before the colon.
    let colon = url.find(':')?;
    let (before, path) = (&url[..colon], &url[colon + 1..]);
    if before.contains('/') || path.is_empty() {
        return None;
    }
    let at = before.find('@')?;
    let (user, host) = (&before[..at], &before[at + 1..]);
    if user.is_empty() || host.is_empty() || host.contains('@') {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    Some(format!("{user}@{host}:{path}"))
}

/// Paths of secret pieces below `dir`, relative to it and `/`-separated, sorted.
fn secret_paths_under(survey: &PieceSurvey, dir: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in survey.pieces.iter().filter(|p| p.secret.is_some()) {
        let abs: Vec<String> = match p.kind {
            PieceKind::Files => p
                .members
                .iter()
                .map(|m| format!("{}/{}", p.path, m.path()))
                .collect(),
            _ => vec![p.path.clone()],
        };
        for a in abs {
            if let Some(r) = rel_path(dir, &a) {
                if !r.is_empty() {
                    out.push(r);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `path` relative to `base`, `/`-separated, if it lies inside it.
fn rel_path(base: &str, path: &str) -> Option<String> {
    let rel = Path::new(path).strip_prefix(Path::new(base)).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

fn find_member<'a>(p: &'a Piece, rel: &str) -> Option<&'a Member> {
    match p.members.binary_search_by(|m| m.path().cmp(rel)) {
        Ok(k) => p.members.get(k),
        Err(_) => p.members.iter().find(|m| m.path() == rel),
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::pushable_url;

    #[test]
    fn pushable_urls() {
        assert_eq!(
            pushable_url("https://user:pw@github.com/o/r.git?x=1#f").as_deref(),
            Some("https://github.com/o/r.git")
        );
        assert_eq!(
            pushable_url("ssh://git@host:22/o/r").as_deref(),
            Some("ssh://git@host:22/o/r")
        );
        assert_eq!(
            pushable_url("git@github.com:o/r.git").as_deref(),
            Some("git@github.com:o/r.git")
        );
        assert_eq!(pushable_url("http://host/r"), None);
        assert_eq!(pushable_url("git://host/r"), None);
        assert_eq!(pushable_url("file:///srv/r"), None);
        assert_eq!(pushable_url("/srv/r.git"), None);
        assert_eq!(pushable_url("host:r.git"), None);
        assert_eq!(pushable_url("./a@b:c"), None);
    }
}
