//! Restore, verify and receipt rebuild: everything that reads the bucket.
//!
//! Bucket contents are untrusted input. Every entry is validated before the target is created
//! (paths, duplicates, link targets, sizes, chunk references, memory shares). Data is written
//! only into files this restore created in an empty target; hard links follow, symlinks are
//! created last, so no write can go through a symlink. Every chunk is checked against its
//! BLAKE3 id, and every restored file against its whole-file BLAKE3.

use crate::error::{Error, Result};
use crate::hash::ChunkId;
use crate::meta::{self, Kind};
use crate::pack::{decode_segment, pack_key, parse_pack, seal_and_check, PACK_READ_MAX};
use crate::receipt::Receipt;
use crate::repo::{valid_run, Budget, ChunkMap, Entry, Meter, Repo, LINK_MAX};
use crate::transport::{RemoteObject, RetryPolicy, Transport};
use crate::walk::{rel_path, validate_rel};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct RestoreOpts {
    pub target: PathBuf,
    /// None means the newest complete snapshot.
    pub run: Option<String>,
    pub keep_setid: bool,
    pub budget: Budget,
    pub max_objects: usize,
    pub policy: RetryPolicy,
}

#[derive(Debug, Default, Serialize)]
pub struct RestoreReport {
    pub run: String,
    pub entries: u64,
    pub files: u64,
    pub bytes: u64,
    pub packs_read: u64,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub exit_code: i32,
    pub wall_ms: u64,
    pub peak_rss: u64,
}

/// One planned chunk write: (file index, offset in the file, chunk id, length).
type Write = (usize, u64, ChunkId, u32);

/// A validated plan: entries with decoded paths.
struct Item {
    rel: Vec<u8>,
    e: Entry,
}

fn pick_run(repo: &Repo, run: Option<&str>) -> Result<String> {
    match run {
        Some(r) if valid_run(r) => Ok(r.to_string()),
        Some(r) => Err(Error::Refused(format!("bad run id {r:?}"))),
        None => repo
            .snapshots()?
            .into_iter()
            .rev()
            .find(|s| s.complete)
            .map(|s| s.run)
            .ok_or_else(|| Error::Refused("no complete snapshot; pass --run".into())),
    }
}

struct Loaded {
    run: String,
    map: ChunkMap,
    items: Vec<Item>,
}

fn load(repo: &Repo, run: Option<&str>, b: &Budget) -> Result<Loaded> {
    repo.ensure_format(false)?;
    let run = pick_run(repo, run)?;
    let packs = repo.list_packs()?;
    let idx = repo.list_index()?;
    let mut mi = Meter::new("the chunk map", b.i);
    let (map, _, _) = repo.load_chunk_map(&idx, &packs, &mut mi, None)?;
    let mut mw = Meter::new("the file table", b.w);
    let files = repo.load_files(&run, &mut mw, None)?;
    if files.is_empty() {
        return Err(Error::Refused(format!("run {run} has no committed files")));
    }
    let mut items = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut kinds: HashMap<Vec<u8>, Kind> = HashMap::new();
    for (_, f) in &files {
        for e in &f.entries {
            let rel = e.rel()?;
            validate_rel(&rel)?;
            if !seen.insert(rel.clone()) {
                return Err(Error::corrupt(format!(
                    "{} appears twice in run {run}",
                    String::from_utf8_lossy(&rel)
                )));
            }
            if let Some(t) = e.target_bytes()? {
                if t.len() > LINK_MAX || t.contains(&0) {
                    return Err(Error::corrupt("link target too long or contains NUL"));
                }
            }
            let xb: usize = e.xattrs.iter().map(|(n, v)| (n.len() + v.len()) / 2).sum();
            if xb > meta::XATTR_MAX {
                return Err(Error::corrupt("xattrs exceed 64 KiB"));
            }
            if e.kind == Kind::File {
                let mut total = 0u64;
                for c in &e.chunks {
                    let (_, len) = map.get(c).ok_or_else(|| {
                        Error::corrupt(format!(
                            "{}: chunk {:?} is not in any verified pack",
                            String::from_utf8_lossy(&rel),
                            c
                        ))
                    })?;
                    total += len as u64;
                }
                if total != e.size {
                    return Err(Error::corrupt(format!(
                        "{}: chunks sum to {total}, size is {}",
                        String::from_utf8_lossy(&rel),
                        e.size
                    )));
                }
            }
            kinds.insert(rel.clone(), e.kind);
            items.push(Item { rel, e: e.clone() });
        }
    }
    for it in &items {
        if it.e.kind == Kind::Hardlink {
            let t =
                it.e.target_bytes()?
                    .ok_or_else(|| Error::corrupt("hard link without a target"))?;
            validate_rel(&t)?;
            if kinds.get(&t) != Some(&Kind::File) {
                return Err(Error::corrupt(format!(
                    "hard link {} points to a missing or non-file member",
                    String::from_utf8_lossy(&it.rel)
                )));
            }
        }
        if it.e.kind == Kind::Symlink && it.e.target_bytes()?.is_none() {
            return Err(Error::corrupt("symlink without a target"));
        }
        // every ancestor must be a directory entry of this run
        let comps: Vec<&[u8]> = it.rel.split(|&b| b == b'/').collect();
        for i in 1..comps.len() {
            let d = comps[..i].join(&b'/');
            if kinds.get(&d) != Some(&Kind::Dir) {
                return Err(Error::corrupt(format!(
                    "{}: ancestor {} is not a directory of this run",
                    String::from_utf8_lossy(&it.rel),
                    String::from_utf8_lossy(&d)
                )));
            }
        }
    }
    Ok(Loaded { run, map, items })
}

