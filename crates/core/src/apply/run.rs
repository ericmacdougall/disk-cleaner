//! The apply run: validate, prepare each piece (recheck, activity, secrets, lane evidence),
//! upload what is not yet in storage, then for each fully accounted piece write its rows,
//! mirror the manifest, recheck again and unlink member by member.

use super::fsops::{self, Ident, Unlinked};
use super::input::{Evidence, Inputs, Lane, PieceKind, PlanPiece};
use super::manifest::{Manifest, Row, Segment};
use super::recheck::{self, Member};
use super::shard::{self, BlobSrc, UploadHooks};
use super::storage::Rclone;
use super::{canonical, names, ApplyError, ApplyOptions, ApplyReport, CrashPoint, PieceReport};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// What accounts for one member's content.
#[derive(Debug, Clone)]
pub(crate) enum Content {
    Blob {
        sha256: String,
    },
    Link {
        target: String,
    },
    /// Raw-identical to a HEAD blob of a git directory that is itself backed up.
    #[allow(dead_code)] // built by the vcs lane (next commit)
    GitBlob {
        oid: String,
        sha256: String,
        git_dir: PathBuf,
    },
    /// Noise with re-checked evidence.
    Noise,
}

#[derive(Debug, Clone)]
pub(crate) struct PMember {
    pub rel: String,
    pub id: Ident,
    pub content: Content,
}

/// A piece ready for the backup and removal steps.
#[derive(Debug, Clone)]
pub(crate) struct Prepared {
    pub piece: usize,
    pub members: Vec<PMember>,
    pub dirs: Vec<String>,
    pub remove_root: bool,
    pub extra_rows: Vec<Row>,
    pub restore: String,
    pub reason: String,
}

fn random_id(now: i64) -> String {
    let mut h = Sha256::new();
    h.update(now.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(t.to_le_bytes());
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut b = [0u8; 16];
        if f.read_exact(&mut b).is_ok() {
            h.update(b);
        }
    }
    format!("{now}-{}", &hex::encode(h.finalize())[..16])
}

struct Lock {
    _fd: rustix::fd::OwnedFd,
}

fn take_lock(dir: &Path) -> Result<Lock, ApplyError> {
    std::fs::create_dir_all(dir).map_err(|e| ApplyError::Io(format!("{}: {e}", dir.display())))?;
    let p = dir.join("apply.lock");
    let fd = rustix::fs::open(
        &p,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(|e| ApplyError::Io(format!("{}: {e}", p.display())))?;
    rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| ApplyError::Locked(p.clone()))?;
    Ok(Lock { _fd: fd })
}

/// Newest mtime of any non-directory entry under `dir` (not following symlinks, skipping
/// `.git` directories). Errors count as "now" so the group is judged active (unknown means keep).
fn newest_mtime(dir: &Path, cap_now: i64) -> i64 {
    let mut newest = 0i64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            return cap_now;
        };
        for ent in rd {
            let Ok(ent) = ent else { return cap_now };
            let Ok(md) = ent.path().symlink_metadata() else {
                return cap_now;
            };
            if md.is_dir() {
                if ent.file_name() != ".git" {
                    stack.push(ent.path());
                }
            } else {
                use std::os::unix::fs::MetadataExt;
                newest = newest.max(md.mtime());
            }
        }
    }
    newest
}

pub(crate) struct Ctx<'a> {
    pub opts: &'a ApplyOptions,
    pub inputs: &'a Inputs,
    pub rc: Rclone,
    pub manifest: Manifest,
    pub group_cache: BTreeMap<PathBuf, Option<String>>,
    /// (dev, ino) of entries unlinked in this run (hard-link ctime drift).
    pub unlinked_inodes: std::collections::BTreeSet<(u64, u64)>,
    pub report: ApplyReport,
}

