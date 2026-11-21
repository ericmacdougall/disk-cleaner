//! The repository format (namespace `v1/`), its objects, the memory budget and loading.
//!
//! Layout (every object immutable):
//! - `v1/format.json`: constant bytes; a different `v1/format.json` makes a run refuse.
//! - `v1/packs/<xx>/<blake3>.pack`: segments, trailer, length, magic (see [`crate::pack`]).
//! - `v1/index/<run>/<seq:06>.idx`: the chunk map of one commit (packs and chunk locations).
//! - `v1/files/<run>/<seq:06>.files`: the file entries of one commit.
//! - `v1/snapshots/<run>.json`: written last, `complete: true`.
//!
//! Loading charges every retained entry against a named share of `--mem-budget` while parsing,
//! and stops with `Budget` as soon as a share is exceeded.

use crate::error::{Error, Result};
use crate::hash::{ChunkId, HashKind};
use crate::meta::{Identity, Kind};
use crate::pack::bounded_zstd;
use crate::transport::{verified_put, RemoteObject, RetryPolicy, Transport};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const FORMAT_KEY: &str = "v1/format.json";
/// Constant for format v1: every writer produces exactly these bytes.
pub const FORMAT_BYTES: &[u8] = b"{\"format\":\"prunedisk-xfer\",\"v\":1,\"chunker\":\"fastcdc-v2020\",\"min\":262144,\"avg\":1048576,\"max\":4194304,\"small_file\":1048576,\"chunk_hash\":\"blake3\",\"pack\":\"XFPK1\",\"segment_max\":4194304}\n";
pub const RECEIPT_V: u32 = 1;
/// One commit object, decoded, at most this large.
pub const OBJECT_JSON_MAX: usize = 64 << 20;
/// Entries per commit object.
pub const ENTRIES_MAX: usize = 2_000_000;
pub const DEFAULT_MAX_OBJECTS: usize = 100_001;
pub const LINK_MAX: usize = 4096;
/// Commit objects fetched at once while loading (each request is one store round trip).
pub const FETCH_PARALLEL: usize = 8;
/// Compressed bytes of commit objects fetched at once (a larger single object is fetched alone).
pub const FETCH_BYTES: u64 = 16 << 20;

/// Consecutive ranges of `objs` to fetch at once: at most `FETCH_PARALLEL` objects and
/// `FETCH_BYTES` listed bytes each, except that an object larger than that is a batch alone.
pub fn fetch_batches(objs: &[crate::transport::RemoteObject]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < objs.len() {
        let mut j = i + 1;
        let mut bytes = objs[i].size;
        while j < objs.len() && j - i < FETCH_PARALLEL && bytes + objs[j].size <= FETCH_BYTES {
            bytes += objs[j].size;
            j += 1;
        }
        out.push(i..j);
        i = j;
    }
    out
}

/// Crate version plus build commit: recorded in every commit object and receipt.
pub fn engine() -> String {
    format!(
        "{}+{}",
        crate::ENGINE,
        option_env!("XFER_BUILD_COMMIT").unwrap_or("unknown")
    )
}

// ---------------------------------------------------------------------------------------------
// Objects

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ObjRef {
    pub key: String,
    pub size: u64,
    /// The store's checksum (sha1 or md5, see `hash_kind`) of the object bytes.
    pub hash: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChunkRef {
    pub id: ChunkId,
    /// Index into the object's `packs`.
    pub p: u32,
    pub len: u32,
}

/// `.idx`: the packs one commit added, and where each chunk lives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdxObj {
    pub v: u32,
    pub engine: String,
    pub run: String,
    pub seq: u32,
    pub root: String,
    pub hash_kind: HashKind,
    pub packs: Vec<ObjRef>,
    pub chunks: Vec<ChunkRef>,
}

/// One file-table entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_hex: Option<String>,
    pub kind: Kind,
    pub mode: u32,
    pub mtime_ns: i64,
    pub size: u64,
    /// Source identity at backup (lstat, or fstat around the read).
    pub id: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChunkId>,
    /// Symlink target, or for a hard link the path of its group's data file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_hex: Option<String>,
    /// (name hex, value hex)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub xattrs: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub flags: u32,
    pub alloc: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default)]
    pub meta_incomplete: bool,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// Bytes as (utf8, hex) fields: UTF-8 when possible, hex otherwise.
pub fn enc_bytes(b: &[u8]) -> (Option<String>, Option<String>) {
    match std::str::from_utf8(b) {
        Ok(s) => (Some(s.to_string()), None),
        Err(_) => (None, Some(crate::util::hex(b))),
    }
}

