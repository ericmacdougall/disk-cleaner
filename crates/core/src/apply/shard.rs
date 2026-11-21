//! Content-deduplicated, size-bounded shards uploaded by a pool of N rclone pipelines (spec C6,
//! I-3, AC-2/3/4/9). A shard is the raw concatenation of blob segments (no compression), named
//! by the sha256 of its segment descriptor so a resumed run produces the same name. Nothing is
//! staged on disk: each segment is read from its source file straight into rclone's stdin.

use super::fsops::{self, Ident};
use super::manifest::Segment;
use super::storage::Rclone;
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Where a blob's bytes come from: a regular file below `root`, with the identity it had when
/// it was hashed.
#[derive(Debug, Clone)]
pub struct BlobSrc {
    pub sha256: String,
    pub size: u64,
    pub root: PathBuf,
    pub rel: String,
    pub id: Ident,
}

/// A segment of a planned shard.
#[derive(Debug, Clone)]
pub struct PlannedSeg {
    pub blob: usize,
    pub blob_offset: u64,
    pub len: u64,
    pub shard_offset: u64,
}

/// A planned shard.
#[derive(Debug, Clone)]
pub struct PlannedShard {
    pub id: String,
    pub size: u64,
    pub segs: Vec<PlannedSeg>,
}

/// Pack blobs (sorted by sha256 by the caller) into shards of at most `bound` bytes; a blob that
/// does not fit is split across shards.
pub fn pack(blobs: &[BlobSrc], bound: u64, label: &str) -> Vec<PlannedShard> {
    let bound = bound.max(1);
    let mut shards: Vec<PlannedShard> = Vec::new();
    let mut cur: Vec<PlannedSeg> = Vec::new();
    let mut used = 0u64;
    for (bi, b) in blobs.iter().enumerate() {
        let mut off = 0u64;
        while off < b.size || (b.size == 0 && off == 0) {
            let room = bound - used;
            let take = (b.size - off).min(room);
            cur.push(PlannedSeg {
                blob: bi,
                blob_offset: off,
                len: take,
                shard_offset: used,
            });
            used += take;
            off += take;
            if used == bound {
                shards.push(finish(std::mem::take(&mut cur), used, blobs, label));
                used = 0;
            }
            if b.size == 0 {
                break;
            }
        }
    }
    if !cur.is_empty() {
        shards.push(finish(cur, used, blobs, label));
    }
    shards
}

fn finish(segs: Vec<PlannedSeg>, size: u64, blobs: &[BlobSrc], label: &str) -> PlannedShard {
    let mut h = Sha256::new();
    for s in &segs {
        h.update(format!("{} {} {}\n", blobs[s.blob].sha256, s.blob_offset, s.len).as_bytes());
    }
    let id = format!("shards/{label}/{}", hex::encode(h.finalize()));
    PlannedShard { id, size, segs }
}

/// A verified shard.
#[derive(Debug, Clone)]
pub struct Verified {
    pub id: String,
    pub size: u64,
    pub verified_by: String,
}

/// Test seams for the upload path.
#[derive(Debug, Clone, Default)]
pub struct UploadHooks {
    /// Treat the listing as having no usable hash, forcing a read-back (AC-4 read-back case).
    pub force_readback: bool,
}

struct Hashes {
    sha1: Sha1,
    md5: Md5,
    sha256: Sha256,
}

/// Stream one shard into rclone and verify it. Every source file must still have the identity
/// it had when it was hashed, before and after its bytes are read; a blob wholly inside this
/// shard must also hash to its sha256 as streamed (I-3).
pub fn upload_one(
    rc: &Rclone,
    sh: &PlannedShard,
    blobs: &[BlobSrc],
    hooks: &UploadHooks,
) -> Result<Verified, String> {
    let hashes = Arc::new(Mutex::new(Hashes {
        sha1: Sha1::new(),
        md5: Md5::new(),
        sha256: Sha256::new(),
    }));
    let h2 = hashes.clone();
    rc.rcat(&sh.id, sh.size, move |pipe: &mut dyn Write| {
        let mut buf = vec![0u8; 1 << 20];
        for s in &sh.segs {
            let b = &blobs[s.blob];
            let root =
                fsops::open_dir_abs(&b.root).map_err(|e| format!("{}: {e}", b.root.display()))?;
            let (mut f, id) =
                fsops::open_file_rel(&root, &b.rel).map_err(|e| format!("{}: {e}", b.rel))?;
            if !id.same_strict(&b.id) {
                return Err(format!("{} changed since it was hashed", b.rel));
            }
            f.seek(SeekFrom::Start(s.blob_offset))
                .map_err(|e| e.to_string())?;
            let whole = s.blob_offset == 0 && s.len == b.size;
            let mut bh = Sha256::new();
            let mut left = s.len;
            while left > 0 {
                let want = (left as usize).min(buf.len());
                let n = f.read(&mut buf[..want]).map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err(format!("{} is shorter than when it was hashed", b.rel));
                }
                let chunk = &buf[..n];
                {
                    let mut h = h2.lock().expect("hash lock");
                    h.sha1.update(chunk);
                    h.md5.update(chunk);
                    h.sha256.update(chunk);
                }
                if whole {
                    bh.update(chunk);
                }
                pipe.write_all(chunk)
                    .map_err(|e| format!("writing to rclone: {e}"))?;
                left -= n as u64;
            }
            let after = rustix::fs::fstat(&f).map_err(|e| e.to_string())?;
            if !fsops::ident_of(&after).same_strict(&b.id) {
                return Err(format!("{} changed while it was uploaded", b.rel));
            }
            if whole && hex::encode(bh.finalize()) != b.sha256 {
                return Err(format!(
                    "{} no longer has the content that was hashed",
                    b.rel
                ));
            }
        }
        pipe.flush()
            .map_err(|e| format!("writing to rclone: {e}"))?;
        Ok(())
    })?;
    let h = Arc::try_unwrap(hashes)
        .map_err(|_| "hash state still shared".to_string())?
        .into_inner()
        .map_err(|_| "hash lock poisoned".to_string())?;
    let sha1 = hex::encode(h.sha1.finalize());
    let md5 = hex::encode(h.md5.finalize());
    let sha256 = hex::encode(h.sha256.finalize());
    verify(rc, &sh.id, sh.size, &sha1, &md5, &sha256, hooks)
}