impl Ctx<'_> {
    fn group_active(&mut self, p: &PlanPiece) -> Option<String> {
        let group = p.group.clone().unwrap_or_else(|| p.path.clone());
        if let Some(c) = self.group_cache.get(&group) {
            return c.clone();
        }
        let mut why = None;
        match &self.opts.process {
            crate::ProcessCheck::Unknown { reason } => {
                why = Some(format!("the process check failed: {reason}"))
            }
            pc => {
                if let Some(cwd) = pc.inside(&group) {
                    why = Some(format!("a running process works in {}", cwd.display()));
                }
            }
        }
        if why.is_none() {
            if let Some(a) = self
                .inputs
                .active
                .iter()
                .find(|a| group.starts_with(a) || a.starts_with(&group))
            {
                why = Some(format!("{} is marked active", a.display()));
            }
        }
        if why.is_none() && group.exists() {
            let n = newest_mtime(&group, self.opts.now);
            // Same second as the scan counts as after it: the conservative side.
            if n >= self.inputs.scan_generated_at {
                why = Some(format!("{} was modified after the scan", group.display()));
            }
        }
        self.group_cache.insert(group, why.clone());
        why
    }
}

fn rel_join(path: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        path.to_path_buf()
    } else {
        path.join(rel)
    }
}

/// Run apply.
pub fn apply(
    scan_text: &str,
    plan_text: &str,
    opts: &ApplyOptions,
) -> Result<ApplyReport, ApplyError> {
    let inputs = super::parse_inputs(scan_text, plan_text, opts)?;
    let run = random_id(opts.now);
    let _lock = if opts.act {
        Some(take_lock(&opts.state_dir)?)
    } else {
        None
    };
    let mut manifest = Manifest::load(&opts.manifest, &run)?;
    #[cfg(feature = "test-hooks")]
    {
        manifest.fail_after = opts.hooks.manifest_fail_after;
    }
    let rc = Rclone {
        bin: opts.rclone.clone(),
        root: opts.remote.clone(),
        timeout: opts.shard_timeout,
    };
    let mut ctx = Ctx {
        opts,
        inputs: &inputs,
        rc,
        manifest,
        group_cache: BTreeMap::new(),
        unlinked_inodes: Default::default(),
        report: ApplyReport {
            dry_run: !opts.act,
            run: run.clone(),
            ..Default::default()
        },
    };
    let rederived = opts.oracle.rederive(scan_text, plan_text);
    if opts.act && !ctx.manifest.rows.is_empty() {
        seed_unmirrored(&mut ctx)?;
    }
    let begun = ctx.manifest.begun();
    let done = ctx.manifest.done();

    // 1. Prepare every removing piece.
    let mut prepared: Vec<Prepared> = Vec::new();
    for p in &inputs.pieces {
        let mut rep = PieceReport {
            id: p.id,
            path: p.path.clone(),
            lane: p.lane,
            outcome: String::new(),
            reason: p.reason.clone(),
            restore: String::new(),
            bytes: p.bytes,
        };
        if p.lane == Lane::Keep {
            rep.outcome = "kept".into();
            ctx.report.pieces.push(rep);
            continue;
        }
        let key = (p.path.clone(), p.id);
        if !begun.contains_key(&key) && done.contains(&key) && already_gone(p) {
            rep.outcome = "removed".into();
            rep.reason = "already removed by an earlier run".into();
            ctx.report.pieces.push(rep);
            continue;
        }
        let res = if let Some(run) = begun.get(&key) {
            resume_prepared(&ctx, p, run)
        } else {
            prepare(&mut ctx, p, rederived.as_ref())
        };
        match res {
            Ok(pr) => {
                rep.restore = pr.restore.clone();
                rep.outcome = if opts.act {
                    "pending".into()
                } else {
                    dry_word(p.lane).into()
                };
                if !pr.reason.is_empty() {
                    rep.reason = pr.reason.clone();
                }
                prepared.push(pr);
            }
            Err(why) => {
                rep.outcome = "kept".into();
                rep.reason = why;
            }
        }
        ctx.report.pieces.push(rep);
    }

    if !opts.act {
        // Dry run: count what would be uploaded against the local manifest only (no network).
        let known = ctx.manifest.blobs(&opts.remote);
        let mut seen = BTreeSet::new();
        let mut bytes = 0u64;
        for pr in &prepared {
            for m in &pr.members {
                if let Content::Blob { sha256 } = &m.content {
                    if !known.contains_key(sha256) && seen.insert(sha256.clone()) {
                        bytes += m.id.len;
                    }
                }
            }
        }
        ctx.report.uploaded_bytes = bytes;
        return Ok(ctx.report);
    }

    // 2. Upload every blob not already verified in storage.
    let available = upload_phase(&mut ctx, &prepared)?;

    // 3. Remove, in batches, every piece whose content is all accounted for.
    let mut ready: Vec<Prepared> = Vec::new();
    for pr in prepared {
        let missing = pr.members.iter().find(|m| match &m.content {
            Content::Blob { sha256 } => !available.contains(sha256),
            _ => false,
        });
        if let Some(m) = missing {
            set_outcome(
                &mut ctx.report,
                pr.piece,
                "kept",
                &format!("{} could not be backed up (its shard failed)", m.rel),
            );
            continue;
        }
        ready.push(pr);
    }
    let result = remove_phase(&mut ctx, ready);
    // Closing mirror, whatever happened (revision 2c, finding 4).
    let _ = mirror(&mut ctx);
    result?;
    Ok(ctx.report)
}