pub fn dec_bytes(s: &Option<String>, h: &Option<String>) -> Result<Option<Vec<u8>>> {
    match (s, h) {
        (Some(s), None) => Ok(Some(s.as_bytes().to_vec())),
        (None, Some(h)) => crate::util::unhex(h)
            .map(Some)
            .ok_or_else(|| Error::corrupt("bad hex path")),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(Error::corrupt("both utf8 and hex forms present")),
    }
}

impl Entry {
    pub fn rel(&self) -> Result<Vec<u8>> {
        dec_bytes(&self.path, &self.path_hex)?.ok_or_else(|| Error::corrupt("entry without a path"))
    }
    pub fn target_bytes(&self) -> Result<Option<Vec<u8>>> {
        dec_bytes(&self.target, &self.target_hex)
    }
    pub fn xattr_bytes(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.xattrs
            .iter()
            .map(|(n, v)| {
                Ok((
                    crate::util::unhex(n).ok_or_else(|| Error::corrupt("xattr name"))?,
                    crate::util::unhex(v).ok_or_else(|| Error::corrupt("xattr value"))?,
                ))
            })
            .collect()
    }
    /// Memory this entry keeps when retained (charged against a budget share).
    pub fn charge(&self) -> u64 {
        let s = |o: &Option<String>| o.as_ref().map_or(0, |s| s.len() as u64);
        let x: u64 = self
            .xattrs
            .iter()
            .map(|(n, v)| (n.len() + v.len()) as u64 + 48)
            .sum();
        std::mem::size_of::<Entry>() as u64
            + s(&self.path)
            + s(&self.path_hex)
            + s(&self.blake3)
            + s(&self.target)
            + s(&self.target_hex)
            + s(&self.group)
            + 32 * self.chunks.len() as u64
            + x
    }
}

/// `.files`: the entries one commit made restorable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilesObj {
    pub v: u32,
    pub engine: String,
    pub run: String,
    pub seq: u32,
    pub root: String,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CommitRef {
    pub seq: u32,
    pub idx: ObjRef,
    pub files: ObjRef,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Totals {
    pub entries: u64,
    pub files: u64,
    pub bytes: u64,
    pub packs: u64,
    pub pack_bytes: u64,
    pub changed: u64,
    pub skipped: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub v: u32,
    pub engine: String,
    pub run: String,
    pub root: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub commits: Vec<CommitRef>,
    pub totals: Totals,
    pub complete: bool,
}

pub fn idx_key(run: &str, seq: u32) -> String {
    format!("v1/index/{run}/{seq:06}.idx")
}
pub fn files_key(run: &str, seq: u32) -> String {
    format!("v1/files/{run}/{seq:06}.files")
}
pub fn snapshot_key(run: &str) -> String {
    format!("v1/snapshots/{run}.json")
}

/// Run ids are generated by [`crate::util::new_run_id`]; anything else in a key is refused.
pub fn valid_run(run: &str) -> bool {
    !run.is_empty()
        && run.len() <= 64
        && run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// (run, seq) from `v1/index/<run>/<seq>.idx` or `v1/files/<run>/<seq>.files`.
pub fn parse_commit_key(key: &str) -> Option<(String, u32)> {
    let rest = key
        .strip_prefix("v1/index/")
        .and_then(|r| r.strip_suffix(".idx"))
        .or_else(|| {
            key.strip_prefix("v1/files/")
                .and_then(|r| r.strip_suffix(".files"))
        })?;
    let (run, seq) = rest.split_once('/')?;
    if !valid_run(run) || seq.len() != 6 {
        return None;
    }
    Some((run.to_string(), seq.parse().ok()?))
}

/// Serialize as zstd JSON, then re-parse before upload (the same decode-before-upload rule as
/// packs).
pub fn encode<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(v).map_err(|e| Error::Bug(format!("json: {e}")))?;
    if json.len() > OBJECT_JSON_MAX {
        return Err(Error::Budget(format!(
            "commit object of {} bytes exceeds the 64 MiB limit",
            json.len()
        )));
    }
    let z = zstd::bulk::compress(&json, 3).map_err(|e| Error::Bug(format!("zstd: {e}")))?;
    let back: T = decode(&z)?;
    let again = serde_json::to_vec(&back).map_err(|e| Error::Bug(format!("json: {e}")))?;
    if again != json {
        return Err(Error::Bug("commit object does not round-trip".into()));
    }
    Ok(z)
}

/// Decode one zstd JSON commit object, parsing as it decompresses: the JSON text is never held
/// whole, so the transient cost of an object is its decoded value plus small buffers. More than
/// `OBJECT_JSON_MAX` decoded bytes (leading whitespace included) is refused.
pub fn decode<T: for<'de> Deserialize<'de>>(z: &[u8]) -> Result<T> {
    // the broken-restore-guards variant lifts the object limit (failing-first evidence only)
    let limit = if cfg!(feature = "broken-restore-guards") {
        8 * OBJECT_JSON_MAX
    } else {
        OBJECT_JSON_MAX
    };
    if cfg!(feature = "broken-mem-plan") {
        let json = bounded_zstd(z, limit)?;
        return serde_json::from_slice(&json)
            .map_err(|e| Error::corrupt(format!("commit object: {e}")));
    }
    let over = std::cell::Cell::new(false);
    let dec =
        zstd::stream::read::Decoder::new(z).map_err(|e| Error::corrupt(format!("zstd: {e}")))?;
    let capped = Capped {
        inner: dec,
        left: limit as u64,
        over: &over,
    };
    let mut de =
        serde_json::Deserializer::from_reader(std::io::BufReader::with_capacity(64 << 10, capped));
    let v = T::deserialize(&mut de).and_then(|v| de.end().map(|()| v));
    v.map_err(|e| {
        if over.get() {
            Error::corrupt(format!("decoded data exceeds the {limit} byte limit"))
        } else if e.is_io() {
            Error::corrupt(format!("zstd: {e}"))
        } else {
            Error::corrupt(format!("commit object: {e}"))
        }
    })
}

/// A reader that fails once more than `left` bytes have come through it.
struct Capped<'a, R> {
    inner: R,
    left: u64,
    over: &'a std::cell::Cell<bool>,
}