#[cfg(unix)]
fn write_at(f: &std::fs::File, buf: &[u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
fn write_at(f: &std::fs::File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

fn file_blake3(p: &Path) -> std::io::Result<String> {
    let mut h = blake3::Hasher::new();
    let mut f = std::fs::File::open(p)?;
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().to_hex().to_string())
}

/// How many packs to fetch at once within the budget (each holds at most one pack buffer).
fn pack_threads(b: &Budget) -> usize {
    let free = b.total.saturating_sub(b.i + b.w + b.ser);
    ((free / (PACK_READ_MAX + (8 << 20))) as usize).clamp(1, b.k.max(1))
}

pub fn restore(t: Arc<dyn Transport>, o: &RestoreOpts) -> Result<RestoreReport> {
    let t0 = Instant::now();
    let repo = Repo::new(t, o.max_objects, o.policy)?;
    let ld = load(&repo, o.run.as_deref(), &o.budget)?;
    let mut rep = RestoreReport {
        run: ld.run.clone(),
        entries: ld.items.len() as u64,
        ..Default::default()
    };
    // The target must be empty or absent; it is created only now, after every check passed.
    match std::fs::symlink_metadata(&o.target) {
        Ok(m) if m.is_dir() => {
            if std::fs::read_dir(&o.target)?.next().is_some() {
                return Err(Error::Refused(format!(
                    "restore target {} is not empty",
                    o.target.display()
                )));
            }
        }
        Ok(_) => {
            return Err(Error::Refused(format!(
                "restore target {} is not a directory",
                o.target.display()
            )))
        }
        Err(_) => std::fs::create_dir_all(&o.target)?,
    }
    let tp = |rel: &[u8]| o.target.join(rel_path(rel));

    // directories, parents first
    let mut dirs: Vec<&Item> = ld.items.iter().filter(|i| i.e.kind == Kind::Dir).collect();
    dirs.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut failed: HashSet<Vec<u8>> = HashSet::new();
    for d in &dirs {
        if let Err(e) = std::fs::create_dir(tp(&d.rel)) {
            rep.errors.push(format!(
                "{}: create dir: {e} (a case or normalisation collision if it already exists)",
                String::from_utf8_lossy(&d.rel)
            ));
            failed.insert(d.rel.clone());
        }
    }
    // regular files: create empty, sized; plan chunk writes per pack
    let mut plan: HashMap<u32, Vec<Write>> = HashMap::new();
    let files: Vec<&Item> = ld.items.iter().filter(|i| i.e.kind == Kind::File).collect();
    for (fi, it) in files.iter().enumerate() {
        let p = tp(&it.rel);
        let r = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .and_then(|f| f.set_len(it.e.size));
        if let Err(e) = r {
            rep.errors.push(format!(
                "{}: create: {e} (a case or normalisation collision if it already exists)",
                String::from_utf8_lossy(&it.rel)
            ));
            failed.insert(it.rel.clone());
            continue;
        }
        let mut off = 0u64;
        for c in &it.e.chunks {
            let l = ld.map.chunks[c];
            plan.entry(l.pack).or_default().push((fi, off, *c, l.len));
            off += l.len as u64;
        }
    }
    // fetch packs in parallel, each checked by its content address, every chunk by its id
    let work: Mutex<Vec<(u32, Vec<Write>)>> = Mutex::new(plan.into_iter().collect());
    let errs: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let bad_files: Mutex<HashSet<usize>> = Mutex::new(HashSet::new());
    let packs_read = std::sync::atomic::AtomicU64::new(0);
    let nthreads = pack_threads(&o.budget);
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            s.spawn(|| loop {
                let Some((pi, mut writes)) = work.lock().expect("work").pop() else {
                    break;
                };
                let pr = &ld.map.packs[pi as usize];
                let res = (|| -> Result<()> {
                    let b = repo.t.get(&pr.key, pr.size.min(PACK_READ_MAX))?;
                    if pack_key(&b) != pr.key {
                        return Err(Error::corrupt(format!(
                            "{} does not match its address",
                            pr.key
                        )));
                    }
                    let tr = parse_pack(&b)?;
                    let loc: HashMap<ChunkId, usize> = tr
                        .chunks
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (c.id, i))
                        .collect();
                    writes.sort_by_key(|w| loc.get(&w.2).map(|&i| tr.chunks[i].seg));
                    let mut cur: Option<(u32, Vec<u8>)> = None;
                    for (fi, off, id, len) in writes {
                        let r = (|| -> Result<()> {
                            let ci = *loc.get(&id).ok_or_else(|| {
                                Error::corrupt(format!("{} lacks chunk {id:?}", pr.key))
                            })?;
                            let c = tr.chunks[ci];
                            if cur.as_ref().map(|x| x.0) != Some(c.seg) {
                                cur = Some((
                                    c.seg,
                                    decode_segment(&b, &tr.segments[c.seg as usize])?,
                                ));
                            }
                            let seg = &cur.as_ref().expect("segment").1;
                            let slice = &seg[c.off as usize..(c.off + c.len) as usize];
                            if c.len != len || ChunkId::of(slice) != id {
                                return Err(Error::corrupt(format!(
                                    "chunk {id:?} does not match its id"
                                )));
                            }
                            let f = std::fs::OpenOptions::new()
                                .write(true)
                                .open(tp(&files[fi].rel))?;
                            write_at(&f, slice, off)?;
                            Ok(())
                        })();
                        if let Err(e) = r {
                            errs.lock()
                                .expect("errs")
                                .push(format!("{}: {e}", String::from_utf8_lossy(&files[fi].rel)));
                            bad_files.lock().expect("bad").insert(fi);
                        }
                    }
                    packs_read.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                })();
                if let Err(e) = res {
                    errs.lock()
                        .expect("errs")
                        .push(format!("pack {}: {e}", pr.key));
                }
            });
        }
    });
    rep.packs_read = packs_read.into_inner();
    rep.errors.extend(errs.into_inner().expect("errs"));
    let bad = bad_files.into_inner().expect("bad");
    // whole-file check
    for (fi, it) in files.iter().enumerate() {
        if failed.contains(&it.rel) || bad.contains(&fi) {
            continue;
        }
        let p = tp(&it.rel);
        match file_blake3(&p) {
            Ok(h) if Some(&h) == it.e.blake3.as_ref() => {
                rep.files += 1;
                rep.bytes += it.e.size;
            }
            Ok(_) => rep.errors.push(format!(
                "{}: restored bytes do not match the recorded BLAKE3",
                String::from_utf8_lossy(&it.rel)
            )),
            Err(e) => rep.errors.push(format!(
                "{}: read back: {e}",
                String::from_utf8_lossy(&it.rel)
            )),
        }
        let x = it.e.xattr_bytes().unwrap_or_default();
        rep.warnings.extend(meta::apply(
            &p,
            Kind::File,
            it.e.mode,
            it.e.mtime_ns,
            &x,
            it.e.flags,
            o.keep_setid,
        ));
    }
    // hard links, then symlinks (last), then directory metadata deepest first
    for it in ld.items.iter().filter(|i| i.e.kind == Kind::Hardlink) {
        let target = it.e.target_bytes()?.unwrap_or_default();
        if failed.contains(&target) {
            continue;
        }
        if let Err(e) = std::fs::hard_link(tp(&target), tp(&it.rel)) {
            rep.errors.push(format!(
                "{}: hard link: {e}",
                String::from_utf8_lossy(&it.rel)
            ));
        }
    }
    for it in ld.items.iter().filter(|i| i.e.kind == Kind::Symlink) {
        let target = it.e.target_bytes()?.unwrap_or_default();
        let p = tp(&it.rel);
        #[cfg(unix)]
        let r = std::os::unix::fs::symlink(rel_path(&target), &p);
        #[cfg(not(unix))]
        let r: std::io::Result<()> = Err(std::io::Error::other(
            "symlinks are not restored on this platform",
        ));
        match r {
            Ok(()) => rep.warnings.extend(meta::apply(
                &p,
                Kind::Symlink,
                it.e.mode,
                it.e.mtime_ns,
                &it.e.xattr_bytes().unwrap_or_default(),
                it.e.flags,
                o.keep_setid,
            )),
            Err(e) => rep.errors.push(format!(
                "{}: symlink: {e}",
                String::from_utf8_lossy(&it.rel)
            )),
        }
    }
    for d in dirs.iter().rev() {
        if failed.contains(&d.rel) {
            continue;
        }
        rep.warnings.extend(meta::apply(
            &tp(&d.rel),
            Kind::Dir,
            d.e.mode,
            d.e.mtime_ns,
            &d.e.xattr_bytes().unwrap_or_default(),
            d.e.flags,
            o.keep_setid,
        ));
    }
    rep.wall_ms = t0.elapsed().as_millis() as u64;
    rep.peak_rss = crate::util::peak_rss_bytes();
    rep.exit_code = if rep.errors.is_empty() { 0 } else { 3 };
    Ok(rep)
}