/// Nothing of the piece is left on disk (a re-run after a completed removal).
fn already_gone(p: &PlanPiece) -> bool {
    match p.kind {
        PieceKind::Files => p
            .files
            .iter()
            .all(|f| p.path.join(f).symlink_metadata().is_err()),
        _ => p.path.symlink_metadata().is_err(),
    }
}

fn dry_word(l: Lane) -> &'static str {
    match l {
        Lane::Backup => "would back up, verify, then remove",
        Lane::Noise => "would remove (evidence re-checked)",
        Lane::Git => "would recover, back up the git directory, then remove",
        Lane::Keep => "kept",
    }
}

fn set_outcome(rep: &mut ApplyReport, piece: usize, outcome: &str, reason: &str) {
    if let Some(r) = rep.pieces.iter_mut().find(|r| r.id == piece) {
        r.outcome = outcome.to_string();
        if !reason.is_empty() {
            r.reason = reason.to_string();
        }
    }
}

/// Secret check over members (C2, revision 3 F4). Inside an artifact (noise) only the
/// non-substring rules apply, like the scan.
fn secret_member(members: &[Member], lane: Lane) -> Option<String> {
    members.iter().find_map(|m| {
        let hit = if lane == Lane::Noise {
            names::rel_is_secret_strict(&m.rel)
        } else {
            names::rel_is_secret(&m.rel)
        };
        hit.then(|| m.rel.clone())
    })
}

fn prepare(
    ctx: &mut Ctx,
    p: &PlanPiece,
    rederived: Option<&BTreeMap<usize, String>>,
) -> Result<Prepared, String> {
    let sp = &ctx.inputs.scan_pieces[p.scan_piece];
    if p.secret.is_some() && !ctx.opts.secrets_upload {
        return Err("secret-named, stays local (upload only with --secrets upload)".into());
    }
    if p.lane == Lane::Noise && p.secret.is_some() {
        return Err("a secret is never removed without a backup".into());
    }
    if let Some(why) = ctx.group_active(p) {
        return Err(format!("active at apply time: {why}"));
    }
    if matches!(p.lane, Lane::Noise | Lane::Git) {
        let ok = rederived
            .and_then(|m| m.get(&p.id))
            .is_some_and(|c| *c == canonical(p));
        if !ok {
            return Err(if rederived.is_none() {
                "the plan's lane could not be re-derived from the scan by this build, so nothing is removed without a backup".into()
            } else {
                "the plan differs from what the scan supports".into()
            });
        }
    }
    let removed = ctx.manifest.removed_under(p.scan_piece, &sp.path);
    recheck::recheck(sp, &removed).map_err(|d| format!("changed since the scan: {}", d.0))?;
    let en = recheck::enumerate(p.kind, &p.path, &p.files, &p.excludes).map_err(|d| d.0)?;
    if let Some(m) = en.members.iter().find(|m| m.id.kind == 'o') {
        return Err(format!("{} is a FIFO, socket or device file", m.rel));
    }
    if !(ctx.opts.secrets_upload && p.lane != Lane::Noise) {
        if let Some(s) = secret_member(&en.members, p.lane) {
            return Err(format!(
                "{s} has a secret name and stays local (upload only with --secrets upload)"
            ));
        }
    }
    match p.lane {
        Lane::Backup => prepare_backup(p, &en),
        Lane::Noise => prepare_noise(ctx, p, &en),
        Lane::Git => super::gitlane::prepare(ctx.opts, ctx.inputs, p, &en),
        Lane::Keep => Err("kept".into()),
    }
}