/// Verify an object: size plus a storage hash, or a read-back hash when the backend lists none
/// we can compare. An empty hash never counts (C6).
pub fn verify(
    rc: &Rclone,
    id: &str,
    size: u64,
    sha1: &str,
    md5: &str,
    sha256: &str,
    hooks: &UploadHooks,
) -> Result<Verified, String> {
    let l = rc
        .stat(id)?
        .ok_or_else(|| format!("{id} is not in storage after upload"))?;
    if l.size != size {
        return Err(format!(
            "{id}: storage reports {} bytes, uploaded {size}",
            l.size
        ));
    }
    if !hooks.force_readback {
        for (name, ours) in [("sha1", sha1), ("md5", md5), ("sha256", sha256)] {
            if let Some(theirs) = l.hashes.get(name).filter(|t| !t.trim().is_empty()) {
                if theirs != ours {
                    return Err(format!(
                        "{id}: storage {name} {theirs} differs from the uploaded bytes"
                    ));
                }
                return Ok(Verified {
                    id: id.to_string(),
                    size,
                    verified_by: format!("{name}:{ours}"),
                });
            }
        }
    }
    let back = rc.cat(id)?;
    let got = hex::encode(Sha256::digest(&back));
    if back.len() as u64 != size || got != sha256 {
        return Err(format!(
            "{id}: the read-back bytes differ from the uploaded bytes"
        ));
    }
    Ok(Verified {
        id: id.to_string(),
        size,
        verified_by: format!("readback-sha256:{sha256}"),
    })
}

/// Upload shards with `jobs` concurrent pipelines. Returns one result per shard, in order.
pub fn upload_all(
    rc: &Rclone,
    shards: &[PlannedShard],
    blobs: &[BlobSrc],
    jobs: usize,
    hooks: &UploadHooks,
) -> Vec<Result<Verified, String>> {
    let jobs = jobs.clamp(1, 64);
    let next = Arc::new(Mutex::new(0usize));
    let (tx, rx) = mpsc::channel::<(usize, Result<Verified, String>)>();
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(shards.len().max(1)) {
            let tx = tx.clone();
            let next = next.clone();
            scope.spawn(move || loop {
                let k = {
                    let mut n = next.lock().expect("queue lock");
                    let k = *n;
                    *n += 1;
                    k
                };
                if k >= shards.len() {
                    return;
                }
                let r = upload_one(rc, &shards[k], blobs, hooks);
                let _ = tx.send((k, r));
            });
        }
    });
    drop(tx);
    let mut out: Vec<Option<Result<Verified, String>>> = vec![None; shards.len()];
    for (k, r) in rx {
        out[k] = Some(r);
    }
    out.into_iter()
        .map(|r| r.unwrap_or_else(|| Err("shard was not attempted".into())))
        .collect()
}

/// Segments of each blob (by index) across the planned shards.
pub fn blob_segments(shards: &[PlannedShard], nblobs: usize) -> Vec<Vec<(usize, Segment)>> {
    let mut out: Vec<Vec<(usize, Segment)>> = vec![Vec::new(); nblobs];
    for (si, sh) in shards.iter().enumerate() {
        for s in &sh.segs {
            out[s.blob].push((
                si,
                Segment {
                    shard: sh.id.clone(),
                    offset: s.shard_offset,
                    len: s.len,
                },
            ));
        }
    }
    out
}

/// sha256 of a file's content read through an fd, with the identity before and after; refuses
/// when the file changed while it was read.
pub fn hash_file(root: &std::path::Path, rel: &str) -> Result<(String, Ident), String> {
    let r = fsops::open_dir_abs(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let (mut f, id) = fsops::open_file_rel(&r, rel).map_err(|e| format!("{rel}: {e}"))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("{rel}: {e}"))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let after = rustix::fs::fstat(&f).map_err(|e| e.to_string())?;
    if !fsops::ident_of(&after).same_strict(&id) {
        return Err(format!("{rel} changed while it was read"));
    }
    Ok((hex::encode(h.finalize()), id))
}