impl<R: std::io::Read> std::io::Read for Capped<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            self.over.set(true);
            return Err(std::io::Error::other("decoded data over the limit"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------------------------
// Memory budget (Revision 6)

pub const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct Budget {
    pub total: u64,
    pub p_max: u64,
    /// chunk bytes in flight between readers and the packer
    pub c: u64,
    /// packs being sealed and checked
    pub s: u64,
    pub ser: u64,
    /// chunk maps (all runs)
    pub i: u64,
    /// walk plus the previous run's file table; at restore, one run's file table
    pub w: u64,
    /// zstd working memory of the compressors (a compression takes its level's workspace and
    /// output buffer from this share before it starts)
    pub z: u64,
    /// rclone rcd's own runtime (its upload buffers are in the 2·P_max of each upload)
    pub r: u64,
    /// concurrent uploads
    pub k: usize,
}

/// rcd's resident memory beyond its upload buffers (Go runtime, binary, connections).
pub const RCD_SHARE: u64 = 64 * MIB;
/// Of that share, what stays outside rcd's Go heap (binary text, stacks).
const RCD_NON_HEAP: u64 = 32 * MIB;

impl Budget {
    pub fn compute(total: u64, p_max: u64, k_cap: usize) -> Result<Budget> {
        if p_max > crate::pack::PACK_CEILING {
            return Err(Error::Refused(format!(
                "--max-pack {} exceeds the 128 MiB single-part ceiling",
                crate::util::human(p_max)
            )));
        }
        if p_max < crate::control::MIN_PACK {
            return Err(Error::Refused("--max-pack must be at least 16 MiB".into()));
        }
        let c = (256 * MIB).min(total / 8);
        let s = 4 * p_max;
        let ser = 32 * MIB;
        let i = (256 * MIB).min(total / 8);
        let w = total / 4;
        let z = (total / 20).max(crate::compress::min_share());
        let r = RCD_SHARE;
        let fixed = c + s + ser + i + w + z + r;
        let k = total.saturating_sub(fixed) / (2 * p_max);
        let k = (k as usize).min(k_cap.clamp(1, 16));
        if k < 1 || c == 0 || i == 0 || w == 0 {
            return Err(Error::Refused(format!(
                "--mem-budget {} leaves no room for one upload of {} (shares C {} S {} I {} W {} Z {} rcd {})",
                crate::util::human(total),
                crate::util::human(p_max),
                crate::util::human(c),
                crate::util::human(s),
                crate::util::human(i),
                crate::util::human(w),
                crate::util::human(z),
                crate::util::human(r)
            )));
        }
        Ok(Budget {
            total,
            p_max,
            c,
            s,
            ser,
            i,
            w,
            z,
            r,
            k,
        })
    }

    /// The Go heap limit (GOMEMLIMIT) rcd runs under: its K upload buffers of P_max + 1 MiB
    /// (the streaming cutoff) plus the heap part of its share.
    pub fn rcd_heap(&self) -> u64 {
        self.k as u64 * (self.p_max + MIB) + (self.r - RCD_NON_HEAP)
    }

    /// One line naming what the run may use, for the operator.
    pub fn describe(&self) -> String {
        let h = crate::util::human;
        format!(
            "memory plan within --mem-budget {}: chunks {} + sealing {} + serialization {} + chunk maps {} + walk {} + compression {} + rcd {} + {} uploads of 2 x {}",
            h(self.total),
            h(self.c),
            h(self.s),
            h(self.ser),
            h(self.i),
            h(self.w),
            h(self.z),
            h(self.r),
            self.k,
            h(self.p_max)
        )
    }
}

/// A named share of the budget, charged while parsing.
#[derive(Debug)]
pub struct Meter {
    pub what: &'static str,
    pub limit: u64,
    pub used: u64,
}

impl Meter {
    pub fn new(what: &'static str, limit: u64) -> Meter {
        Meter {
            what,
            limit,
            used: 0,
        }
    }
    pub fn charge(&mut self, n: u64) -> Result<()> {
        self.used = self.used.saturating_add(n);
        if self.used > self.limit && !cfg!(feature = "broken-restore-guards") {
            return Err(Error::Budget(format!(
                "{} needs more than its {} share of --mem-budget",
                self.what,
                crate::util::human(self.limit)
            )));
        }
        Ok(())
    }
}

/// Bytes charged per chunk-map entry (key, value and hash-map overhead).
pub const CHUNK_ENTRY_COST: u64 = 72;

// ---------------------------------------------------------------------------------------------
// Repository access

#[derive(Clone, Copy, Debug)]
pub struct Loc {
    pub pack: u32,
    pub len: u32,
}

/// The union of committed chunk maps whose packs are still listed with the recorded size and
/// hash.
#[derive(Default)]
pub struct ChunkMap {
    pub packs: Vec<ObjRef>,
    pack_ix: HashMap<String, u32>,
    pub chunks: HashMap<ChunkId, Loc>,
}

impl ChunkMap {
    pub fn pack_index(&mut self, p: &ObjRef) -> u32 {
        if let Some(&i) = self.pack_ix.get(&p.key) {
            return i;
        }
        let i = self.packs.len() as u32;
        self.packs.push(p.clone());
        self.pack_ix.insert(p.key.clone(), i);
        i
    }
    pub fn has_pack(&self, key: &str) -> bool {
        self.pack_ix.contains_key(key)
    }
    pub fn get(&self, id: &ChunkId) -> Option<(&ObjRef, u32)> {
        self.chunks
            .get(id)
            .map(|l| (&self.packs[l.pack as usize], l.len))
    }
}

pub struct Repo {
    pub t: std::sync::Arc<dyn Transport>,
    pub kind: HashKind,
    pub max_objects: usize,
    pub policy: RetryPolicy,
}

impl Repo {
    pub fn new(
        t: std::sync::Arc<dyn Transport>,
        max_objects: usize,
        policy: RetryPolicy,
    ) -> Result<Repo> {
        let kind = t.hash_kind().ok_or_else(|| Error::Unverifiable {
            key: "(remote)".into(),
            why: "the remote reports no sha1 or md5 checksum; uploads cannot be verified".into(),
        })?;
        Ok(Repo {
            t,
            kind,
            max_objects,
            policy,
        })
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>> {
        let v = self.t.list(prefix)?;
        if v.len() > self.max_objects && !cfg!(feature = "broken-repo-guards") {
            return Err(Error::Refused(format!(
                "{prefix} holds {} objects, more than the cap of {}; pass --max-objects to raise it",
                v.len(),
                self.max_objects
            )));
        }
        Ok(v)
    }

    /// Write `v1/format.json` if absent; refuse if it exists with other bytes.
    pub fn ensure_format(&self, write: bool) -> Result<()> {
        if self.format_present()? {
            return Ok(());
        }
        self.write_format(write)
    }

    /// Whether `v1/format.json` exists with this engine's bytes (false: absent). Refuses if it
    /// exists with other bytes. A stat whose size and store checksum match needs no download.
    pub fn format_present(&self) -> Result<bool> {
        let Some(o) = self.t.stat(FORMAT_KEY)? else {
            return Ok(false);
        };
        let size_ok = o.size == FORMAT_BYTES.len() as u64;
        let known = o
            .hash
            .as_deref()
            .is_some_and(|h| h.eq_ignore_ascii_case(&self.kind.digest(FORMAT_BYTES)));
        let differs = if size_ok && known {
            false
        } else {
            !size_ok || self.t.get(FORMAT_KEY, 64 << 10)? != FORMAT_BYTES
        };
        if differs && !cfg!(feature = "broken-repo-guards") {
            return Err(Error::Refused(
                "v1/format.json differs from this engine's format v1; refusing to write".into(),
            ));
        }
        Ok(true)
    }

    /// Write `v1/format.json` (only when `write`; otherwise there is no repository here).
    pub fn write_format(&self, write: bool) -> Result<()> {
        if !write {
            return Err(Error::Refused("no v1 repository at this location".into()));
        }
        verified_put(&*self.t, FORMAT_KEY, FORMAT_BYTES, self.policy, &|_| {})?;
        Ok(())
    }

    /// Fetch several commit objects at once (each checked as [`Repo::fetch`] does), keeping
    /// their order. At most `FETCH_PARALLEL` requests and about `FETCH_BYTES` are in flight.
    pub fn fetch_many(&self, objs: &[RemoteObject], cache: Option<&Cache>) -> Result<Vec<Vec<u8>>> {
        self.fetch_each(objs, cache).into_iter().collect()
    }

    /// As [`Repo::fetch_many`], with one result per object. A batch is fetched only after the
    /// previous one completed, so at most one batch of bodies is held beyond what was returned.
    pub fn fetch_each(&self, objs: &[RemoteObject], cache: Option<&Cache>) -> Vec<Result<Vec<u8>>> {
        let mut out = Vec::with_capacity(objs.len());
        for r in fetch_batches(objs) {
            let batch = &objs[r];
            if batch.len() == 1 || cfg!(feature = "broken-serial-load") {
                out.extend(batch.iter().map(|o| self.fetch(o, cache)));
            } else {
                std::thread::scope(|s| {
                    let hs: Vec<_> = batch
                        .iter()
                        .map(|o| s.spawn(move || self.fetch(o, cache)))
                        .collect();
                    out.extend(hs.into_iter().map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(Error::Bug("fetch thread panicked".into())))
                    }));
                });
            }
        }
        out
    }

    /// Fetch one commit object and check it against its listing (size and store hash).
    pub fn fetch(&self, o: &RemoteObject, cache: Option<&Cache>) -> Result<Vec<u8>> {
        if o.size > OBJECT_JSON_MAX as u64 {
            return Err(Error::corrupt(format!("{} is larger than 64 MiB", o.key)));
        }
        if let (Some(c), Some(h)) = (cache, &o.hash) {
            if let Some(b) = c.read(&o.key, o.size, self.kind, h) {
                return Ok(b);
            }
        }
        let b = self.t.get(&o.key, o.size)?;
        if b.len() as u64 != o.size {
            return Err(Error::corrupt(format!(
                "{} changed size while reading",
                o.key
            )));
        }
        if let Some(h) = &o.hash {
            if !self.kind.digest(&b).eq_ignore_ascii_case(h) {
                return Err(Error::corrupt(format!(
                    "{} does not match its listed checksum",
                    o.key
                )));
            }
            if let Some(c) = cache {
                c.write(&o.key, &b);
            }
        }
        Ok(b)
    }

    /// Listed packs: key -> (size, hash). Hashes may be absent on some backends.
    pub fn list_packs(&self) -> Result<HashMap<String, RemoteObject>> {
        Ok(self
            .list("v1/packs")?
            .into_iter()
            .filter(|o| o.key.ends_with(".pack"))
            .map(|o| (o.key.clone(), o))
            .collect())
    }

    /// Load every committed chunk map. Chunks whose pack is not listed with the recorded size
    /// and hash are dropped (they will be re-uploaded). Returns the map and the commit count.
    /// Every `.idx` object (one LIST).
    pub fn list_index(&self) -> Result<Vec<RemoteObject>> {
        Ok(self
            .list("v1/index")?
            .into_iter()
            .filter(|o| parse_commit_key(&o.key).is_some())
            .collect())
    }

    /// Returns the map, the number of index objects, and every pack any index references with
    /// its recorded size and hash (valid or not), so unreferenced and damaged packs can be named.
    pub fn load_chunk_map(
        &self,
        idx: &[RemoteObject],
        packs: &HashMap<String, RemoteObject>,
        meter: &mut Meter,
        cache: Option<&Cache>,
    ) -> Result<(ChunkMap, usize, HashMap<String, ObjRef>)> {
        let mut referenced: HashMap<String, ObjRef> = HashMap::new();
        let mut map = ChunkMap::default();
        let z_total: u64 = idx.iter().map(|o| o.size).sum();
        if z_total > meter.limit {
            return Err(Error::Budget(format!(
                "{} index objects ({} compressed) exceed the index share {}",
                idx.len(),
                crate::util::human(z_total),
                crate::util::human(meter.limit)
            )));
        }
        // fetched in parallel batches, decoded and charged one at a time in listing order; a
        // batch is decoded before the next is fetched, so at most one batch of bodies is held
        for r in fetch_batches(idx) {
            let group = &idx[r];
            let bodies = self.fetch_many(group, cache)?;
            for (o, b) in group.iter().zip(bodies) {
                self.add_index(o, &b, packs, meter, &mut map, &mut referenced)?;
                drop(b);
                crate::util::release_free_memory();
            }
        }
        Ok((map, idx.len(), referenced))
    }

    fn add_index(
        &self,
        o: &RemoteObject,
        b: &[u8],
        packs: &HashMap<String, RemoteObject>,
        meter: &mut Meter,
        map: &mut ChunkMap,
        referenced: &mut HashMap<String, ObjRef>,
    ) -> Result<()> {
        {
            let obj: IdxObj = decode(b)?;
            if obj.v != 1 {
                return Err(Error::Refused(format!("{}: unknown index version", o.key)));
            }
            if obj.chunks.len() > ENTRIES_MAX || obj.packs.len() > ENTRIES_MAX {
                return Err(Error::corrupt(format!("{}: too many entries", o.key)));
            }
            let mut local: Vec<Option<u32>> = Vec::with_capacity(obj.packs.len());
            for p in &obj.packs {
                if referenced.len() < ENTRIES_MAX {
                    referenced.insert(p.key.clone(), p.clone());
                }
                let ok = packs.get(&p.key).is_some_and(|l| {
                    l.size == p.size
                        && l.hash.as_deref().is_some_and(|h| {
                            obj.hash_kind == self.kind && h.eq_ignore_ascii_case(&p.hash)
                        })
                });
                if ok {
                    meter.charge(p.key.len() as u64 + 96)?;
                    local.push(Some(map.pack_index(p)));
                } else {
                    local.push(None);
                }
            }
            for c in &obj.chunks {
                let Some(Some(pi)) = local.get(c.p as usize) else {
                    continue;
                };
                if map
                    .chunks
                    .insert(
                        c.id,
                        Loc {
                            pack: *pi,
                            len: c.len,
                        },
                    )
                    .is_none()
                {
                    meter.charge(CHUNK_ENTRY_COST)?;
                }
            }
        }
        Ok(())
    }

    /// The file table of one run: every `.files` object of that run whose `.idx` of the same
    /// seq is also stored, in commit order. A commit puts both at once, so a `.files` without
    /// its `.idx` is a commit that never completed: nothing was receipted from it.
    pub fn load_files(
        &self,
        run: &str,
        meter: &mut Meter,
        cache: Option<&Cache>,
    ) -> Result<Vec<(RemoteObject, FilesObj)>> {
        if !valid_run(run) {
            return Err(Error::Refused(format!("bad run id {run:?}")));
        }
        let with_idx: std::collections::HashSet<u32> = self
            .list(&format!("v1/index/{run}"))?
            .into_iter()
            .filter_map(|o| parse_commit_key(&o.key).map(|(_, s)| s))
            .collect();
        let mut objs: Vec<(u32, RemoteObject)> = self
            .list(&format!("v1/files/{run}"))?
            .into_iter()
            .filter_map(|o| parse_commit_key(&o.key).map(|(_, s)| (s, o)))
            .filter(|(s, _)| with_idx.contains(s) || cfg!(feature = "broken-commit-pair"))
            .collect();
        objs.sort_by_key(|(s, _)| *s);
        self.decode_files(run, objs, meter, cache)
    }

    /// The file table of a completed run, from the commits its snapshot lists (no LIST needed).
    pub fn load_commit_files(
        &self,
        snap: &Snapshot,
        meter: &mut Meter,
        cache: Option<&Cache>,
    ) -> Result<Vec<(RemoteObject, FilesObj)>> {
        if !valid_run(&snap.run) {
            return Err(Error::Refused(format!("bad run id {:?}", snap.run)));
        }
        let mut objs = Vec::new();
        for c in &snap.commits {
            if c.files.key != files_key(&snap.run, c.seq) {
                return Err(Error::corrupt(format!(
                    "snapshot {} names {} for commit {}",
                    snap.run, c.files.key, c.seq
                )));
            }
            objs.push((
                c.seq,
                RemoteObject {
                    key: c.files.key.clone(),
                    size: c.files.size,
                    hash: Some(c.files.hash.clone()),
                },
            ));
        }
        objs.sort_by_key(|(s, _)| *s);
        self.decode_files(&snap.run, objs, meter, cache)
    }

    fn decode_files(
        &self,
        run: &str,
        objs: Vec<(u32, RemoteObject)>,
        meter: &mut Meter,
        cache: Option<&Cache>,
    ) -> Result<Vec<(RemoteObject, FilesObj)>> {
        let mut out = Vec::new();
        let all: Vec<RemoteObject> = objs.iter().map(|(_, o)| o.clone()).collect();
        for r in fetch_batches(&all) {
            let (group, listed) = (&objs[r.clone()], &all[r]);
            let bodies = self.fetch_many(listed, cache)?;
            for ((seq, o), b) in group.iter().zip(bodies) {
                let f: FilesObj = decode(&b)?;
                drop(b);
                crate::util::release_free_memory();
                if f.v != 1 || f.run != run || f.seq != *seq {
                    return Err(Error::corrupt(format!(
                        "{}: header does not match its key",
                        o.key
                    )));
                }
                if f.entries.len() > ENTRIES_MAX {
                    return Err(Error::corrupt(format!("{}: too many entries", o.key)));
                }
                for e in &f.entries {
                    meter.charge(e.charge())?;
                }
                out.push((o.clone(), f));
            }
        }
        Ok(out)
    }

    /// Every snapshot object (one LIST).
    pub fn list_snapshots(&self) -> Result<Vec<RemoteObject>> {
        Ok(self
            .list("v1/snapshots")?
            .into_iter()
            .filter(|o| o.key.ends_with(".json") && o.size <= (16 << 20))
            .collect())
    }

    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        let listed = self.list_snapshots()?;
        self.load_snapshots(&listed, None)
    }

    /// Fetch and parse listed snapshots (checked against the listing), oldest first by end time.
    /// Objects that do not parse as a v1 snapshot for their own key are ignored.
    pub fn load_snapshots(
        &self,
        listed: &[RemoteObject],
        cache: Option<&Cache>,
    ) -> Result<Vec<Snapshot>> {
        let mut v = Vec::new();
        for (o, b) in listed.iter().zip(self.fetch_each(listed, cache)) {
            // a transport failure stops the load; a body that does not match its listing is
            // ignored like one that does not parse
            let b = match b {
                Ok(b) => b,
                Err(Error::Corrupt(_)) => continue,
                Err(e) => return Err(e),
            };
            match serde_json::from_slice::<Snapshot>(&b) {
                Ok(s) if s.v == 1 && valid_run(&s.run) && o.key == snapshot_key(&s.run) => {
                    v.push(s)
                }
                _ => {}
            }
        }
        v.sort_by_key(|s| s.end_ms);
        Ok(v)
    }

    /// Run ids that have at least one `.files` commit, with their newest commit seq.
    pub fn runs(&self) -> Result<Vec<(String, u32)>> {
        let mut m: HashMap<String, u32> = HashMap::new();
        for o in self.list("v1/files")? {
            if let Some((r, s)) = parse_commit_key(&o.key) {
                let e = m.entry(r).or_insert(0);
                *e = (*e).max(s);
            }
        }
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort();
        Ok(v)
    }
}