/// Hash every regular member, read every link: the content to upload.
pub(crate) fn hash_members(root: &Path, members: &[Member]) -> Result<Vec<PMember>, String> {
    let mut out = Vec::with_capacity(members.len());
    for m in members {
        match m.id.kind {
            'f' => {
                let (sha, id) = shard::hash_file(root, &m.rel)?;
                if !id.same_strict(&m.id) {
                    return Err(format!("{} changed after it was listed", m.rel));
                }
                out.push(PMember {
                    rel: m.rel.clone(),
                    id,
                    content: Content::Blob { sha256: sha },
                });
            }
            'l' => {
                let r = fsops::open_dir_abs(root).map_err(|e| e.to_string())?;
                let t = fsops::readlink_rel(&r, &m.rel).map_err(|e| format!("{}: {e}", m.rel))?;
                let target = String::from_utf8(t)
                    .map_err(|_| format!("{}: link target is not UTF-8", m.rel))?;
                out.push(PMember {
                    rel: m.rel.clone(),
                    id: m.id,
                    content: Content::Link { target },
                });
            }
            _ => return Err(format!("{} is not a regular file or symlink", m.rel)),
        }
    }
    Ok(out)
}

fn prepare_backup(p: &PlanPiece, en: &recheck::Enumerated) -> Result<Prepared, String> {
    let members = hash_members(&p.path, &en.members)?;
    Ok(Prepared {
        piece: p.id,
        members,
        dirs: en.dirs.clone(),
        remove_root: p.kind == PieceKind::Tree,
        extra_rows: Vec::new(),
        restore: "disk-cleaner restore --path <path>".into(),
        reason: String::new(),
    })
}

fn prepare_noise(ctx: &Ctx, p: &PlanPiece, en: &recheck::Enumerated) -> Result<Prepared, String> {
    let ev = p.evidence.as_ref().ok_or("noise without evidence")?;
    let mut extra = Vec::new();
    let restore = match ev {
        Evidence::Rule {
            rule,
            markers,
            node,
        } => {
            let item = ctx
                .inputs
                .items
                .iter()
                .find(|i| i.node == *node)
                .ok_or("the evidence names a node without a plan item")?;
            if item.action != "remove-regenerable" || !p.path.starts_with(&item.path) {
                return Err("the evidence's plan item does not remove this directory".into());
            }
            let base = item.path.parent().unwrap_or(&item.path);
            for m in markers {
                let mp = Path::new(m);
                let found = if mp.is_absolute() {
                    mp.symlink_metadata().is_ok()
                } else {
                    base.ancestors()
                        .any(|a| a.join(m).symlink_metadata().is_ok())
                };
                if !found {
                    return Err(format!("the marker {m} is gone"));
                }
            }
            extra.push(Row::Regenerable {
                plan_piece: p.id,
                path: p.path.clone(),
                rule: rule.clone(),
                markers: markers.clone(),
            });
            format!("regenerate ({rule})")
        }
        Evidence::Duplicate {
            sha256,
            size,
            retained_copy,
            retained_piece,
        } => {
            if p.kind != PieceKind::Files || en.members.len() != 1 {
                return Err("duplicate evidence on a piece that is not one file".into());
            }
            let m = &en.members[0];
            let me = p.path.join(&m.rel);
            if me == *retained_copy {
                return Err("the retained copy is the duplicate itself".into());
            }
            let (got, _) = shard::hash_file(&p.path, &m.rel)?;
            if got != *sha256 || m.id.len != *size {
                return Err(
                    "the duplicate no longer has the recorded content; it is backed up instead"
                        .into(),
                );
            }
            let kept_ok = retained_piece
                .and_then(|k| ctx.inputs.pieces.get(k))
                .is_some_and(|rp| rp.lane == Lane::Keep && retained_copy.starts_with(&rp.path));
            if !kept_ok {
                return Err("the retained copy is not in a kept piece".into());
            }
            let rmd = retained_copy
                .symlink_metadata()
                .map_err(|_| "the retained copy is gone")?;
            if !rmd.file_type().is_file() || rmd.len() != *size {
                return Err(
                    "the retained copy is not a regular file of the recorded length".into(),
                );
            }
            let parent = retained_copy
                .parent()
                .ok_or("retained copy has no parent")?;
            let leaf = retained_copy
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("retained name is not UTF-8")?;
            let (rsha, _) = shard::hash_file(parent, leaf)?;
            if rsha != *sha256 {
                return Err("the retained copy no longer has the recorded content".into());
            }
            extra.push(Row::Retains {
                plan_piece: p.id,
                path: me,
                retained: retained_copy.clone(),
                sha256: sha256.clone(),
                size: *size,
            });
            format!("copy of {}", retained_copy.display())
        }
        Evidence::Archive { .. } => {
            return Err("archive evidence is not produced by plan; kept".into())
        }
    };
    Ok(Prepared {
        piece: p.id,
        members: en
            .members
            .iter()
            .map(|m| PMember {
                rel: m.rel.clone(),
                id: m.id,
                content: Content::Noise,
            })
            .collect(),
        dirs: en.dirs.clone(),
        remove_root: p.kind == PieceKind::Tree,
        extra_rows: extra,
        restore,
        reason: String::new(),
    })
}

