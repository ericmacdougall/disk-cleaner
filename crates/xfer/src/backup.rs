//! The backup pipeline. Nothing is staged on local disk.
//!
//! ```text
//! walk ─► schedule ─► readers (CDC, BLAKE3, dedup) ─► segmenter ─► compressors ─► packer
//!                         │                                                        │ seal_and_check
//!                         ▼                                                        ▼
//!                     committer ◄──────────── uploaders (K, verified_put) ◄── sealed packs
//!                         │ .idx then .files (both verified) then receipts
//! ```
//!
//! A path's receipt is written only after every pack holding its chunks and both commit objects
//! listing it were verified by the store's checksum.

use crate::chunker::chunk_stream;
use crate::compress::{class_of, probe_compressible, Class, RateMatcher};
use crate::control::{self, Shared};
use crate::error::{Error, Result};
use crate::hash::ChunkId;
use crate::meta::{self, Identity, Kind};
use crate::pack::{
    pack_key, seal_and_check, PackBuilder, CODEC_RAW, CODEC_ZSTD, PACK_READ_MAX, SEG_MAX,
};
use crate::receipt::{Receipt, Writer};
use crate::repo::{
    enc_bytes, encode, engine, files_key, idx_key, snapshot_key, valid_run, Budget, Cache,
    ChunkMap, ChunkRef, CommitRef, Entry, FilesObj, IdxObj, Loc, Meter, ObjRef, Repo, Snapshot,
    Totals,
};
use crate::transport::{verified_put, RetryPolicy, Transport};
use crate::walk::{self, Filter, Node, WalkOpts};
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

pub struct Options {
    /// The repository string, recorded in receipts.
    pub repo: String,
    pub source: PathBuf,
    pub filter: Filter,
    pub files_from: Option<Vec<Vec<u8>>>,
    pub receipts: Option<PathBuf>,
    /// Cache directory; None disables the cache.
    pub state: Option<PathBuf>,
    pub cache_budget: u64,
    pub budget: Budget,
    pub level: Option<i32>,
    /// Fixed K (controller off).
    pub streams: Option<usize>,
    pub one_file_system: bool,
    pub rehash: bool,
    pub root_label: String,
    pub commit_bytes: u64,
    pub commit_every: Duration,
    pub readers: usize,
    pub compressors: usize,
    pub policy: RetryPolicy,
    pub max_objects: usize,
    pub run: Option<String>,
    /// Unreferenced packs up to this many bytes are downloaded, checked and adopted.
    pub adopt_limit: u64,
    pub epoch: Duration,
    /// Where a notice for the person running the backup goes (the CLI prints it to stderr).
    pub notice: Option<fn(&str)>,
}