#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub run: String,
    pub packs: u64,
    pub files: u64,
    pub bytes: u64,
    pub errors: Vec<String>,
    pub exit_code: i32,
}

/// Shallow: every pack the run depends on is listed with its recorded size and checksum (one
/// LIST). Deep: every such pack is downloaded, decoded and BLAKE3-checked per chunk, and every
/// file's whole-file BLAKE3 is recomputed from its chunks in order.
pub fn verify(
    t: Arc<dyn Transport>,
    run: Option<&str>,
    deep: bool,
    b: &Budget,
    max_objects: usize,
) -> Result<VerifyReport> {
    let repo = Repo::new(t, max_objects, RetryPolicy::default())?;
    repo.ensure_format(false)?;
    let run = pick_run(&repo, run)?;
    let listed = repo.list_packs()?;
    let idx = repo.list_index()?;
    let mut mi = Meter::new("the chunk map", b.i);
    // Chunks of unlisted packs drop out of the map; `referenced` names the missing packs.
    let (map, _, referenced) = repo.load_chunk_map(&idx, &listed, &mut mi, None)?;
    let mut mw = Meter::new("the file table", b.w);
    let files = repo.load_files(&run, &mut mw, None)?;
    let mut rep = VerifyReport {
        run: run.clone(),
        ..Default::default()
    };
    for (k, rec) in &referenced {
        match listed.get(k) {
            None => rep
                .errors
                .push(format!("pack {k} is missing from the store")),
            Some(l) => {
                let same = l.size == rec.size
                    && l.hash
                        .as_deref()
                        .is_some_and(|h| h.eq_ignore_ascii_case(&rec.hash));
                if !same {
                    rep.errors.push(format!(
                        "pack {k} does not match its recorded size and checksum"
                    ));
                }
            }
        }
    }
    let mut needed: Vec<u32> = Vec::new();
    let mut need_set = HashSet::new();
    let entries: Vec<&Entry> = files.iter().flat_map(|(_, f)| f.entries.iter()).collect();
    for e in &entries {
        for c in &e.chunks {
            match map.chunks.get(c) {
                Some(l) => {
                    if need_set.insert(l.pack) {
                        needed.push(l.pack);
                    }
                }
                None => rep.errors.push(format!(
                    "{}: chunk {c:?} is in no listed, checksum-matching pack",
                    String::from_utf8_lossy(&e.rel().unwrap_or_default())
                )),
            }
        }
    }
    rep.packs = needed.len() as u64;
    if deep && rep.errors.is_empty() {
        // check every pack once, then each file in order with a small pack cache
        for &pi in &needed {
            let pr = &map.packs[pi as usize];
            let r = repo
                .t
                .get(&pr.key, pr.size.min(PACK_READ_MAX))
                .and_then(|b| {
                    if pack_key(&b) != pr.key {
                        return Err(Error::corrupt("content address mismatch"));
                    }
                    seal_and_check(&b).map(|_| ())
                });
            if let Err(e) = r {
                rep.errors.push(format!("pack {}: {e}", pr.key));
            }
        }
        if rep.errors.is_empty() {
            let mut cache: Vec<(u32, Vec<u8>, crate::pack::Trailer)> = Vec::new();
            for e in entries.iter().filter(|e| e.kind == Kind::File) {
                let mut h = blake3::Hasher::new();
                let mut ok = true;
                for c in &e.chunks {
                    let l = map.chunks[c];
                    if !cache.iter().any(|x| x.0 == l.pack) {
                        let pr = &map.packs[l.pack as usize];
                        let b = repo.t.get(&pr.key, pr.size.min(PACK_READ_MAX))?;
                        let tr = parse_pack(&b)?;
                        if cache.len() >= 2 {
                            cache.remove(0);
                        }
                        cache.push((l.pack, b, tr));
                    }
                    let (_, b, tr) = cache.iter().find(|x| x.0 == l.pack).expect("cached");
                    match tr.chunks.iter().find(|x| x.id == *c) {
                        Some(cl) => {
                            let seg = decode_segment(b, &tr.segments[cl.seg as usize])?;
                            h.update(&seg[cl.off as usize..(cl.off + cl.len) as usize]);
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                let hex = h.finalize().to_hex().to_string();
                if !ok || Some(&hex) != e.blake3.as_ref() {
                    rep.errors.push(format!(
                        "{}: whole-file BLAKE3 does not match",
                        String::from_utf8_lossy(&e.rel().unwrap_or_default())
                    ));
                } else {
                    rep.files += 1;
                    rep.bytes += e.size;
                }
            }
        }
    }
    rep.exit_code = if rep.errors.is_empty() { 0 } else { 3 };
    Ok(rep)
}

/// Rebuild a run's receipts from the bucket alone. `engine` comes from the objects, never from
/// this binary.
pub fn rebuild_receipts(
    t: Arc<dyn Transport>,
    repo_label: &str,
    run: &str,
    b: &Budget,
    max_objects: usize,
) -> Result<Vec<Receipt>> {
    let repo = Repo::new(t, max_objects, RetryPolicy::default())?;
    repo.ensure_format(false)?;
    if !valid_run(run) {
        return Err(Error::Refused(format!("bad run id {run:?}")));
    }
    let idx: HashMap<u32, RemoteObject> = repo
        .list(&format!("v1/index/{run}"))?
        .into_iter()
        .filter_map(|o| crate::repo::parse_commit_key(&o.key).map(|(_, s)| (s, o)))
        .collect();
    let mut mw = Meter::new("the file table", b.w);
    let mut out = Vec::new();
    for (fo, f) in repo.load_files(run, &mut mw, None)? {
        let Some(io) = idx.get(&f.seq) else {
            continue;
        };
        let (Some(ih), Some(fh)) = (&io.hash, &fo.hash) else {
            continue;
        };
        let iref = crate::repo::ObjRef {
            key: io.key.clone(),
            size: io.size,
            hash: ih.to_ascii_lowercase(),
        };
        let fref = crate::repo::ObjRef {
            key: fo.key.clone(),
            size: fo.size,
            hash: fh.to_ascii_lowercase(),
        };
        for e in &f.entries {
            out.push(Receipt::from_entry(
                e, repo_label, run, &f.engine, &iref, &fref,
            ));
        }
    }
    Ok(out)
}