/// A begun piece (remove-begin without remove-done): continue from its file rows (I-4 resume).
fn resume_prepared(ctx: &Ctx, p: &PlanPiece, run: &str) -> Result<Prepared, String> {
    let rows = ctx.manifest.file_rows_for(p.id, run);
    let mut members = Vec::new();
    let r = fsops::open_dir_abs(&p.path).ok();
    for (path, row) in rows {
        let Row::File {
            plan_piece,
            rel,
            id,
            sha256,
            link_target,
            ..
        } = row
        else {
            continue;
        };
        if plan_piece != p.id {
            continue;
        }
        let _ = path;
        if let Some(rfd) = &r {
            if let Ok(now) = fsops::lstat_rel(rfd, &rel) {
                if !now.same_loose(&id) {
                    return Err(format!("{rel} changed while the removal was interrupted"));
                }
                if let Some(s) = &sha256 {
                    let (got, _) = shard::hash_file(&p.path, &rel)?;
                    if got != *s {
                        return Err(format!("{rel} changed while the removal was interrupted"));
                    }
                }
            }
        }
        let content = match (sha256, link_target) {
            (Some(s), _) => Content::Blob { sha256: s },
            (None, Some(t)) => Content::Link { target: t },
            _ => Content::Noise,
        };
        members.push(PMember { rel, id, content });
    }
    let dirs: Vec<String> = ctx
        .manifest
        .rows
        .iter()
        .filter_map(|e| match &e.row {
            Row::Dir {
                plan_piece, path, ..
            } if *plan_piece == p.id && e.run == run => path
                .strip_prefix(&p.path)
                .ok()
                .map(|r| r.to_string_lossy().to_string()),
            _ => None,
        })
        .filter(|r| !r.is_empty())
        .collect();
    Ok(Prepared {
        piece: p.id,
        members,
        dirs,
        remove_root: p.kind == PieceKind::Tree,
        extra_rows: Vec::new(),
        restore: "disk-cleaner restore --path <path>".into(),
        reason: "resuming an interrupted removal".into(),
    })
}