impl Options {
    pub fn new(repo: &str, source: &Path, budget: Budget) -> Options {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Options {
            repo: repo.to_string(),
            source: source.to_path_buf(),
            filter: Filter::default(),
            files_from: None,
            receipts: None,
            state: None,
            cache_budget: 64 << 20,
            budget,
            level: None,
            streams: None,
            one_file_system: false,
            rehash: false,
            root_label: source.to_string_lossy().into_owned(),
            commit_bytes: 256 << 20,
            commit_every: Duration::from_secs(15),
            readers: cores.clamp(2, 8),
            compressors: cores.saturating_sub(1).clamp(1, 16),
            policy: RetryPolicy::default(),
            max_objects: crate::repo::DEFAULT_MAX_OBJECTS,
            run: None,
            adopt_limit: 1 << 30,
            epoch: Duration::from_secs(5),
            notice: None,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub run: String,
    pub exit_code: i32,
    pub budget: Option<Budget>,
    pub entries: u64,
    pub receipted: u64,
    pub files: u64,
    pub reused_files: u64,
    pub read_bytes: u64,
    pub source_bytes: u64,
    pub new_chunks: u64,
    pub new_chunk_bytes: u64,
    pub dedup_chunks: u64,
    pub packs: u64,
    pub pack_bytes: u64,
    pub adopted_packs: u64,
    pub commits: u32,
    pub changed: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub unreceipted: Vec<String>,
    pub excluded: u64,
    pub errors: Vec<String>,
    pub wall_ms: u64,
    pub walk_ms: u64,
    pub load_ms: u64,
    pub freeable_bytes: u64,
    pub t50_ms: Option<u64>,
    pub k_trace: Vec<(u64, usize, u64)>,
    /// (ms since start, allocated bytes receipted so far, entries in this commit) per commit:
    /// the freed-space curve.
    pub commit_trace: Vec<(u64, u64, u64)>,
    /// ms since start at which each pack was verified.
    pub pack_trace: Vec<u64>,
    pub level_histogram: Vec<(i32, u64)>,
    pub throttles: u64,
    pub peak_rss: u64,
    /// Times a reader waited for a commit to make room in the hot table (pass boundaries).
    pub index_waits: u64,
    /// New chunks the hot table holds at once.
    pub hot_room: u64,
    /// The index limits, said at start when the tree comes within 2x of them.
    pub limits: Option<String>,
}

fn shown(rel: &[u8]) -> String {
    String::from_utf8_lossy(rel).into_owned()
}

/// A counting semaphore over bytes (the chunk-bytes share C).
struct ByteSem {
    m: Mutex<u64>,
    cv: Condvar,
    cap: u64,
}

impl ByteSem {
    fn new(cap: u64) -> ByteSem {
        ByteSem {
            m: Mutex::new(0),
            cv: Condvar::new(),
            cap,
        }
    }
    fn acquire(&self, n: u64) {
        let mut u = self.m.lock().expect("sem");
        while *u > 0 && *u + n > self.cap {
            u = self.cv.wait(u).expect("sem");
        }
        *u += n;
    }
    fn release(&self, n: u64) {
        let mut u = self.m.lock().expect("sem");
        *u = u.saturating_sub(n);
        self.cv.notify_all();
    }
}

struct ChunkMsg {
    id: ChunkId,
    data: Vec<u8>,
    class: Class,
    group: Vec<u8>,
}

struct RawSeg {
    chunks: Vec<(ChunkId, u32, u32)>,
    data: Vec<u8>,
    class: Class,
}

struct CompSeg {
    chunks: Vec<(ChunkId, u32, u32)>,
    ulen: u64,
    codec: u8,
    stored: Vec<u8>,
}

struct Sealed {
    key: String,
    bytes: Vec<u8>,
    chunks: Vec<(ChunkId, u32)>,
}

enum Event {
    /// A file read completely and unchanged; waits for its chunks.
    File(Box<Entry>, usize),
    /// Reused through the metadata fast path; its chunks are already committed.
    Reused(Box<Entry>, usize),
    Changed(usize),
    Skipped(usize, String),
    Uploaded(ObjRef, Vec<(ChunkId, u32)>),
    Failed(String),
}

fn base_entry(n: &Node, kind: Kind) -> Entry {
    let (path, path_hex) = enc_bytes(&n.rel);
    Entry {
        path,
        path_hex,
        kind,
        mode: n.stat.mode,
        mtime_ns: n.stat.id.mtime_ns,
        size: if kind == Kind::File {
            n.stat.id.size
        } else {
            0
        },
        id: n.stat.id,
        blake3: None,
        chunks: Vec::new(),
        target: None,
        target_hex: None,
        xattrs: Vec::new(),
        flags: n.stat.flags,
        alloc: n.stat.alloc,
        group: None,
        meta_incomplete: false,
    }
}

fn set_extra(e: &mut Entry, ex: meta::Extra) {
    e.xattrs = ex
        .xattrs
        .iter()
        .map(|(n, v)| (crate::util::hex(n), crate::util::hex(v)))
        .collect();
    e.meta_incomplete = ex.incomplete.is_some();
}

fn same_identity(a: &Identity, b: &Identity) -> bool {
    a.dev == b.dev
        && a.ino == b.ino
        && a.size == b.size
        && a.mtime_ns == b.mtime_ns
        && a.ctime_ns == b.ctime_ns
}

/// Order regular files by expected bytes freed per byte uploaded: unchanged first, then
/// size-collision groups (likely duplicates, adjacent), compressible, unknown, incompressible.
fn schedule(nodes: &[Node], files: &[usize], prev: &HashMap<Vec<u8>, Entry>) -> Vec<usize> {
    let mut by_size: HashMap<u64, usize> = HashMap::new();
    for &i in files {
        let s = nodes[i].stat.id.size;
        if s >= 64 << 10 {
            *by_size.entry(s).or_insert(0) += 1;
        }
    }
    let rank = |i: usize| -> (u8, u64) {
        let n = &nodes[i];
        if prev
            .get(&n.rel)
            .is_some_and(|e| same_identity(&e.id, &n.stat.id))
        {
            return (0, 0);
        }
        let s = n.stat.id.size;
        if s >= 64 << 10 && by_size.get(&s).copied().unwrap_or(0) > 1 {
            return (1, u64::MAX - s);
        }
        let name = n.rel.rsplit(|&b| b == b'/').next().unwrap_or(&n.rel);
        match class_of(name) {
            Class::Compressible => (2, 0),
            Class::Unknown => (3, 0),
            Class::Incompressible => (4, 0),
        }
    };
    let mut v: Vec<(u8, u64, usize)> = files
        .iter()
        .map(|&i| {
            let (a, b) = rank(i);
            (a, b, i)
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, _, i)| i).collect()
}

/// The committed chunk map, shared: readers look chunks up in it, the committer merges each
/// commit's chunks into it.
type Cold = RwLock<ChunkMap>;

fn cold_read(c: &Cold) -> std::sync::RwLockReadGuard<'_, ChunkMap> {
    c.read().unwrap_or_else(|p| p.into_inner())
}

/// This run's new chunks until they are committed (Revision 6.4): each id with whether its pack
/// is uploaded and verified. Allocated once from the index share (`hot_table`), so it never
/// grows; at `room` entries a reader that finds a new chunk waits until a commit moves chunks
/// into the committed map (a pass boundary), instead of the run exceeding `--mem-budget`.
///
/// Every id is in the hot table or the committed map, never neither, while it matters: the
/// committer merges a commit's chunks into the committed map before it removes them here, and a
/// reader looks in the committed map again under this table's lock before it claims an id.
/// Lock order: this table, then the committed map. The committer never holds the committed map
/// while it takes this lock.
struct Hot {
    s: Mutex<HotState>,
    cv: Condvar,
}

struct HotState {
    ids: HashMap<ChunkId, bool>,
    room: usize,
    /// Set once nothing more can be claimed: (whether it is the share, why).
    stop: Option<(bool, String)>,
    /// Whether the share's refusal was reported as an error of the run.
    announced: bool,
    /// Readers waiting for room now.
    parked: usize,
    /// Times a reader waited for room.
    waits: u64,
}

impl Hot {
    fn new(i: u64) -> Hot {
        let (buckets, _) = crate::repo::hot_table(i);
        let cap = if buckets < 8 {
            buckets - 1
        } else {
            buckets / 8 * 7
        };
        Hot {
            s: Mutex::new(HotState {
                ids: HashMap::with_capacity(cap as usize),
                room: crate::repo::hot_room(i),
                stop: None,
                announced: false,
                parked: 0,
                waits: 0,
            }),
            cv: Condvar::new(),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HotState> {
        self.s.lock().unwrap_or_else(|p| p.into_inner())
    }
    /// Claim `id` for this run: Ok(true) when no commit and no other reader holds it (the
    /// caller then sends its bytes), Ok(false) when it is a duplicate. Waits while the table is
    /// full; fails once the run stops taking new chunks.
    fn claim(&self, id: ChunkId, cold: &Cold) -> Result<bool> {
        if cold_read(cold).contains(&id) {
            return Ok(false);
        }
        let mut s = self.lock();
        let mut parked = false;
        let r = loop {
            // a commit may have moved it from here to the committed map since the look above
            if s.ids.contains_key(&id) || cold_read(cold).contains(&id) {
                break Ok(false);
            }
            if let Some((share, why)) = &s.stop {
                break Err(if *share {
                    Error::Budget(why.clone())
                } else {
                    Error::Refused(why.clone())
                });
            }
            if s.ids.len() < s.room {
                s.ids.insert(id, false);
                break Ok(true);
            }
            if !parked {
                parked = true;
                s.parked += 1;
                s.waits += 1;
            }
            s = self.cv.wait(s).unwrap_or_else(|p| p.into_inner());
        };
        if parked {
            s.parked -= 1;
        }
        r
    }
    /// Whether readers are waiting for room (a commit is then due as soon as one can be made).
    fn pressed(&self) -> bool {
        self.lock().parked > 0
    }
    /// Mark the chunks of a verified pack.
    fn verified(&self, cs: &[(ChunkId, u32)]) {
        let mut s = self.lock();
        for (id, _) in cs {
            if let Some(v) = s.ids.get_mut(id) {
                *v = true;
            }
        }
    }
    /// Drop chunks whose pack failed (they are sent again if another file holds them), or that
    /// a commit moved into the committed map, and wake the readers waiting for room.
    fn remove(&self, ids: impl Iterator<Item = ChunkId>) {
        let mut s = self.lock();
        for id in ids {
            s.ids.remove(&id);
        }
        drop(s);
        // the broken-hot-wake variant forgets the wake-up (failing-first evidence only)
        if !cfg!(feature = "broken-hot-wake") {
            self.cv.notify_all();
        }
    }
    /// Stop claims: every waiting and later claim of a new chunk fails with `why`.
    fn halt(&self, share: bool, why: String) {
        let mut s = self.lock();
        if s.stop.is_none() {
            s.stop = Some((share, why));
        }
        drop(s);
        self.cv.notify_all();
    }
    /// Record the chunks of adopted orphan packs, already stored and verified: all of `ids` or,
    /// if they do not fit, none.
    fn admit_verified(&self, ids: &[ChunkId]) -> bool {
        let mut s = self.lock();
        if s.ids.len() + ids.len() > s.room {
            return false;
        }
        for id in ids {
            s.ids.insert(*id, true);
        }
        true
    }
}

struct ReadCtx<'a> {
    root: &'a Path,
    nodes: &'a [Node],
    prev: &'a HashMap<Vec<u8>, Entry>,
    committed: &'a Cold,
    hot: &'a Hot,
    sem: &'a ByteSem,
    chunk_tx: Sender<ChunkMsg>,
    ev_tx: Sender<Event>,
    rehash: bool,
    groups: &'a HashMap<usize, String>,
    read_bytes: &'a AtomicU64,
    new_chunks: &'a AtomicU64,
    new_chunk_bytes: &'a AtomicU64,
    dedup_chunks: &'a AtomicU64,
}

/// An open that failed because a path component became a symlink or a non-directory after the
/// walk: the path changed, it is not "unreadable".
fn swapped(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(e.raw_os_error(), Some(c) if c == libc::ELOOP || c == libc::ENOTDIR)
            || e.to_string().contains("not a regular file")
    }
    #[cfg(not(unix))]
    {
        e.to_string().contains("symlink")
    }
}

enum ReadOutcome {
    Done,
    Changed,
}

fn read_one(c: &ReadCtx, i: usize, last_try: bool) -> ReadOutcome {
    let n = &c.nodes[i];
    let mut e = base_entry(n, Kind::File);
    e.group = c.groups.get(&i).cloned();
    if !c.rehash {
        if let Some(p) = c.prev.get(&n.rel) {
            if p.kind == Kind::File && same_identity(&p.id, &n.stat.id) && {
                let cold = cold_read(c.committed);
                p.chunks.iter().all(|id| cold.contains(id))
            } {
                let mut r = p.clone();
                r.id = n.stat.id;
                r.mode = n.stat.mode;
                r.alloc = n.stat.alloc;
                r.group = e.group.clone();
                let _ = c.ev_tx.send(Event::Reused(Box::new(r), i));
                return ReadOutcome::Done;
            }
        }
    }
    let mut f = match walk::open_beneath(c.root, &n.rel) {
        Ok(f) => f,
        Err(err) => {
            // a path swapped for a symlink (or vanished) after the walk is not read
            let why = format!("open: {err}");
            if swapped(&err) {
                if last_try {
                    let _ = c.ev_tx.send(Event::Changed(i));
                    return ReadOutcome::Done;
                }
                return ReadOutcome::Changed;
            }
            let _ = c.ev_tx.send(Event::Skipped(i, why));
            return ReadOutcome::Done;
        }
    };
    let before = match f.metadata() {
        Ok(m) => meta::stat_of(&m),
        Err(err) => {
            let _ = c.ev_tx.send(Event::Skipped(i, format!("fstat: {err}")));
            return ReadOutcome::Done;
        }
    };
    // the broken-read-guard variant skips both identity checks (failing-first evidence only)
    let guard = !cfg!(feature = "broken-read-guard");
    if guard && (before.id.dev != n.stat.id.dev || before.id.ino != n.stat.id.ino) {
        if last_try {
            let _ = c.ev_tx.send(Event::Changed(i));
            return ReadOutcome::Done;
        }
        return ReadOutcome::Changed;
    }
    let full = c.root.join(walk::rel_path(&n.rel));
    let extra = meta::capture(&full, Some(&f));
    let name = n.rel.rsplit(|&b| b == b'/').next().unwrap_or(&n.rel);
    let class = class_of(name);
    let group: Vec<u8> = match name.iter().rposition(|&b| b == b'.') {
        Some(p) => name[p..].to_ascii_lowercase(),
        None => Vec::new(),
    };
    let res = chunk_stream(&mut f, before.id.size, |id, data| {
        let fresh = c.hot.claim(id, c.committed)?;
        if fresh {
            c.new_chunks.fetch_add(1, Ordering::Relaxed);
            c.new_chunk_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            c.sem.acquire(data.len() as u64);
            c.chunk_tx
                .send(ChunkMsg {
                    id,
                    data,
                    class,
                    group: group.clone(),
                })
                .map_err(|_| Error::Bug("segmenter stopped".into()))?;
        } else {
            c.dedup_chunks.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });
    let cf = match res {
        Ok(cf) => cf,
        Err(Error::Budget(m)) => {
            // the committed map's share is spent: an error of the run (exit 3), said once
            let first = {
                let mut s = c.hot.lock();
                !std::mem::replace(&mut s.announced, true)
            };
            if first {
                let _ = c.ev_tx.send(Event::Failed(m.clone()));
            }
            let _ = c.ev_tx.send(Event::Skipped(i, format!("not read: {m}")));
            return ReadOutcome::Done;
        }
        Err(err) => {
            let _ = c.ev_tx.send(Event::Skipped(i, format!("read: {err}")));
            return ReadOutcome::Done;
        }
    };
    c.read_bytes.fetch_add(cf.bytes, Ordering::Relaxed);
    let after = match f.metadata() {
        Ok(m) => meta::stat_of(&m),
        Err(err) => {
            let _ = c.ev_tx.send(Event::Skipped(i, format!("fstat: {err}")));
            return ReadOutcome::Done;
        }
    };
    if guard && (!same_identity(&before.id, &after.id) || cf.bytes != before.id.size) {
        if last_try {
            let _ = c.ev_tx.send(Event::Changed(i));
            return ReadOutcome::Done;
        }
        return ReadOutcome::Changed;
    }
    e.id = after.id;
    e.mode = after.mode;
    e.mtime_ns = after.id.mtime_ns;
    e.size = after.id.size;
    e.alloc = after.alloc;
    e.flags = after.flags;
    e.blake3 = Some(crate::util::hex(&cf.blake3));
    e.chunks = cf.chunks.iter().map(|(id, _)| *id).collect();
    set_extra(&mut e, extra);
    let _ = c.ev_tx.send(Event::File(Box::new(e), i));
    ReadOutcome::Done
}

/// Groups small chunks of one extension into segments of up to 4 MiB; large or incompressible
/// chunks become their own segment.
/// An open segment is emitted once this many bytes of later chunks have passed it, so a small
/// chunk (a large file's tail, say) is never held back behind much later data. Packs then fill
/// in read order, and a file completes with its own packs (freed space first).
const SEG_AGE: u64 = 2 << 20;

fn segmenter(rx: Receiver<ChunkMsg>, tx: Sender<RawSeg>) {
    let mut open: HashMap<(Vec<u8>, Class), RawSeg> = HashMap::new();
    // bytes seen so far, and the count at which each open segment took its first chunk
    let mut through = 0u64;
    let mut opened: HashMap<(Vec<u8>, Class), u64> = HashMap::new();
    let mut buffered = 0usize;
    let emit = |s: RawSeg, tx: &Sender<RawSeg>| {
        let _ = tx.send(s);
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(m) => {
                through += m.data.len() as u64;
                let stale: Vec<_> = opened
                    .iter()
                    .filter(|(_, &at)| {
                        through - at > SEG_AGE && !cfg!(feature = "broken-read-order")
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in stale {
                    opened.remove(&k);
                    if let Some(s) = open.remove(&k) {
                        buffered -= s.data.len();
                        emit(s, &tx);
                    }
                }
                if m.class == Class::Incompressible || m.data.len() >= 512 << 10 {
                    let len = m.data.len() as u32;
                    emit(
                        RawSeg {
                            chunks: vec![(m.id, 0, len)],
                            data: m.data,
                            class: m.class,
                        },
                        &tx,
                    );
                    continue;
                }
                let k = (m.group, m.class);
                opened.entry(k.clone()).or_insert(through);
                let seg = open.entry(k.clone()).or_insert_with(|| RawSeg {
                    chunks: Vec::new(),
                    data: Vec::new(),
                    class: m.class,
                });
                if seg.data.len() + m.data.len() > SEG_MAX {
                    let full = std::mem::replace(
                        seg,
                        RawSeg {
                            chunks: Vec::new(),
                            data: Vec::new(),
                            class: m.class,
                        },
                    );
                    buffered -= full.data.len();
                    emit(full, &tx);
                    opened.insert(k.clone(), through);
                }
                let seg = open.get_mut(&k).expect("segment");
                let off = seg.data.len() as u32;
                seg.data.extend_from_slice(&m.data);
                seg.chunks.push((m.id, off, m.data.len() as u32));
                buffered += m.data.len();
                if buffered > 16 << 20 {
                    let big = open
                        .iter()
                        .max_by_key(|(_, s)| s.data.len())
                        .map(|(k, _)| k.clone());
                    if let Some(k) = big {
                        opened.remove(&k);
                        if let Some(s) = open.remove(&k) {
                            buffered -= s.data.len();
                            emit(s, &tx);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                for (_, s) in open.drain() {
                    emit(s, &tx);
                }
                opened.clear();
                buffered = 0;
            }
            Err(RecvTimeoutError::Disconnected) => {
                for (_, s) in open.drain() {
                    emit(s, &tx);
                }
                return;
            }
        }
    }
}

fn compressor(rx: Receiver<RawSeg>, tx: Sender<CompSeg>, rm: &RateMatcher, sem: &ByteSem) {
    for s in rx {
        let ulen = s.data.len() as u64;
        let (codec, stored) = if s.class == Class::Incompressible || !probe_compressible(&s.data) {
            (CODEC_RAW, s.data)
        } else {
            let c = rm.compress(&s.data);
            if c.data.len() < s.data.len() {
                (CODEC_ZSTD, c.data)
            } else {
                (CODEC_RAW, s.data)
            }
        };
        sem.release(ulen);
        if tx
            .send(CompSeg {
                chunks: s.chunks,
                ulen,
                codec,
                stored,
            })
            .is_err()
        {
            return;
        }
    }
}

fn packer(
    rx: Receiver<CompSeg>,
    tx: Sender<Sealed>,
    shared: &Shared,
    flush_after: Duration,
    ev_tx: Sender<Event>,
    hot: &Hot,
) {
    let mut pb = PackBuilder::new();
    let mut chunks: Vec<(ChunkId, u32)> = Vec::new();
    let mut first: Option<Instant> = None;
    let seal = |pb: PackBuilder, chunks: Vec<(ChunkId, u32)>| -> bool {
        let r = pb.finish().and_then(|(bytes, _)| {
            seal_and_check(&bytes)?;
            Ok(bytes)
        });
        match r {
            Ok(bytes) => {
                let t = Instant::now();
                let ok = tx
                    .send(Sealed {
                        key: pack_key(&bytes),
                        bytes,
                        chunks,
                    })
                    .is_ok();
                shared
                    .blocked_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                ok
            }
            Err(e) => {
                let _ = ev_tx.send(Event::Failed(format!("pack failed its decode check: {e}")));
                hot.remove(chunks.iter().map(|(id, _)| *id));
                true
            }
        }
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(s) => {
                let max = shared.max_pack as usize;
                if !pb.is_empty() && pb.len() + s.stored.len() + (1 << 20) > max {
                    let full = std::mem::take(&mut pb);
                    if !seal(full, std::mem::take(&mut chunks)) {
                        return;
                    }
                    first = None;
                }
                pb.add_stored(&s.chunks, s.ulen, s.codec, &s.stored);
                chunks.extend(s.chunks.iter().map(|&(id, _, len)| (id, len)));
                first.get_or_insert_with(Instant::now);
                if pb.len() as u64 >= shared.target_pack.load(Ordering::Relaxed) {
                    let full = std::mem::take(&mut pb);
                    if !seal(full, std::mem::take(&mut chunks)) {
                        return;
                    }
                    first = None;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // readers waiting for room in the hot table need this pack committed now
                if !pb.is_empty()
                    && (first.is_some_and(|t| t.elapsed() >= flush_after) || hot.pressed())
                {
                    let full = std::mem::take(&mut pb);
                    if !seal(full, std::mem::take(&mut chunks)) {
                        return;
                    }
                    first = None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if !pb.is_empty() {
                    seal(pb, chunks);
                }
                return;
            }
        }
    }
}

fn uploader(
    t: &dyn Transport,
    rx: Receiver<Sealed>,
    shared: &Shared,
    policy: RetryPolicy,
    ev_tx: Sender<Event>,
    hot: &Hot,
) {
    loop {
        shared.acquire();
        let p = match rx.recv() {
            Ok(p) => p,
            Err(_) => {
                shared.release();
                return;
            }
        };
        let r = verified_put(t, &p.key, &p.bytes, policy, &|e| {
            if e.is_throttle() {
                shared.throttled();
            }
        });
        shared.release();
        match r {
            Ok(v) => {
                shared.uploaded.fetch_add(v.size, Ordering::Relaxed);
                let _ = ev_tx.send(Event::Uploaded(
                    ObjRef {
                        key: v.key,
                        size: v.size,
                        hash: v.hash,
                    },
                    p.chunks,
                ));
            }
            Err(e) => {
                let _ = ev_tx.send(Event::Failed(format!("upload {}: {e}", p.key)));
                // its chunks leave the hot table, so readers waiting for room go on
                hot.remove(p.chunks.iter().map(|(id, _)| *id));
            }
        }
    }
}

/// Download unreferenced packs (left by an interrupted run), check them, and return their chunk
/// lists so this run can commit them instead of uploading the data again.
fn adopt_orphans(
    repo: &Repo,
    packs: &HashMap<String, crate::transport::RemoteObject>,
    referenced: &HashMap<String, ObjRef>,
    limit: u64,
    committed: &ChunkMap,
    hot: &Hot,
) -> Vec<(ObjRef, Vec<(ChunkId, u32)>)> {
    let mut orphans: Vec<_> = packs
        .values()
        // referenced: held by the committed map, or named by an index without matching
        .filter(|o| {
            !committed.has_pack(&o.key) && !referenced.contains_key(&o.key) && o.hash.is_some()
        })
        .collect();
    orphans.sort_by(|a, b| a.key.cmp(&b.key));
    let total: u64 = orphans.iter().map(|o| o.size).sum();
    if orphans.is_empty() || total > limit {
        return Vec::new();
    }
    let mut out = Vec::new();
    for o in orphans {
        if o.size > PACK_READ_MAX {
            continue;
        }
        let Ok(b) = repo.t.get(&o.key, o.size) else {
            continue;
        };
        let listed = o.hash.as_deref().unwrap_or("");
        if pack_key(&b) != o.key || !repo.kind.digest(&b).eq_ignore_ascii_case(listed) {
            continue;
        }
        let Ok(tr) = seal_and_check(&b) else {
            continue;
        };
        // a pack is adopted whole or not at all; its chunks are new chunks of this run
        let fresh: Vec<ChunkId> = tr
            .chunks
            .iter()
            .map(|c| c.id)
            .filter(|id| !committed.contains(id) && !hot.lock().ids.contains_key(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !hot.admit_verified(&fresh) {
            break;
        }
        out.push((
            ObjRef {
                key: o.key.clone(),
                size: o.size,
                hash: listed.to_ascii_lowercase(),
            },
            tr.chunks.iter().map(|c| (c.id, c.len)).collect(),
        ));
    }
    out
}

pub fn default_state_dir(repo: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let h = blake3::hash(repo.as_bytes()).to_hex();
    Some(base.join("prunedisk-xfer").join(&h[..16]))
}

/// Run one backup. The returned report carries the exit code: 0 when every input path was
/// receipted, 2 when some were changed, skipped or not receipted, 3 on a verification or fatal
/// error.
pub fn backup(t: Arc<dyn Transport>, o: &Options) -> Result<Report> {
    let t0 = Instant::now();
    let start_ms = crate::util::now_ms();
    let run = o.run.clone().unwrap_or_else(crate::util::new_run_id);
    if !valid_run(&run) {
        return Err(Error::Refused(format!("bad run id {run:?}")));
    }
    let b = o.budget;
    let mut rep = Report {
        run: run.clone(),
        budget: Some(b),
        ..Report::default()
    };
    let repo = Repo::new(t.clone(), o.max_objects, o.policy)?;

    // Load. Every store request is a round trip (0.1 to 1 s on B2), so independent requests run
    // at once: the index, pack and snapshot listings and the format check, then the chunk maps
    // beside the previous run's file table. Commit objects come from the state dir's cache when
    // their listed checksum still matches, so a rerun downloads only what other runs added.
    let serial = cfg!(feature = "broken-serial-load");
    let (idx_list, packs, snap_list, format_ok) = if serial {
        (
            repo.list_index(),
            repo.list_packs(),
            repo.list_snapshots(),
            repo.format_present(),
        )
    } else {
        std::thread::scope(|s| {
            let a = s.spawn(|| repo.list_index());
            let p = s.spawn(|| repo.list_packs());
            let n = s.spawn(|| repo.list_snapshots());
            let f = repo.format_present();
            let j = |e| Error::Bug(format!("load thread panicked: {e:?}"));
            (
                a.join().map_err(j).and_then(|r| r),
                p.join().map_err(j).and_then(|r| r),
                n.join().map_err(j).and_then(|r| r),
                f,
            )
        })
    };
    let idx_list = idx_list?;
    if idx_list.len() * 10 > o.max_objects * 9 && !cfg!(feature = "broken-repo-guards") {
        return Err(Error::Refused(format!(
            "v1/index holds {} objects, over 90% of the cap of {}; the repository needs compaction (or pass --max-objects)",
            idx_list.len(),
            o.max_objects
        )));
    }
    if !format_ok? {
        repo.write_format(true)?;
    }
    let packs = packs?;
    let snap_list = snap_list?;
    let listed_idx_bytes: u64 = idx_list.iter().map(|x| x.size).sum();
    let cache = match &o.state {
        Some(d) => Cache::open(d, &run, o.cache_budget, listed_idx_bytes)?,
        None => None,
    };
    // The index share holds the hot table (this run's uncommitted new chunks, allocated once)
    // and the committed chunk map (the rest). Revision 6.4.
    let hot = Hot::new(b.i);
    let mut mi = Meter::new(
        "the chunk map",
        b.i.saturating_sub(crate::repo::hot_table(b.i).1),
    );
    // The previous run's file table, for the metadata fast path.
    let mut mw = Meter::new("the walk and the previous file table", b.w);
    let load_prev = |mw: &mut Meter| -> Result<HashMap<Vec<u8>, Entry>> {
        let mut prev: HashMap<Vec<u8>, Entry> = HashMap::new();
        let prev_snap = repo
            .load_snapshots(&snap_list, cache.as_ref())?
            .into_iter()
            .rev()
            .find(|s| s.complete && s.root == o.root_label);
        if let Some(s) = &prev_snap {
            for (_, f) in repo.load_commit_files(s, mw, cache.as_ref())? {
                for e in f.entries {
                    if e.kind == Kind::File {
                        if let Ok(rel) = e.rel() {
                            prev.insert(rel, e);
                        }
                    }
                }
            }
        }
        Ok(prev)
    };
    let (maps, prev) = if serial {
        let m = repo.load_map(&idx_list, &packs, &mut mi, cache.as_ref(), true);
        (m, load_prev(&mut mw))
    } else {
        std::thread::scope(|s| {
            let h = s.spawn(|| load_prev(&mut mw));
            let m = repo.load_map(&idx_list, &packs, &mut mi, cache.as_ref(), true);
            let p = h
                .join()
                .unwrap_or_else(|_| Err(Error::Bug("load thread panicked".into())));
            (m, p)
        })
    };
    let (committed, _n_idx, referenced) = maps?;
    let prev = prev?;
    let adopted = adopt_orphans(&repo, &packs, &referenced, o.adopt_limit, &committed, &hot);
    let committed: Cold = RwLock::new(committed);
    rep.adopted_packs = adopted.len() as u64;
    rep.load_ms = t0.elapsed().as_millis() as u64;

    // Walk.
    let tw = Instant::now();
    let walked = walk::walk(
        &o.source,
        &WalkOpts {
            filter: &o.filter,
            files_from: o.files_from.as_deref(),
            one_file_system: o.one_file_system,
        },
    )?;
    for n in &walked.nodes {
        mw.charge(n.rel.len() as u64 + n.target.as_ref().map_or(0, |t| t.len() as u64) + 200)?;
    }
    rep.walk_ms = tw.elapsed().as_millis() as u64;
    rep.excluded = walked.excluded;
    for (p, why) in &walked.skipped {
        rep.skipped.push((shown(p), why.clone()));
    }
    let nodes = walked.nodes;

    // Hard-link groups: the first member (path order) carries the data.
    let mut groups: HashMap<usize, String> = HashMap::new();
    let mut link_of: HashMap<usize, usize> = HashMap::new();
    let mut group_size: HashMap<String, usize> = HashMap::new();
    for ((dev, ino), mut members) in walk::link_groups(&nodes) {
        members.sort_by(|a, c| nodes[*a].rel.cmp(&nodes[*c].rel));
        let gid = format!("{dev:x}-{ino:x}");
        group_size.insert(gid.clone(), members.len());
        for &m in &members {
            groups.insert(m, gid.clone());
        }
        for &m in &members[1..] {
            link_of.insert(m, members[0]);
        }
    }
    let mut ready: Vec<Entry> = Vec::new();
    let mut data_files: Vec<usize> = Vec::new();
    let mut hard: Vec<(usize, usize)> = Vec::new();
    let mut freeable = 0u64;
    for (i, n) in nodes.iter().enumerate() {
        match n.stat.kind {
            Ok(Kind::Dir) => {
                let mut e = base_entry(n, Kind::Dir);
                let full = o.source.join(walk::rel_path(&n.rel));
                set_extra(&mut e, meta::capture(&full, None));
                ready.push(e);
            }
            Ok(Kind::Symlink) => {
                let mut e = base_entry(n, Kind::Symlink);
                let (a, h) = enc_bytes(n.target.as_deref().unwrap_or(b""));
                e.target = a;
                e.target_hex = h;
                let full = o.source.join(walk::rel_path(&n.rel));
                set_extra(&mut e, meta::capture(&full, None));
                ready.push(e);
            }
            Ok(Kind::File) => {
                if let Some(&d) = link_of.get(&i) {
                    hard.push((i, d));
                } else {
                    data_files.push(i);
                    freeable += n.stat.alloc;
                    rep.source_bytes += n.stat.id.size;
                }
            }
            _ => {}
        }
    }
    rep.freeable_bytes = freeable;
    rep.files = (data_files.len() + hard.len()) as u64;
    // Say the limits up front when this tree comes within 2x of them: at least one chunk per
    // file, plus one per MiB (the chunker's average) of the larger files.
    {
        let est = data_files.len() as u64 + rep.source_bytes / (1 << 20);
        let have = cold_read(&committed).len() as u64;
        // the conservative case: packs of 1 MiB chunks of large files, each pack a cost too
        let cold = b.i.saturating_sub(crate::repo::hot_table(b.i).1);
        let fit = crate::repo::map_fit(cold, (b.p_max >> 20).max(1)) as u64;
        if (have + est) * 2 >= fit {
            let m = format!(
                "this tree has {} files (about {est} chunks) and the repository holds {have} \
                 chunks; at --mem-budget {} the chunk map holds at most {fit} chunks in all and \
                 {} new chunks at a time before a commit makes room. Past {fit} the run stops \
                 reading with an error; back the tree up in parts or raise --mem-budget",
                rep.files,
                crate::util::human(b.total),
                crate::repo::hot_room(b.i)
            );
            if let Some(f) = o.notice {
                f(&m);
            }
            rep.limits = Some(m);
        }
    }
    let order = schedule(&nodes, &data_files, &prev);

    // Pipeline.
    let k_cap = o.streams.unwrap_or(b.k).min(b.k).max(1);
    let shared = Shared::new(
        o.streams.unwrap_or(k_cap),
        k_cap,
        o.streams.is_some(),
        b.p_max,
    );
    let rm = RateMatcher::new(o.level, o.compressors, shared.goodput.clone(), b.z);
    let sem = ByteSem::new(b.c);
    let read_bytes = AtomicU64::new(0);
    let new_chunks = AtomicU64::new(0);
    let new_chunk_bytes = AtomicU64::new(0);
    let dedup_chunks = AtomicU64::new(0);
    let mut writer = Writer::open(o.receipts.as_deref())?;
    let eng = engine();

    let (chunk_tx, chunk_rx) = bounded::<ChunkMsg>(64);
    let (seg_tx, seg_rx) = bounded::<RawSeg>(o.compressors * 2);
    let (comp_tx, comp_rx) = bounded::<CompSeg>(4);
    let (up_tx, up_rx) = bounded::<Sealed>(1);
    let (ev_tx, ev_rx) = unbounded::<Event>();

    let mut commit = Committer {
        repo: &repo,
        run: &run,
        root_label: &o.root_label,
        repo_label: &o.repo,
        engine: &eng,
        nodes: &nodes,
        committed: &committed,
        cold_meter: mi,
        hot: &hot,
        cache: cache.as_ref(),
        waiting: Vec::new(),
        hard_waiting: hard,
        done_data: HashSet::new(),
        ready,
        new_packs: adopted,
        pending_pack_bytes: 0,
        seq: 0,
        commits: Vec::new(),
        writer: &mut writer,
        broken: false,
        group_left: group_size,
        freeable,
        receipted_alloc: 0,
        t0,
        rep: &mut rep,
    };

    std::thread::scope(|sc| -> Result<()> {
        // controller
        let sh = shared.clone();
        let epoch = o.epoch;
        let ctl = sc.spawn(move || control::run(sh, epoch));
        // uploaders: one thread per possible stream; the gate admits K at a time
        let mut ups = Vec::new();
        for _ in 0..k_cap {
            let rx = up_rx.clone();
            let ev = ev_tx.clone();
            let sh = &*shared;
            let tt = &*t;
            let pol = o.policy;
            let hot = &hot;
            ups.push(sc.spawn(move || uploader(tt, rx, sh, pol, ev, hot)));
        }
        drop(up_rx);
        // packer
        {
            let sh = &*shared;
            let ev = ev_tx.clone();
            let flush = o.commit_every / 2;
            let hot = &hot;
            sc.spawn(move || packer(comp_rx, up_tx, sh, flush, ev, hot));
        }
        // compressors
        for _ in 0..o.compressors.max(1) {
            let rx = seg_rx.clone();
            let tx = comp_tx.clone();
            let rm = &rm;
            let sem = &sem;
            sc.spawn(move || compressor(rx, tx, rm, sem));
        }
        drop(seg_rx);
        drop(comp_tx);
        // segmenter
        sc.spawn(move || segmenter(chunk_rx, seg_tx));
        // readers: the schedule, then one retry pass over files that changed during the read
        {
            let ctx = ReadCtx {
                root: &o.source,
                nodes: &nodes,
                prev: &prev,
                committed: &committed,
                hot: &hot,
                sem: &sem,
                chunk_tx,
                ev_tx: ev_tx.clone(),
                rehash: o.rehash,
                groups: &groups,
                read_bytes: &read_bytes,
                new_chunks: &new_chunks,
                new_chunk_bytes: &new_chunk_bytes,
                dedup_chunks: &dedup_chunks,
            };
            let order = &order;
            let readers = o.readers.max(1);
            sc.spawn(move || {
                let next = AtomicUsize::new(0);
                let retry: Mutex<Vec<usize>> = Mutex::new(Vec::new());
                // Large files are read one at a time, in schedule order, so their chunks fill
                // packs file after file and each file completes (and frees space) as its own
                // packs verify. Interleaved, every file would finish only with the last pack.
                // Small files still read in parallel; reading is far faster than the uplink.
                let serial = Mutex::new(());
                std::thread::scope(|rs| {
                    for _ in 0..readers {
                        rs.spawn(|| loop {
                            let j = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&i) = order.get(j) else { break };
                            // the broken-read-order variant reads large files in parallel and
                            // interleaves them (failing-first evidence only)
                            let one_at_a_time = ctx.nodes[i].stat.id.size >= SERIAL_READ
                                && !cfg!(feature = "broken-read-order");
                            let _one =
                                one_at_a_time.then(|| serial.lock().expect("serial read lock"));
                            if let ReadOutcome::Changed = read_one(&ctx, i, false) {
                                retry.lock().expect("retry").push(i);
                            }
                        });
                    }
                });
                for i in retry.into_inner().expect("retry") {
                    read_one(&ctx, i, true);
                }
                // dropping ctx closes the chunk channel and this event sender
            });
        }
        drop(ev_tx);

        let res = commit.run(&ev_rx, o.commit_bytes, o.commit_every);
        // readers waiting for room must not outlive the committer
        hot.halt(false, "the run stopped committing".into());
        shared.stop.store(true, Ordering::Relaxed);
        for u in ups {
            let _ = u.join();
        }
        let _ = ctl.join();
        res
    })?;

    let commits = std::mem::take(&mut commit.commits);
    let seq = commit.seq;
    let fatal = commit.broken;
    drop(commit);

    rep.read_bytes = read_bytes.load(Ordering::Relaxed);
    rep.new_chunks = new_chunks.load(Ordering::Relaxed);
    rep.new_chunk_bytes = new_chunk_bytes.load(Ordering::Relaxed);
    rep.dedup_chunks = dedup_chunks.load(Ordering::Relaxed);
    rep.commits = seq;
    rep.k_trace = shared.k_trace.lock().expect("trace").clone();
    rep.throttles = shared.throttles.load(Ordering::Relaxed);
    rep.level_histogram = rm.histogram();
    {
        let h = hot.lock();
        rep.index_waits = h.waits;
        rep.hot_room = h.room as u64;
    }

    if !fatal && rep.errors.is_empty() {
        let snap = Snapshot {
            v: 1,
            engine: eng.clone(),
            run: run.clone(),
            root: o.root_label.clone(),
            start_ms,
            end_ms: crate::util::now_ms(),
            commits,
            totals: Totals {
                entries: rep.entries,
                files: rep.files,
                bytes: rep.source_bytes,
                packs: rep.packs,
                pack_bytes: rep.pack_bytes,
                changed: rep.changed.len() as u64,
                skipped: rep.skipped.len() as u64,
            },
            complete: true,
        };
        let body = serde_json::to_vec(&snap).map_err(|e| Error::Bug(format!("snapshot: {e}")))?;
        match verified_put(&*t, &snapshot_key(&run), &body, o.policy, &|_| {}) {
            Ok(_) => {
                if let Some(c) = &cache {
                    c.write(&snapshot_key(&run), &body);
                }
            }
            Err(e) => rep.errors.push(format!("snapshot: {e}")),
        }
    }
    rep.wall_ms = t0.elapsed().as_millis() as u64;
    rep.peak_rss = crate::util::peak_rss_bytes();
    rep.exit_code = if !rep.errors.is_empty() {
        3
    } else if !rep.changed.is_empty() || !rep.skipped.is_empty() || !rep.unreceipted.is_empty() {
        2
    } else {
        0
    };
    Ok(rep)
}

struct Committer<'a> {
    repo: &'a Repo,
    run: &'a str,
    root_label: &'a str,
    repo_label: &'a str,
    engine: &'a str,
    nodes: &'a [Node],
    /// the committed chunk map; each commit's chunks are merged in, charged to `cold_meter`
    committed: &'a Cold,
    cold_meter: Meter,
    /// this run's chunks until committed, with whether each is uploaded and verified
    hot: &'a Hot,
    /// the state dir's cache of commit objects: what this run commits goes in as it verifies
    cache: Option<&'a Cache>,
    waiting: Vec<(Entry, usize)>,
    /// (hard-link member, its group's data file)
    hard_waiting: Vec<(usize, usize)>,
    /// data files already committed
    done_data: HashSet<usize>,
    ready: Vec<Entry>,
    new_packs: Vec<(ObjRef, Vec<(ChunkId, u32)>)>,
    pending_pack_bytes: u64,
    seq: u32,
    commits: Vec<CommitRef>,
    writer: &'a mut Writer,
    broken: bool,
    group_left: HashMap<String, usize>,
    freeable: u64,
    receipted_alloc: u64,
    t0: Instant,
    rep: &'a mut Report,
}

/// Files at least this large are read one at a time (see the reader loop).
const SERIAL_READ: u64 = 4 << 20;

/// Entries per `.files` object (keeps each object well under the 64 MiB decode limit).
const FILES_PER_COMMIT: usize = 100_000;

/// Move committed chunks from the hot table to the committed map: merge first, then remove, so
/// a reader finds each id in one of the two at every moment. All or nothing: a merge that does
/// not fit leaves the map as it was and the chunks in the hot table.
fn migrate(
    committed: &Cold,
    meter: &mut Meter,
    hot: &Hot,
    packs: &[(ObjRef, Vec<(ChunkId, u32)>)],
) -> Result<()> {
    let ids = || {
        packs
            .iter()
            .flat_map(|(_, cs)| cs.iter().map(|(id, _)| *id))
    };
    // the broken-migrate-order variant removes before it merges (failing-first evidence only)
    if cfg!(feature = "broken-migrate-order") {
        hot.remove(ids());
        // the window a real scheduler opens here at random, held open
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut cold = committed.write().unwrap_or_else(|p| p.into_inner());
    let before = meter.used;
    let mut add = Vec::new();
    for (p, cs) in packs {
        if !cold.has_pack(&p.key) {
            if let Err(e) = meter.charge(crate::repo::pack_cost(p)) {
                meter.used = before;
                return Err(e);
            }
        }
        let pi = cold.pack_index(p);
        add.extend(cs.iter().map(|&(id, len)| (id, Loc { pack: pi, len })));
    }
    cold.merge(add, meter)?;
    drop(cold);
    if !cfg!(feature = "broken-migrate-order") {
        hot.remove(ids());
    }
    Ok(())
}

/// Whether a chunk is stored and verified: committed before, or uploaded in this run. Holds
/// both tables for a scan (hot first: the lock order).
struct Avail<'g> {
    hot: std::sync::MutexGuard<'g, HotState>,
    cold: std::sync::RwLockReadGuard<'g, ChunkMap>,
}

impl Avail<'_> {
    fn has(&self, id: &ChunkId) -> bool {
        self.hot.ids.get(id) == Some(&true) || self.cold.contains(id)
    }
    fn all(&self, e: &Entry) -> bool {
        e.chunks.iter().all(|c| self.has(c))
    }
}

impl Committer<'_> {
    fn avail(&self) -> Avail<'_> {
        let hot = self.hot.lock();
        Avail {
            hot,
            cold: cold_read(self.committed),
        }
    }

    /// Stop the readers too: a committer that can no longer commit must not leave them waiting
    /// for room.
    fn set_broken(&mut self) {
        self.broken = true;
        self.hot.halt(false, "the run stopped committing".into());
    }

    /// After a commit is stored: merge its chunks into the committed map, then take them out of
    /// the hot table (in that order, so a reader always finds each in one of the two). If the
    /// committed map's share cannot hold them, they stay in the hot table (still verified, so
    /// files waiting on them commit) and the run takes no more new chunks.
    fn migrate(&mut self, packs: &[(ObjRef, Vec<(ChunkId, u32)>)]) {
        if packs.is_empty() {
            return;
        }
        match migrate(self.committed, &mut self.cold_meter, self.hot, packs) {
            Ok(()) => {}
            Err(_) => {
                let have = cold_read(self.committed).len();
                let m = format!(
                    "the chunk map is full: its {} share of --mem-budget holds about {} \
                     chunks and this run committed past it; the files receipted so far are \
                     backed up, the rest were not read. Back the tree up in parts, or with a \
                     larger --mem-budget",
                    crate::util::human(self.cold_meter.limit),
                    have.max(crate::repo::chunk_map_fit(self.cold_meter.limit))
                );
                self.hot.halt(true, m);
            }
        }
    }

    fn run(&mut self, ev_rx: &Receiver<Event>, commit_bytes: u64, every: Duration) -> Result<()> {
        let mut last = Instant::now();
        loop {
            // Wait for an event only while no commit is due, and never past the end of the
            // one-second spacing: a due commit starts at once, not at the next 500 ms tick.
            let ev = if self.due(last, commit_bytes, every)
                && !cfg!(feature = "broken-commit-spacing")
            {
                ev_rx.try_recv().map_err(|e| match e {
                    crossbeam_channel::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    crossbeam_channel::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            } else {
                let wait = Duration::from_secs(1)
                    .saturating_sub(last.elapsed())
                    .clamp(Duration::from_millis(10), Duration::from_millis(500));
                ev_rx.recv_timeout(wait)
            };
            let closed = matches!(ev, Err(RecvTimeoutError::Disconnected));
            match ev {
                Ok(Event::File(e, i)) => self.waiting.push((*e, i)),
                Ok(Event::Reused(e, i)) => {
                    self.rep.reused_files += 1;
                    self.waiting.push((*e, i));
                }
                Ok(Event::Changed(i)) => self.rep.changed.push(shown(&self.nodes[i].rel)),
                Ok(Event::Skipped(i, why)) => {
                    self.rep.skipped.push((shown(&self.nodes[i].rel), why))
                }
                Ok(Event::Uploaded(p, cs)) => {
                    self.rep
                        .pack_trace
                        .push(self.t0.elapsed().as_millis() as u64);
                    self.rep.packs += 1;
                    self.rep.pack_bytes += p.size;
                    self.pending_pack_bytes += p.size;
                    self.hot.verified(&cs);
                    self.new_packs.push((p, cs));
                }
                Ok(Event::Failed(e)) => {
                    self.rep.errors.push(e);
                }
                Err(_) => {}
            }
            let due = self.due(last, commit_bytes, every);
            if closed {
                // final commits (possibly several, each bounded); hard links follow their data
                while self.commit_once()? {
                    let more = !self.ready.is_empty()
                        || !self.new_packs.is_empty()
                        || self.any_ready()
                        || self
                            .hard_waiting
                            .iter()
                            .any(|(_, d)| self.done_data.contains(d));
                    if !more {
                        break;
                    }
                }
                for (e, _) in self.waiting.drain(..) {
                    self.rep
                        .unreceipted
                        .push(shown(&e.rel().unwrap_or_default()));
                }
                for (m, _) in self.hard_waiting.drain(..) {
                    self.rep.unreceipted.push(shown(&self.nodes[m].rel));
                }
                return Ok(());
            }
            if due {
                // the one-second spacing runs from the start of a commit, so a commit slower
                // than that (four store round trips under a busy uplink) is followed at once
                let started = Instant::now();
                self.commit_once()?;
                last = if cfg!(feature = "broken-commit-spacing") {
                    Instant::now()
                } else {
                    started
                };
            }
        }
    }

    /// At most one commit per second, counted from the start of the previous one; each check
    /// scans the waiting list once. Freed space first: commit as soon as files worth 5% of the
    /// freeable bytes could be receipted, besides the byte, count and time triggers.
    fn due(&self, last: Instant, commit_bytes: u64, every: Duration) -> bool {
        last.elapsed() >= Duration::from_secs(1)
            && (self.pending_pack_bytes >= commit_bytes
                // readers wait for room in the hot table: commit what is verified now
                || (!self.new_packs.is_empty() && self.hot.pressed())
                || self.waiting.len() + self.ready.len() >= FILES_PER_COMMIT
                || (last.elapsed() >= every
                    && (!self.waiting.is_empty()
                        || !self.ready.is_empty()
                        || !self.new_packs.is_empty()))
                || (self.freeable > 0 && self.ready_alloc() * 20 >= self.freeable))
    }

    /// Allocated bytes of the files a commit now would receipt.
    fn ready_alloc(&self) -> u64 {
        let a = self.avail();
        let w: u64 = self
            .waiting
            .iter()
            .filter(|(e, _)| a.all(e))
            .map(|(e, _)| e.alloc)
            .sum();
        w + self.ready.iter().map(|e| e.alloc).sum::<u64>()
    }

    fn any_ready(&self) -> bool {
        let a = self.avail();
        self.waiting.iter().any(|(e, _)| a.all(e))
    }

    /// One commit: `.idx` (new packs), then `.files` (entries whose chunks are all verified),
    /// both verified, then receipts. Returns whether anything was committed.
    fn commit_once(&mut self) -> Result<bool> {
        if self.broken {
            return Ok(false);
        }
        // entries whose chunks are all stored and verified
        let mut batch: Vec<Entry> = Vec::new();
        let take = FILES_PER_COMMIT;
        while batch.len() < take {
            let Some(e) = self.ready.pop() else { break };
            batch.push(e);
        }
        let mut data_now: Vec<usize> = Vec::new();
        let mut keep = Vec::new();
        let waiting = std::mem::take(&mut self.waiting);
        {
            let a = self.avail();
            for (e, i) in waiting {
                if batch.len() < take && a.all(&e) {
                    data_now.push(i);
                    batch.push(e);
                } else {
                    keep.push((e, i));
                }
            }
        }
        self.waiting = keep;
        // hard links whose data file is committed now or before
        let now: HashSet<usize> = data_now.iter().copied().collect();
        let mut hkeep = Vec::new();
        for (m, d) in std::mem::take(&mut self.hard_waiting) {
            if batch.len() < take && (now.contains(&d) || self.done_data.contains(&d)) {
                let n = &self.nodes[m];
                let mut e = base_entry(n, Kind::Hardlink);
                let (a, h) = enc_bytes(&self.nodes[d].rel);
                e.target = a;
                e.target_hex = h;
                e.size = 0;
                e.group = Some(format!("{:x}-{:x}", n.stat.id.dev, n.stat.id.ino));
                batch.push(e);
            } else {
                hkeep.push((m, d));
            }
        }
        self.hard_waiting = hkeep;
        if batch.is_empty() && self.new_packs.is_empty() && self.seq > 0 {
            return Ok(false);
        }
        // the .idx lists every pack verified since the last commit
        let mut packs = Vec::new();
        let mut chunks = Vec::new();
        for (p, cs) in &self.new_packs {
            let pi = packs.len() as u32;
            packs.push(p.clone());
            for &(id, len) in cs {
                chunks.push(ChunkRef { id, p: pi, len });
            }
        }
        let seq = self.seq;
        let idx = IdxObj {
            v: 1,
            engine: self.engine.to_string(),
            run: self.run.to_string(),
            seq,
            root: self.root_label.to_string(),
            hash_kind: self.repo.kind,
            packs,
            chunks,
        };
        let files = FilesObj {
            v: 1,
            engine: self.engine.to_string(),
            run: self.run.to_string(),
            seq,
            root: self.root_label.to_string(),
            entries: batch,
        };
        let r = (|| -> Result<(ObjRef, ObjRef, Vec<Receipt>)> {
            let iz = encode(&idx)?;
            let ikey = idx_key(self.run, seq);
            let fz = encode(&files)?;
            let fkey = files_key(self.run, seq);
            let (t, pol) = (&*self.repo.t, self.repo.policy);
            let put_idx = || verified_put(t, &ikey, &iz, pol, &|_| {});
            let put_files = || verified_put(t, &fkey, &fz, pol, &|_| {});
            let make = |ir: &ObjRef, fr: &ObjRef| -> Vec<Receipt> {
                files
                    .entries
                    .iter()
                    .map(|e| Receipt::from_entry(e, self.repo_label, self.run, self.engine, ir, fr))
                    .collect()
            };
            let as_ref = |v: crate::transport::Verified| ObjRef {
                key: v.key,
                size: v.size,
                hash: v.hash,
            };
            if cfg!(feature = "broken-commit-order") {
                // The broken variant: receipts after the .idx alone (AC-2 must catch this).
                let iref = as_ref(put_idx()?);
                let pre = ObjRef {
                    key: fkey.clone(),
                    size: fz.len() as u64,
                    hash: self.repo.kind.digest(&fz),
                };
                self.writer.write_batch(&make(&iref, &pre))?;
                put_files()?;
                return Ok((iref, pre, Vec::new()));
            }
            // The .idx and the .files go up at once (one round trip instead of two). Receipts
            // need both verified; readers ignore a .files whose .idx is not stored.
            let (vi, vf) = std::thread::scope(|s| {
                let h = s.spawn(put_idx);
                let vf = put_files();
                let vi = h
                    .join()
                    .unwrap_or_else(|_| Err(Error::Bug("commit thread panicked".into())));
                (vi, vf)
            });
            let (iref, fref) = (as_ref(vi?), as_ref(vf?));
            if let Some(c) = self.cache {
                c.write(&iref.key, &iz);
                c.write(&fref.key, &fz);
            }
            let rs = make(&iref, &fref);
            Ok((iref, fref, rs))
        })();
        match r {
            Ok((iref, fref, rs)) => {
                if let Err(e) = self.writer.write_batch(&rs) {
                    self.rep.errors.push(format!("receipts: {e}"));
                    self.set_broken();
                    return Ok(false);
                }
                self.rep.entries += files.entries.len() as u64;
                self.rep.receipted += files.entries.len() as u64;
                for e in &files.entries {
                    match e.kind {
                        Kind::File | Kind::Hardlink => match &e.group {
                            // a hard-link group frees its blocks once every link is receipted
                            Some(g) => {
                                if let Some(left) = self.group_left.get_mut(g) {
                                    *left = left.saturating_sub(1);
                                    if *left == 0 {
                                        self.receipted_alloc += e.alloc;
                                    }
                                }
                            }
                            None => self.receipted_alloc += e.alloc,
                        },
                        _ => {}
                    }
                }
                self.rep.commit_trace.push((
                    self.t0.elapsed().as_millis() as u64,
                    self.receipted_alloc,
                    files.entries.len() as u64,
                ));
                if self.rep.t50_ms.is_none()
                    && self.freeable > 0
                    && self.receipted_alloc * 2 >= self.freeable
                {
                    self.rep.t50_ms = Some(self.t0.elapsed().as_millis() as u64);
                }
                self.done_data.extend(data_now);
                let packs = std::mem::take(&mut self.new_packs);
                // test builds only: die between a stored, receipted commit and its merge
                if cfg!(feature = "crash-hooks")
                    && !packs.is_empty()
                    && std::env::var_os("XFER_ABORT_BEFORE_MIGRATE").is_some()
                {
                    std::process::abort();
                }
                self.migrate(&packs);
                self.pending_pack_bytes = 0;
                self.commits.push(CommitRef {
                    seq,
                    idx: iref,
                    files: fref,
                });
                self.seq += 1;
                Ok(true)
            }
            Err(e) => {
                self.rep.errors.push(format!("commit {seq}: {e}"));
                for e in files.entries {
                    self.rep
                        .unreceipted
                        .push(shown(&e.rel().unwrap_or_default()));
                }
                self.set_broken();
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The hot table as shared state (Revision 6.4): readers waiting for room are woken by a
    //! failed pack and by a stopped committer, and a commit's migration never lets a chunk be
    //! claimed twice. Seeds are fixed; each wait is bounded, so a missed wake-up fails the test
    //! instead of hanging it.
    use super::*;
    use crate::repo::MIB;
    use std::sync::mpsc;

    fn ids(seed: u64, n: usize) -> Vec<ChunkId> {
        let mut x = seed.max(1);
        (0..n)
            .map(|_| {
                let mut id = [0u8; 32];
                for part in id.chunks_mut(8) {
                    x ^= x >> 12;
                    x ^= x << 25;
                    x ^= x >> 27;
                    part.copy_from_slice(&x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
                }
                ChunkId(id)
            })
            .collect()
    }

    fn cold(limit: u64) -> (Cold, Meter) {
        let mut m = Meter::new("test", limit);
        let map = ChunkMap::new(&mut m).expect("slots fit");
        (RwLock::new(map), m)
    }

    fn pack(n: usize, cs: &[ChunkId]) -> (ObjRef, Vec<(ChunkId, u32)>) {
        (
            ObjRef {
                key: format!("v1/packs/00/{n:064x}.pack"),
                size: 1,
                hash: String::new(),
            },
            cs.iter().map(|id| (*id, 1)).collect(),
        )
    }

    const BOUND: Duration = Duration::from_secs(10);

    fn wait_parked(hot: &Hot, n: usize) {
        let t = Instant::now();
        while hot.lock().parked < n {
            assert!(t.elapsed() < BOUND, "readers did not wait for room");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn readers_waiting_for_room_go_on_when_a_pack_fails() {
        let hot = Arc::new(Hot::new(1024));
        assert_eq!(hot.lock().room, 3);
        let (c, _) = cold(MIB);
        let c = Arc::new(c);
        let v = ids(5, 7);
        for id in &v[..3] {
            assert!(hot.claim(*id, &c).expect("room"));
        }
        let (tx, rx) = mpsc::channel();
        for id in v[3..7].iter().copied() {
            let (hot, c, tx) = (hot.clone(), c.clone(), tx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(hot.claim(id, &c).map_err(|e| e.to_string()));
            });
        }
        // all four wait: the table is full
        wait_parked(&hot, 4);
        assert!(rx.try_recv().is_err());
        // the pack holding the first three failed: three readers go on within the bound
        hot.remove(v[..3].iter().copied());
        for _ in 0..3 {
            let r = rx
                .recv_timeout(BOUND)
                .expect("a reader waiting for room was not woken when its room freed");
            assert_eq!(r, Ok(true));
        }
        // the fourth still waits; a stopped run releases it with an error
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        hot.halt(false, "the run stopped committing".into());
        let r = rx
            .recv_timeout(BOUND)
            .expect("a halted reader was not woken");
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn readers_waiting_for_room_stop_with_the_committer() {
        for share in [false, true] {
            let hot = Arc::new(Hot::new(1024));
            let (c, _) = cold(MIB);
            let c = Arc::new(c);
            let v = ids(9, 5);
            for id in &v[..3] {
                assert!(hot.claim(*id, &c).expect("room"));
            }
            let (tx, rx) = mpsc::channel();
            for id in v[3..5].iter().copied() {
                let (hot, c, tx) = (hot.clone(), c.clone(), tx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(hot.claim(id, &c));
                });
            }
            wait_parked(&hot, 2);
            // a commit failed (or the committed map is full): the committer halts the readers
            hot.halt(share, "stop".into());
            for _ in 0..2 {
                let r = rx
                    .recv_timeout(BOUND)
                    .expect("a waiting reader was not woken");
                match (share, r) {
                    (true, Err(Error::Budget(_))) | (false, Err(Error::Refused(_))) => {}
                    (_, r) => panic!("share {share}: {r:?}"),
                }
            }
            // a chunk already held is still a duplicate after the halt, not an error
            assert!(!hot.claim(v[0], &c).expect("a held chunk is no error"));
        }
    }

    /// Readers claim ids from a pool (every id in every reader's sequence, each reader in its
    /// own fixed order) while a committer verifies and migrates whatever the hot table holds.
    /// Each id must be claimed exactly once and the committed map must end holding exactly the
    /// pool: what a plain set gives.
    #[test]
    fn migration_racing_claims_never_claims_a_chunk_twice() {
        const READERS: usize = 4;
        let pool = ids(20261121, 4000);
        let hot = Arc::new(Hot::new(64 << 10));
        let room = hot.lock().room;
        assert!(room < pool.len(), "the race needs passes: room {room}");
        let (c, mut meter) = cold(64 * MIB);
        let c = Arc::new(c);
        let claimed = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        let mut hs = Vec::new();
        for r in 0..READERS {
            let mut order = pool.clone();
            let mut x = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(r as u64 + 1);
            for i in (1..order.len()).rev() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                order.swap(i, (x % (i as u64 + 1)) as usize);
            }
            let (hot, c, claimed, done) = (hot.clone(), c.clone(), claimed.clone(), done.clone());
            hs.push(std::thread::spawn(move || {
                for id in order {
                    if hot.claim(id, &c).expect("no halt in this test") {
                        claimed.lock().expect("claimed").push(id);
                    }
                }
                done.fetch_add(1, Ordering::SeqCst);
            }));
        }
        let mut n = 0;
        let t = Instant::now();
        loop {
            let finished = done.load(Ordering::SeqCst) == READERS;
            let batch: Vec<ChunkId> = hot.lock().ids.keys().copied().collect();
            if !batch.is_empty() {
                let p = pack(n, &batch);
                n += 1;
                hot.verified(&p.1);
                migrate(&c, &mut meter, &hot, std::slice::from_ref(&p)).expect("fits");
            }
            if finished && hot.lock().ids.is_empty() {
                break;
            }
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "the race did not finish"
            );
        }
        for h in hs {
            h.join().expect("reader");
        }
        let claimed = claimed.lock().expect("claimed").clone();
        let distinct: HashSet<ChunkId> = claimed.iter().copied().collect();
        assert_eq!(
            claimed.len(),
            distinct.len(),
            "{} chunks were claimed twice (sent twice) across {n} migrations",
            claimed.len() - distinct.len()
        );
        let want: HashSet<ChunkId> = pool.iter().copied().collect();
        assert_eq!(distinct, want);
        let cold = cold_read(&c);
        assert_eq!(cold.len(), want.len());
        assert!(want.iter().all(|id| cold.contains(id)));
        assert!(n > 1, "one migration is no race");
    }

    #[test]
    fn a_full_committed_map_keeps_the_chunks_in_the_hot_table() {
        let hot = Hot::new(64 << 10);
        let (c, mut meter) = cold(4096);
        let v = ids(3, 100);
        for id in &v {
            assert!(hot.claim(*id, &c).expect("room"));
        }
        let p = pack(0, &v);
        hot.verified(&p.1);
        assert!(matches!(
            migrate(&c, &mut meter, &hot, std::slice::from_ref(&p)),
            Err(Error::Budget(_))
        ));
        // nothing was lost: every chunk is still held, verified, so its files still commit
        let s = hot.lock();
        assert!(v.iter().all(|id| s.ids.get(id) == Some(&true)));
        assert_eq!(cold_read(&c).len(), 0);
    }
}