// ---------------------------------------------------------------------------------------------
// Local cache of verified commit objects

/// Holds verified copies of commit objects (`.idx`, `.files`, snapshots: immutable under their
/// keys), fetched or written by this engine, within `--cache-budget`. Temporary files are
/// `<name>.tmp-<run>`; at start only tmp files older than one hour are removed, so a concurrent
/// run's in-flight files survive. A cached file is trusted only if its size and store checksum
/// still match the listing (or the snapshot's commit record).
pub struct Cache {
    dir: PathBuf,
    run: String,
    budget: u64,
    used: std::sync::Mutex<u64>,
}

/// Commit objects are immutable once stored under their key: `.idx`, `.files` and snapshots.
fn cacheable(key: &str) -> bool {
    key.ends_with(".idx")
        || key.ends_with(".files")
        || (key.starts_with("v1/snapshots/") && key.ends_with(".json"))
}

fn cache_name(key: &str) -> String {
    key.replace('/', "_")
}

impl Cache {
    /// None when the cache is disabled (budget 0) or the listed index bytes exceed the budget.
    pub fn open(dir: &Path, run: &str, budget: u64, listed_bytes: u64) -> Result<Option<Cache>> {
        if budget == 0 || listed_bytes > budget {
            return Ok(None);
        }
        std::fs::create_dir_all(dir)?;
        let mut used = 0u64;
        let now = std::time::SystemTime::now();
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let md = e.metadata()?;
            if name.contains(".tmp-") {
                let old = md
                    .modified()
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .is_some_and(|d| d.as_secs() > 3600)
                    || cfg!(feature = "broken-repo-guards");
                if old {
                    let _ = std::fs::remove_file(e.path());
                }
                continue;
            }
            used += md.len();
        }
        Ok(Some(Cache {
            dir: dir.to_path_buf(),
            run: run.to_string(),
            budget,
            used: std::sync::Mutex::new(used),
        }))
    }

    pub fn read(&self, key: &str, size: u64, kind: HashKind, hash: &str) -> Option<Vec<u8>> {
        if !cacheable(key) {
            return None;
        }
        let p = self.dir.join(cache_name(key));
        let b = std::fs::read(&p).ok()?;
        let trust = cfg!(feature = "broken-repo-guards");
        if trust || (b.len() as u64 == size && kind.digest(&b).eq_ignore_ascii_case(hash)) {
            Some(b)
        } else {
            // corrupt or stale: never trusted, removed and refetched
            let _ = std::fs::remove_file(&p);
            None
        }
    }

    pub fn write(&self, key: &str, data: &[u8]) {
        if !cacheable(key) {
            return;
        }
        let mut used = self.used.lock().expect("cache lock");
        if *used + data.len() as u64 > self.budget {
            return;
        }
        let name = cache_name(key);
        let fin = self.dir.join(&name);
        if fin.exists() {
            return;
        }
        let tmp = self.dir.join(format!("{name}.tmp-{}", self.run));
        let ok = std::fs::write(&tmp, data).is_ok() && std::fs::rename(&tmp, &fin).is_ok();
        if ok {
            *used += data.len() as u64;
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_profiles_match_revision_6() {
        let d = Budget::compute(1640 * MIB, 32 * MIB, 8).expect("default");
        assert_eq!(
            (d.c / MIB, d.s / MIB, d.i / MIB, d.w / MIB, d.k),
            (205, 128, 205, 410, 8)
        );
        let m = Budget::compute(640 * MIB, 16 * MIB, 4).expect("mac");
        assert_eq!(
            (m.c / MIB, m.s / MIB, m.i / MIB, m.w / MIB, m.k),
            (80, 64, 80, 160, 4)
        );
        // the compression and rcd shares (after the Mac finding: engine plus rcd at 820 MiB
        // on a 640 MiB budget) close the sum with K unchanged on both profiles
        for b in [d, m] {
            let sum = b.c + b.s + b.ser + b.i + b.w + b.z + b.r + b.k as u64 * 2 * b.p_max;
            assert!(sum <= b.total, "{b:?}: shares sum to {} MiB", sum / MIB);
        }
        assert_eq!((d.z / MIB, m.z / MIB, m.r / MIB), (82, 32, 64));
        assert_eq!(m.rcd_heap() / MIB, 4 * 17 + 32);
        assert!(Budget::compute(200 * MIB, 32 * MIB, 8).is_err());
        assert!(Budget::compute(4096 * MIB, 129 * MIB, 8).is_err());
    }
    #[test]
    fn keys() {
        assert_eq!(
            parse_commit_key("v1/index/r00000000001a-1234abcd/000003.idx"),
            Some(("r00000000001a-1234abcd".into(), 3))
        );
        assert_eq!(parse_commit_key("v1/index/../000003.idx"), None);
        assert_eq!(parse_commit_key("v1/files/r1/3.files"), None);
    }
    #[test]
    fn format_bytes_are_json() {
        let v: serde_json::Value = serde_json::from_slice(FORMAT_BYTES).expect("json");
        assert_eq!(v["v"], 1);
    }
}