/// Upload every needed blob; returns the set of sha256 available (verified) in storage.
fn upload_phase(ctx: &mut Ctx, prepared: &[Prepared]) -> Result<BTreeSet<String>, ApplyError> {
    let opts = ctx.opts;
    let mut needed: BTreeMap<String, BlobSrc> = BTreeMap::new();
    for pr in prepared {
        let path = &ctx.inputs.pieces[pr.piece].path;
        for m in &pr.members {
            if let Content::Blob { sha256 } = &m.content {
                needed.entry(sha256.clone()).or_insert_with(|| BlobSrc {
                    sha256: sha256.clone(),
                    size: m.id.len,
                    root: path.clone(),
                    rel: m.rel.clone(),
                    id: m.id,
                });
            }
        }
    }
    let known_blobs = ctx.manifest.blobs(&opts.remote);
    let known_shards = ctx.manifest.shards(&opts.remote);
    let mut available = BTreeSet::new();
    let mut confirmed: BTreeMap<String, bool> = BTreeMap::new();
    let mut to_upload: Vec<BlobSrc> = Vec::new();
    for (sha, src) in needed {
        let reuse = known_blobs.get(&sha).is_some_and(|(_, segs)| {
            segs.iter().all(|s| {
                *confirmed.entry(s.shard.clone()).or_insert_with(|| {
                    let want = known_shards.get(&s.shard).copied();
                    match (want, ctx.rc.stat(&s.shard)) {
                        (Some(w), Ok(Some(l))) => l.size == w,
                        _ => false,
                    }
                })
            })
        });
        if reuse {
            available.insert(sha);
        } else {
            to_upload.push(src);
        }
    }
    to_upload.sort_by(|a, b| a.sha256.cmp(&b.sha256));
    let shards = shard::pack(&to_upload, opts.shard_bytes, &opts.label);
    #[allow(unused_mut)]
    let mut hooks = UploadHooks::default();
    #[cfg(feature = "test-hooks")]
    {
        hooks.force_readback = opts.hooks.force_readback;
    }
    let results = shard::upload_all(&ctx.rc, &shards, &to_upload, opts.jobs, &hooks);
    let mut ok_shards = BTreeSet::new();
    let mut rows = Vec::new();
    for (sh, r) in shards.iter().zip(results) {
        match r {
            Ok(v) => {
                ok_shards.insert(sh.id.clone());
                ctx.report.shards_uploaded += 1;
                ctx.report.uploaded_bytes += sh.size;
                rows.push(Row::Shard {
                    remote: opts.remote.clone(),
                    id: v.id,
                    size: v.size,
                    verified_by: v.verified_by,
                });
            }
            Err(e) => {
                eprintln!("shard {} failed: {}", sh.id, crate::redact::redact_text(&e));
                ctx.report.shards_failed.push(sh.id.clone());
            }
        }
    }
    let segs = shard::blob_segments(&shards, to_upload.len());
    for (bi, b) in to_upload.iter().enumerate() {
        if segs[bi]
            .iter()
            .all(|(si, _)| ok_shards.contains(&shards[*si].id))
        {
            let segments: Vec<Segment> = segs[bi].iter().map(|(_, s)| s.clone()).collect();
            rows.push(Row::Blob {
                remote: opts.remote.clone(),
                sha256: b.sha256.clone(),
                size: b.size,
                segments,
            });
            available.insert(b.sha256.clone());
        }
    }
    ctx.manifest.append(opts.now, rows)?;
    opts.crash(CrashPoint::AfterBlobRows)?;
    Ok(available)
}

/// Queue for mirroring every local row the remote copy lacks (a run that stopped before its
/// mirror leaves rows only in the local file). Rows naming another remote stay local.
fn seed_unmirrored(ctx: &mut Ctx) -> Result<(), ApplyError> {
    let remote_rows = super::restore::rows_from_remote(&ctx.rc, &ctx.opts.label)?;
    let line = |e: &super::manifest::Envelope| serde_json::to_string(e).unwrap_or_default();
    let have: std::collections::BTreeSet<String> = remote_rows.iter().map(line).collect();
    for e in &ctx.manifest.rows {
        let other_remote = match &e.row {
            Row::Shard { remote, .. } | Row::Blob { remote, .. } => remote != &ctx.opts.remote,
            _ => false,
        };
        let l = line(e);
        if !other_remote && !have.contains(&l) {
            ctx.manifest.unmirrored.push(l);
        }
    }
    Ok(())
}

/// Upload the manifest rows not yet mirrored as a new immutable object, verified.
fn mirror(ctx: &mut Ctx) -> Result<(), ApplyError> {
    if ctx.manifest.unmirrored.is_empty() {
        return Ok(());
    }
    let mut text = ctx.manifest.unmirrored.join("\n");
    text.push('\n');
    let seq = ctx.manifest.rows.len();
    let name = format!(
        "manifests/{}/{}-{:08}.jsonl",
        ctx.opts.label, ctx.manifest.run, seq
    );
    let bytes = text.into_bytes();
    let b2 = bytes.clone();
    ctx.rc
        .rcat(&name, bytes.len() as u64, move |w| {
            w.write_all(&b2).map_err(|e| e.to_string())
        })
        .map_err(ApplyError::Storage)?;
    use sha1::Sha1;
    let sha1 = hex::encode(Sha1::digest(&bytes));
    let md5 = hex::encode(md5::Md5::digest(&bytes));
    let sha256 = hex::encode(Sha256::digest(&bytes));
    shard::verify(
        &ctx.rc,
        &name,
        bytes.len() as u64,
        &sha1,
        &md5,
        &sha256,
        &UploadHooks::default(),
    )
    .map_err(ApplyError::Storage)?;
    ctx.manifest.unmirrored.clear();
    Ok(())
}

fn remove_phase(ctx: &mut Ctx, mut ready: Vec<Prepared>) -> Result<(), ApplyError> {
    let opts = ctx.opts;
    // Deepest first, and files pieces before the tree at the same path, so a residual tree or a
    // parent with excluded child pieces finds its directory empty when it removes it.
    ready.sort_by_key(|pr| {
        let p = &ctx.inputs.pieces[pr.piece];
        (
            std::cmp::Reverse(p.path.components().count()),
            p.kind != PieceKind::Files,
            p.id,
        )
    });
    for batch in ready.chunks(64) {
        let mut go: Vec<&Prepared> = Vec::new();
        let mut rows = Vec::new();
        let begun_now = ctx.manifest.begun();
        for pr in batch {
            let p = &ctx.inputs.pieces[pr.piece];
            let begun = begun_now.contains_key(&(p.path.clone(), p.id));
            if !begun {
                // Second recheck (contract (b)): after the backup, before the first removal.
                let sp = &ctx.inputs.scan_pieces[p.scan_piece];
                let removed = ctx.manifest.removed_under(p.scan_piece, &sp.path);
                #[cfg(feature = "test-hooks")]
                if let Some(f) = opts.hooks.before_remove {
                    f(&p.path);
                }
                if let Err(d) = recheck::recheck(sp, &removed) {
                    set_outcome(
                        &mut ctx.report,
                        pr.piece,
                        "kept",
                        &format!("changed during the backup: {}", d.0),
                    );
                    continue;
                }
                for m in &pr.members {
                    rows.push(file_row(p, pr, m));
                }
                for d in &pr.dirs {
                    rows.push(Row::Dir {
                        plan_piece: p.id,
                        path: p.path.join(d),
                        mode: 0o755,
                    });
                }
                rows.extend(pr.extra_rows.iter().cloned());
            }
            go.push(pr);
        }
        ctx.manifest.append(opts.now, rows)?;
        opts.crash(CrashPoint::AfterFileRows)?;
        let mut begins = Vec::new();
        for pr in &go {
            let p = &ctx.inputs.pieces[pr.piece];
            if !begun_now.contains_key(&(p.path.clone(), p.id)) {
                begins.push(Row::RemoveBegin {
                    plan_piece: p.id,
                    scan_piece: p.scan_piece,
                    path: p.path.clone(),
                });
            }
        }
        ctx.manifest.append(opts.now, begins)?;
        opts.crash(CrashPoint::AfterRemoveBegin)?;
        mirror(ctx)?;
        opts.crash(CrashPoint::AfterMirror)?;
        for pr in go {
            unlink_piece(ctx, pr)?;
        }
    }
    Ok(())
}

fn file_row(p: &PlanPiece, pr: &Prepared, m: &PMember) -> Row {
    let (sha256, git_oid, git_dir, link_target) = match &m.content {
        Content::Blob { sha256 } => (Some(sha256.clone()), None, None, None),
        Content::Link { target } => (None, None, None, Some(target.clone())),
        Content::GitBlob {
            oid,
            sha256,
            git_dir,
        } => (
            Some(sha256.clone()),
            Some(oid.clone()),
            Some(git_dir.clone()),
            None,
        ),
        Content::Noise => (None, None, None, None),
    };
    let lane = p.lane.as_str().to_string();
    let path = rel_join(&p.path, &m.rel);
    let restore = match &m.content {
        Content::Noise => pr.restore.clone(),
        _ => format!(
            "disk-cleaner restore --path {}",
            shell_quote(&path.to_string_lossy())
        ),
    };
    Row::File {
        plan_piece: p.id,
        scan_piece: p.scan_piece,
        lane,
        path,
        rel: m.rel.clone(),
        id: m.id,
        sha256,
        git_oid,
        git_dir,
        link_target,
        link_group: None,
        reason: p.reason.clone(),
        restore,
    }
}

/// POSIX shell quoting for printed commands.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn unlink_piece(ctx: &mut Ctx, pr: &Prepared) -> Result<(), ApplyError> {
    let opts = ctx.opts;
    let p = &ctx.inputs.pieces[pr.piece];
    let root = match fsops::open_dir_abs(&p.path) {
        Ok(r) => r,
        Err(e) => {
            set_outcome(
                &mut ctx.report,
                pr.piece,
                "kept",
                &format!("cannot open {}: {e}", p.path.display()),
            );
            return Ok(());
        }
    };
    let loose = pr.reason == "resuming an interrupted removal";
    let mut rows = Vec::new();
    let mut kept = 0usize;
    let half = pr.members.len() / 2;
    for (k, m) in pr.members.iter().enumerate() {
        if k == half && k > 0 {
            ctx.manifest.append(opts.now, std::mem::take(&mut rows))?;
            opts.crash(CrashPoint::MidUnlink)?;
        }
        let path = rel_join(&p.path, &m.rel);
        // Unlinking one name of a hard-linked inode moves the ctime of its other names: compare
        // those without ctime (dev, ino, size and mtime still must match).
        let linked = ctx.unlinked_inodes.contains(&(m.id.dev, m.id.ino));
        match fsops::unlink_guarded(&root, &m.rel, &m.id, loose || linked) {
            Ok(Unlinked::Done) | Ok(Unlinked::Absent) => {
                ctx.unlinked_inodes.insert((m.id.dev, m.id.ino));
                ctx.report.removed_entries += 1;
                rows.push(Row::Removed {
                    plan_piece: p.id,
                    path,
                });
            }
            Ok(Unlinked::Changed(_)) => {
                kept += 1;
                rows.push(Row::Kept {
                    plan_piece: p.id,
                    path,
                    reason: "changed after it was backed up".into(),
                });
            }
            Err(e) => {
                kept += 1;
                rows.push(Row::Kept {
                    plan_piece: p.id,
                    path,
                    reason: format!("unlink failed: {e}"),
                });
            }
        }
    }
    let mut dirs = pr.dirs.clone();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for d in &dirs {
        fsops::rmdir_if_empty(&root, d);
    }
    drop(root);
    if pr.remove_root {
        if let (Some(parent), Some(leaf)) =
            (p.path.parent(), p.path.file_name().and_then(|n| n.to_str()))
        {
            if let Ok(pfd) = fsops::open_dir_abs(parent) {
                fsops::rmdir_if_empty(&pfd, leaf);
            }
        }
    }
    rows.push(Row::RemoveDone {
        plan_piece: p.id,
        path: p.path.clone(),
    });
    ctx.manifest.append(opts.now, rows)?;
    let outcome = if kept == 0 { "removed" } else { "partial" };
    let why = if kept == 0 {
        String::new()
    } else {
        format!("{kept} entries changed after the backup and were kept")
    };
    set_outcome(&mut ctx.report, pr.piece, outcome, &why);
    Ok(())
}
